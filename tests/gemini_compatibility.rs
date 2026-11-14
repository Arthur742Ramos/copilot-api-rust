//! Credential-free black-box Gemini Developer API coverage through the router.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::{json_body, send, send_full};
use copilot_api::libs::config::{set_cached_config_for_test, AppConfig, AuthConfig};
use copilot_api::libs::state;
use serde_json::{json, Value};
use tokio::sync::oneshot;

const CLIENT_KEY: &str = "gemini-fixture-client-key";

#[derive(Clone, Default)]
struct FixtureState {
    requests: Arc<Mutex<Vec<Value>>>,
}

struct Fixture {
    base_url: String,
    state: FixtureState,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Fixture {
    async fn start() -> Self {
        let state = FixtureState::default();
        let app = Router::new()
            .route("/chat/completions", post(copilot_chat))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Copilot fixture");
        let addr = listener.local_addr().expect("fixture address");
        let (shutdown, receiver) = oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = receiver.await;
                })
                .await
                .expect("serve Copilot fixture");
        });
        Self {
            base_url: format!("http://{addr}"),
            state,
            shutdown: Some(shutdown),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.state.requests.lock().expect("capture lock").clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn copilot_chat(State(state): State<FixtureState>, Json(body): Json<Value>) -> Response {
    state
        .requests
        .lock()
        .expect("capture lock")
        .push(body.clone());
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        let frames = [
            json!({
                "id": "chunk-1",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": body["model"],
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":"}
                    }]},
                    "finish_reason": null
                }]
            })
            .to_string(),
            json!({
                "id": "chunk-1",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": body["model"],
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "function": {"arguments": "\"README.md\"}"}
                    }]},
                    "finish_reason": null
                }]
            })
            .to_string(),
            json!({
                "id": "chunk-1",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": body["model"],
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
            })
            .to_string(),
            json!({
                "id": "chunk-1",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": body["model"],
                "choices": [],
                "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}
            })
            .to_string(),
        ];
        let stream = async_stream::stream! {
            for frame in frames {
                let event = format!("data: {frame}\n\n");
                let split = event.len() / 2;
                yield Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(&event.as_bytes()[..split]));
                yield Ok(Bytes::copy_from_slice(&event.as_bytes()[split..]));
            }
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .expect("stream response");
    }
    Json(json!({
        "id": "chat_gemini_fixture",
        "object": "chat.completion",
        "created": 1,
        "model": body["model"],
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ready"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}
    }))
    .into_response()
}

fn configure(fixture: &Fixture) {
    set_cached_config_for_test(AppConfig {
        auth: Some(AuthConfig {
            api_keys: Some(vec![json!(CLIENT_KEY)]),
            ..Default::default()
        }),
        model_mappings: Some(BTreeMap::from([(
            "gemini-proxy".to_string(),
            json!("gpt-fixture"),
        )])),
        ..Default::default()
    });
    state::with_state_mut(|state| {
        state.copilot_token = Some("fixture-copilot-token".to_string());
        state.copilot_api_url = Some(fixture.base_url.clone());
    });
}

#[tokio::test]
#[serial_test::serial]
async fn list_models_returns_gemini_shape() {
    let fixture = Fixture::start().await;
    configure(&fixture);
    state::with_state_mut(|state| {
        state.models = Some(Arc::new(
            serde_json::from_value(json!({
                "data": [{
                    "id": "gpt-fixture",
                    "name": "Fixture model",
                    "capabilities": {
                        "limits": {
                            "max_context_window_tokens": 4096,
                            "max_output_tokens": 512
                        }
                    }
                }],
                "object": "list"
            }))
            .expect("fixture models"),
        ));
    });

    let request = Request::builder()
        .method("GET")
        .uri("/v1beta/models")
        .header("x-goog-api-key", CLIENT_KEY)
        .body(Body::empty())
        .expect("model list request");
    let response = send(request).await;

    assert_eq!(response.0, StatusCode::OK);
    let body = json_body(&response.1);
    assert_eq!(body["models"][0]["name"], "models/gpt-fixture");
    assert_eq!(body["models"][0]["displayName"], "Fixture model");
    assert_eq!(
        body["models"][0]["supportedGenerationMethods"],
        json!(["generateContent", "countTokens"])
    );
    assert_eq!(body["models"][0]["inputTokenLimit"], 4096);
    assert_eq!(body["models"][0]["outputTokenLimit"], 512);
}

fn request(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-goog-api-key", CLIENT_KEY)
        .body(Body::from(body.to_string()))
        .expect("request")
}

#[tokio::test]
#[serial_test::serial]
async fn generate_content_translates_gemini_history_tools_and_response() {
    let fixture = Fixture::start().await;
    configure(&fixture);
    let request_body = json!({
        "systemInstruction": {"parts": [{"text": "Be concise."}]},
        "contents": [
            {
                "role": "model",
                "parts": [{
                    "functionCall": {
                        "id": "prior-call",
                        "name": "read_file",
                        "args": {"path": "README.md"}
                    }
                }]
            },
            {
                "role": "user",
                "parts": [
                    {"functionResponse": {"name": "read_file", "response": {"output": "docs"}}},
                    {"text": "Summarize this."},
                    {"inlineData": {"mimeType": "image/png", "data": "aGVsbG8="}}
                ]
            }
        ],
        "tools": [{
            "functionDeclarations": [{
                "name": "read_file",
                "description": "Read a file.",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }]
        }],
        "generationConfig": {"temperature": 0.2, "maxOutputTokens": 99}
    });
    let (status, response_headers, bytes) = send_full(request(
        "/v1beta/models/gemini-proxy:generateContent",
        request_body,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(response_headers["content-type"]
        .to_str()
        .unwrap()
        .contains("application/json"));
    let response = json_body(&bytes);
    assert_eq!(response["candidates"][0]["content"]["role"], "model");
    assert_eq!(
        response["candidates"][0]["content"]["parts"][0]["text"],
        "ready"
    );
    assert_eq!(response["candidates"][0]["finishReason"], "STOP");
    assert_eq!(response["usageMetadata"]["promptTokenCount"], 8);

    let upstream = fixture.requests().pop().expect("upstream request");
    assert_eq!(upstream["model"], "gpt-fixture");
    assert_eq!(upstream["messages"][0]["content"], "Be concise.");
    assert_eq!(upstream["messages"][1]["tool_calls"][0]["id"], "prior-call");
    assert_eq!(upstream["messages"][2]["tool_call_id"], "prior-call");
    assert_eq!(
        upstream["messages"][2]["content"][0]["text"],
        "docs\n\nSummarize this."
    );
    assert_eq!(
        upstream["messages"][2]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );
    assert_eq!(upstream["tools"][0]["function"]["name"], "read_file");
    assert_eq!(upstream["max_tokens"], 99);
}

#[tokio::test]
#[serial_test::serial]
async fn streaming_content_translates_tool_calls_across_split_sse_chunks() {
    let fixture = Fixture::start().await;
    configure(&fixture);
    let request_body = json!({
        "contents": [{"role": "user", "parts": [{"text": "Read README.md."}]}],
        "tools": [{"functionDeclarations": [{"name": "read_file"}]}]
    });
    let (status, headers, bytes) = send_full(request(
        "/v1beta/models/gemini-proxy:streamGenerateContent?alt=sse",
        request_body,
    ))
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    let body = String::from_utf8(bytes).expect("SSE is UTF-8");
    let frames: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("valid Gemini SSE event"))
        .collect();
    let call = frames
        .iter()
        .find_map(|frame| frame["candidates"][0]["content"]["parts"][0].get("functionCall"))
        .expect("function call part");
    assert_eq!(call["name"], "read_file");
    assert_eq!(call["args"]["path"], "README.md");
    let terminal = frames.last().expect("terminal event");
    assert_eq!(terminal["candidates"][0]["finishReason"], "STOP");
    assert_eq!(terminal["usageMetadata"]["totalTokenCount"], 16);
}

#[tokio::test]
#[serial_test::serial]
async fn count_tokens_returns_gemini_shape_without_upstream_dispatch() {
    let fixture = Fixture::start().await;
    configure(&fixture);
    let response = send(request(
        "/v1beta/models/gemini-proxy:countTokens",
        json!({"contents": [{"role": "user", "parts": [{"text": "Count these tokens."}]}]}),
    ))
    .await;
    assert_eq!(response.0, StatusCode::OK);
    let body = json_body(&response.1);
    assert!(body["totalTokens"].as_i64().is_some_and(|count| count > 0));
    assert!(fixture.requests().is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn malformed_gemini_request_uses_google_error_envelope() {
    let fixture = Fixture::start().await;
    configure(&fixture);
    let (status, bytes) = send(request(
        "/v1beta/models/gemini-proxy:generateContent",
        json!({"contents": "not-an-array"}),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body = json_body(&bytes);
    assert_eq!(body["error"]["code"], StatusCode::BAD_REQUEST.as_u16());
    assert_eq!(body["error"]["status"], "INVALID_ARGUMENT");
    assert!(fixture.requests().is_empty());
}
