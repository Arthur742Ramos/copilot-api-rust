//! Exercises the `/:provider/v1/messages` route through the real router. We only
//! lock the resilient behavior: an unknown provider resolves to a 404
//! "not found" before any upstream call. We deliberately avoid asserting the
//! `openai-responses` 501 status, since a parallel branch is wiring that path to
//! working — coupling to 501 would break when that lands.
//!
//! Config is installed via the seam (no api keys -> auth allows the route), so
//! these tests run serially against the process-global config.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{json_body, send, set_config};
use serde_json::json;

fn post_provider_messages(provider: &str) -> Request<Body> {
    let body = json!({
        "model": "some-model",
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    Request::builder()
        .method(Method::POST)
        .uri(format!("/{provider}/v1/messages"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn unknown_provider_returns_404() {
    // No keys configured -> general auth allows the request to reach the handler.
    set_config(&[], None);

    let (status, body) = send(post_provider_messages("definitely-not-a-real-provider")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let value = json_body(&body);
    let message = value["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("not found") || message.contains("disabled"),
        "expected a not-found/disabled error, got: {message}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn invalid_body_is_rejected_before_resolution() {
    set_config(&[], None);

    // Missing required `max_tokens` / `messages` -> 400 invalid_request_error.
    let request = Request::builder()
        .method(Method::POST)
        .uri("/some-provider/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "model": "x" }).to_string()))
        .unwrap();
    let (status, _) = send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
