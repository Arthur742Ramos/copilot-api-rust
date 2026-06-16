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
//!
//! To extend: add a `/v1/messages` (Anthropic) request, or assert on the parsed
//! choices. Keep it best-effort — the goal is a human-run end-to-end check.

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
}
