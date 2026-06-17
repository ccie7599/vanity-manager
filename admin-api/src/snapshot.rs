//! S3 snapshot publish + cold-start seed.
//!
//! Every mutation triggers `publish_snapshot()`, which dumps the current
//! KV state to `<prefix>/rules.json` and an immutable timestamped copy
//! under `<prefix>/history/`. On cluster cold-start, `ensure_seeded()`
//! restores the KV store from the current snapshot.

use crate::storage::{get_object, put_object};
use shared::kv::{get_json, set_json};
use shared::store::Backend;
use shared::{
    k_host, k_rule, HostMeta, Redirect, RulesSnapshot, META_LAST_SNAPSHOT, META_SEEDED,
    PREFIX_HOST, PREFIX_RULE,
};

fn snapshot_prefix() -> String {
    spin_sdk::variables::get("snapshot_prefix")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "catalog".into())
}

fn current_key() -> String {
    format!("{}/rules.json", snapshot_prefix())
}

fn history_key(ts: &str) -> String {
    format!("{}/history/rules-{ts}.json", snapshot_prefix())
}

/// Dump all rules + host metadata to S3. Best-effort: callers should log
/// but not fail the request if this errors — KV remains the authoritative
/// store.
pub async fn publish_snapshot(b: &Backend) -> anyhow::Result<()> {
    let mut rules: Vec<Redirect> = Vec::new();
    let mut hosts: Vec<HostMeta> = Vec::new();

    for key in b.keys_with_prefix(PREFIX_RULE).await? {
        if let Some(r) = get_json::<Redirect>(b, &key).await? {
            rules.push(r);
        }
    }
    for key in b.keys_with_prefix(PREFIX_HOST).await? {
        if let Some(h) = get_json::<HostMeta>(b, &key).await? {
            hosts.push(h);
        }
    }

    rules.sort_by(|a, b| (a.host.as_str(), a.path.as_str()).cmp(&(b.host.as_str(), b.path.as_str())));
    hosts.sort_by(|a, b| a.host.cmp(&b.host));

    let now = chrono::Utc::now();
    let ts = now.format("%Y%m%dT%H%M%SZ").to_string();
    let snapshot = RulesSnapshot {
        snapshot_version: 1,
        generated_at: now.to_rfc3339(),
        rule_count: rules.len(),
        host_count: hosts.len(),
        rules,
        hosts,
    };

    let bytes = serde_json::to_vec_pretty(&snapshot)?;
    let current = current_key();
    put_object(&current, "application/json", bytes.clone()).await?;
    put_object(&history_key(&ts), "application/json", bytes).await?;

    b.set(META_LAST_SNAPSHOT, now.to_rfc3339().as_bytes()).await?;
    Ok(())
}

/// On cold-start, if KV is empty, restore from the current S3 snapshot.
/// Sentinel `meta:seeded` prevents re-seeding on subsequent invocations.
pub async fn ensure_seeded(b: &Backend) -> anyhow::Result<()> {
    if b.exists(META_SEEDED).await? {
        return Ok(());
    }
    // Fast path: any rule already in KV means we've been running.
    if !b.keys_with_prefix(PREFIX_RULE).await?.is_empty() {
        b.set(META_SEEDED, b"1").await?;
        return Ok(());
    }

    let current = current_key();
    match get_object(&current).await {
        Ok(Some(bytes)) => match serde_json::from_slice::<RulesSnapshot>(&bytes) {
            Ok(snapshot) => {
                for r in &snapshot.rules {
                    let mt = r.match_type;
                    set_json(b, &k_rule(&r.host, mt, &r.path), r).await?;
                }
                for h in &snapshot.hosts {
                    set_json(b, &k_host(&h.host), h).await?;
                }
                b.set(META_SEEDED, b"1").await?;
                println!(
                    "seed: restored {} rules, {} hosts from {}",
                    snapshot.rule_count, snapshot.host_count, current
                );
            }
            Err(e) => {
                println!("seed: snapshot malformed: {e}");
                b.set(META_SEEDED, b"1").await?;
            }
        },
        Ok(None) => {
            println!("seed: no prior snapshot — starting fresh");
            b.set(META_SEEDED, b"1").await?;
        }
        Err(e) => {
            println!("seed: S3 unreachable: {e}");
        }
    }
    Ok(())
}
