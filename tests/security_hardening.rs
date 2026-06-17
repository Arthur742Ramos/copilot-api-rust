//! Integration tests for the Cluster D security hardening: `/token` no longer
//! leaks the raw Copilot bearer, `/metrics` is gated behind auth when API keys
//! are configured, and CORS only reflects loopback origins (never a wildcard /
//! arbitrary remote origin).
//!
//! These drive the real router via `tower::ServiceExt::oneshot`. Tests that
//! install a config touch the process-global cached config, so they are
//! `#[serial_test::serial]`.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{json_body, send, send_full, set_config};
use copilot_api::libs::state;

const REGULAR_KEY: &str = "regular-key";

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

// /token never returns the raw bearer, only presence + expiry, even when no API
// keys are configured (the auth layer would otherwise allow it through).
#[tokio::test]
#[serial_test::serial]
async fn token_endpoint_does_not_leak_raw_bearer() {
    set_config(&[], None);
    state::with_state_mut(|s| s.copilot_token = Some("tid=abc;exp=1700000000;sku=copilot".into()));

    let (status, body) = send(get("/token")).await;
    assert_eq!(status, StatusCode::OK);
    let json = json_body(&body);

    assert_eq!(json.get("hasToken").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        json.get("expiresAt").and_then(|v| v.as_i64()),
        Some(1_700_000_000)
    );
    // The secret itself must never appear in the response.
    assert!(json.get("token").is_none(), "raw token must not be exposed");
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("sku=copilot"),
        "token contents leaked: {text}"
    );

    state::with_state_mut(|s| s.copilot_token = None);
}

#[tokio::test]
#[serial_test::serial]
async fn token_endpoint_reports_absence() {
    set_config(&[], None);
    state::with_state_mut(|s| s.copilot_token = None);

    let (status, body) = send(get("/token")).await;
    assert_eq!(status, StatusCode::OK);
    let json = json_body(&body);
    assert_eq!(json.get("hasToken").and_then(|v| v.as_bool()), Some(false));
}

// /metrics requires a valid key once API keys are configured.
#[tokio::test]
#[serial_test::serial]
async fn metrics_requires_auth_when_keys_configured() {
    set_config(&[REGULAR_KEY], None);
    let (status, _) = send(get("/metrics")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let with_key = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .header("x-api-key", REGULAR_KEY)
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(with_key).await;
    assert_eq!(status, StatusCode::OK);
}

// With no keys configured, /metrics stays open (current behavior preserved).
#[tokio::test]
#[serial_test::serial]
async fn metrics_open_when_no_keys() {
    set_config(&[], None);
    let (status, _) = send(get("/metrics")).await;
    assert_eq!(status, StatusCode::OK);
}

// /readyz stays unauthenticated even with keys configured.
#[tokio::test]
#[serial_test::serial]
async fn readyz_is_unauthenticated() {
    set_config(&[REGULAR_KEY], None);
    let (status, _) = send(get("/readyz")).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

// A loopback Origin is reflected back (CORS allowed for local tooling).
#[tokio::test]
#[serial_test::serial]
async fn cors_reflects_loopback_origin() {
    set_config(&[], None);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header("origin", "http://localhost:5173")
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = send_full(request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("http://localhost:5173"),
    );
}

// An arbitrary remote Origin gets NO Access-Control-Allow-Origin header, so a
// browser will block the cross-origin read of /token et al.
#[tokio::test]
#[serial_test::serial]
async fn cors_rejects_remote_origin() {
    set_config(&[], None);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header("origin", "https://evil.example.com")
        .body(Body::empty())
        .unwrap();
    let (_, headers, _) = send_full(request).await;
    assert!(
        headers.get("access-control-allow-origin").is_none(),
        "remote origin must not be granted ACAO",
    );
}
