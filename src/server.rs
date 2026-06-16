use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::libs::request_auth::{check_auth, AuthOptions};
use crate::libs::request_context::{resolve_trace_id, run_with_context, RequestContext};

/// Maximum accepted request-body size (32 MiB). Generous enough for large
/// multimodal / Anthropic payloads while bounding memory per request.
const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Build the axum application, mirroring src/server.ts route table and
/// middleware stack (trace -> cors -> general auth -> admin auth).
pub fn build_router() -> Router {
    Router::new()
        .route("/", get(|| async { "Server running" }))
        .route("/usage-viewer", get(usage_viewer))
        .route("/usage-viewer/", get(usage_viewer_redirect))
        // Implemented spine routes
        .route(
            "/chat/completions",
            post(crate::routes::chat_completions::route::post_chat_completions),
        )
        .route(
            "/v1/chat/completions",
            post(crate::routes::chat_completions::route::post_chat_completions),
        )
        .route("/models", get(crate::routes::models::get_models_route))
        .route("/v1/models", get(crate::routes::models::get_models_route))
        .route(
            "/embeddings",
            post(crate::routes::embeddings::post_embeddings),
        )
        .route(
            "/v1/embeddings",
            post(crate::routes::embeddings::post_embeddings),
        )
        .route("/usage", get(crate::routes::usage::get_usage))
        .route("/token", get(crate::routes::token::get_token))
        // Token-usage subsystem (implemented). A single `nest` already serves
        // `/token-usage` and `/token-usage/...`; the bare trailing-slash form is
        // redirected like `/usage-viewer/` (nesting it twice panics axum with an
        // overlapping-route error at router construction).
        .nest("/token-usage", crate::routes::token_usage::router())
        .route("/token-usage/", get(token_usage_redirect))
        .route(
            "/responses",
            post(crate::routes::responses::route::post_responses),
        )
        .route(
            "/v1/responses",
            post(crate::routes::responses::route::post_responses),
        )
        .route(
            "/v1/messages",
            post(crate::routes::messages::route::post_messages),
        )
        .route(
            "/v1/messages/count_tokens",
            post(crate::routes::messages::route::post_count_tokens),
        )
        .route(
            "/admin/config/model-mappings",
            get(crate::routes::admin_config::get_model_mappings_route)
                .post(crate::routes::admin_config::post_model_mappings_route),
        )
        .route(
            "/:provider/v1/messages",
            post(crate::routes::provider::messages::post_provider_messages),
        )
        .route(
            "/:provider/v1/messages/count_tokens",
            post(crate::routes::provider::count_tokens::post_provider_count_tokens),
        )
        .route(
            "/:provider/v1/models",
            get(crate::routes::provider::models::get_provider_models),
        )
        // Middleware stack (innermost first; trace ends up outermost).
        .layer(from_fn(
            crate::libs::zstd_request::zstd_decompression_middleware,
        ))
        .layer(from_fn(admin_auth_middleware))
        .layer(from_fn(general_auth_middleware))
        .layer(CorsLayer::permissive())
        // Cap request-body size before any handler buffers it.
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        // Convert a panic in any handler into a 500 JSON response instead of an
        // abruptly reset connection.
        .layer(CatchPanicLayer::custom(handle_panic))
        .layer(from_fn(trace_middleware))
        // Per-request access logging (method/path/status/latency) for all
        // requests, including those rejected by auth.
        .layer(TraceLayer::new_for_http())
}

/// Render a handler panic as a 500 JSON error (instead of dropping the
/// connection). The panic is still logged by the default panic hook.
fn handle_panic(_err: Box<dyn std::any::Any + Send + 'static>) -> Response<Body> {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "message": "Internal server error.",
                "type": "internal_error",
            }
        })),
    )
        .into_response()
}

async fn trace_middleware(req: Request, next: Next) -> Response {
    let incoming_trace = req
        .headers()
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let trace_id = resolve_trace_id(incoming_trace.as_deref());

    let user_agent = header_string(&req, "user-agent").unwrap_or_default();
    let session_affinity = header_string(&req, "x-session-affinity")
        .or_else(|| header_string(&req, "x-client-request-id"));
    let parent_session_id = header_string(&req, "x-parent-session-id");

    let context = RequestContext {
        trace_id: trace_id.clone(),
        start_time: crate::libs::request_context::now_millis(),
        user_agent,
        session_affinity,
        parent_session_id,
    };

    let mut response = run_with_context(context, next.run(req)).await;
    if let Ok(value) = HeaderValue::from_str(&trace_id) {
        response.headers_mut().insert("x-trace-id", value);
    }
    response
}

fn header_string(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn general_auth_middleware(req: Request, next: Next) -> Response {
    let options = AuthOptions::general();
    if let Some(rejection) = check_auth(
        &options,
        req.method(),
        req.uri().path(),
        req.headers(),
        false,
    ) {
        return rejection;
    }
    next.run(req).await
}

async fn admin_auth_middleware(req: Request, next: Next) -> Response {
    let options = AuthOptions::admin();
    if let Some(rejection) = check_auth(
        &options,
        req.method(),
        req.uri().path(),
        req.headers(),
        true,
    ) {
        return rejection;
    }
    next.run(req).await
}

async fn usage_viewer() -> Response {
    // The usage dashboard HTML (pages/index.html) ships with the SQLite usage
    // subsystem, which is deferred. Serve a placeholder so the route exists.
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Body::from(
            "<!doctype html><title>Usage Viewer</title><p>Usage viewer is not available in this build.</p>",
        ))
        .unwrap()
}

async fn usage_viewer_redirect() -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header("location", "/usage-viewer")
        .body(Body::empty())
        .unwrap()
}

async fn token_usage_redirect() -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header("location", "/token-usage")
        .body(Body::empty())
        .unwrap()
}

fn deferred(name: &str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": {
                "message": format!("The {name} endpoint is not yet implemented in this build"),
                "type": "not_implemented",
            }
        })),
    )
        .into_response()
}
