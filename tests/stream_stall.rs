//! End-to-end coverage for proxy-side stall detection on streaming responses.
//!
//! Reproduces the production failure this guard exists for: the upstream accepts
//! the request, returns `200` plus SSE headers and an opening frame, then holds
//! the connection open and sends nothing more. Before the stall budget existed
//! the proxy waited on that wedged socket for the full upstream read-timeout
//! (600s) while emitting keep-alive pings, so every real client hit its own,
//! much shorter deadline first and reported a truncated response
//! ("Response stalled mid-stream") with no terminal event to act on.
//!
//! This file owns its process (each `tests/*.rs` is a separate crate), so it can
//! set the heartbeat/stall environment knobs to sub-second values without racing
//! the other integration suites.

mod common;

use std::sync::{Arc, Once};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use bytes::Bytes;
use common::{send, set_config};
use copilot_api::services::copilot::get_models::{Model, ModelsResponse};
use serde_json::{json, Value};
use tokio::sync::oneshot;

const CLIENT_KEY: &str = "stall-fixture-client-key";
const STALLING_MODEL: &str = "claude-stall-fixture";
const HEALTHY_MODEL: &str = "claude-healthy-fixture";

/// Heartbeat and stall windows for this suite. Scaled down from the 15s/120s
/// production defaults so the test runs in about a second while exercising the
/// exact same code path.
const HEARTBEAT_SECS: u64 = 1;
const STALL_SECS: u64 = 3;

static INIT: Once = Once::new();

fn init_env() {
    INIT.call_once(|| {
        let dir =
            std::env::temp_dir().join(format!("copilot-api-stream-stall-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create stall-suite home");
        std::env::set_var("COPILOT_API_HOME", dir);
        std::env::set_var("COPILOT_API_SSE_HEARTBEAT_SECS", HEARTBEAT_SECS.to_string());
        std::env::set_var("COPILOT_API_SSE_STALL_TIMEOUT_SECS", STALL_SECS.to_string());
    });
}

fn message_start_frame(model: &str) -> String {
    let event = json!({
        "type": "message_start",
        "message": {
            "id": "msg_stall_fixture",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [],
            "stop_reason": Value::Null,
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 11, "output_tokens": 0}
        }
    });
    format!("event: message_start\ndata: {event}\n\n")
}

/// Upstream that opens the stream normally and then wedges: headers and one
/// frame are delivered, after which the connection stays open forever with no
/// further bytes and no FIN.
fn stalling_body(model: &str) -> Body {
    let opening = message_start_frame(model);
    Body::from_stream(async_stream::stream! {
        yield Ok::<Bytes, std::io::Error>(Bytes::from(opening));
        // Never yield again and never end the stream.
        std::future::pending::<()>().await;
    })
}

/// Control upstream: a complete, well-formed stream that terminates properly.
fn healthy_body(model: &str) -> Body {
    let mut text = message_start_frame(model);
    text.push_str(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    text.push_str(
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    );
    text.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    text.push_str(
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
    );
    text.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    Body::from(text)
}

async fn upstream_messages(axum::Json(body): axum::Json<Value>) -> Response {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(STALLING_MODEL)
        .to_string();
    let stream_body = if model == HEALTHY_MODEL {
        healthy_body(&model)
    } else {
        stalling_body(&model)
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(stream_body)
        .expect("build upstream SSE response")
}

struct Upstream {
    base_url: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Upstream {
    async fn start() -> Self {
        init_env();
        let app = Router::new().route("/v1/messages", post(upstream_messages));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stall fixture");
        let addr = listener.local_addr().expect("fixture address");
        let (shutdown, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = receiver.await;
                })
                .await;
        });
        Self {
            base_url: format!("http://{addr}"),
            shutdown: Some(shutdown),
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn configure(upstream: &Upstream) {
    set_config(&[CLIENT_KEY], None);

    let models = ModelsResponse {
        object: "list".to_string(),
        data: [STALLING_MODEL, HEALTHY_MODEL]
            .into_iter()
            .map(|id| Model {
                id: id.to_string(),
                name: id.to_string(),
                supported_endpoints: Some(vec!["/v1/messages".to_string()]),
                ..Default::default()
            })
            .collect(),
    };
    copilot_api::libs::state::with_state_mut(|state| {
        state.provider_only = None;
        state.copilot_token = Some("stall-fixture-token".to_string());
        state.copilot_api_url = Some(upstream.base_url.clone());
        state.account_type = "individual".to_string();
        state.models = Some(Arc::new(models));
        state.premium_interactions = None;
    });
}

fn stream_request(model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .body(Body::from(
            json!({
                "model": model,
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true
            })
            .to_string(),
        ))
        .expect("build client request")
}

fn sse_events(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| !data.is_empty() && *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("SSE data is JSON"))
        .collect()
}

/// A wedged upstream must be ended *by the proxy*, with a terminal error the
/// client can act on — not left hanging until the client's own deadline.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(stream_stall)]
async fn wedged_upstream_is_terminated_with_a_retryable_error() {
    let upstream = Upstream::start().await;
    configure(&upstream);

    // The whole point of the guard: the body completes on its own. Bound it well
    // under the 600s upstream read-timeout so a regression fails fast instead of
    // hanging the suite.
    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        send(stream_request(STALLING_MODEL)),
    )
    .await
    .expect("proxy must end a stalled stream itself, not wait for the client to give up");

    assert_eq!(status, StatusCode::OK);
    let events = sse_events(&body);
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or_default())
        .collect();

    assert_eq!(
        kinds.first(),
        Some(&"message_start"),
        "the opening frame the upstream did send must still reach the client: {kinds:?}"
    );
    assert!(
        kinds.contains(&"ping"),
        "keep-alives must still be emitted while inside the budget: {kinds:?}"
    );

    let terminal = events.last().expect("stream must carry a terminal event");
    assert_eq!(
        terminal["type"], "error",
        "a stalled stream must end with an explicit error, not silence: {kinds:?}"
    );
    assert_eq!(
        terminal["error"]["type"], "overloaded_error",
        "the stall is transient, so it must be the retryable type clients back off and retry on"
    );
    assert!(
        terminal["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("retry"),
        "the message should tell the client this is retryable: {terminal}"
    );
}

/// Proof that the guard above is what ends the stream: with the budget disabled
/// (`COPILOT_API_SSE_STALL_TIMEOUT_SECS=0`, the pre-fix behavior) the very same
/// wedged upstream leaves the response hanging indefinitely, pinging forever,
/// which is exactly what pushed clients into their own "stalled mid-stream"
/// deadline.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(stream_stall)]
async fn disabling_the_budget_reproduces_the_indefinite_hang() {
    let upstream = Upstream::start().await;
    configure(&upstream);

    std::env::set_var("COPILOT_API_SSE_STALL_TIMEOUT_SECS", "0");
    let outcome = tokio::time::timeout(
        // Comfortably past the 3s budget the other tests rely on.
        std::time::Duration::from_secs(STALL_SECS * 4),
        send(stream_request(STALLING_MODEL)),
    )
    .await;
    std::env::set_var("COPILOT_API_SSE_STALL_TIMEOUT_SECS", STALL_SECS.to_string());

    assert!(
        outcome.is_err(),
        "without the stall budget the wedged stream must never terminate on its own — \
         if this now completes, the guard is no longer the thing ending it and the \
         other tests in this file have stopped proving anything"
    );
}

/// The budget measures consecutive silence, so a well-behaved stream must pass
/// through untouched — no spurious ping storm and no injected error.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(stream_stall)]
async fn healthy_stream_is_not_affected_by_the_stall_budget() {
    let upstream = Upstream::start().await;
    configure(&upstream);

    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        send(stream_request(HEALTHY_MODEL)),
    )
    .await
    .expect("healthy stream completes");

    assert_eq!(status, StatusCode::OK);
    let kinds: Vec<String> = sse_events(&body)
        .iter()
        .map(|e| e["type"].as_str().unwrap_or_default().to_string())
        .collect();

    assert_eq!(
        kinds.last().map(String::as_str),
        Some("message_stop"),
        "healthy stream must terminate normally: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|kind| kind == "error"),
        "the stall guard must not inject an error into a healthy stream: {kinds:?}"
    );
}
