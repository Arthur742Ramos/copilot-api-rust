//! Cheap route-table regression checks: landing page, 404 for unknown routes,
//! the trace-id response header, and the usage-viewer placeholder. These hit
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
async fn usage_viewer_returns_placeholder() {
    let (status, body) = send(get("/usage-viewer")).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Usage Viewer"));
}
