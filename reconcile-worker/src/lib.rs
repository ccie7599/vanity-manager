//! Reconcile worker — drains `pending-ekv:*` markers by retrying EdgeKV pushes.
//!
//! Phase 1: HTTP-triggered via `POST /_reconcile/drain`. Primary drivers:
//!   - External cron (GitHub Actions / curl from bastion)
//!   - Admin activity: every admin mutation already push-inline, and any
//!     marker left behind will be drained on next `/_reconcile/drain`
//!   - EdgeWorker metrics beacon: drains a few markers per beacon hit
//!
//! Phase 2: swap to `[[trigger.cron]]` once task #8 confirms Akamai Functions
//! honors it. The drain implementation in `shared::sync::drain_pending` is
//! identical, so only the trigger shape changes.

use shared::store::Backend;
use shared::sync;
use spin_sdk::http::{IntoResponse, Request, Response};
use spin_sdk::http_component;

const DRAIN_BUDGET: usize = 25;

#[http_component]
async fn handle(_req: Request) -> anyhow::Result<impl IntoResponse> {
    let backend = Backend::open_from_config()?;
    let (drained, errors) = sync::drain_pending(&backend, DRAIN_BUDGET).await;

    let body = serde_json::json!({
        "drained": drained,
        "errors": errors,
        "budget": DRAIN_BUDGET,
    })
    .to_string();

    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(body)
        .build())
}
