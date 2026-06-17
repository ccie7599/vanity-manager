//! Higher-level helpers over `Backend` (JSON (de)serialization, counters).

use serde::{de::DeserializeOwned, Serialize};

use crate::store::Backend;

pub async fn get_json<T: DeserializeOwned>(b: &Backend, key: &str) -> anyhow::Result<Option<T>> {
    match b.get(key).await? {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        None => Ok(None),
    }
}

pub async fn set_json<T: Serialize>(b: &Backend, key: &str, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    b.set(key, &bytes).await
}

/// Increment a counter by 1, returning the post-increment value.
pub async fn incr_counter(b: &Backend, key: &str) -> anyhow::Result<u64> {
    Ok(b.incr(key, 1).await?.max(0) as u64)
}

/// Read a counter as u64 (0 on miss / parse error).
pub async fn read_counter(b: &Backend, key: &str) -> u64 {
    b.get(key)
        .await
        .ok()
        .flatten()
        .and_then(|v| std::str::from_utf8(&v).ok().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}
