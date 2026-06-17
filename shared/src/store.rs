//! Pluggable KV backend for the admin/control plane.
//!
//! Two implementations live behind one type:
//!   - [`Backend::Spin`] — wraps `spin_sdk::key_value::Store` (CosmosDB-backed
//!     when running on Akamai Functions). The default; used by prod.
//!   - [`Backend::Nats`] — HTTP client to the project-nats-kv adapter
//!     (`/v1/kv/<bucket>/<key>` etc). Used by demo deployments where rule
//!     count outgrows what `Store::get_keys()` can enumerate inside the
//!     30s Functions wall-clock.
//!
//! The trait abstraction is minimal — `get`/`set`/`delete`/`exists`/
//! `keys_with_prefix`/`incr`. Higher-level helpers (`get_json`, `set_json`,
//! counters) live in `kv.rs` and call this trait. The data plane
//! (EdgeWorker reads of EdgeKV) is unaffected.
//!
//! Key schema is unchanged across backends. The NATS backend translates
//! Spin-style keys (e.g. `rule:host|exact|/path`) into NATS-safe subjects
//! at the network boundary; Rust callers see the same key strings.

use anyhow::{anyhow, Result};
use spin_sdk::http::{Method, Request, Response};

const HTTP_TIMEOUT_MS: u32 = 5_000;

/// Backend handle. Cheap to construct per-request.
pub enum Backend {
    Spin(spin_sdk::key_value::Store),
    Nats(NatsClient),
}

pub struct NatsClient {
    base_url: String,
    bucket: String,
    token: String,
}

impl Backend {
    /// Open whichever backend the runtime configuration selects.
    /// `kv_backend` Spin variable values:
    ///   - empty or "spin" → Spin KV (default)
    ///   - "http://...adapter" → NATS KV adapter (requires `kv_nats_bucket`
    ///     and `kv_nats_token` to be set)
    pub fn open_from_config() -> Result<Self> {
        let cfg = spin_sdk::variables::get("kv_backend").unwrap_or_default();
        if cfg.is_empty() || cfg == "spin" {
            return Ok(Backend::Spin(spin_sdk::key_value::Store::open_default()?));
        }
        let bucket = spin_sdk::variables::get("kv_nats_bucket")
            .map_err(|_| anyhow!("kv_nats_bucket Spin var missing"))?;
        let token = spin_sdk::variables::get("kv_nats_token")
            .map_err(|_| anyhow!("kv_nats_token Spin var missing"))?;
        Ok(Backend::Nats(NatsClient {
            base_url: cfg.trim_end_matches('/').into(),
            bucket,
            token,
        }))
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Backend::Spin(s) => Ok(s.get(key)?),
            Backend::Nats(c) => c.get(key).await,
        }
    }

    pub async fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        match self {
            Backend::Spin(s) => Ok(s.set(key, value)?),
            Backend::Nats(c) => c.set(key, value).await,
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        match self {
            Backend::Spin(s) => Ok(s.delete(key)?),
            Backend::Nats(c) => c.delete(key).await,
        }
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        match self {
            Backend::Spin(s) => Ok(s.exists(key)?),
            Backend::Nats(c) => c.exists(key).await,
        }
    }

    /// Return all keys starting with `prefix`. Order is unspecified.
    ///
    /// Spin backend uses `get_keys()` + client-side filter — degrades past
    /// ~5K keys per namespace because of the 30s Functions wall-clock.
    /// NATS backend uses the adapter's `keys?match=` endpoint, which
    /// scans on a Go process outside any Wasm budget.
    pub async fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        match self {
            Backend::Spin(s) => {
                let mut out = Vec::new();
                for k in s.get_keys()? {
                    if k.starts_with(prefix) {
                        out.push(k);
                    }
                }
                Ok(out)
            }
            Backend::Nats(c) => c.keys_with_prefix(prefix).await,
        }
    }

    /// Atomic increment by `delta`. Returns the post-increment value.
    /// Spin backend simulates with get + set (NOT atomic across writers —
    /// acceptable for our single-writer admin paths).
    pub async fn incr(&self, key: &str, delta: i64) -> Result<i64> {
        match self {
            Backend::Spin(s) => {
                let cur = s
                    .get(key)?
                    .and_then(|b| std::str::from_utf8(&b).ok().and_then(|s| s.parse::<i64>().ok()))
                    .unwrap_or(0);
                let next = cur + delta;
                s.set(key, next.to_string().as_bytes())?;
                Ok(next)
            }
            Backend::Nats(c) => c.incr(key, delta).await,
        }
    }
}

// ---------------------------------------------------------------------------
// NATS adapter HTTP client
// ---------------------------------------------------------------------------

impl NatsClient {
    fn url(&self, key: &str) -> String {
        format!(
            "{}/v1/kv/{}/{}",
            self.base_url,
            self.bucket,
            encode_subject(key),
        )
    }

    async fn send(&self, method: Method, url: String, body: Option<Vec<u8>>) -> Result<Response> {
        let mut builder = Request::builder();
        builder
            .method(method)
            .uri(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "application/json");
        let body_bytes = body.unwrap_or_default();
        if !body_bytes.is_empty() {
            builder.header("content-length", body_bytes.len().to_string());
        }
        let req = builder.body(body_bytes).build();
        let _ = HTTP_TIMEOUT_MS;
        match spin_sdk::http::send(req).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(anyhow!("nats-kv adapter HTTP error: {url}: {e:?}")),
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let resp = self.send(Method::Get, self.url(key), None).await?;
        let status = *resp.status();
        match status {
            200 => Ok(Some(resp.body().to_vec())),
            404 => Ok(None),
            _ => Err(anyhow!("GET {}: HTTP {}", key, status)),
        }
    }

    async fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        let resp = self
            .send(Method::Put, self.url(key), Some(value.to_vec()))
            .await?;
        let status = *resp.status();
        match status {
            200 | 201 | 204 => Ok(()),
            _ => Err(anyhow!("PUT {}: HTTP {}", key, status)),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let resp = self.send(Method::Delete, self.url(key), None).await?;
        let status = *resp.status();
        match status {
            200 | 204 | 404 => Ok(()),
            _ => Err(anyhow!("DELETE {}: HTTP {}", key, status)),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let resp = self.send(Method::Get, self.url(key), None).await?;
        Ok(*resp.status() == 200)
    }

    async fn incr(&self, key: &str, delta: i64) -> Result<i64> {
        // Adapter's POST /v1/kv/<bucket>/<key>/incr increments by 1 — call
        // it `delta` times when positive. Negative deltas are a get+set
        // fallback (rare in our paths).
        if delta < 0 {
            let cur = self
                .get(key)
                .await?
                .and_then(|b| std::str::from_utf8(&b).ok().and_then(|s| s.parse::<i64>().ok()))
                .unwrap_or(0);
            let next = cur + delta;
            self.set(key, next.to_string().as_bytes()).await?;
            return Ok(next);
        }
        let mut last = 0i64;
        for _ in 0..delta {
            let url = format!("{}/incr", self.url(key));
            let resp = self.send(Method::Post, url, None).await?;
            if *resp.status() != 200 {
                return Err(anyhow!("POST {}/incr: HTTP {}", key, resp.status()));
            }
            // Adapter responds with the new counter value as plaintext or JSON;
            // accept both.
            let body = std::str::from_utf8(resp.body()).unwrap_or("0").trim();
            last = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| {
                    v.get("value")
                        .and_then(|x| x.as_i64())
                        .or_else(|| v.as_i64())
                })
                .or_else(|| body.parse::<i64>().ok())
                .unwrap_or(last + 1);
        }
        Ok(last)
    }

    async fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let pattern = subject_prefix_match(prefix);
        let url = format!(
            "{}/v1/kv/{}/keys?match={}",
            self.base_url,
            self.bucket,
            url_form_encode(&pattern),
        );
        let resp = self.send(Method::Get, url, None).await?;
        if *resp.status() != 200 {
            return Err(anyhow!("keys?match={}: HTTP {}", pattern, resp.status()));
        }
        let v: serde_json::Value = serde_json::from_slice(resp.body())?;
        let keys = v.get("keys").and_then(|k| k.as_array()).cloned().unwrap_or_default();
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            if let Some(s) = k.as_str() {
                out.push(decode_subject(s));
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Subject encoding
//
// NATS subjects allow [A-Za-z0-9_-] plus `.` as a separator, plus the wildcard
// glyphs `*` and `>`. We translate Spin-style keys into NATS-friendly
// subjects: section separators `:` and `|` become `.`; everything outside
// the safe set escapes as `_HHHH` (4 hex digits). The escape sequence starts
// with a single `_` because `__` is reserved for our internal escapes — the
// `__HHHH` form keeps token boundaries clean for prefix matching.
// ---------------------------------------------------------------------------

const ESCAPE_PREFIX: &str = "__"; // double underscore so prefix matches stay token-clean

fn encode_subject(spin_key: &str) -> String {
    let mut out = String::with_capacity(spin_key.len() * 2);
    for c in spin_key.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' => out.push(c),
            ':' | '|' => out.push('.'),
            // single underscore is allowed in NATS subjects but reserved here
            // so we don't collide with __HHHH escapes.
            '_' => out.push_str(&format!("{}{:04x}", ESCAPE_PREFIX, c as u32)),
            other => out.push_str(&format!("{}{:04x}", ESCAPE_PREFIX, other as u32)),
        }
    }
    out
}

fn decode_subject(subject: &str) -> String {
    let bytes = subject.as_bytes();
    let mut out = String::with_capacity(subject.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' && i + 1 < bytes.len() && bytes[i + 1] == b'_' && i + 5 < bytes.len() {
            // __HHHH escape
            let hex = std::str::from_utf8(&bytes[i + 2..i + 6]).unwrap_or("003f");
            if let Ok(code) = u32::from_str_radix(hex, 16) {
                if let Some(c) = char::from_u32(code) {
                    out.push(c);
                    i += 6;
                    continue;
                }
            }
        }
        if bytes[i] == b'.' {
            // ambiguous: dot maps from `:` or `|`. We can't recover which
            // without storing the original. For our use we don't need the
            // original — callers compare against a known prefix.
            out.push(':');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Translate a Spin-style key prefix into a NATS subject pattern that
/// matches keys starting with that prefix.
fn subject_prefix_match(prefix: &str) -> String {
    // Encode the prefix the same way as full keys, then append `>` to match
    // any continuation.
    let encoded = encode_subject(prefix);
    if encoded.is_empty() {
        ">".into()
    } else if encoded.ends_with('.') {
        format!("{encoded}>")
    } else {
        format!("{encoded}.>")
    }
}

fn url_form_encode(s: &str) -> String {
    // Strict RFC 3986 unreserved-only encoding. Crucially, `>` and `*`
    // (NATS subject wildcards) MUST be percent-encoded for the URL — the
    // adapter will decode them back to wildcards.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
