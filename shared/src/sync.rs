//! Write-through and reconcile drain logic, shared between admin-api (inline
//! push after each mutation) and reconcile-worker (periodic drain of pending
//! markers).

use crate::ekv;
use crate::keys::{
    k_host, k_pending_ekv, parse_rule_key, rule_key_prefix, COUNTER_EKV_ERRORS,
    COUNTER_EKV_PUSHES, META_LAST_EKV_PUSH, PREFIX_PENDING_EKV,
};
use crate::kv::{get_json, incr_counter, set_json};
use crate::model::{EkvPrefixEntry, HostMeta, MatchType, Redirect};
use crate::store::Backend;

// ---------------------------------------------------------------------------
// Public inline write-through entrypoints (called from admin-api handlers)
// ---------------------------------------------------------------------------

pub async fn push_rule_inline(b: &Backend, rule_key: &str) {
    let cfg = match ekv::load_config() {
        Ok(c) => c,
        Err(_) => {
            queue(b, rule_key).await;
            return;
        }
    };
    record(b, rule_key, try_push_rule(b, &cfg, rule_key).await).await;
}

pub async fn push_host_inline(b: &Backend, host: &str) {
    let target = format!("host:{host}");
    let cfg = match ekv::load_config() {
        Ok(c) => c,
        Err(_) => {
            queue(b, &target).await;
            return;
        }
    };
    record(b, &target, try_push_host(b, &cfg, host).await).await;
}

pub async fn push_delete_rule_inline(b: &Backend, rule_key: &str) {
    let target = format!("DELETE_RULE:{rule_key}");
    let cfg = match ekv::load_config() {
        Ok(c) => c,
        Err(_) => {
            queue(b, &target).await;
            return;
        }
    };
    record(b, &target, try_delete_rule(b, &cfg, rule_key).await).await;
}

pub async fn push_delete_host_inline(b: &Backend, host: &str) {
    let target = format!("DELETE_HOST:{host}");
    let cfg = match ekv::load_config() {
        Ok(c) => c,
        Err(_) => {
            queue(b, &target).await;
            return;
        }
    };
    record(b, &target, ekv::delete_host(&cfg, host).await).await;
}

// ---------------------------------------------------------------------------
// Reconcile drain — called from reconcile-worker and from the metrics beacon
// ---------------------------------------------------------------------------

pub async fn drain_pending(b: &Backend, budget: usize) -> (u64, u64) {
    let markers: Vec<String> = b
        .keys_with_prefix(PREFIX_PENDING_EKV)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|k| k.strip_prefix(PREFIX_PENDING_EKV).map(|t| t.to_string()))
        .take(budget)
        .collect();

    let cfg = match ekv::load_config() {
        Ok(c) => c,
        Err(e) => {
            println!("reconcile: ekv config missing: {e}");
            for target in &markers {
                let _ = incr_counter(b, COUNTER_EKV_ERRORS).await;
                bump(b, target).await;
            }
            return (0, markers.len() as u64);
        }
    };

    // Group markers so we don't redo per-host work. Every prefix-rule marker
    // for a host triggers the same full manifest rebuild + multi-shard push;
    // collapsing them turns N markers per host into one EKV operation.
    //
    // Buckets:
    //   exact_targets[]     — independent rule keys, one EKV PUT each
    //   prefix_hosts{}      — set of hosts whose prefix manifest needs push
    //   delete_targets[]    — DELETE_RULE / DELETE_HOST passthroughs
    //   host_meta_targets{} — set of hosts whose host_meta needs push
    let mut exact_targets: Vec<String> = Vec::new();
    let mut prefix_hosts: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut host_meta_targets: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut delete_targets: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();

    for target in &markers {
        if target.starts_with("DELETE_RULE:") || target.starts_with("DELETE_HOST:") {
            delete_targets.push(target.clone());
        } else if let Some(host) = target.strip_prefix("host:") {
            host_meta_targets
                .entry(host.to_string())
                .or_default()
                .push(target.clone());
        } else if target.starts_with("rule:") {
            match parse_rule_key(target) {
                Some((host, MatchType::Exact, _)) => {
                    let _ = host;
                    exact_targets.push(target.clone());
                }
                Some((host, MatchType::Prefix, _)) => {
                    prefix_hosts.entry(host).or_default().push(target.clone());
                }
                None => unknown.push(target.clone()),
            }
        } else {
            unknown.push(target.clone());
        }
    }

    let mut drained = 0u64;
    let mut errors = 0u64;

    // 1) Exact rule pushes — one EKV PUT per marker, all concurrent.
    let exact_futs = exact_targets.iter().map(|t| {
        let cfg = &cfg;
        async move { (t.clone(), try_push_rule(b, cfg, t).await) }
    });
    for (target, outcome) in futures::future::join_all(exact_futs).await {
        match outcome {
            Ok(_) => {
                drained += 1;
                let _ = b.delete(&k_pending_ekv(&target)).await;
                let _ = incr_counter(b, COUNTER_EKV_PUSHES).await;
            }
            Err(e) => {
                errors += 1;
                println!("reconcile: drain failed for {target}: {e}");
                let _ = incr_counter(b, COUNTER_EKV_ERRORS).await;
                bump(b, &target).await;
            }
        }
    }

    // 2) Prefix-rule pushes — one full-manifest push per unique host, even if
    // many markers under that host are queued. On success, drop ALL markers
    // for that host.
    for (host, targets) in &prefix_hosts {
        let outcome: anyhow::Result<()> = async {
            let manifest = build_manifest_for_host(b, host).await?;
            ekv::push_prefixes(&cfg, host, &manifest).await
        }
        .await;
        match outcome {
            Ok(_) => {
                for t in targets {
                    let _ = b.delete(&k_pending_ekv(t)).await;
                    drained += 1;
                    let _ = incr_counter(b, COUNTER_EKV_PUSHES).await;
                }
            }
            Err(e) => {
                println!("reconcile: prefix push failed for host {host}: {e}");
                for t in targets {
                    errors += 1;
                    let _ = incr_counter(b, COUNTER_EKV_ERRORS).await;
                    bump(b, t).await;
                }
            }
        }
    }

    // 3) Host-meta pushes — same dedupe, one push per host.
    for (host, targets) in &host_meta_targets {
        let outcome = try_push_host(b, &cfg, host).await;
        match outcome {
            Ok(_) => {
                for t in targets {
                    let _ = b.delete(&k_pending_ekv(t)).await;
                    drained += 1;
                    let _ = incr_counter(b, COUNTER_EKV_PUSHES).await;
                }
            }
            Err(e) => {
                println!("reconcile: host_meta push failed for {host}: {e}");
                for t in targets {
                    errors += 1;
                    let _ = incr_counter(b, COUNTER_EKV_ERRORS).await;
                    bump(b, t).await;
                }
            }
        }
    }

    // 4) Deletes — one EKV op each, concurrent.
    let del_futs = delete_targets.iter().map(|t| {
        let cfg = &cfg;
        async move {
            let outcome = if let Some(rest) = t.strip_prefix("DELETE_RULE:") {
                try_delete_rule(b, cfg, rest).await
            } else if let Some(host) = t.strip_prefix("DELETE_HOST:") {
                ekv::delete_host(cfg, host).await
            } else {
                Ok(())
            };
            (t.clone(), outcome)
        }
    });
    for (target, outcome) in futures::future::join_all(del_futs).await {
        match outcome {
            Ok(_) => {
                drained += 1;
                let _ = b.delete(&k_pending_ekv(&target)).await;
                let _ = incr_counter(b, COUNTER_EKV_PUSHES).await;
            }
            Err(e) => {
                errors += 1;
                println!("reconcile: drain failed for {target}: {e}");
                let _ = incr_counter(b, COUNTER_EKV_ERRORS).await;
                bump(b, &target).await;
            }
        }
    }

    for t in &unknown {
        println!("reconcile: unknown marker target: {t}");
        errors += 1;
        bump(b, t).await;
    }

    if drained > 0 {
        let _ = b
            .set(META_LAST_EKV_PUSH, chrono::Utc::now().to_rfc3339().as_bytes())
            .await;
    }
    (drained, errors)
}

// ---------------------------------------------------------------------------
// Internal operations
// ---------------------------------------------------------------------------

async fn try_push_rule(
    b: &Backend,
    cfg: &ekv::EkvConfig,
    rule_key: &str,
) -> anyhow::Result<()> {
    let (host, mt, _path) = crate::keys::parse_rule_key(rule_key)
        .ok_or_else(|| anyhow::anyhow!("bad rule key: {rule_key}"))?;

    let rule = match get_json::<Redirect>(b, rule_key).await? {
        Some(r) => r,
        None => return Ok(()), // deleted since queue — treat as success
    };

    match mt {
        MatchType::Exact => {
            if rule.enabled {
                ekv::push_exact(cfg, &rule).await?;
            } else {
                ekv::delete_exact(cfg, &rule.host, &rule.path).await?;
            }
        }
        MatchType::Prefix => {
            let manifest = build_manifest_for_host(b, &host).await?;
            ekv::push_prefixes(cfg, &host, &manifest).await?;
        }
    }

    if let Some(mut stamped) = get_json::<Redirect>(b, rule_key).await? {
        stamped.ekv_synced_at = chrono::Utc::now().to_rfc3339();
        set_json(b, rule_key, &stamped).await?;
    }
    Ok(())
}

async fn try_push_host(
    b: &Backend,
    cfg: &ekv::EkvConfig,
    host: &str,
) -> anyhow::Result<()> {
    let meta = match get_json::<HostMeta>(b, &k_host(host)).await? {
        Some(m) => m,
        None => return ekv::delete_host(cfg, host).await,
    };
    ekv::push_host(cfg, &meta).await?;
    if let Some(mut stamped) = get_json::<HostMeta>(b, &k_host(host)).await? {
        stamped.ekv_synced_at = chrono::Utc::now().to_rfc3339();
        set_json(b, &k_host(host), &stamped).await?;
    }
    Ok(())
}

async fn try_delete_rule(
    b: &Backend,
    cfg: &ekv::EkvConfig,
    rule_key: &str,
) -> anyhow::Result<()> {
    let (host, mt, path) = crate::keys::parse_rule_key(rule_key)
        .ok_or_else(|| anyhow::anyhow!("bad rule key: {rule_key}"))?;
    match mt {
        MatchType::Exact => ekv::delete_exact(cfg, &host, &path).await,
        MatchType::Prefix => {
            let manifest = build_manifest_for_host(b, &host).await?;
            ekv::push_prefixes(cfg, &host, &manifest).await
        }
    }
}

async fn build_manifest_for_host(b: &Backend, host: &str) -> anyhow::Result<Vec<EkvPrefixEntry>> {
    // Sequential per-key fetch. Tried bounded concurrent fan-out (4-64) to
    // beat the wall-clock on 24K-rule hosts — Akamai Functions enforces a
    // very tight per-component outbound HTTP limit, and any concurrent
    // fan-out trips `ConnectionLimitReached` not only here but on every
    // other handler in the app. Sequential keeps the app healthy. The
    // tradeoff: this can't rebuild a 24K-rule manifest within 30s. Callers
    // should keep the manifest source of truth in EKV warm — the typical
    // operating mode (per-mutation inline push) never rebuilds wholesale.
    let prefix = rule_key_prefix(host);
    let mut rules = Vec::new();
    for key in b.keys_with_prefix(&prefix).await? {
        if let Some(r) = get_json::<Redirect>(b, &key).await? {
            rules.push(r);
        }
    }
    Ok(ekv::build_prefix_manifest(&rules))
}

// ---------------------------------------------------------------------------
// Pending-marker bookkeeping
// ---------------------------------------------------------------------------

async fn queue(b: &Backend, target: &str) {
    let key = k_pending_ekv(target);
    let payload = serde_json::json!({
        "queued_at": chrono::Utc::now().to_rfc3339(),
        "target": target,
        "attempts": 0,
    });
    let _ = set_json(b, &key, &payload).await;
}

/// Skip the inline EKV push and just queue a pending marker. Used by bulk
/// import paths that cannot afford 24K serial EKV calls inside one Functions
/// invocation; callers drain via the reconcile worker after returning.
pub async fn enqueue_rule_push(b: &Backend, rule_key: &str) {
    queue(b, rule_key).await
}

pub async fn enqueue_host_push(b: &Backend, host: &str) {
    queue(b, &format!("host:{host}")).await
}

pub async fn enqueue_rule_delete(b: &Backend, rule_key: &str) {
    queue(b, &format!("DELETE_RULE:{rule_key}")).await
}

async fn record(b: &Backend, target: &str, outcome: anyhow::Result<()>) {
    match outcome {
        Ok(_) => {
            let _ = incr_counter(b, COUNTER_EKV_PUSHES).await;
            let _ = b.delete(&k_pending_ekv(target)).await;
            let _ = b
                .set(META_LAST_EKV_PUSH, chrono::Utc::now().to_rfc3339().as_bytes())
                .await;
        }
        Err(e) => {
            println!("ekv push failed for {target}: {e}");
            let _ = incr_counter(b, COUNTER_EKV_ERRORS).await;
            bump(b, target).await;
        }
    }
}

async fn bump(b: &Backend, target: &str) {
    let key = k_pending_ekv(target);
    let existing: serde_json::Value = get_json(b, &key)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| serde_json::json!({}));
    let attempts = existing.get("attempts").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
    let payload = serde_json::json!({
        "queued_at": existing
            .get("queued_at")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(chrono::Utc::now().to_rfc3339())),
        "target": target,
        "attempts": attempts,
        "last_error_at": chrono::Utc::now().to_rfc3339(),
    });
    let _ = set_json(b, &key, &payload).await;
}
