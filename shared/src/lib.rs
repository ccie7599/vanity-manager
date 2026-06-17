//! Shared types, key schema, KV helpers, and (with the `ekv-client` feature)
//! the EdgeKV client + write-through sync orchestrator.
//!
//! Redirect-handler depends on this crate without the `ekv-client` feature —
//! it only reads Spin KV and never calls out to EdgeKV.

pub mod keys;
pub mod kv;
pub mod model;
pub mod store;

#[cfg(feature = "ekv-client")]
pub mod ekv;

#[cfg(feature = "ekv-client")]
pub mod sync;

pub use keys::*;
pub use kv::*;
pub use model::*;
