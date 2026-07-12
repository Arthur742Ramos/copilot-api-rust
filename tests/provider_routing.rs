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
use copilot_api::libs::state;
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
async fn unknown_provider_returns_complete_anthropic_404() {
    // No keys configured -> general auth allows the request to reach the handler.
    set_config(&[], None);

    let (status, body) = send(post_provider_messages("definitely-not-a-real-provider")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert_eq!(
        json_body(&body),
        json!({
            "type": "error",
            "error": {
                "message": "Provider 'definitely-not-a-real-provider' not found or disabled",
                "type": "invalid_request_error",
            },
        })
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

#[tokio::test]
#[serial_test::serial]
async fn direct_provider_route_cannot_bypass_shared_admission() {
    set_config(&[], None);
    state::with_state_mut(|state| {
        state.rate_limit_seconds = Some(3_600);
        state.rate_limit_wait = false;
    });

    // The first request owns the rate-limit slot and reaches provider
    // resolution. The second must be rejected before the same early dispatch.
    let (first, _) = send(post_provider_messages("missing-provider-a")).await;
    let (second, _) = send(post_provider_messages("missing-provider-b")).await;
    assert_eq!(first, StatusCode::NOT_FOUND);
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);

    state::with_state_mut(|state| state.rate_limit_seconds = None);
    copilot_api::libs::rate_limit::check_rate_limit()
        .await
        .expect("disabling the limiter clears its schedule");
}

#[tokio::test]
#[serial_test::serial]
async fn messages_early_dispatches_cannot_bypass_shared_admission() {
    set_config(&[], None);
    state::with_state_mut(|state| {
        state.rate_limit_seconds = Some(3_600);
        state.rate_limit_wait = false;
    });
    copilot_api::libs::rate_limit::check_rate_limit()
        .await
        .expect("seed the occupied rate-limit slot");

    let request = |model: &str, tools: serde_json::Value| {
        Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": model,
                    "max_tokens": 16,
                    "messages": [{ "role": "user", "content": "hi" }],
                    "tools": tools,
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (alias_status, _) = send(request("missing-provider/model", json!([]))).await;
    let (web_search_status, _) = send(request(
        "gpt-5-mini",
        json!([{ "type": "web_search_20250305", "name": "web_search" }]),
    ))
    .await;
    assert_eq!(alias_status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(web_search_status, StatusCode::TOO_MANY_REQUESTS);

    state::with_state_mut(|state| state.rate_limit_seconds = None);
    copilot_api::libs::rate_limit::check_rate_limit()
        .await
        .expect("disabling the limiter clears its schedule");
}
