//! Axum router assembly: wires every route, the auth/trace middleware stack,
//! CORS, body limits, and panic handling into the application `Router`.

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use metrics::{counter, histogram};
use serde_json::json;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::{info_span, Instrument, Level};

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
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
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
        .layer(cors_layer())
        // Cap request-body size before any handler buffers it.
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        // Convert a panic in any handler into a 500 JSON response instead of an
        // abruptly reset connection.
        .layer(CatchPanicLayer::custom(handle_panic))
        // Per-request access logging (method/path/status/latency) for all
        // requests, including those rejected by auth. The default `on_response`
        // emits at DEBUG, which is below the prod `info` filter — bump the
        // success line to INFO and failures to WARN so per-request logs appear.
        //
        // ORDERING: tower applies layers bottom-up, so the LAST `.layer()` call is
        // the OUTERMOST middleware. `trace_middleware` (below) creates the
        // `info_span!("request", trace_id, ...)`, so it MUST be listed AFTER
        // (outermost of) this `TraceLayer`. That way the TraceLayer's
        // `on_response`/`on_failure` access logs are emitted INSIDE the trace_id
        // span and inherit `trace_id` — otherwise they would fire outside the span
        // and the per-request access log lines would lack the trace id.
        .layer(
            TraceLayer::new_for_http()
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::WARN)),
        )
        // Record request count + latency metrics for every request. Inside the
        // trace span (listed before trace_middleware), outside the access log.
        .layer(from_fn(metrics_middleware))
        // Outermost application middleware: establishes the trace_id span +
        // task-local RequestContext so EVERY inner layer (metrics + TraceLayer
        // access logs) and handler runs within the span. Must remain the last
        // `.layer()` call.
        .layer(from_fn(trace_middleware))
}

/// CORS policy for the API.
///
/// We intentionally allow any origin: the OpenAI / Anthropic browser SDKs call
/// this gateway directly from arbitrary web origins, so origin-restricting would
/// break client compatibility. Methods and headers are restricted to the set the
/// SDKs actually use rather than `Any`, making the policy explicit.
///
/// SECURITY: credentials are NOT allowed (`allow_credentials` is never set), so
/// browsers will not send cookies / HTTP-auth on cross-origin requests and the
/// wildcard origin cannot be combined with credentials. This gateway relies on
/// bearer/x-api-key tokens in request headers for auth, not ambient credentials.
/// Operators SHOULD still front this service with their own authentication.
fn cors_layer() -> CorsLayer {
    use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
    use axum::http::{HeaderName, Method};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            CONTENT_TYPE,
            AUTHORIZATION,
            HeaderName::from_static("x-api-key"),
            HeaderName::from_static("anthropic-version"),
            HeaderName::from_static("anthropic-beta"),
        ])
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

    // Correlation span: every log line emitted while handling the request carries
    // the trace id (also returned as `x-trace-id`), plus method/path. The
    // task-local `RequestContext` is kept intact for token_usage to consume.
    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let span = info_span!("request", trace_id = %trace_id, method = %method, path = %path);

    let mut response = run_with_context(context, next.run(req).instrument(span)).await;
    if let Ok(value) = HeaderValue::from_str(&trace_id) {
        response.headers_mut().insert("x-trace-id", value);
    }
    response
}

/// Times each request and records a total-requests counter plus a
/// latency histogram, labelled by method, matched route and response status.
async fn metrics_middleware(req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_owned();
    // Capture the matched route template (e.g. `/:provider/v1/messages`) BEFORE
    // `next.run` consumes `req`. Using the template rather than the raw URI keeps
    // the histogram label set bounded (no per-id explosion). Unmatched requests
    // (404s) fall back to a fixed `unmatched` bucket.
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());
    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();
    counter!("http_requests_total", "method" => method, "status" => status.clone()).increment(1);
    histogram!(
        "http_request_duration_seconds",
        "route" => route,
        "status" => status,
    )
    .record(elapsed);
    response
}

/// Prometheus text-exposition endpoint. Mounted outside auth via the general
/// layer's `allow_unauthenticated_paths` so scrapers need no API key.
async fn metrics_handler() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4")
        .body(Body::from(crate::libs::metrics::render()))
        .unwrap()
}

/// Readiness probe: ready only once a copilot token is held and the model list
/// has been cached. Distinct from `/` (liveness), which is always 200.
async fn readyz() -> Response {
    let ready = crate::libs::state::with_state(|s| s.copilot_token.is_some() && s.models.is_some());
    if ready {
        (StatusCode::OK, Json(json!({"status": "ready"}))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready"})),
        )
            .into_response()
    }
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
    // Self-contained dashboard (inline CSS/JS, no external deps) that renders the
    // /token-usage JSON API. Embedded at compile time so it ships in the binary.
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Body::from(include_str!("usage_viewer.html")))
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
