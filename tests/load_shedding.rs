//! Concurrency-admission regression tests. These use tiny limits and synthetic
//! response bodies so no test contacts GitHub or any configured provider.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, Response, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use copilot_api::libs::admission::{admission_middleware, AdmissionController};
use copilot_api::libs::config::{
    set_cached_config_for_test, AppConfig, AuthConfig, ModelConfig, ProviderConfig,
};
use futures_util::future::join_all;
use http_body_util::BodyExt;
use serde_json::{json, Map, Value};
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;

fn limited(limit: usize) -> AdmissionController {
    AdmissionController::limited(NonZeroUsize::new(limit).expect("test limit is non-zero"))
}

fn request(method: Method, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("valid request")
}

async fn assert_overloaded(response: Response<Body>, expect_body: bool, path: &str) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    let body = response
        .into_body()
        .collect()
        .await
        .expect("overload body")
        .to_bytes();
    if !expect_body {
        assert!(body.is_empty(), "HEAD responses must not include a body");
        return;
    }
    let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON overload body");
    if copilot_api::libs::error::is_openai_native_path(path) {
        assert!(value.get("type").is_none());
        assert_eq!(value["error"]["type"], "server_error");
        assert_eq!(value["error"]["code"], "server_overloaded");
    } else {
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], "overloaded_error");
    }
}

fn guarded_test_router(controller: AdmissionController, router: Router) -> Router {
    router.route_layer(from_fn_with_state(controller, admission_middleware))
}

#[tokio::test]
#[serial_test::serial]
async fn unlimited_remains_the_default() {
    let controller = AdmissionController::default();
    assert_eq!(controller.limit(), None);

    let permits: Vec<_> = (0..126)
        .map(|_| controller.try_acquire().expect("unlimited admission"))
        .collect();
    assert_eq!(controller.current(), 126);
    drop(permits);
    assert_eq!(controller.current(), 0);
}

#[tokio::test]
#[serial_test::serial]
async fn configured_overload_covers_upstream_routes_but_not_control_plane() {
    common::set_config(&[], None);
    let controller = limited(1);
    let held = controller.try_acquire().expect("occupy only slot");
    let app = copilot_api::server::build_router_with_admission(controller.clone());

    let guarded_routes = [
        (Method::POST, "/chat/completions"),
        (Method::POST, "/v1/chat/completions"),
        (Method::GET, "/models"),
        (Method::GET, "/v1/models"),
        (Method::GET, "/models/test"),
        (Method::GET, "/v1/models/test"),
        (Method::HEAD, "/models"),
        (Method::HEAD, "/v1/models"),
        (Method::HEAD, "/models/test"),
        (Method::HEAD, "/v1/models/test"),
        (Method::POST, "/embeddings"),
        (Method::POST, "/v1/embeddings"),
        (Method::POST, "/images/generations"),
        (Method::POST, "/images/edits"),
        (Method::POST, "/v1/images/generations"),
        (Method::POST, "/v1/images/edits"),
        (Method::POST, "/responses"),
        (Method::POST, "/v1/responses"),
        (Method::POST, "/responses/compact"),
        (Method::POST, "/v1/responses/compact"),
        (Method::POST, "/v1/messages"),
        (Method::POST, "/v1/messages/count_tokens"),
        (Method::POST, "/test-provider/v1/messages"),
        (Method::POST, "/test-provider/v1/messages/count_tokens"),
        (Method::POST, "/test-provider/v1/images/generations"),
        (Method::POST, "/test-provider/v1/images/edits"),
        (Method::GET, "/test-provider/v1/models"),
        (Method::HEAD, "/test-provider/v1/models"),
    ];
    for (method, path) in guarded_routes {
        let expect_body = method != Method::HEAD;
        let response = app
            .clone()
            .oneshot(request(method, path))
            .await
            .expect("router response");
        assert_overloaded(response, expect_body, path).await;
    }

    for path in ["/", "/version", "/readyz", "/admin/config"] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path))
            .await
            .expect("control response");
        assert_ne!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("1"),
            "{path} must bypass upstream admission"
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("control body")
            .to_bytes();
        assert!(
            !String::from_utf8_lossy(&body).contains("overloaded_error"),
            "{path} returned an overload body"
        );
    }

    let metrics = app
        .clone()
        .oneshot(request(Method::GET, "/metrics"))
        .await
        .expect("metrics response");
    assert_eq!(metrics.status(), StatusCode::OK);
    let metrics = metrics
        .into_body()
        .collect()
        .await
        .expect("metrics body")
        .to_bytes();
    let metrics = String::from_utf8_lossy(&metrics);
    assert!(metrics.contains("proxy_upstream_concurrency_limit 1"));
    assert!(metrics.contains("proxy_upstream_requests_active 1"));
    assert!(metrics.contains("proxy_upstream_overload_rejections_total"));

    drop(held);
    assert_eq!(controller.current(), 0);
}

async fn controlled_stream(State(release): State<Arc<tokio::sync::Semaphore>>) -> Response<Body> {
    let stream = async_stream::stream! {
        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"first"));
        let _release = release.acquire().await.expect("release semaphore open");
        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"last"));
    };
    Response::new(Body::from_stream(stream))
}

#[tokio::test]
#[serial_test::serial]
async fn streaming_permit_is_held_until_body_completion() {
    let controller = limited(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let app = guarded_test_router(
        controller.clone(),
        Router::new()
            .route("/work", get(controlled_stream))
            .with_state(Arc::clone(&release)),
    );

    let first = app
        .clone()
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("first response");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(controller.current(), 1);

    let mut body = first.into_body();
    let first_frame = body
        .frame()
        .await
        .expect("first frame")
        .expect("first frame is successful");
    assert_eq!(
        first_frame.into_data().expect("data frame"),
        Bytes::from_static(b"first")
    );
    assert_eq!(controller.current(), 1);

    let excess = app
        .clone()
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("overload response");
    assert_overloaded(excess, true, "/work").await;

    release.add_permits(1);
    let last = body
        .frame()
        .await
        .expect("last frame")
        .expect("last frame is successful");
    assert_eq!(
        last.into_data().expect("data frame"),
        Bytes::from_static(b"last")
    );
    assert!(body.frame().await.is_none(), "stream should reach EOF");
    drop(body);
    assert_eq!(controller.current(), 0);

    let next = app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("slot is reusable");
    assert_eq!(next.status(), StatusCode::OK);
    drop(next);
    assert_eq!(controller.current(), 0);
}

async fn error_stream() -> Response<Body> {
    let stream = futures_util::stream::once(async {
        Err::<Bytes, std::io::Error>(std::io::Error::other("synthetic body failure"))
    });
    Response::new(Body::from_stream(stream))
}

#[tokio::test]
#[serial_test::serial]
async fn cancellation_and_body_errors_release_permits() {
    let controller = limited(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let cancel_app = guarded_test_router(
        controller.clone(),
        Router::new()
            .route("/work", get(controlled_stream))
            .with_state(release),
    );

    let response = cancel_app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("stream response");
    assert_eq!(controller.current(), 1);
    drop(response);
    assert_eq!(controller.current(), 0);

    let error_app = guarded_test_router(
        controller.clone(),
        Router::new().route("/work", get(error_stream)),
    );
    let response = error_app
        .clone()
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("error stream response");
    assert_eq!(controller.current(), 1);
    assert!(
        response.into_body().collect().await.is_err(),
        "synthetic body error should propagate"
    );
    assert_eq!(controller.current(), 0);

    let next = error_app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("slot is reusable after error");
    assert_eq!(next.status(), StatusCode::OK);
    drop(next);
    assert_eq!(controller.current(), 0);
}

async fn panic_handler() -> Response<Body> {
    panic!("synthetic handler panic");
}

#[tokio::test]
#[serial_test::serial]
async fn handler_panics_release_permits() {
    let controller = limited(1);
    let app = guarded_test_router(
        controller.clone(),
        Router::new().route("/work", get(panic_handler)),
    )
    .layer(CatchPanicLayer::new());

    let response = app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("panic is converted to a response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(controller.current(), 0);
}

async fn pending_stream() -> impl IntoResponse {
    let stream = futures_util::stream::pending::<Result<Bytes, Infallible>>();
    Body::from_stream(stream)
}

#[tokio::test]
#[serial_test::serial]
async fn burst_of_126_requests_is_bounded_without_an_upstream() {
    const BURST: usize = 126;
    const LIMIT: usize = 4;

    let controller = limited(LIMIT);
    let app = guarded_test_router(
        controller.clone(),
        Router::new().route("/work", get(pending_stream)),
    );

    let responses =
        join_all((0..BURST).map(|_| app.clone().oneshot(request(Method::GET, "/work")))).await;
    let mut responses: Vec<Response<Body>> = responses
        .into_iter()
        .map(|result| result.expect("router response"))
        .collect();

    let accepted = responses
        .iter()
        .filter(|response| response.status() == StatusCode::OK)
        .count();
    let rejected = responses
        .iter()
        .filter(|response| response.status() == StatusCode::SERVICE_UNAVAILABLE)
        .count();
    assert_eq!(accepted, LIMIT);
    assert_eq!(rejected, BURST - LIMIT);
    assert_eq!(controller.current(), LIMIT);

    responses.clear();
    assert_eq!(controller.current(), 0);

    let next = app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("slot is reusable after burst drains");
    assert_eq!(next.status(), StatusCode::OK);
    drop(next);
    assert_eq!(controller.current(), 0);
}

#[derive(Clone, Default)]
struct UltracodeUpstreamState {
    requests: Arc<AtomicUsize>,
    sessions: Arc<Mutex<HashSet<String>>>,
}

async fn ultracode_messages_upstream(
    State(state): State<UltracodeUpstreamState>,
    Json(body): Json<Value>,
) -> Response<Body> {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let session = body
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .expect("stress request carries a session id")
        .to_string();
    state
        .sessions
        .lock()
        .expect("session capture lock")
        .insert(session.clone());
    let model = body["model"].as_str().expect("stress model");
    let response_id = format!("msg_{session}");
    let text = format!("OK:{session}");
    let events = [
        (
            "message_start",
            json!({
                "type":"message_start",
                "message":{
                    "id":response_id,
                    "type":"message",
                    "role":"assistant",
                    "model":model,
                    "content":[],
                    "stop_reason":Value::Null,
                    "stop_sequence":Value::Null,
                    "usage":{"input_tokens":1,"output_tokens":0}
                }
            }),
        ),
        (
            "content_block_start",
            json!({
                "type":"content_block_start",
                "index":0,
                "content_block":{"type":"text","text":""}
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type":"content_block_delta",
                "index":0,
                "delta":{"type":"text_delta","text":text}
            }),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            json!({
                "type":"message_delta",
                "delta":{"stop_reason":"end_turn","stop_sequence":Value::Null},
                "usage":{"output_tokens":1}
            }),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ];
    let body = events
        .into_iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect::<String>();
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .expect("valid stress response")
}

fn configure_ultracode_provider(base_url: String) {
    let models = BTreeMap::from([("claude-stress".to_string(), ModelConfig::default())]);
    let providers = BTreeMap::from([(
        "ultracode-fixture".to_string(),
        ProviderConfig {
            provider_type: Some("anthropic".to_string()),
            enabled: Some(true),
            base_url: Some(base_url),
            api_key: Some("fixture-key".to_string()),
            auth_type: Some("x-api-key".to_string()),
            models: Some(models),
            capabilities: None,
            adjust_input_tokens: Some(false),
            extra: Map::new(),
        },
    )]);
    set_cached_config_for_test(AppConfig {
        auth: Some(AuthConfig {
            api_keys: Some(Vec::new()),
            admin_api_key: None,
        }),
        providers: Some(providers),
        ..Default::default()
    });
}

fn ultracode_messages_request(index: usize) -> Request<Body> {
    let session = format!("ultracode-worker-{index:03}");
    let tools = (0..24)
        .map(|tool| {
            json!({
                "name":format!("fixture_tool_{tool:02}"),
                "description":"A deterministic Claude Code stress-test tool.",
                "input_schema":{
                    "type":"object",
                    "properties":{"value":{"type":"string"}},
                    "required":["value"]
                }
            })
        })
        .collect::<Vec<_>>();
    let body = json!({
        "model":"ultracode-fixture/claude-stress",
        "max_tokens":32000,
        "system":[{
            "type":"text",
            "text":"You are an isolated Ultracode worker.",
            "cache_control":{"type":"ephemeral"}
        }],
        "messages":[{"role":"user","content":"Return the worker sentinel."}],
        "tools":tools,
        "thinking":{"type":"adaptive","display":"omitted"},
        "output_config":{"effort":"xhigh"},
        "context_management":{
            "edits":[{"type":"clear_thinking_20251015","keep":"all"}]
        },
        "metadata":{"user_id":session},
        "stream":true
    });
    Request::builder()
        .method(Method::POST)
        .uri("/v1/messages?beta=true")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            "interleaved-thinking-2025-05-14,context-management-2025-06-27",
        )
        .header("user-agent", "claude-code/2.1.209")
        .header("x-claude-code-session-id", format!("session-{index:03}"))
        .body(Body::from(body.to_string()))
        .expect("valid Ultracode request")
}

async fn assert_worker_stream(response: Response<Body>, index: usize) {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("worker stream body")
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).expect("worker stream is UTF-8");
    assert!(body.contains("event: message_start"));
    assert!(body.contains(&format!("OK:ultracode-worker-{index:03}")));
    assert!(body.contains("event: message_stop"));
}

#[tokio::test]
#[serial_test::serial]
async fn ultracode_sized_messages_burst_is_bounded_and_recovers_without_cross_talk() {
    const BURST: usize = 64;
    const LIMIT: usize = 8;

    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let upstream_state = UltracodeUpstreamState::default();
    let upstream = Router::new()
        .route("/v1/messages", post(ultracode_messages_upstream))
        .with_state(upstream_state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Ultracode fixture");
    let address = listener.local_addr().expect("Ultracode fixture address");
    let (shutdown, receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream)
            .with_graceful_shutdown(async {
                let _ = receiver.await;
            })
            .await
            .expect("serve Ultracode fixture");
    });
    configure_ultracode_provider(format!("http://{address}"));

    let controller = limited(LIMIT);
    let app = copilot_api::server::build_router_with_admission(controller.clone());
    let responses = join_all((0..BURST).map(|index| {
        let app = app.clone();
        async move {
            (
                index,
                app.oneshot(ultracode_messages_request(index))
                    .await
                    .expect("router response"),
            )
        }
    }))
    .await;

    let mut accepted = Vec::new();
    let mut rejected = 0;
    for (index, response) in responses {
        if response.status() == StatusCode::OK {
            accepted.push((index, response));
        } else {
            assert_overloaded(response, true, "/v1/messages").await;
            rejected += 1;
        }
    }
    assert_eq!(accepted.len(), LIMIT);
    assert_eq!(rejected, BURST - LIMIT);
    assert_eq!(controller.current(), LIMIT);
    assert_eq!(upstream_state.requests.load(Ordering::Relaxed), LIMIT);
    assert_eq!(
        upstream_state
            .sessions
            .lock()
            .expect("session capture lock")
            .len(),
        LIMIT
    );

    for (position, (index, response)) in accepted.into_iter().enumerate() {
        if position % 2 == 0 {
            assert_worker_stream(response, index).await;
        } else {
            drop(response);
        }
    }
    assert_eq!(controller.current(), 0);

    let recovery = join_all((BURST..BURST + LIMIT).map(|index| {
        let app = app.clone();
        async move {
            (
                index,
                app.oneshot(ultracode_messages_request(index))
                    .await
                    .expect("recovery response"),
            )
        }
    }))
    .await;
    for (index, response) in recovery {
        assert_eq!(response.status(), StatusCode::OK);
        assert_worker_stream(response, index).await;
    }
    assert_eq!(controller.current(), 0);
    assert_eq!(upstream_state.requests.load(Ordering::Relaxed), LIMIT * 2);
    assert_eq!(
        upstream_state
            .sessions
            .lock()
            .expect("session capture lock")
            .len(),
        LIMIT * 2
    );

    let _ = shutdown.send(());
    let _ = server.await;
}
