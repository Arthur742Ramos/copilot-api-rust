//! Shared helpers for the integration tests. Each `tests/*.rs` is its own crate
//! that links the `copilot_api` library, so these helpers live in a `common`
//! module that every test file includes via `mod common;`.
//!
//! Tests drive the real axum router (`copilot_api::server::build_router`) through
//! `tower::ServiceExt::oneshot`, so no network listener is involved. Because the
//! router and auth read the process-global cached config, tests that install a
//! config MUST be marked `#[serial_test::serial]`.

#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use copilot_api::libs::config::{set_cached_config_for_test, AppConfig, AuthConfig};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Build the production router exactly as `main.rs` does.
pub fn router() -> Router {
    copilot_api::server::build_router()
}

/// Install a cached config with the given regular API keys and optional admin
/// key, bypassing disk. Pass an empty slice / `None` to model "no keys".
pub fn set_config(api_keys: &[&str], admin_key: Option<&str>) {
    let api_keys_value: Vec<Value> = api_keys.iter().map(|k| json!(k)).collect();
    let config = AppConfig {
        auth: Some(AuthConfig {
            api_keys: Some(api_keys_value),
            admin_api_key: admin_key.map(|k| json!(k)),
        }),
        ..Default::default()
    };
    set_cached_config_for_test(config);
}

/// Send a request through the router and return the status plus the raw body
/// bytes. Consumes a fresh router per call (oneshot takes ownership).
pub async fn send(request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response: Response<Body> = router().oneshot(request).await.expect("router responds");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec();
    (status, bytes)
}

/// Like [`send`] but also returns the response headers (needed for trace-id /
/// WWW-Authenticate assertions).
pub async fn send_full(request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response: Response<Body> = router().oneshot(request).await.expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

/// Parse body bytes as JSON.
pub fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("response body is JSON")
}
