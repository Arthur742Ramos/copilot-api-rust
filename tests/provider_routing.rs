//! Exercises the direct provider routes and public provider/model aliases through
//! the real router. All failure regressions stop before an upstream request, so
//! they are deterministic and credential-free.
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

fn post_count_tokens(path: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}

fn count_tokens_body(model: &str) -> String {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
    })
    .to_string()
}

fn assert_anthropic_error(body: &[u8], error_type: &str, message_fragment: &str) {
    let value = json_body(body);
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], error_type);
    let message = value["error"]["message"]
        .as_str()
        .expect("error message is a string");
    assert!(
        message.contains(message_fragment),
        "expected message containing {message_fragment:?}, got {message:?}"
    );
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
async fn unknown_provider_count_tokens_returns_complete_anthropic_404() {
    set_config(&[], None);

    let provider = "definitely-not-a-real-provider";
    let (status, body) = send(post_count_tokens(
        &format!("/{provider}/v1/messages/count_tokens"),
        count_tokens_body("some-model"),
    ))
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&body),
        json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "Provider 'definitely-not-a-real-provider' not found or disabled",
            },
        })
    );
}

#[tokio::test]
#[serial_test::serial]
async fn provider_model_alias_count_tokens_returns_complete_anthropic_404() {
    set_config(&[], None);

    let (status, body) = send(post_count_tokens(
        "/v1/messages/count_tokens",
        count_tokens_body("definitely-not-a-real-provider/some-model"),
    ))
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&body),
        json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "Provider 'definitely-not-a-real-provider' not found or disabled",
            },
        })
    );
}

#[tokio::test]
#[serial_test::serial]
async fn provider_count_tokens_auth_failure_is_a_complete_anthropic_401() {
    // A synthetic configured key enables auth without relying on any real
    // credential. Omitting it from the request must fail before provider lookup.
    set_config(&["test-only-api-key"], None);

    let (status, body) = send(post_count_tokens(
        "/some-provider/v1/messages/count_tokens",
        count_tokens_body("some-model"),
    ))
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_body(&body),
        json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": "Unauthorized",
            },
        })
    );
}

#[tokio::test]
#[serial_test::serial]
async fn direct_and_alias_count_tokens_malformed_json_returns_anthropic_400() {
    set_config(&[], None);

    for path in [
        "/some-provider/v1/messages/count_tokens",
        "/v1/messages/count_tokens",
    ] {
        let (status, body) = send(post_count_tokens(path, "{not valid json")).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert_anthropic_error(&body, "invalid_request_error", "Invalid JSON");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn direct_and_alias_count_tokens_invalid_payloads_return_anthropic_400() {
    set_config(&[], None);

    for (path, model) in [
        ("/some-provider/v1/messages/count_tokens", "some-model"),
        ("/v1/messages/count_tokens", "some-provider/some-model"),
    ] {
        // Valid JSON, but the required `messages` field is absent.
        let (status, body) = send(post_count_tokens(
            path,
            json!({ "model": model }).to_string(),
        ))
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert_anthropic_error(&body, "invalid_request_error", "Invalid request payload");
        assert!(
            json_body(&body)["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("messages"),
            "{path} should identify the invalid field"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn direct_and_alias_count_tokens_body_limits_return_anthropic_413() {
    set_config(&[], None);

    for (path, model) in [
        ("/some-provider/v1/messages/count_tokens", "some-model"),
        ("/v1/messages/count_tokens", "some-provider/some-model"),
    ] {
        let padding = "x".repeat(copilot_api::libs::http::MAX_REQUEST_BODY_BYTES);
        let body = format!(r#"{{"model":"{model}","messages":[],"padding":"{padding}"}}"#);
        assert!(body.len() > copilot_api::libs::http::MAX_REQUEST_BODY_BYTES);

        let (status, body) = send(post_count_tokens(path, body)).await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        assert_eq!(
            json_body(&body),
            json!({
                "type": "error",
                "error": {
                    "type": "request_too_large",
                    "message": "Request body is too large.",
                },
            }),
            "{path}"
        );
    }
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
