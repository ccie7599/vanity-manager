//! Redirect handler — EdgeKV-error fallback resolver.
//!
//! In production this component is NOT the primary serving path.
//! The EdgeWorker at 4200 Akamai locations serves redirects by reading
//! EdgeKV directly. This component exists so that when EdgeKV reads at
//! the edge fail (timeout / transient error — NOT genuine misses), the
//! EdgeWorker makes a subrequest to Functions and gets the right answer
//! from Spin KV instead of sending the user to origin uninformed.
//!
//! Primary entrypoint: GET /resolve?host=...&path=...
//! Returns:
//!   200 { "redirect": { "target": ..., "status_code": ..., ... } }  on match
//!   200 { "match": false }                                          on miss
//!
//! Secondary: the original per-request redirect path (Host header →
//! 301/302) is retained for local testing and zero-EW smoke deploys.

use shared::kv::get_json;
use shared::store::Backend;
use shared::{
    k_host, k_rule, normalize_host, normalize_path, parse_rule_key, HostMeta, MatchType, Redirect,
};
use spin_sdk::http::{IntoResponse, Request, Response};
use spin_sdk::http_component;

#[http_component]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    let path = req.path();

    // /resolve — JSON lookup used by EdgeWorker fallback
    if path == "/resolve" {
        return Ok(resolve_json(&req).await);
    }

    // Default: per-request redirect by Host header (local testing path)
    Ok(resolve_redirect(&req).await)
}

async fn resolve_json(req: &Request) -> Response {
    let host = query_param(req, "host").unwrap_or_default();
    let path = query_param(req, "path").unwrap_or_else(|| "/".into());
    let host = normalize_host(&host);
    let path = normalize_path(&path);

    match lookup(&host, &path).await {
        Ok(Some(r)) => json_ok(&serde_json::json!({
            "match": true,
            "rule": r,
        })),
        Ok(None) => json_ok(&serde_json::json!({ "match": false })),
        Err(e) => Response::builder()
            .status(500)
            .header("content-type", "application/json")
            .body(format!(r#"{{"error":"{e}"}}"#))
            .build(),
    }
}

async fn resolve_redirect(req: &Request) -> Response {
    let host = extract_host(req);
    if host.is_empty() {
        return Response::builder()
            .status(400)
            .header("content-type", "text/plain")
            .body("missing host")
            .build();
    }
    let path = normalize_path(req.path());
    let query = extract_query(req);

    let r = match lookup(&host, &path).await {
        Ok(Some(r)) => r,
        _ => {
            return Response::builder()
                .status(404)
                .header("content-type", "text/plain")
                .header("cache-control", "public, max-age=60")
                .body(format!("no redirect for {host}{path}"))
                .build();
        }
    };

    let mut location = r.target_url.clone();
    if r.match_type == MatchType::Prefix && r.preserve_path && path.len() > r.path.len() {
        let suffix = &path[r.path.len()..];
        location = format!("{}{suffix}", location.trim_end_matches('/'));
    }
    if r.preserve_query && !query.is_empty() {
        let sep = if location.contains('?') { "&" } else { "?" };
        location = format!("{location}{sep}{query}");
    }

    let cache_ttl = spin_sdk::variables::get("cache_ttl").unwrap_or_else(|_| "86400".into());
    Response::builder()
        .status(r.status_code as u16)
        .header("location", location)
        .header("cache-control", format!("public, max-age={cache_ttl}"))
        .header("x-vanity-manager", "vanity")
        .body("")
        .build()
}

async fn lookup(host: &str, path: &str) -> anyhow::Result<Option<Redirect>> {
    let backend = Backend::open_from_config()?;
    if !backend.exists(&k_host(host)).await? {
        return Ok(None);
    }

    // Apply host's case_sensitive flag.
    let lookup_path = match get_json::<HostMeta>(&backend, &k_host(host)).await? {
        Some(h) if !h.case_sensitive => path.to_lowercase(),
        _ => path.to_string(),
    };

    // Exact first.
    if let Some(r) =
        get_json::<Redirect>(&backend, &k_rule(host, MatchType::Exact, &lookup_path)).await?
    {
        if r.enabled {
            return Ok(Some(r));
        }
    }

    // Longest-prefix match among enabled prefix rules for this host.
    let prefix = shared::rule_key_prefix(host);
    let mut best: Option<Redirect> = None;
    for key in backend.keys_with_prefix(&prefix).await? {
        let (_, mt, rule_path) = match parse_rule_key(&key) {
            Some(v) => v,
            None => continue,
        };
        if mt != MatchType::Prefix {
            continue;
        }
        if lookup_path == rule_path || lookup_path.starts_with(&format!("{rule_path}/")) {
            if let Some(r) = get_json::<Redirect>(&backend, &key).await? {
                if !r.enabled {
                    continue;
                }
                let better = match &best {
                    None => true,
                    Some(b) => r.path.len() > b.path.len()
                        || (r.path.len() == b.path.len() && r.priority > b.priority),
                };
                if better {
                    best = Some(r);
                }
            }
        }
    }
    Ok(best)
}

fn json_ok(value: &serde_json::Value) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("cache-control", "public, max-age=30")
        .body(value.to_string())
        .build()
}

fn extract_host(req: &Request) -> String {
    for header in &["x-forwarded-host", "true-client-host", "host"] {
        if let Some(v) = req.header(header).and_then(|v| v.as_str()) {
            let h = v.split(':').next().unwrap_or("").trim().to_lowercase();
            if !h.is_empty() {
                return h;
            }
        }
    }
    if let Some(url) = req.header("spin-full-url").and_then(|v| v.as_str()) {
        if let Some(after) = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
        {
            let authority = after.split('/').next().unwrap_or("");
            let host = authority.split(':').next().unwrap_or("").trim().to_lowercase();
            if !host.is_empty() && host != "127.0.0.1" && host != "localhost" {
                return host;
            }
        }
    }
    String::new()
}

fn extract_query(req: &Request) -> String {
    req.header("spin-full-url")
        .and_then(|v| v.as_str())
        .and_then(|url| url.split_once('?'))
        .map(|(_, q)| q.to_string())
        .unwrap_or_default()
}

fn query_param(req: &Request, name: &str) -> Option<String> {
    req.header("spin-full-url")
        .and_then(|v| v.as_str())
        .and_then(|url| url.split_once('?'))
        .and_then(|(_, q)| {
            q.split('&').find_map(|p| {
                let (k, v) = p.split_once('=')?;
                if k == name {
                    Some(
                        v.replace("%2F", "/")
                            .replace("%3A", ":")
                            .replace('+', " "),
                    )
                } else {
                    None
                }
            })
        })
}
