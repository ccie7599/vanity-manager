use serde::{Deserialize, Serialize};

/// A redirect rule. Composite primary key is `(host, match_type, path)`.
///
/// Exact rules land as individual EdgeKV items; prefix rules are projected
/// into a per-host manifest so the EdgeWorker can resolve longest-prefix
/// with a single KV read after an exact-match miss.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Redirect {
    pub host: String,
    #[serde(default = "default_path")]
    pub path: String,
    pub target_url: String,
    #[serde(default = "default_status")]
    pub status_code: i64,
    #[serde(default = "default_match_type")]
    pub match_type: MatchType,
    #[serde(default)]
    pub preserve_path: bool,
    #[serde(default = "default_true")]
    pub preserve_query: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub notes: String,
    /// Import-time signal: when true on input, the host's case_sensitive
    /// flag is flipped to false and the rule's path is lowercased before
    /// storage. Cleared after application; case-insensitivity is host-scoped
    /// in storage (see HostMeta.case_sensitive).
    #[serde(default, skip_serializing_if = "is_false")]
    pub case_insensitive: bool,
    /// Last-modified timestamp (RFC3339). Set on every write.
    #[serde(default)]
    pub updated_at: String,
    /// Last-synced-to-EdgeKV timestamp (RFC3339). Empty while pending.
    #[serde(default)]
    pub ekv_synced_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MatchType {
    Exact,
    Prefix,
}

fn default_path() -> String {
    "/".into()
}
fn default_status() -> i64 {
    301
}
fn default_match_type() -> MatchType {
    MatchType::Exact
}
fn default_true() -> bool {
    true
}

impl MatchType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchType::Exact => "exact",
            MatchType::Prefix => "prefix",
        }
    }
}

/// Per-host metadata, mirrored to EdgeKV as the `_hosts::{host}` sentinel
/// so the EdgeWorker can fast-reject unmanaged hosts with a single KV read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostMeta {
    pub host: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When true, path matching is case-sensitive (RFC-correct default).
    /// When false, EW lowercases the request path before lookup.
    #[serde(default = "default_true")]
    pub case_sensitive: bool,
    #[serde(default)]
    pub rule_count: u64,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub ekv_synced_at: String,
}

impl HostMeta {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            enabled: true,
            case_sensitive: true,
            rule_count: 0,
            updated_at: chrono::Utc::now().to_rfc3339(),
            ekv_synced_at: String::new(),
        }
    }
}

/// Compact shape pushed to EdgeKV as `{host}::exact::{path}`. Every byte
/// matters — EdgeKV has a 512KB per-item limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EkvExact {
    pub t: String, // target_url
    pub s: i64,    // status_code
    #[serde(default)]
    pub pp: u8, // preserve_path (0|1)
    #[serde(default = "one")]
    pub pq: u8, // preserve_query (0|1)
}

/// Element of the per-host prefix manifest stored at `{host}::prefixes`.
/// Array is sorted by `p.len()` DESC so EW picks longest match first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EkvPrefixEntry {
    pub p: String, // path prefix
    pub t: String,
    pub s: i64,
    #[serde(default)]
    pub pp: u8,
    #[serde(default = "one")]
    pub pq: u8,
}

fn one() -> u8 {
    1
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl From<&Redirect> for EkvExact {
    fn from(r: &Redirect) -> Self {
        Self {
            t: r.target_url.clone(),
            s: r.status_code,
            pp: r.preserve_path as u8,
            pq: r.preserve_query as u8,
        }
    }
}

impl From<&Redirect> for EkvPrefixEntry {
    fn from(r: &Redirect) -> Self {
        Self {
            p: r.path.clone(),
            t: r.target_url.clone(),
            s: r.status_code,
            pp: r.preserve_path as u8,
            pq: r.preserve_query as u8,
        }
    }
}

/// Snapshot written to S3 as `catalog/rules.json` after every mutation,
/// and again as `catalog/history/rules-{timestamp}.json` for versioned rollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesSnapshot {
    pub snapshot_version: u32,
    pub generated_at: String,
    pub rule_count: usize,
    pub host_count: usize,
    pub rules: Vec<Redirect>,
    pub hosts: Vec<HostMeta>,
}

/// Host normalization: lowercase, strip port, trim whitespace.
pub fn normalize_host(raw: &str) -> String {
    raw.trim()
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Path normalization: ensure leading slash, collapse repeated slashes,
/// strip trailing slash except on root. Case preserved (per-host
/// case_sensitive flag determines whether it's lowercased before lookup).
pub fn normalize_path(raw: &str) -> String {
    let mut p = raw.trim().to_string();
    if !p.starts_with('/') {
        p = format!("/{p}");
    }
    // Collapse repeated slashes.
    while p.contains("//") {
        p = p.replace("//", "/");
    }
    if p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}
