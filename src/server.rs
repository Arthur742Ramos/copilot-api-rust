//! Axum router assembly: wires every route, the auth/trace middleware stack,
//! CORS, body limits, and panic handling into the application `Router`.

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn, from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::FutureExt;
use metrics::{counter, gauge, histogram};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::{info_span, Instrument, Level};

use crate::libs::request_auth::{check_auth, AuthOptions, AuthOutcome};
use crate::libs::request_context::{resolve_trace_id, run_with_context, RequestContext};

use crate::libs::http::MAX_REQUEST_BODY_BYTES;

/// Build the axum application, mirroring src/server.ts route table and
/// middleware stack (trace -> cors -> general auth -> admin auth).
pub fn build_router() -> Router {
    build_router_with_admission(crate::libs::admission::AdmissionController::default())
}

/// Build the production router with an explicit admission controller. Keeping
/// the controller injectable lets embedders and integration tests use a low
/// deterministic limit without mutating process-global environment variables.
pub fn build_router_with_admission(
    admission: crate::libs::admission::AdmissionController,
) -> Router {
    Router::new()
        .route("/", get(|| async { "Server running" }))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
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
        .route("/models/:id", get(crate::routes::models::get_model_route))
        .route(
            "/v1/models/:id",
            get(crate::routes::models::get_model_route),
        )
        .route(
            "/embeddings",
            post(crate::routes::embeddings::post_embeddings),
        )
        .route(
            "/v1/embeddings",
            post(crate::routes::embeddings::post_embeddings),
        )
        .route(
            "/images/generations",
            post(crate::routes::images::post_images),
        )
        .route(
            "/v1/images/generations",
            post(crate::routes::images::post_images),
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
            "/responses/compact",
            post(crate::routes::responses::compact::post_responses_compact),
        )
        .route(
            "/v1/responses/compact",
            post(crate::routes::responses::compact::post_responses_compact),
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
            "/admin/config/reload",
            post(crate::routes::admin_config::post_reload_route),
        )
        .route(
            "/admin/config/providers",
            get(crate::routes::admin_config::get_providers_route)
                .post(crate::routes::admin_config::post_providers_route),
        )
        .route(
            "/admin/config",
            get(crate::routes::admin_config::get_effective_config_route),
        )
        .route(
            "/admin/providers/health",
            get(crate::routes::admin_config::get_providers_health_route),
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
        .fallback(api_not_found)
        .method_not_allowed_fallback(api_method_not_allowed)
        // Middleware stack (innermost first; trace ends up outermost).
        .layer(from_fn(
            crate::libs::zstd_request::zstd_decompression_middleware,
        ))
        // Fail fast before decompression/handler work when an upstream-facing
        // route has no available concurrency slot. The middleware attaches its
        // permit to the response body, so returning a streaming HEAD does not
        // release capacity early. Control-plane routes bypass this layer.
        .layer(from_fn_with_state(admission, upstream_admission_middleware))
        .layer(from_fn(admin_auth_middleware))
        .layer(from_fn(general_auth_middleware))
        .layer(cors_layer())
        // Cap request-body size before any handler buffers it.
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        // Rewrite the plain-text 413 that the body-limit layer / Bytes extractor
        // produces into the Anthropic JSON error envelope, so an oversize request
        // gets the same error shape as every other client error. Listed AFTER
        // (outside) DefaultBodyLimit so it sees that layer's response.
        .layer(from_fn(normalize_oversize_response))
        // Convert handler/inner-middleware panics while still inside the request
        // tracing layers, so those layers observe the route-native 500 response.
        .layer(from_fn(panic_middleware))
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
        // application-observability `.layer()` call.
        .layer(from_fn(trace_middleware))
        // Absolute outer safety net: panics in trace/metrics/access-log
        // middleware must also become route-native JSON instead of resetting the
        // connection. The inner catcher above keeps normal handler panics visible
        // to observability middleware; this one protects the observability stack.
        .layer(from_fn(panic_middleware))
}

/// Select the route families that can consume upstream connections. Exact
/// matched templates keep this list bounded and prevent user-controlled path
/// values from becoming metric/log labels.
fn is_upstream_proxy_route(method: &Method, route: &str) -> bool {
    match route {
        "/chat/completions"
        | "/v1/chat/completions"
        | "/embeddings"
        | "/v1/embeddings"
        | "/images/generations"
        | "/v1/images/generations"
        | "/responses"
        | "/v1/responses"
        | "/responses/compact"
        | "/v1/responses/compact"
        | "/v1/messages"
        | "/v1/messages/count_tokens"
        | "/:provider/v1/messages"
        | "/:provider/v1/messages/count_tokens" => method == Method::POST,
        "/models" | "/v1/models" | "/models/:id" | "/v1/models/:id" | "/:provider/v1/models" => {
            method == Method::GET || method == Method::HEAD
        }
        _ => false,
    }
}

async fn upstream_admission_middleware(
    State(admission): State<crate::libs::admission::AdmissionController>,
    request: Request,
    next: Next,
) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("");
    if !is_upstream_proxy_route(request.method(), route) {
        return next.run(request).await;
    }

    crate::libs::admission::admit_request(admission, request, next).await
}

/// CORS policy for the API.
///
/// SECURITY: the previous policy reflected `Any` origin (ACAO: `*`) router-wide.
/// Combined with a `0.0.0.0` bind, that turned `/token` and the other sensitive
/// routes into a cross-origin drive-by target: any web page the operator visited
/// could `fetch()` this gateway and read the response. We instead reflect an
/// explicit allowlist of *loopback* origins (`http://localhost`, `127.0.0.1`,
/// `[::1]` on any port). This keeps the local usage-viewer / usage endpoints
/// working when served from a different local port, but no longer hands an
/// `Access-Control-Allow-Origin` to arbitrary internet origins, so a malicious
/// remote page cannot read `/token` (or anything else) from a victim's browser.
///
/// Credentials are NOT allowed (`allow_credentials` is never set), so browsers
/// will not send cookies / HTTP-auth cross-origin. This gateway relies on
/// bearer / x-api-key tokens in request headers for auth, not ambient
/// credentials. Non-browser clients (curl, SDKs) are unaffected: CORS only
/// constrains browser script, never the server-side auth check.
fn cors_layer() -> CorsLayer {
    use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
    use axum::http::{HeaderName, Method};

    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _request_parts| {
            is_loopback_origin(origin.as_bytes())
        }))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            CONTENT_TYPE,
            AUTHORIZATION,
            HeaderName::from_static("x-api-key"),
            HeaderName::from_static("anthropic-version"),
            HeaderName::from_static("anthropic-beta"),
        ])
}

/// Whether an `Origin` header value names a loopback host (localhost / 127.0.0.0/8
/// / ::1) on any port and scheme. Used to scope CORS to local browser tooling
/// without reflecting arbitrary remote origins.
fn is_loopback_origin(origin: &[u8]) -> bool {
    let origin = match std::str::from_utf8(origin) {
        Ok(s) => s,
        Err(_) => return false,
    };
    // Strip scheme (http:// or https://).
    let host_port = match origin.split_once("://") {
        Some((_, rest)) => rest,
        None => return false,
    };
    // An origin has no path; the authority is everything up to an optional port.
    // Handle bracketed IPv6 hosts (`[::1]:8080`) before splitting on ':'.
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        host_port.split(':').next().unwrap_or("")
    };

    if host.eq_ignore_ascii_case("localhost") || host == "::1" {
        return true;
    }
    // 127.0.0.0/8 loopback range.
    matches!(host.parse::<std::net::Ipv4Addr>(), Ok(ip) if ip.is_loopback())
}

/// Catch handler panics while the request URI is still available, so the error
/// envelope remains native to the selected public protocol. The panic hook still
/// records the original diagnostic.
async fn panic_middleware(req: Request, next: Next) -> Response {
    let openai_native = crate::libs::error::is_openai_native_path(req.uri().path());
    match std::panic::AssertUnwindSafe(next.run(req))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) if openai_native => crate::libs::error::openai_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            Some("server_error"),
            "Internal server error.",
        ),
        Err(_) => crate::libs::error::anthropic_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "Internal server error.",
        ),
    }
}

async fn api_not_found(uri: axum::http::Uri) -> Response {
    if crate::libs::error::is_openai_native_path(uri.path()) {
        crate::libs::error::openai_error_response(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            Some("not_found"),
            format!("No route for {}.", uri.path()),
        )
    } else {
        crate::libs::error::anthropic_error_response(
            StatusCode::NOT_FOUND,
            "not_found_error",
            format!("No route for {}.", uri.path()),
        )
    }
}

async fn api_method_not_allowed(uri: axum::http::Uri) -> Response {
    if crate::libs::error::is_openai_native_path(uri.path()) {
        crate::libs::error::openai_error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid_request_error",
            Some("method_not_allowed"),
            format!("Method is not allowed for {}.", uri.path()),
        )
    } else {
        crate::libs::error::anthropic_error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid_request_error",
            format!("Method is not allowed for {}.", uri.path()),
        )
    }
}

/// Rewrite a `413 Payload Too Large` whose body is not already JSON (the
/// plain-text rejection axum's body-limit layer / `Bytes` extractor emits) into
/// the complete Anthropic `request_too_large` envelope, so clients that parse error
/// JSON get a consistent shape. Other responses pass through untouched.
async fn normalize_oversize_response(req: Request, next: Next) -> Response {
    let openai_native = crate::libs::error::is_openai_native_path(req.uri().path());
    let response = next.run(req).await;
    if response.status() != StatusCode::PAYLOAD_TOO_LARGE {
        return response;
    }
    let is_json = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    if is_json {
        return response;
    }
    if openai_native {
        crate::libs::error::openai_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            Some("request_too_large"),
            "Request body is too large.",
        )
    } else {
        crate::libs::error::anthropic_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "Request body is too large.",
        )
    }
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

    let context = RequestContext::new(
        trace_id.clone(),
        crate::libs::request_context::now_millis(),
        user_agent,
        session_affinity,
        parent_session_id,
    );

    // Correlation span: every log line emitted while handling the request carries
    // the trace id (also returned as `x-trace-id`), plus method/path. The
    // task-local `RequestContext` is kept intact for token_usage to consume.
    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let span = info_span!("request", trace_id = %trace_id, method = %method, path = %path);

    let mut response = run_with_context(context.clone(), next.run(req).instrument(span)).await;
    if let Ok(value) = HeaderValue::from_str(&trace_id) {
        response.headers_mut().insert("x-trace-id", value);
    }

    // Emit the single `request.completed` summary line for NON-streaming model
    // responses here, where TTFT/tokens/outcome are already known. Streaming
    // responses (text/event-stream) return their HEAD before those are known, so
    // their line is emitted from the StreamTimer drop instead — never both
    // (the `emitted` flag also guards against a double-call). Requests that
    // never reached a dispatch flow (no `flow` recorded) are skipped to keep the
    // event scoped to actual model traffic.
    if !is_event_stream_response(&response) {
        let has_flow = context
            .summary
            .lock()
            .map(|s| s.flow.is_some())
            .unwrap_or_else(|p| p.into_inner().flow.is_some());
        if has_flow {
            context.set_outcome_if_unset(if response.status().is_success() {
                "ok"
            } else {
                "error"
            });
            crate::libs::request_context::emit_request_completed(&context);
        }
    }
    response
}

/// Whether a response is an SSE (`text/event-stream`) body — the signal that its
/// `request.completed` line is owned by the StreamTimer drop, not the middleware.
fn is_event_stream_response(response: &Response) -> bool {
    response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}

/// Decrements the in-flight gauge on drop, so a panicking handler can't leak the
/// gauge (the panic unwinds through this guard before the CatchPanicLayer).
struct InFlightGuard;

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        gauge!("http_requests_in_flight").decrement(1.0);
    }
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
    // Track concurrent in-flight requests. A drop guard decrements even if a
    // handler panics (the CatchPanicLayer is outside this middleware), so the
    // gauge can't leak upward on the error path.
    gauge!("http_requests_in_flight").increment(1.0);
    let _in_flight = InFlightGuard;
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

/// Prometheus text-exposition endpoint. NOT in the unauthenticated allowlist: it
/// is subject to the normal API-key check (see `request_auth.rs`), so it is open
/// only when `auth.apiKeys` is empty and requires a valid key once keys are
/// configured — this avoids leaking LAN traffic patterns to anonymous clients.
async fn metrics_handler() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4")
        .body(Body::from(crate::libs::metrics::render()))
        .unwrap()
}

/// Readiness probe: ready only once a copilot token is held, that token is not
/// already past its expiry, and the model list has been cached. Distinct from
/// `/` (liveness), which is always 200.
///
/// In provider-only mode the Copilot token and model cache are not used, so
/// readiness is declared immediately: the proxy is ready to forward traffic to
/// the configured provider as soon as it binds.
///
/// Asserting freshness (not just presence) avoids a false-green during a refresh
/// outage: the refresh loop retries with backoff while continuing to hold the
/// now-expired token, so a presence-only check would keep reporting "ready" even
/// though every upstream call is failing with 401. Tokens without a parseable
/// `exp=` fall back to presence-based readiness.
async fn readyz() -> Response {
    // In provider-only mode there is no Copilot token — readiness is immediate.
    let provider_only = crate::libs::state::with_state(|s| s.provider_only.clone());
    if let Some(provider) = provider_only {
        return (
            StatusCode::OK,
            Json(json!({"status": "ready", "mode": "provider_only", "provider": provider})),
        )
            .into_response();
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Compute the three readiness signals separately so a 503 can report WHICH
    // one failed — "token refresh failing" (auth/network) and "models never
    // loaded" (cold start) are different incidents an operator must distinguish
    // without log-diving.
    let (token_present, token_fresh, models_cached) = crate::libs::state::with_state(|s| {
        let token = s.copilot_token.as_deref();
        let present = token.is_some_and(|t| !t.is_empty());
        let fresh =
            token.is_some_and(|t| crate::routes::token::copilot_token_is_fresh(t, now_secs));
        (present, fresh, s.models.is_some())
    });

    if token_present && token_fresh && models_cached {
        return (StatusCode::OK, Json(json!({"status": "ready"}))).into_response();
    }

    let mut reasons: Vec<&str> = Vec::new();
    if !token_present {
        reasons.push("token_missing");
    } else if !token_fresh {
        reasons.push("token_expired");
    }
    if !models_cached {
        reasons.push("models_not_loaded");
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"status": "not_ready", "reasons": reasons})),
    )
        .into_response()
}

/// Build/version metadata endpoint: returns the git SHA and build timestamp
/// captured at compile time by `build.rs`, plus the crate version. Mounted
/// outside auth so liveness/version probes need no API key.
async fn version() -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "git_sha": env!("GIT_SHA"),
            "build_timestamp": env!("BUILD_TIMESTAMP"),
        })),
    )
        .into_response()
}

fn header_string(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn general_auth_middleware(req: Request, next: Next) -> Response {
    let options = AuthOptions::general();
    match check_auth(
        &options,
        req.method(),
        req.uri().path(),
        req.headers(),
        false,
    ) {
        AuthOutcome::Reject(rejection) => return rejection,
        AuthOutcome::Allow(Some(label)) => {
            // Record which named key served this request so token usage can be
            // attributed per client. The trace layer installed the task-local
            // context outermost, so this fills its (interior-mutable) cell.
            crate::libs::request_context::set_request_api_key_label(label);
        }
        AuthOutcome::Allow(None) => {}
    }
    next.run(req).await
}

async fn admin_auth_middleware(req: Request, next: Next) -> Response {
    let options = AuthOptions::admin();
    match check_auth(
        &options,
        req.method(),
        req.uri().path(),
        req.headers(),
        true,
    ) {
        AuthOutcome::Reject(rejection) => return rejection,
        AuthOutcome::Allow(Some(label)) => {
            crate::libs::request_context::set_request_api_key_label(label);
        }
        AuthOutcome::Allow(None) => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn panic_handler() -> &'static str {
        panic!("fixture panic")
    }

    async fn panic_before_next(_req: Request, _next: Next) -> Response {
        panic!("fixture middleware panic")
    }

    async fn panic_response(path: &str) -> serde_json::Value {
        let app = Router::new()
            .route("/v1/responses", get(panic_handler))
            .route("/v1/messages", get(panic_handler))
            .layer(from_fn(panic_middleware));
        let response = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .expect("panic middleware responds");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect panic body")
            .to_bytes();
        serde_json::from_slice(&body).expect("panic body is JSON")
    }

    #[tokio::test]
    async fn panic_errors_are_protocol_native() {
        let openai = panic_response("/v1/responses").await;
        assert!(openai.get("type").is_none());
        assert_eq!(openai["error"]["type"], "server_error");

        let anthropic = panic_response("/v1/messages").await;
        assert_eq!(anthropic["type"], "error");
        assert_eq!(anthropic["error"]["type"], "api_error");
    }

    #[tokio::test]
    async fn outer_panic_catcher_protects_panicking_middleware() {
        for (path, expected_type) in [
            ("/v1/responses", "server_error"),
            ("/v1/messages", "api_error"),
        ] {
            let app = Router::new()
                .route("/v1/responses", get(|| async { "ok" }))
                .route("/v1/messages", get(|| async { "ok" }))
                .layer(from_fn(panic_before_next))
                .layer(from_fn(panic_middleware));
            let response = app
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .expect("outer panic middleware responds");
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let body = response
                .into_body()
                .collect()
                .await
                .expect("collect panic body")
                .to_bytes();
            let json: serde_json::Value =
                serde_json::from_slice(&body).expect("panic body is JSON");
            assert_eq!(json["error"]["type"], expected_type);
        }
    }
}
