//! Akamai EdgeKV client with EdgeGrid (EG1-HMAC-SHA256) request signing.
//!
//! EdgeGrid spec: signature is HMAC-SHA256(signing_key, data_to_sign), base64.
//!   signing_key = HMAC-SHA256(client_secret, timestamp) base64
//!   data_to_sign = METHOD\tscheme\thost\tpath_and_query\tcanonical_headers\tcontent_hash\tauth_header_no_sig
//!
//! EdgeKV REST:
//!   PUT    /edgekv/v1/networks/{network}/namespaces/{ns}/groups/{group}/items/{item_key}
//!   GET    same
//!   DELETE same
//!
//! All three admin-plane components call into this module so the reconcile
//! worker retries use identical wire encoding as write-through.

use crate::model::{EkvExact, EkvPrefixEntry, HostMeta, Redirect};
use anyhow::{anyhow, Context};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hmac::{Hmac, Mac};
use rand::{distributions::Alphanumeric, Rng};
use sha2::{Digest, Sha256};
use spin_sdk::http::{Method, Request, Response};

type HmacSha256 = Hmac<Sha256>;

pub struct EkvConfig {
    pub host: String,           // akab-xxx.luna.akamaiapis.net (no scheme)
    pub client_token: String,
    pub client_secret: String,
    pub access_token: String,
    pub network: String,        // "staging" | "production"
    pub namespace: String,
    pub group: String,
}

pub fn load_config() -> anyhow::Result<EkvConfig> {
    let host = spin_sdk::variables::get("akamai_host").context("akamai_host not set")?;
    let client_token =
        spin_sdk::variables::get("akamai_client_token").context("akamai_client_token not set")?;
    let client_secret =
        spin_sdk::variables::get("akamai_client_secret").context("akamai_client_secret not set")?;
    let access_token =
        spin_sdk::variables::get("akamai_access_token").context("akamai_access_token not set")?;
    let network = spin_sdk::variables::get("ekv_network").unwrap_or_else(|_| "staging".into());
    let namespace =
        spin_sdk::variables::get("ekv_namespace").unwrap_or_else(|_| "vanity-manager".into());
    let group = spin_sdk::variables::get("ekv_group").unwrap_or_else(|_| "redirects".into());

    if host.is_empty() || client_token.is_empty() {
        return Err(anyhow!(
            "Akamai credentials incomplete — run `make provision` or set variables"
        ));
    }
    Ok(EkvConfig {
        host,
        client_token,
        client_secret,
        access_token,
        network,
        namespace,
        group,
    })
}

// ---------------------------------------------------------------------------
// Key builders — identical to the EdgeWorker's lookup shape
// ---------------------------------------------------------------------------

// Logical key shapes — documented for the EdgeWorker, which rebuilds them
// and applies encode_ekv_key() before doing an EdgeKV read. Keep these in
// sync with edgeworker/main.js.
pub fn ekv_host_key(host: &str) -> String {
    encode_ekv_key(&format!("_hosts::{host}"))
}
pub fn ekv_exact_key(host: &str, path: &str) -> String {
    encode_ekv_key(&format!("{host}::exact::{path}"))
}
pub fn ekv_prefixes_key(host: &str) -> String {
    encode_ekv_key(&format!("{host}::prefixes"))
}
pub fn ekv_prefixes_shard_key(host: &str, shard: usize) -> String {
    // Shard 0 lives at the unsuffixed key for backwards compat with
    // single-shard manifests; shards 1..N at "prefixes-1", "prefixes-2", etc.
    if shard == 0 {
        ekv_prefixes_key(host)
    } else {
        encode_ekv_key(&format!("{host}::prefixes-{shard}"))
    }
}

/// Manifest entries-per-shard. ~155 bytes per entry observed; 2500 keeps each
/// shard well under EdgeKV's 512 KB per-item cap with cushion.
pub const PREFIX_SHARD_LIMIT: usize = 2500;

// ---------------------------------------------------------------------------
// EdgeGrid signing
// ---------------------------------------------------------------------------

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn sha256(data: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().to_vec()
}

fn eg_timestamp() -> String {
    // Format: YYYYMMDDTHH:MM:SS+0000
    chrono::Utc::now().format("%Y%m%dT%H:%M:%S+0000").to_string()
}

fn eg_nonce() -> String {
    // EdgeGrid nonces are UUID-like; any unique token works.
    let r: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    format!(
        "{}-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        r
    )
}

/// Produce the full `Authorization: EG1-HMAC-SHA256 ...` header value.
fn sign(
    cfg: &EkvConfig,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> String {
    let ts = eg_timestamp();
    let nonce = eg_nonce();

    // auth header sans signature (trailing `signature=` is required)
    let auth_prefix = format!(
        "EG1-HMAC-SHA256 client_token={};access_token={};timestamp={};nonce={};",
        cfg.client_token, cfg.access_token, ts, nonce
    );

    // content_hash: base64(sha256(body)) for POST only (EdgeGrid spec).
    // PUT requests do NOT include a content hash — matches Akamai's own
    // akamai-edgegrid JS reference (node_modules/akamai-edgegrid/src/helpers.js).
    let content_hash = if method == "POST" && !body.is_empty() {
        B64.encode(sha256(body))
    } else {
        String::new()
    };

    // data_to_sign: tab-separated fields, literal newlines are NOT used
    let data_to_sign = format!(
        "{}\thttps\t{}\t{}\t\t{}\t{}",
        method.to_uppercase(),
        cfg.host,
        path_and_query,
        content_hash,
        auth_prefix
    );

    let signing_key = B64.encode(hmac(cfg.client_secret.as_bytes(), ts.as_bytes()));
    let signature = B64.encode(hmac(signing_key.as_bytes(), data_to_sign.as_bytes()));


    format!("{}signature={}", auth_prefix, signature)
}

// ---------------------------------------------------------------------------
// HTTP operations
// ---------------------------------------------------------------------------

fn item_path(cfg: &EkvConfig, item_key: &str) -> String {
    // item_key is already EdgeKV-safe (encode_ekv_key applied by callers).
    format!(
        "/edgekv/v1/networks/{}/namespaces/{}/groups/{}/items/{}",
        cfg.network, cfg.namespace, cfg.group, item_key
    )
}

/// Minimal URL-path-segment encoding (only really needed for the `/` case,
/// but applied for defence-in-depth).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'.' | b'_' | b'~'
            | b':' | b'@' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Encode a logical item key into the EdgeKV-safe character set.
/// EdgeKV accepts only `[A-Za-z0-9_-]` in item names, so every other
/// byte is escaped as `_XX` (uppercase hex). The `_` character is
/// itself escaped (`_5F`) to keep the encoding unambiguous.
///
/// The EdgeWorker's JS implementation MUST apply the identical
/// transformation — see `edgeworker/main.js` `encEkvKey()`.
pub fn encode_ekv_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' => out.push(*b as char),
            _ => out.push_str(&format!("_{:02X}", b)),
        }
    }
    out
}

async fn send(
    cfg: &EkvConfig,
    method: Method,
    method_str: &str,
    path: &str,
    body: Vec<u8>,
) -> anyhow::Result<(u16, Vec<u8>)> {
    let authorization = sign(cfg, method_str, path, &body);
    let url = format!("https://{}{}", cfg.host, path);

    let mut builder = Request::builder();
    builder
        .method(method)
        .uri(url)
        .header("authorization", authorization)
        .header("accept", "application/json");
    if !body.is_empty() {
        builder
            .header("content-type", "application/json")
            .header("content-length", body.len().to_string());
    }
    let req = builder.body(body).build();

    let resp: Response = spin_sdk::http::send(req).await.map_err(|e| anyhow!("ekv send: {e}"))?;
    Ok((*resp.status(), resp.body().to_vec()))
}

async fn put(cfg: &EkvConfig, item_key: &str, body: Vec<u8>) -> anyhow::Result<()> {
    let (status, body) = send(cfg, Method::Put, "PUT", &item_path(cfg, item_key), body).await?;
    if !(200..=299).contains(&status) {
        let msg = String::from_utf8_lossy(&body);
        return Err(anyhow!("ekv put {item_key} returned {status}: {msg}"));
    }
    Ok(())
}

async fn delete(cfg: &EkvConfig, item_key: &str) -> anyhow::Result<()> {
    let (status, body) = send(cfg, Method::Delete, "DELETE", &item_path(cfg, item_key), vec![]).await?;
    // 404 is fine — key already gone
    if !(200..=299).contains(&status) && status != 404 {
        let msg = String::from_utf8_lossy(&body);
        return Err(anyhow!("ekv delete {item_key} returned {status}: {msg}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Typed push / delete helpers
// ---------------------------------------------------------------------------

pub async fn push_host(cfg: &EkvConfig, meta: &HostMeta) -> anyhow::Result<()> {
    let payload = serde_json::json!({
        "enabled": meta.enabled,
        "case_sensitive": meta.case_sensitive,
        "rule_count": meta.rule_count,
        "updated_at": meta.updated_at,
    });
    let body = serde_json::to_vec(&payload)?;
    put(cfg, &ekv_host_key(&meta.host), body).await
}

pub async fn push_exact(cfg: &EkvConfig, rule: &Redirect) -> anyhow::Result<()> {
    let payload = EkvExact::from(rule);
    let body = serde_json::to_vec(&payload)?;
    put(cfg, &ekv_exact_key(&rule.host, &rule.path), body).await
}

/// Push a (possibly sharded) prefix manifest. If the manifest has at most
/// PREFIX_SHARD_LIMIT entries, shard 0's body is the bare entry array
/// (backwards-compat with the EdgeWorker's one-shard reader). Larger
/// manifests are split: shard 0 becomes `{"n": N, "p": [entries]}`, and
/// shards 1..N-1 are bare arrays at `prefixes-1`, `prefixes-2`, etc.
///
/// Orphan shards from a previously-larger manifest are NOT auto-deleted
/// here — callers that shrink should call `delete_prefix_shards_above`
/// with the old shard count.
pub async fn push_prefixes(
    cfg: &EkvConfig,
    host: &str,
    manifest: &[EkvPrefixEntry],
) -> anyhow::Result<()> {
    let total = manifest.len();
    if total <= PREFIX_SHARD_LIMIT {
        // Backwards-compatible single-shard format: bare array at shard 0.
        let body = serde_json::to_vec(manifest)?;
        return put(cfg, &ekv_prefixes_key(host), body).await;
    }

    let n_shards = total.div_ceil(PREFIX_SHARD_LIMIT);
    for s in 0..n_shards {
        let lo = s * PREFIX_SHARD_LIMIT;
        let hi = (lo + PREFIX_SHARD_LIMIT).min(total);
        let chunk = &manifest[lo..hi];
        let body = if s == 0 {
            // Wrap shard 0 with the total shard count so the reader knows
            // how many additional shards to pull.
            serde_json::to_vec(&serde_json::json!({"n": n_shards, "p": chunk}))?
        } else {
            serde_json::to_vec(chunk)?
        };
        put(cfg, &ekv_prefixes_shard_key(host, s), body).await?;
    }
    Ok(())
}

/// Delete shards >= `from_shard`. Use after shrinking a manifest so old
/// shards don't shadow the new (smaller) view.
pub async fn delete_prefix_shards_above(
    cfg: &EkvConfig,
    host: &str,
    from_shard: usize,
) -> anyhow::Result<()> {
    // Best-effort: try a fixed bounded range; ignore not-found errors.
    for s in from_shard..(from_shard + 32) {
        let _ = delete(cfg, &ekv_prefixes_shard_key(host, s)).await;
    }
    Ok(())
}

pub async fn delete_exact(cfg: &EkvConfig, host: &str, path: &str) -> anyhow::Result<()> {
    delete(cfg, &ekv_exact_key(host, path)).await
}

pub async fn delete_host(cfg: &EkvConfig, host: &str) -> anyhow::Result<()> {
    // Delete the sentinel and any prefix manifest shards; exact keys are
    // deleted individually via delete_exact as their rule rows are removed.
    delete(cfg, &ekv_host_key(host)).await?;
    let _ = delete_prefix_shards_above(cfg, host, 0).await;
    Ok(())
}

/// Build the sorted (longest-first) prefix manifest for a host given a set
/// of that host's rules. Callers pass a slice including both exact and
/// prefix rules; this filters to enabled prefix rules.
pub fn build_prefix_manifest(rules: &[Redirect]) -> Vec<EkvPrefixEntry> {
    use crate::model::MatchType;
    let mut entries: Vec<EkvPrefixEntry> = rules
        .iter()
        .filter(|r| r.match_type == MatchType::Prefix && r.enabled)
        .map(EkvPrefixEntry::from)
        .collect();
    entries.sort_by(|a, b| b.p.len().cmp(&a.p.len()).then(a.p.cmp(&b.p)));
    entries
}
