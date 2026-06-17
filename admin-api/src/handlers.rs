//! HTTP route handlers for the admin API.

use crate::snapshot;
use shared::kv::{get_json, read_counter, set_json};
use shared::store::Backend;
use shared::{
    k_host, k_rule, normalize_host, normalize_path, parse_rule_key, rule_key_prefix, sync,
    HostMeta, MatchType, Redirect, COUNTER_EKV_ERRORS, COUNTER_EKV_PUSHES,
    COUNTER_PENDING_DRAINED, META_LAST_EKV_PUSH, META_LAST_SNAPSHOT, PREFIX_HOST,
    PREFIX_PENDING_EKV, PREFIX_RULE,
};
use spin_sdk::http::{Params, Request, Response};

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

pub fn json_resp(status: u16, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .body(body.to_string())
        .build()
}

fn json_ok<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_string(value) {
        Ok(s) => json_resp(200, &s),
        Err(e) => json_resp(500, &format!(r#"{{"error":"json: {e}"}}"#)),
    }
}

fn json_created<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_string(value) {
        Ok(s) => json_resp(201, &s),
        Err(e) => json_resp(500, &format!(r#"{{"error":"json: {e}"}}"#)),
    }
}

fn json_err(status: u16, msg: &str) -> Response {
    json_resp(
        status,
        &format!(r#"{{"error":"{}"}}"#, msg.replace('"', "\\\"")),
    )
}

// ---------------------------------------------------------------------------
// Style mode — Zoolander (default) or Plain (query param ?zoolander=off,
// stickied via the vm-style cookie). Used by HTML + SVG handlers.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Zoolander,
    Plain,
}

pub fn pick_style(req: &Request) -> Style {
    // Query param wins. ?zoolander=on/1/true → goofy mode; off/0/false → plain.
    if let Some(v) = query_param(req, "zoolander") {
        let lc = v.to_lowercase();
        if matches!(lc.as_str(), "on" | "1" | "true" | "yes" | "zoolander") {
            return Style::Zoolander;
        }
        if matches!(lc.as_str(), "off" | "0" | "false" | "no" | "plain") {
            return Style::Plain;
        }
    }
    if let Some(s) = query_param(req, "style") {
        if s.eq_ignore_ascii_case("zoolander") {
            return Style::Zoolander;
        }
    }
    // Cookie fallback — only zoolander mode is sticky.
    if let Some(c) = req.header("cookie").and_then(|h| h.as_str()) {
        for kv in c.split(';') {
            if let Some(v) = kv.trim().strip_prefix("vm-style=") {
                if v == "zoolander" {
                    return Style::Zoolander;
                }
            }
        }
    }
    Style::Plain
}

/// Build a Set-Cookie header value for the chosen style. Only emit when the
/// style was specified explicitly via query param.
fn style_cookie_for(req: &Request, style: Style) -> Option<String> {
    let q = query_param(req, "zoolander").or_else(|| query_param(req, "style"));
    q?;
    Some(match style {
        Style::Zoolander => "vm-style=zoolander; Path=/; Max-Age=2592000; SameSite=Lax".into(),
        Style::Plain => "vm-style=; Path=/; Max-Age=0; SameSite=Lax".into(),
    })
}

fn html_resp(req: &Request, body: &'static str) -> Response {
    let mut b = Response::builder();
    b.status(200)
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", "no-cache")
        .header("vary", "cookie");
    if let Some(c) = style_cookie_for(req, pick_style(req)) {
        b.header("set-cookie", c);
    }
    b.body(body).build()
}

// ---------------------------------------------------------------------------
// UI + health (public)
// ---------------------------------------------------------------------------

pub fn serve_ui(req: &Request) -> Response {
    html_resp(req, include_str!("../ui/index.html"))
}

pub fn serve_docs(req: &Request) -> Response {
    html_resp(req, include_str!("../ui/docs.html"))
}

pub fn serve_readme(req: &Request) -> Response {
    html_resp(req, include_str!("../ui/readme.html"))
}

pub fn serve_openapi() -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "application/yaml")
        .header("cache-control", "public, max-age=300")
        .body(include_str!("../ui/openapi.yaml"))
        .build()
}

pub fn serve_architecture_svg(req: &Request) -> Response {
    let body = match pick_style(req) {
        Style::Plain => include_str!("../ui/architecture-plain.svg"),
        Style::Zoolander => include_str!("../ui/architecture.svg"),
    };
    let mut b = Response::builder();
    b.status(200)
        .header("content-type", "image/svg+xml")
        .header("cache-control", "public, max-age=300")
        .header("vary", "cookie");
    if let Some(c) = style_cookie_for(req, pick_style(req)) {
        b.header("set-cookie", c);
    }
    b.body(body).build()
}

pub fn serve_features_svg(req: &Request) -> Response {
    let body = match pick_style(req) {
        Style::Plain => include_str!("../ui/features-plain.svg"),
        Style::Zoolander => include_str!("../ui/features.svg"),
    };
    let mut b = Response::builder();
    b.status(200)
        .header("content-type", "image/svg+xml")
        .header("cache-control", "public, max-age=300")
        .header("vary", "cookie");
    if let Some(c) = style_cookie_for(req, pick_style(req)) {
        b.header("set-cookie", c);
    }
    b.body(body).build()
}

pub fn serve_doc_source(name: &str) -> Response {
    let body = match name {
        "readme" => include_str!("../../README.md"),
        "decisions" => include_str!("../../DECISIONS.md"),
        "scope" => include_str!("../../SCOPE.md"),
        "ds2" => include_str!("../../docs/ds2-fields.md"),
        _ => {
            return Response::builder()
                .status(404)
                .header("content-type", "text/plain")
                .body("unknown doc")
                .build()
        }
    };
    // Rewrite repo-relative SVG references (which GitHub renders correctly)
    // to the in-app endpoints that serve the embedded plain-mode SVGs.
    // Markdown is served as-is on GH; the in-app docs viewer needs the
    // absolute API paths.
    let rewritten = body
        .replace("./docs/architecture.svg", "/api/v1/architecture.svg")
        .replace("./docs/features.svg", "/api/v1/features.svg");
    Response::builder()
        .status(200)
        .header("content-type", "text/markdown; charset=utf-8")
        .header("cache-control", "public, max-age=300")
        .body(rewritten)
        .build()
}

pub fn health() -> Response {
    match Backend::open_from_config() {
        Ok(_) => json_resp(200, r#"{"status":"healthy","service":"vanity-manager"}"#),
        Err(_) => json_resp(503, r#"{"status":"unhealthy","reason":"kv open failed"}"#),
    }
}

// ---------------------------------------------------------------------------
// Rule CRUD
// ---------------------------------------------------------------------------

pub async fn list_rules(req: Request, _params: Params) -> anyhow::Result<Response> {
    let backend = Backend::open_from_config()?;
    let host_filter = query_param(&req, "host").map(|h| normalize_host(&h));
    let limit = query_param(&req, "limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200)
        .min(2000);
    let offset = query_param(&req, "offset")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    // First pass: collect matching rule keys (cheap — no per-rule reads).
    let prefix = match &host_filter {
        Some(h) => rule_key_prefix(h),
        None => PREFIX_RULE.to_string(),
    };
    let mut keys: Vec<String> = backend.keys_with_prefix(&prefix).await?;
    keys.sort();

    let matched = keys.len();
    let page = keys.into_iter().skip(offset).take(limit);

    let mut rules = Vec::with_capacity(limit);
    for key in page {
        if let Some(r) = get_json::<Redirect>(&backend, &key).await? {
            rules.push(r);
        }
    }
    rules.sort_by(|a, b| (a.host.as_str(), a.path.as_str()).cmp(&(b.host.as_str(), b.path.as_str())));

    Ok(json_resp(
        200,
        &serde_json::json!({
            "items": rules,
            "matched_total": matched,
            "limit": limit,
            "offset": offset,
        })
        .to_string(),
    ))
}

pub async fn create_rule(req: Request, _params: Params) -> anyhow::Result<Response> {
    let async_ekv = query_param(&req, "async")
        .map(|s| matches!(s.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let mut r: Redirect = match serde_json::from_slice(req.body()) {
        Ok(r) => r,
        Err(e) => return Ok(json_err(400, &format!("invalid json: {e}"))),
    };
    if let Err(e) = validate_rule(&r) {
        return Ok(json_err(400, &e));
    }
    r.host = normalize_host(&r.host);
    r.path = normalize_path(&r.path);
    r.updated_at = chrono::Utc::now().to_rfc3339();
    r.ekv_synced_at = String::new();

    let backend = Backend::open_from_config()?;
    apply_ci(&backend, &mut r).await?;
    let key = k_rule(&r.host, r.match_type, &r.path);
    if backend.exists(&key).await? {
        return Ok(json_err(409, "rule already exists — use PUT to update"));
    }
    set_json(&backend, &key, &r).await?;
    ensure_host_meta(&backend, &r.host, 1).await?;

    // EdgeKV write: inline (sync, ~1-2s per call) by default; ?async=1 just
    // queues a pending-ekv marker and returns immediately. The reconcile
    // worker drains markers in a separate Functions invocation.
    if async_ekv {
        sync::enqueue_rule_push(&backend, &key).await;
        sync::enqueue_host_push(&backend, &r.host).await;
    } else {
        sync::push_rule_inline(&backend, &key).await;
        sync::push_host_inline(&backend, &r.host).await;
    }

    // Snapshot publish is decoupled from the mutation hot path — call
    // POST /api/v1/snapshot explicitly to refresh the S3 catalog.
    Ok(json_created(&r))
}

pub async fn update_rule(req: Request, params: Params) -> anyhow::Result<Response> {
    let async_ekv = query_param(&req, "async")
        .map(|s| matches!(s.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let (host, mt, path) = match parse_composite(&params) {
        Some(v) => v,
        None => return Ok(json_err(400, "invalid composite key in URL")),
    };

    let mut patch: Redirect = match serde_json::from_slice(req.body()) {
        Ok(r) => r,
        Err(e) => return Ok(json_err(400, &format!("invalid json: {e}"))),
    };
    if let Err(e) = validate_rule(&patch) {
        return Ok(json_err(400, &e));
    }
    patch.host = normalize_host(&patch.host);
    patch.path = normalize_path(&patch.path);
    // PUT does not flip host case-insensitivity (which would orphan existing
    // mixed-case rule keys). To flip a host to ci, re-import all its rules.
    patch.case_insensitive = false;

    if patch.host != host || patch.match_type != mt || patch.path != path {
        return Ok(json_err(
            400,
            "body (host, match_type, path) must match URL composite key",
        ));
    }
    patch.updated_at = chrono::Utc::now().to_rfc3339();
    patch.ekv_synced_at = String::new();

    let backend = Backend::open_from_config()?;
    apply_ci(&backend, &mut patch).await?;
    let key = k_rule(&host, mt, &patch.path);
    if !backend.exists(&key).await? {
        return Ok(json_err(404, "rule not found"));
    }
    set_json(&backend, &key, &patch).await?;
    if async_ekv {
        sync::enqueue_rule_push(&backend, &key).await;
    } else {
        sync::push_rule_inline(&backend, &key).await;
    }

    Ok(json_ok(&patch))
}

pub async fn delete_rule(req: Request, params: Params) -> anyhow::Result<Response> {
    let async_ekv = query_param(&req, "async")
        .map(|s| matches!(s.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let (host, mt, path) = match parse_composite(&params) {
        Some(v) => v,
        None => return Ok(json_err(400, "invalid composite key in URL")),
    };
    let backend = Backend::open_from_config()?;
    let key = k_rule(&host, mt, &path);
    if !backend.exists(&key).await? {
        return Ok(json_err(404, "rule not found"));
    }
    backend.delete(&key).await?;
    decrement_host_meta(&backend, &host).await?;

    if async_ekv {
        sync::enqueue_rule_delete(&backend, &key).await;
        sync::enqueue_host_push(&backend, &host).await;
    } else {
        sync::push_delete_rule_inline(&backend, &key).await;
        sync::push_host_inline(&backend, &host).await;
    }

    Ok(json_resp(200, r#"{"deleted":true}"#))
}

pub async fn get_rule(_req: Request, params: Params) -> anyhow::Result<Response> {
    let (host, mt, path) = match parse_composite(&params) {
        Some(v) => v,
        None => return Ok(json_err(400, "invalid composite key in URL")),
    };
    let backend = Backend::open_from_config()?;
    let key = k_rule(&host, mt, &path);
    match get_json::<Redirect>(&backend, &key).await? {
        Some(r) => Ok(json_ok(&r)),
        None => Ok(json_err(404, "rule not found")),
    }
}

// ---------------------------------------------------------------------------
// Bulk import / export
//
// Default mode is `upsert` — payload adds new rules and updates changed ones;
// rules absent from the payload are LEFT IN PLACE. `?mode=replace` provides
// full-sync semantics: anything not in the payload is deleted.
//
// Response: {mode, total, added, updated, unchanged, deleted, errors[],
//            duration_ms}.
// ---------------------------------------------------------------------------

const IMPORT_LOCK_KEY: &str = "_import_lock";
const IMPORT_LOCK_STALE_SEC: i64 = 600;

async fn acquire_import_lock(b: &Backend) -> anyhow::Result<bool> {
    if let Some(bytes) = b.get(IMPORT_LOCK_KEY).await? {
        if let Ok(s) = std::str::from_utf8(&bytes) {
            if let Ok(prev) = chrono::DateTime::parse_from_rfc3339(s) {
                let age = chrono::Utc::now()
                    .signed_duration_since(prev.with_timezone(&chrono::Utc));
                if age.num_seconds() < IMPORT_LOCK_STALE_SEC {
                    return Ok(false);
                }
            }
        }
    }
    b.set(IMPORT_LOCK_KEY, chrono::Utc::now().to_rfc3339().as_bytes())
        .await?;
    Ok(true)
}

async fn release_import_lock(b: &Backend) {
    let _ = b.delete(IMPORT_LOCK_KEY).await;
}

/// Compare two rules ignoring fields that change on every write.
fn rule_content_eq(a: &Redirect, b: &Redirect) -> bool {
    a.target_url == b.target_url
        && a.status_code == b.status_code
        && a.match_type == b.match_type
        && a.preserve_path == b.preserve_path
        && a.preserve_query == b.preserve_query
        && a.enabled == b.enabled
        && a.priority == b.priority
        && a.notes == b.notes
}

pub async fn import_rules(req: Request, _params: Params) -> anyhow::Result<Response> {
    let started = chrono::Utc::now();
    let rules: Vec<Redirect> = match serde_json::from_slice(req.body()) {
        Ok(v) => v,
        Err(e) => return Ok(json_err(400, &format!("invalid json: {e}"))),
    };
    let mode = query_param(&req, "mode")
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "upsert".into());
    let replace = match mode.as_str() {
        "upsert" | "merge" | "" => false,
        "replace" | "sync" | "full" => true,
        other => return Ok(json_err(400, &format!("unknown mode: {other}"))),
    };

    let backend = Backend::open_from_config()?;
    let force = query_param(&req, "force")
        .map(|s| matches!(s.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let async_ekv = query_param(&req, "async")
        .map(|s| matches!(s.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if force {
        let _ = backend.delete(IMPORT_LOCK_KEY).await;
    }
    if !acquire_import_lock(&backend).await? {
        return Ok(json_err(409, "import already in progress (use ?force=1 to override)"));
    }
    let result = run_import(&backend, started, replace, async_ekv, &rules).await;
    release_import_lock(&backend).await;
    result
}

async fn run_import(
    b: &Backend,
    started: chrono::DateTime<chrono::Utc>,
    replace: bool,
    async_ekv: bool,
    rules: &[Redirect],
) -> anyhow::Result<Response> {
    let mut added = 0u64;
    let mut updated = 0u64;
    let mut unchanged = 0u64;
    let mut deleted = 0u64;
    let mut errors: Vec<String> = Vec::new();
    let mut touched_hosts = std::collections::BTreeSet::<String>::new();
    let mut payload_keys = std::collections::HashSet::<String>::with_capacity(rules.len());

    for mut r in rules.iter().cloned() {
        if let Err(e) = validate_rule(&r) {
            errors.push(format!("{} {}: {}", r.host, r.path, e));
            continue;
        }
        r.host = normalize_host(&r.host);
        r.path = normalize_path(&r.path);
        if let Err(e) = apply_ci(b, &mut r).await {
            errors.push(format!("{} {}: ci: {e}", r.host, r.path));
            continue;
        }
        let key = k_rule(&r.host, r.match_type, &r.path);
        payload_keys.insert(key.clone());

        let existing = match get_json::<Redirect>(b, &key).await {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{} {}: get: {e}", r.host, r.path));
                continue;
            }
        };
        if let Some(prev) = existing.as_ref() {
            if rule_content_eq(prev, &r) {
                unchanged += 1;
                continue;
            }
        }

        r.updated_at = started.to_rfc3339();
        r.ekv_synced_at = String::new();
        if let Err(e) = set_json(b, &key, &r).await {
            errors.push(format!("{} {}: set: {e}", r.host, r.path));
            continue;
        }
        if existing.is_none() {
            if let Err(e) = ensure_host_meta(b, &r.host, 1).await {
                errors.push(format!("{} {}: host_meta: {e}", r.host, r.path));
                continue;
            }
            added += 1;
        } else {
            updated += 1;
        }
        if async_ekv {
            sync::enqueue_rule_push(b, &key).await;
        } else {
            sync::push_rule_inline(b, &key).await;
        }
        touched_hosts.insert(r.host.clone());
    }

    if replace {
        let all_keys = b.keys_with_prefix(PREFIX_RULE).await.unwrap_or_default();
        for key in all_keys {
            if !key.starts_with(PREFIX_RULE) {
                continue;
            }
            if payload_keys.contains(&key) {
                continue;
            }
            let host = match parse_rule_key(&key) {
                Some((h, _, _)) => h,
                None => continue,
            };
            if let Err(e) = b.delete(&key).await {
                errors.push(format!("delete {key}: {e}"));
                continue;
            }
            if let Err(e) = decrement_host_meta(b, &host).await {
                errors.push(format!("delete {key}: host_meta: {e}"));
                continue;
            }
            if async_ekv {
                sync::enqueue_rule_delete(b, &key).await;
            } else {
                sync::push_delete_rule_inline(b, &key).await;
            }
            touched_hosts.insert(host);
            deleted += 1;
        }
    }

    for host in &touched_hosts {
        if async_ekv {
            sync::enqueue_host_push(b, host).await;
        } else {
            sync::push_host_inline(b, host).await;
        }
    }

    // Catalog snapshot is no longer published per-import — it scaled poorly
    // past a few thousand rules (sequential GETs blew the 30s wall-clock).
    // Operators trigger it explicitly via POST /api/v1/snapshot.
    let _ = (added, updated, deleted);

    let duration_ms = chrono::Utc::now()
        .signed_duration_since(started)
        .num_milliseconds();

    Ok(json_resp(
        200,
        &serde_json::json!({
            "mode": if replace { "replace" } else { "upsert" },
            "ekv_sync": if async_ekv { "queued" } else { "inline" },
            "total": rules.len(),
            "added": added,
            "updated": updated,
            "unchanged": unchanged,
            "deleted": deleted,
            "errors": errors,
            "duration_ms": duration_ms,
        })
        .to_string(),
    ))
}

/// Bulk-stamp `ekv_synced_at` on rules and host_meta. Use after an
/// out-of-band EKV bulk load (push_ekv.py) so the UI stops showing
/// every rule as PENDING. Operator asserts the data is in EKV by
/// calling this. Incremental: stamps up to `?budget=` items per call
/// (default 1000), `?cursor=<offset>` for resume. Returns next_cursor
/// and remaining so callers can loop. Stops 5s before wall-clock.
pub async fn mark_synced(req: Request, _params: Params) -> anyhow::Result<Response> {
    let started = chrono::Utc::now();
    let backend = Backend::open_from_config()?;
    let budget: usize = query_param(&req, "budget")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let cursor: usize = query_param(&req, "cursor")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let kind = query_param(&req, "kind").unwrap_or_else(|| "rules".to_string());
    let prefix = match kind.as_str() {
        "hosts" => PREFIX_HOST,
        _ => PREFIX_RULE,
    };
    let mut keys = backend.keys_with_prefix(prefix).await?;
    keys.sort(); // stable cursor across calls
    let total = keys.len();
    let now = chrono::Utc::now().to_rfc3339();
    let mut stamped = 0u64;
    let mut errors = 0u64;
    let stop_after_ms = 25_000i64;
    let mut idx = cursor;
    let end = (cursor + budget).min(total);
    while idx < end {
        let key = &keys[idx];
        let ok = if kind == "hosts" {
            match get_json::<HostMeta>(&backend, key).await {
                Ok(Some(mut h)) => {
                    if h.ekv_synced_at.is_empty() {
                        h.ekv_synced_at = now.clone();
                        set_json(&backend, key, &h).await.is_ok()
                    } else {
                        true
                    }
                }
                _ => false,
            }
        } else {
            match get_json::<Redirect>(&backend, key).await {
                Ok(Some(mut r)) => {
                    if r.ekv_synced_at.is_empty() {
                        r.ekv_synced_at = now.clone();
                        set_json(&backend, key, &r).await.is_ok()
                    } else {
                        true
                    }
                }
                _ => false,
            }
        };
        if ok { stamped += 1; } else { errors += 1; }
        idx += 1;
        let elapsed = chrono::Utc::now().signed_duration_since(started).num_milliseconds();
        if elapsed >= stop_after_ms {
            break;
        }
    }
    let remaining = total.saturating_sub(idx);
    Ok(json_resp(
        200,
        &serde_json::json!({
            "kind": kind,
            "stamped": stamped,
            "errors": errors,
            "next_cursor": idx,
            "remaining": remaining,
            "duration_ms": chrono::Utc::now()
                .signed_duration_since(started)
                .num_milliseconds(),
        })
        .to_string(),
    ))
}

/// Bulk-delete pending-ekv:* markers without re-pushing to EdgeKV. Use when
/// EKV is known to be in sync via an out-of-band path (e.g. direct bulk
/// load via tools/convert/push_ekv.py) and the marker queue is
/// just leftover bookkeeping that should be cleared. Deletes up to
/// `?budget=` markers per call (default 1000); returns counts so the
/// caller can loop until remaining=0. Always stops 5s before the Functions
/// wall-clock to keep the response from timing out.
pub async fn clear_pending(req: Request, _params: Params) -> anyhow::Result<Response> {
    let started = chrono::Utc::now();
    let backend = Backend::open_from_config()?;
    let budget: usize = query_param(&req, "budget")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let keys = backend.keys_with_prefix(PREFIX_PENDING_EKV).await?;
    let total = keys.len();
    let mut deleted = 0u64;
    let mut errors = 0u64;
    let stop_after_ms = 25_000i64; // leave 5s headroom under Functions 30s cap
    for k in keys.iter().take(budget) {
        match backend.delete(k).await {
            Ok(_) => deleted += 1,
            Err(_) => errors += 1,
        }
        let elapsed = chrono::Utc::now().signed_duration_since(started).num_milliseconds();
        if elapsed >= stop_after_ms {
            break;
        }
    }
    let processed = (deleted + errors) as usize;
    let remaining = total.saturating_sub(processed);
    Ok(json_resp(
        200,
        &serde_json::json!({
            "deleted": deleted,
            "errors": errors,
            "remaining": remaining,
            "duration_ms": chrono::Utc::now()
                .signed_duration_since(started)
                .num_milliseconds(),
        })
        .to_string(),
    ))
}

/// Trigger an S3 catalog snapshot publish on demand. Decoupled from the
/// import / mutation hot paths so a single rule write doesn't have to scan
/// 24K keys. Best run from a scheduled job; client should expect this to
/// take longer than 30s on large catalogs and may time out.
pub async fn publish_snapshot_now(
    _req: Request,
    _params: Params,
) -> anyhow::Result<Response> {
    let backend = Backend::open_from_config()?;
    match snapshot::publish_snapshot(&backend).await {
        Ok(()) => Ok(json_resp(200, r#"{"published":true}"#)),
        Err(e) => Ok(json_err(500, &format!("snapshot publish failed: {e}"))),
    }
}

pub async fn export_rules(_req: Request, _params: Params) -> anyhow::Result<Response> {
    let backend = Backend::open_from_config()?;
    let mut rules = Vec::new();
    for key in backend.keys_with_prefix(PREFIX_RULE).await? {
        if let Some(r) = get_json::<Redirect>(&backend, &key).await? {
            rules.push(r);
        }
    }
    rules.sort_by(|a, b| (a.host.as_str(), a.path.as_str()).cmp(&(b.host.as_str(), b.path.as_str())));
    let body = serde_json::to_string_pretty(&rules)?;
    Ok(json_resp(200, &body))
}

// ---------------------------------------------------------------------------
// Host metadata
// ---------------------------------------------------------------------------

pub async fn list_hosts(_req: Request, _params: Params) -> anyhow::Result<Response> {
    let backend = Backend::open_from_config()?;
    let mut hosts = Vec::new();
    for key in backend.keys_with_prefix(PREFIX_HOST).await? {
        if let Some(h) = get_json::<HostMeta>(&backend, &key).await? {
            hosts.push(h);
        }
    }
    hosts.sort_by(|a, b| a.host.cmp(&b.host));
    Ok(json_ok(&hosts))
}

pub async fn update_host(req: Request, params: Params) -> anyhow::Result<Response> {
    let async_ekv = query_param(&req, "async")
        .map(|s| matches!(s.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let host = match params.get("host") {
        Some(h) if !h.is_empty() => normalize_host(h),
        _ => return Ok(json_err(400, "missing host")),
    };

    let mut patch: HostMeta = match serde_json::from_slice(req.body()) {
        Ok(h) => h,
        Err(e) => return Ok(json_err(400, &format!("invalid json: {e}"))),
    };
    patch.host = host.clone();
    patch.updated_at = chrono::Utc::now().to_rfc3339();
    patch.ekv_synced_at = String::new();

    let backend = Backend::open_from_config()?;
    if let Some(existing) = get_json::<HostMeta>(&backend, &k_host(&host)).await? {
        patch.rule_count = existing.rule_count;
    }
    set_json(&backend, &k_host(&host), &patch).await?;
    if async_ekv {
        sync::enqueue_host_push(&backend, &host).await;
    } else {
        sync::push_host_inline(&backend, &host).await;
    }

    Ok(json_ok(&patch))
}

// ---------------------------------------------------------------------------
// Stats + sync status
// ---------------------------------------------------------------------------

pub async fn stats(_req: Request, _params: Params) -> anyhow::Result<Response> {
    let backend = Backend::open_from_config()?;
    let mut total = 0u64;
    let mut enabled = 0u64;
    let by_type = std::collections::BTreeMap::<&str, u64>::new();
    let by_status = std::collections::BTreeMap::<i64, u64>::new();
    let mut unique_hosts = 0u64;

    // Cheap path: enumerate host metadata only. Each host's rule_count is
    // maintained on every mutation, so summing avoids reading every rule.
    for key in backend.keys_with_prefix(PREFIX_HOST).await? {
        unique_hosts += 1;
        if let Some(meta) = get_json::<HostMeta>(&backend, &key).await? {
            total += meta.rule_count;
            if meta.enabled {
                enabled += meta.rule_count;
            }
        }
    }
    let pending = backend.keys_with_prefix(PREFIX_PENDING_EKV).await?.len() as u64;

    let last_snapshot = backend
        .get(META_LAST_SNAPSHOT)
        .await?
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    let last_ekv_push = backend
        .get(META_LAST_EKV_PUSH)
        .await?
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    let ekv_pushes = read_counter(&backend, COUNTER_EKV_PUSHES).await;
    let ekv_errors = read_counter(&backend, COUNTER_EKV_ERRORS).await;
    let drained = read_counter(&backend, COUNTER_PENDING_DRAINED).await;

    let resp = serde_json::json!({
        "total_rules": total,
        "enabled": enabled,
        "disabled": total - enabled,
        "unique_hosts": unique_hosts,
        "by_match_type": by_type,
        "by_status_code": by_status,
        "sync": {
            "pending_ekv_pushes": pending,
            "last_snapshot_at": last_snapshot,
            "last_ekv_push_at": last_ekv_push,
            "ekv_pushes_total": ekv_pushes,
            "ekv_errors_total": ekv_errors,
            "pending_drained_total": drained,
        },
    });
    Ok(json_resp(200, &resp.to_string()))
}

/// List the pending-EKV markers for debugging sync lag.
pub async fn list_pending(_req: Request, _params: Params) -> anyhow::Result<Response> {
    let backend = Backend::open_from_config()?;
    let mut items = Vec::new();
    for key in backend.keys_with_prefix(PREFIX_PENDING_EKV).await? {
        if let Some(v) = get_json::<serde_json::Value>(&backend, &key).await? {
            items.push(v);
        }
    }
    Ok(json_ok(&items))
}

// ---------------------------------------------------------------------------
// Test endpoint — simulates a redirect lookup against Spin KV
// ---------------------------------------------------------------------------

pub async fn test_lookup(_req: Request, params: Params) -> anyhow::Result<Response> {
    let host_raw = params.get("host").unwrap_or("").to_string();
    let path_raw = format!("/{}", params.wildcard().unwrap_or(""));
    let host = normalize_host(&host_raw);
    let path = normalize_path(&path_raw);

    let backend = Backend::open_from_config()?;
    let host_meta = get_json::<HostMeta>(&backend, &k_host(&host)).await?;

    let lookup_path = match &host_meta {
        Some(h) if !h.case_sensitive => path.to_lowercase(),
        _ => path.clone(),
    };

    let exact_key = k_rule(&host, MatchType::Exact, &lookup_path);
    if let Some(rule) = get_json::<Redirect>(&backend, &exact_key).await? {
        return Ok(json_resp(
            200,
            &serde_json::json!({
                "match": true,
                "match_type": "exact",
                "input": {"host": host, "path": path},
                "rule": rule,
            })
            .to_string(),
        ));
    }

    let mut best: Option<Redirect> = None;
    let prefix = rule_key_prefix(&host);
    for key in backend.keys_with_prefix(&prefix).await? {
        if !key.starts_with(&prefix) {
            continue;
        }
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

    if let Some(rule) = best {
        Ok(json_resp(
            200,
            &serde_json::json!({
                "match": true,
                "match_type": "prefix",
                "input": {"host": host, "path": path},
                "rule": rule,
            })
            .to_string(),
        ))
    } else {
        Ok(json_resp(
            200,
            &serde_json::json!({
                "match": false,
                "input": {"host": host, "path": path},
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn validate_rule(r: &Redirect) -> Result<(), String> {
    if r.host.is_empty() {
        return Err("host is required".into());
    }
    if r.target_url.is_empty() {
        return Err("target_url is required".into());
    }
    if !r.target_url.starts_with("http://") && !r.target_url.starts_with("https://") {
        return Err("target_url must be an absolute http(s) URL".into());
    }
    if !matches!(r.status_code, 301 | 302 | 307 | 308) {
        return Err("status_code must be 301, 302, 307, or 308".into());
    }
    Ok(())
}

/// Apply the rule's `case_insensitive` import signal: if the rule arrives
/// with ci=true, flip the host's `case_sensitive` to false. After this call,
/// if the host is case-insensitive (existing or just-flipped), the rule's
/// path is lowercased so it round-trips through the EW lookup (which
/// lowercases the request path for ci hosts).
///
/// The rule-level `case_insensitive` field is cleared on the rule before
/// storage — case-insensitivity is host-scoped in our model.
///
/// Returns true if the host meta was modified (caller should sync the host).
async fn apply_ci(b: &Backend, r: &mut Redirect) -> anyhow::Result<bool> {
    let host_key = k_host(&r.host);
    let mut meta = get_json::<HostMeta>(b, &host_key)
        .await?
        .unwrap_or_else(|| HostMeta::new(&r.host));
    let mut meta_changed = false;
    if r.case_insensitive && meta.case_sensitive {
        meta.case_sensitive = false;
        meta.updated_at = chrono::Utc::now().to_rfc3339();
        set_json(b, &host_key, &meta).await?;
        meta_changed = true;
    }
    r.case_insensitive = false;
    if !meta.case_sensitive {
        r.path = r.path.to_lowercase();
    }
    Ok(meta_changed)
}

async fn ensure_host_meta(b: &Backend, host: &str, delta: i64) -> anyhow::Result<()> {
    let key = k_host(host);
    let mut meta = get_json::<HostMeta>(b, &key)
        .await?
        .unwrap_or_else(|| HostMeta::new(host));
    if delta >= 0 {
        meta.rule_count = meta.rule_count.saturating_add(delta as u64);
    } else {
        meta.rule_count = meta.rule_count.saturating_sub((-delta) as u64);
    }
    meta.updated_at = chrono::Utc::now().to_rfc3339();
    set_json(b, &key, &meta).await?;
    Ok(())
}

async fn decrement_host_meta(b: &Backend, host: &str) -> anyhow::Result<()> {
    ensure_host_meta(b, host, -1).await
}

fn parse_composite(params: &Params) -> Option<(String, MatchType, String)> {
    let host = params.get("host")?;
    let mt = match params.get("type")? {
        "exact" => MatchType::Exact,
        "prefix" => MatchType::Prefix,
        _ => return None,
    };
    let path = format!("/{}", params.wildcard().unwrap_or(""));
    Some((normalize_host(host), mt, normalize_path(&path)))
}

fn query_param(req: &Request, name: &str) -> Option<String> {
    req.header("spin-full-url")
        .and_then(|v| v.as_str())
        .and_then(|url| url.split_once('?'))
        .and_then(|(_, q)| {
            q.split('&').find_map(|param| {
                let (k, v) = param.split_once('=')?;
                if k == name {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        })
}

pub fn check_auth(req: &Request) -> Result<(), Response> {
    let expected = spin_sdk::variables::get("admin_token").unwrap_or_default();
    if expected.is_empty() {
        return Ok(());
    }
    // Accept ?token= or ?auth= — alias lets the same URL shape work with
    // demo-catalog-landingzone's launcher which passes ?auth=.
    let supplied = query_param(req, "token").or_else(|| query_param(req, "auth"));
    if supplied.as_deref() == Some(expected.as_str()) {
        return Ok(());
    }
    Err(json_err(401, "unauthorized — pass ?token=<value>"))
}
