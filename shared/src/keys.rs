//! Spin KV key schema.
//!
//! Keys are flat, namespaced by colon-separated prefixes. Spin KV has no
//! secondary indexes, so we iterate via `store.get_keys()` with prefix
//! filtering — fine at our scale (10K-100K rules across a handful of hosts).

use crate::model::MatchType;

pub const PREFIX_RULE: &str = "rule:";
pub const PREFIX_HOST: &str = "host:";
pub const PREFIX_PENDING_EKV: &str = "pending-ekv:";
pub const PREFIX_COUNTER: &str = "counter:";

pub const META_SEEDED: &str = "meta:seeded";
pub const META_LAST_SNAPSHOT: &str = "meta:last-snapshot";
pub const META_LAST_EKV_PUSH: &str = "meta:last-ekv-push";

pub const COUNTER_EKV_PUSHES: &str = "counter:ekv-pushes";
pub const COUNTER_EKV_ERRORS: &str = "counter:ekv-errors";
pub const COUNTER_PENDING_DRAINED: &str = "counter:pending-drained";

/// Rule key: `rule:{host}|{match_type}|{path}`. Pipe-delimited since
/// paths can contain colons (`:` in encoded params). Host and match_type
/// do not.
pub fn k_rule(host: &str, match_type: MatchType, path: &str) -> String {
    format!("{PREFIX_RULE}{host}|{}|{path}", match_type.as_str())
}

/// Enumerate all rule keys for a host: `rule:{host}|...`
pub fn rule_key_prefix(host: &str) -> String {
    format!("{PREFIX_RULE}{host}|")
}

/// Parse a rule key back into (host, match_type, path). Returns None if
/// the key doesn't match the schema.
///
/// Accepts either `|` (Spin KV native form) or `:` as the host/mt/path
/// separator. The `:` form arrives via the NATS backend, where the
/// subject encoder collapses both `:` and `|` to `.` and the decoder
/// canonicalizes them back to `:`. Hosts and match-type strings cannot
/// contain `:` so the split is unambiguous; paths CAN contain `:` (URL
/// params), which is fine because we splitn from the left.
pub fn parse_rule_key(key: &str) -> Option<(String, MatchType, String)> {
    let rest = key.strip_prefix(PREFIX_RULE)?;
    let split = |s: &str| -> Option<(String, String)> {
        let (a, b) = s.split_once('|').or_else(|| s.split_once(':'))?;
        Some((a.to_string(), b.to_string()))
    };
    let (host, rest) = split(rest)?;
    let (mt_str, path) = split(&rest)?;
    let mt = match mt_str.as_str() {
        "exact" => MatchType::Exact,
        "prefix" => MatchType::Prefix,
        _ => return None,
    };
    Some((host, mt, path))
}

pub fn k_host(host: &str) -> String {
    format!("{PREFIX_HOST}{host}")
}

/// Pending-EKV marker. Value is the rule key (or `host:{host}` for host
/// metadata pushes). Reconcile worker drains these by retrying the push.
pub fn k_pending_ekv(id: &str) -> String {
    format!("{PREFIX_PENDING_EKV}{id}")
}
