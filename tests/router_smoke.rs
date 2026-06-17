//! Cheap route-table regression checks: landing page, 404 for unknown routes,
//! the trace-id response header, and the usage-viewer dashboard. These hit
//! unauthenticated / non-upstream paths, so no config seam is required, but the
//! landing/usage routes read the (possibly shared) cached config indirectly via
//! auth, so we keep them serial to avoid racing other config-mutating tests.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{send, send_full, set_config};

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn root_returns_server_running() {
    let (status, body) = send(get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "Server running");
}

#[tokio::test]
#[serial_test::serial]
async fn unknown_route_returns_404() {
    // No keys configured so the general auth layer lets the request through to
    // routing (with keys, an unknown protected path would be 401 first).
    set_config(&[], None);
    let (status, _) = send(get("/this/route/does/not/exist")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[serial_test::serial]
async fn trace_id_header_is_present() {
    let (status, headers, _) = send_full(get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers.contains_key("x-trace-id"),
        "trace middleware should add x-trace-id"
    );
    assert!(!headers["x-trace-id"].is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn incoming_trace_id_is_echoed() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header("x-trace-id", "trace-abc-123")
        .body(Body::empty())
        .unwrap();
    let (_, headers, _) = send_full(request).await;
    assert_eq!(headers["x-trace-id"], "trace-abc-123");
}

#[tokio::test]
#[serial_test::serial]
async fn usage_viewer_returns_dashboard() {
    let (status, body) = send(get("/usage-viewer")).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Token Usage Dashboard"));
}

#[tokio::test]
#[serial_test::serial]
async fn malformed_json_returns_invalid_request_error_shape() {
    // A malformed body must produce the Anthropic JSON error shape, not axum's
    // default plain-text Json<Value> rejection. The parse happens before any
    // upstream call, so no token/config seam is needed.
    set_config(&[], None);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from("{not valid json"))
        .unwrap();
    let (status, body) = send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let json: serde_json::Value =
        serde_json::from_slice(&body).expect("error body must be JSON, not plain text");
    assert_eq!(json["error"]["type"], "invalid_request_error");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("Invalid JSON"));
}

#[tokio::test]
#[serial_test::serial]
async fn empty_model_returns_invalid_request_error() {
    // An empty model field is rejected up front with a 400, not silently mapped
    // to a default and run (which previously returned 200).
    set_config(&[], None);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model":"","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();
    let (status, body) = send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON error body");
    assert_eq!(json["error"]["type"], "invalid_request_error");
}

#[tokio::test]
#[serial_test::serial]
async fn oversize_body_returns_json_shaped_413() {
    // A body over the 32 MiB limit must return a 413 with the Anthropic JSON
    // error shape, not axum's plain-text "length limit exceeded" rejection.
    set_config(&[], None);
    // Just over the configured request-body limit; derive from the constant so
    // this stays correct if the limit changes.
    let over_limit = copilot_api::libs::http::MAX_REQUEST_BODY_BYTES + 1024;
    let big = "x".repeat(over_limit);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"model":"claude-haiku-4.5","max_tokens":8,"messages":[{{"role":"user","content":"{big}"}}]}}"#
        )))
        .unwrap();
    let (status, body) = send(request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let json: serde_json::Value =
        serde_json::from_slice(&body).expect("413 body must be JSON, not plain text");
    assert_eq!(json["error"]["type"], "invalid_request_error");
}
