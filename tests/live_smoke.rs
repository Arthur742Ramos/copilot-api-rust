//! Gated LIVE smoke test. This is `#[ignore]` so it never runs in CI or a normal
//! `cargo test`. Run it explicitly with a real GitHub token:
//!
//!   COPILOT_SMOKE_TOKEN=<github-token> \
//!     cargo test --test live_smoke -- --ignored --nocapture
//!
//! If `COPILOT_SMOKE_TOKEN` is unset, the test prints a skip message and passes,
//! so `cargo test -- --ignored` across the suite stays green without a token.
//!
//! What it validates when a token IS provided:
//!   1. The Copilot token exchange (`setup_copilot_token`) against the real
//!      GitHub Copilot upstream succeeds.
//!   2. `cache_models` fetches the live model catalogue (non-empty).
//!   3. A minimal `/v1/chat/completions` request through the real router returns
//!      200 with a non-empty body.
//!   4. A minimal streaming `/v1/messages` request returns 200, exercising the
//!      Anthropic translation flow and its streaming metric paths.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::send;
use copilot_api::libs::paths::ensure_paths;
use copilot_api::libs::state;
use copilot_api::libs::token::setup_copilot_token;
use copilot_api::libs::utils::cache_models;
use serde_json::json;

#[tokio::test]
#[ignore = "live smoke test; requires COPILOT_SMOKE_TOKEN and network access"]
async fn live_chat_completion_smoke() {
    let token = match std::env::var("COPILOT_SMOKE_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t,
        _ => {
            eprintln!(
                "SKIP live_smoke: set COPILOT_SMOKE_TOKEN=<github-token> to run this test \
                 (cargo test --test live_smoke -- --ignored)."
            );
            return;
        }
    };

    // Minimal subset of run_server's startup sequence.
    ensure_paths().await.expect("ensure_paths");
    state::with_state_mut(|s| s.github_token = Some(token));

    setup_copilot_token()
        .await
        .expect("setup_copilot_token (token exchange) should succeed with a valid token");

    cache_models()
        .await
        .expect("cache_models should fetch the live catalogue");

    let model_id = state::with_state(|s| {
        s.models
            .as_ref()
            .and_then(|m| m.data.first().map(|model| model.id.clone()))
    })
    .expect("at least one model should be available after cache_models");
    eprintln!("live_smoke: using model {model_id}");

    let payload = json!({
        "model": model_id,
        "max_tokens": 16,
        "messages": [
            { "role": "user", "content": "Reply with the single word: pong" }
        ],
    });

    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let (status, body) = send(request).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "chat completion should return 200; body: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(!body.is_empty(), "response body should be non-empty");
    eprintln!("live_smoke: chat completion OK ({} bytes)", body.len());

    // Also exercise the Anthropic /v1/messages path so the messages/responses
    // flow (and its metric label paths: proxy_stream_*{flow=...},
    // copilot_upstream_request_seconds{endpoint=...}) is covered end-to-end, not
    // just chat_completions. Streaming so the SSE translation + StreamTimer run.
    let messages_payload = json!({
        "model": model_id,
        "max_tokens": 16,
        "stream": true,
        "messages": [
            { "role": "user", "content": "Reply with the single word: pong" }
        ],
    });
    let messages_request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(messages_payload.to_string()))
        .unwrap();
    // Bound the streamed request so a stalled/never-closing SSE connection makes
    // this human-run test fail fast instead of hanging indefinitely.
    let (status, body) =
        tokio::time::timeout(std::time::Duration::from_secs(60), send(messages_request))
            .await
            .expect("/v1/messages should complete within 60s (SSE did not close)");
    assert_eq!(
        status,
        StatusCode::OK,
        "/v1/messages should return 200; body: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        !body.is_empty(),
        "/v1/messages response body should be non-empty"
    );
    eprintln!("live_smoke: /v1/messages OK ({} bytes)", body.len());
}
