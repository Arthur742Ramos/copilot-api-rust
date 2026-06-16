//! Ports tests/request-auth.test.ts through the real axum middleware stack
//! (trace -> cors -> general auth -> admin auth) built by `build_router`.
//!
//! We drive the router via `tower::ServiceExt::oneshot` (no network listener)
//! and target routes that return BEFORE any upstream Copilot/GitHub call:
//!   - `/token` (protected, returns `{}` from local state) for the general layer
//!   - `/admin/config/model-mappings` (admin layer; GET reads config on disk)
//!   - `/` (unauthenticated landing) and `OPTIONS` (CORS preflight bypass)
//!
//! Config is installed via the cached-config test seam, so these tests must run
//! serially against the process-global config.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{send, set_config};

const REGULAR_KEY: &str = "regular-key";
const ADMIN_KEY: &str = "admin-key";

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn get_with_key(path: &str, header: &str, value: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .header(header, value)
        .body(Body::empty())
        .unwrap()
}

// (a) A valid regular key on a protected non-admin route passes auth. It does NOT
// reach an upstream (we hit /token which reads local state), so the only thing we
// assert is that it is NOT rejected with 401.
#[tokio::test]
#[serial_test::serial]
async fn regular_key_passes_on_protected_route() {
    set_config(&[REGULAR_KEY], Some(ADMIN_KEY));
    let (status, _) = send(get_with_key("/token", "x-api-key", REGULAR_KEY)).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED, "regular key should pass");
    assert_eq!(status, StatusCode::OK);
}

// (b) The admin key on an admin route passes both auth layers. GET on the
// model-mappings route returns 200 JSON (reads config; no upstream call).
#[tokio::test]
#[serial_test::serial]
async fn admin_key_passes_on_admin_route() {
    set_config(&[REGULAR_KEY], Some(ADMIN_KEY));
    let (status, _) = send(get_with_key(
        "/admin/config/model-mappings",
        "authorization",
        &format!("Bearer {ADMIN_KEY}"),
    ))
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(status, StatusCode::OK);
}

// (c) A regular (non-admin) key is rejected with 401 on an admin route.
#[tokio::test]
#[serial_test::serial]
async fn regular_key_rejected_on_admin_route() {
    set_config(&[REGULAR_KEY], Some(ADMIN_KEY));
    let (status, _) = send(get_with_key(
        "/admin/config/model-mappings",
        "x-api-key",
        REGULAR_KEY,
    ))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// (extra parity from TS) An admin key is rejected with 401 on a protected
// non-admin route (the general layer only accepts regular keys).
#[tokio::test]
#[serial_test::serial]
async fn admin_key_rejected_on_protected_route() {
    set_config(&[REGULAR_KEY], Some(ADMIN_KEY));
    let (status, _) = send(get_with_key("/token", "x-api-key", ADMIN_KEY)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// (d) With no regular keys configured, general routes are allowed (not 401).
#[tokio::test]
#[serial_test::serial]
async fn no_regular_keys_allows_general_route() {
    set_config(&[], Some(ADMIN_KEY));
    let (status, _) = send(get("/token")).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(status, StatusCode::OK);
}

// (e) With no admin key configured, admin routes are rejected with 401.
#[tokio::test]
#[serial_test::serial]
async fn no_admin_key_rejects_admin_route() {
    set_config(&[REGULAR_KEY], None);
    let (status, _) = send(get("/admin/config/model-mappings")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// (f) OPTIONS requests bypass auth on admin routes (CORS preflight).
#[tokio::test]
#[serial_test::serial]
async fn options_bypasses_admin_auth() {
    set_config(&[REGULAR_KEY], Some(ADMIN_KEY));
    let request = Request::builder()
        .method(Method::OPTIONS)
        .uri("/admin/config/model-mappings")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(request).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

// (g) The unauthenticated landing path `/` returns 200 "Server running" even
// with keys configured.
#[tokio::test]
#[serial_test::serial]
async fn landing_path_is_unauthenticated() {
    set_config(&[REGULAR_KEY], Some(ADMIN_KEY));
    let (status, body) = send(get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "Server running");
}
