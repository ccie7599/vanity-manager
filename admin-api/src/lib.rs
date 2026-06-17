mod handlers;
mod snapshot;
mod storage;

use spin_sdk::http::{Method, Request, Response, Router};
use spin_sdk::http_component;

#[http_component]
async fn handle(req: Request) -> Response {
    if *req.method() == Method::Options {
        return Response::builder()
            .status(204)
            .header("access-control-allow-origin", "*")
            .header("access-control-allow-methods", "GET, POST, PUT, DELETE, OPTIONS")
            .header("access-control-allow-headers", "content-type")
            .header("access-control-max-age", "86400")
            .build();
    }

    let path_info = req
        .header("spin-path-info")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // ------- public docs + UI (no auth) --------
    match path_info.as_str() {
        "/v1/ui" | "/v1/ui/" => return handlers::serve_ui(&req),
        "/v1/docs" | "/v1/docs/" => return handlers::serve_docs(&req),
        "/v1/readme" | "/v1/readme/" => return handlers::serve_readme(&req),
        "/v1/openapi.yaml" | "/v1/openapi.yml" => return handlers::serve_openapi(),
        "/v1/architecture.svg" => return handlers::serve_architecture_svg(&req),
        "/v1/features.svg" => return handlers::serve_features_svg(&req),
        "/health" => return handlers::health(),
        _ => {}
    }
    // /api/v1/docs/source/{name} — markdown served for client-side rendering
    if let Some(rest) = path_info.strip_prefix("/v1/docs/source/") {
        return handlers::serve_doc_source(rest);
    }

    if let Err(resp) = handlers::check_auth(&req) {
        return resp;
    }

    if let Ok(backend) = shared::store::Backend::open_from_config() {
        if let Err(e) = snapshot::ensure_seeded(&backend).await {
            println!("seed error (non-fatal): {e}");
        }
    }

    let mut router = Router::suffix();

    router.get_async("/v1/rules", handlers::list_rules);
    router.post_async("/v1/rules", handlers::create_rule);
    router.get_async("/v1/rules/:host/:type/*", handlers::get_rule);
    router.put_async("/v1/rules/:host/:type/*", handlers::update_rule);
    router.delete_async("/v1/rules/:host/:type/*", handlers::delete_rule);

    router.post_async("/v1/import", handlers::import_rules);
    router.get_async("/v1/export", handlers::export_rules);
    router.post_async("/v1/snapshot", handlers::publish_snapshot_now);
    router.post_async("/v1/clear-pending", handlers::clear_pending);
    router.post_async("/v1/mark-synced", handlers::mark_synced);

    router.get_async("/v1/hosts", handlers::list_hosts);
    router.put_async("/v1/hosts/:host", handlers::update_host);

    router.get_async("/v1/stats", handlers::stats);
    router.get_async("/v1/pending", handlers::list_pending);
    router.get_async("/v1/test/:host/*", handlers::test_lookup);

    router.handle_async(req).await
}
