//! Credential-free black-box compatibility coverage for Claude Code 2.1.207 and
//! Codex CLI 0.144.1.
//!
//! Requests enter through the production Axum router. Provider traffic is sent to
//! a deterministic loopback Axum fixture on an ephemeral port; no provider
//! credentials, external network, paid calls, or port 4141 are used.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Once};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::{json_body, send, send_full};
use copilot_api::libs::config::{
    set_cached_config_for_test, AppConfig, AuthConfig, ModelConfig, ProviderConfig,
};
use copilot_api::routes::messages::responses_translation::{
    encode_reasoning_signature, REASONING_SUMMARY_SEPARATOR, THINKING_TEXT,
};
use copilot_api::services::copilot::get_models::{Model, ModelsResponse};
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;

const CLIENT_KEY: &str = "fixture-client-key";
const UPSTREAM_KEY: &str = "fixture-upstream-key";
const NATIVE_NULL_SHAPE: &str = r#"{"unknown_before":{"keep":true},"id":"resp_native_null","object":null,"created_at":null,"model":"gpt-native-null-shape","output":[],"output_text":null,"status":"completed","usage":null,"metadata":null,"parallel_tool_calls":null,"tools":null,"unknown_after":null}"#;
const COMPACT_NULL_SHAPE: &str = r#"{"unknown_before":{"keep":true},"output":[{"type":"compaction","id":null,"encrypted_content":"enc_raw"}],"metadata":null,"unknown_after":null}"#;
const DIRECT_COMPACT_SHAPE: &str = r#"{"extension_before":null,"output":[{"type":"compaction","id":null,"encrypted_content":"enc_direct","internal_chat_message_metadata_passthrough":{"turn_id":"direct-turn"}}],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3},"extension_after":{"keep":true}}"#;
const PROVIDER_COMPACT_SHAPE: &str = r#"{"extension_before":null,"output":[{"type":"message","id":null,"role":"assistant","status":null,"content":[{"type":"input_text","text":"retained","annotations":null,"future_block_field":true}],"future_item_field":{"keep":true}},{"type":"compaction","encrypted_content":"enc_provider","internal_chat_message_metadata_passthrough":{"turn_id":"provider-turn"}}],"usage":{"input_tokens":5,"input_tokens_details":{"cached_tokens":1},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":1},"total_tokens":7},"extension_after":{"keep":true}}"#;
const DIRECT_RESPONSES_SHAPE: &str = r#"{"extension_before":null,"id":"resp_direct","object":null,"created_at":null,"model":"gpt-direct-response-raw","output":[],"output_text":null,"status":"completed","usage":null,"metadata":null,"extension_after":{"keep":true}}"#;
static INIT_HOME: Once = Once::new();

fn init_home() {
    INIT_HOME.call_once(|| {
        let dir = std::env::temp_dir().join(format!(
            "copilot-api-client-compatibility-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create compatibility home");
        std::env::set_var("COPILOT_API_HOME", dir);
    });
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Clone, Default)]
struct FixtureState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

struct Fixture {
    base_url: String,
    state: FixtureState,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Fixture {
    async fn start() -> Self {
        init_home();
        let state = FixtureState::default();
        let app = Router::new()
            .route("/v1/messages", post(fixture_handler))
            .route("/v1/chat/completions", post(fixture_handler))
            .route("/v1/responses", post(fixture_handler))
            .route("/v1/responses/compact", post(fixture_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind compatibility fixture");
        let addr = listener.local_addr().expect("fixture address");
        let (shutdown, receiver) = oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = receiver.await;
                })
                .await
                .expect("serve compatibility fixture");
        });
        Self {
            base_url: format!("http://{addr}"),
            state,
            shutdown: Some(shutdown),
        }
    }

    fn requests(&self) -> Vec<CapturedRequest> {
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

async fn fixture_handler(
    State(state): State<FixtureState>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state
        .requests
        .lock()
        .expect("capture lock")
        .push(CapturedRequest {
            path: uri.path().to_string(),
            headers,
            body: body.clone(),
        });

    match uri.path() {
        "/v1/messages" => anthropic_fixture(&body),
        "/v1/chat/completions" => chat_completions_fixture(&body),
        "/v1/responses" => responses_fixture(&body),
        "/v1/responses/compact" => compact_fixture(&body),
        other => panic!("unexpected fixture path {other}"),
    }
}

fn chat_completions_fixture(body: &Value) -> Response {
    let model = body["model"].as_str().unwrap_or_default();
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        return chat_completions_stream_fixture(model);
    }
    if matches!(
        model,
        "gpt-chat-response-malformed-json" | "gpt-direct-chat-malformed-json"
    ) {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "chat-malformed-json")
            .header("retry-after", "2")
            .header("x-ratelimit-remaining", "9")
            .header("x-unsafe-secret", "must-not-propagate")
            .body(Body::from("{not-json"))
            .expect("malformed Chat fixture");
    }
    if model == "gpt-chat-response-oversized" {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "chat-oversized")
            .header("retry-after", "2")
            .header("x-ratelimit-remaining", "9")
            .header(
                "content-length",
                (copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES + 1).to_string(),
            )
            .body(Body::from(vec![
                b'x';
                copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
                    + 1
            ]))
            .expect("oversized Chat fixture");
    }
    if matches!(model, "gpt-chat-response-429" | "gpt-direct-chat-429") {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("content-type", "application/json"),
                ("x-request-id", "chat-rate-limit"),
                ("retry-after", "4"),
                ("x-unsafe-secret", "must-not-propagate"),
            ],
            Json(json!({
                "error":{
                    "type":"rate_limit_error",
                    "message":"chat rate limited"
                }
            })),
        )
            .into_response();
    }
    if model == "gpt-chat-response-503" {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                ("content-type", "application/json"),
                ("x-request-id", "chat-unavailable"),
                ("x-unsafe-secret", "must-not-propagate"),
            ],
            Json(json!({
                "error":{
                    "type":"server_error",
                    "message":"chat unavailable"
                }
            })),
        )
            .into_response();
    }

    let mut response = json!({
        "id":"chatcmpl-fixture",
        "object":"chat.completion",
        "created":1,
        "model":body["model"],
        "choices":[{
            "index":0,
            "message":{"role":"assistant","content":"chat fixture"},
            "finish_reason":"stop"
        }],
        "usage":{
            "prompt_tokens":3,
            "completion_tokens":2,
            "total_tokens":5
        }
    });
    match model {
        "gpt-chat-response-extras" | "gpt-direct-chat-response-extras" => {
            response = json!({
                "id":"chatcmpl-extras",
                "object":"chat.completion",
                "created":7,
                "model":model,
                "future_before":null,
                "choices":[{
                    "index":0,
                    "finish_reason":"tool_calls",
                    "logprobs":null,
                    "future_choice":{"keep":true,"null":null},
                    "message":{
                        "role":"assistant",
                        "content":[{
                            "type":"text",
                            "text":"chat text",
                            "future_content":{"keep":true,"null":null}
                        }],
                        "reasoning_text":"reason",
                        "reasoning_content":"reason",
                        "reasoning_opaque":"opaque",
                        "annotations":[{
                            "type":"url_citation",
                            "url":"https://example.test"
                        }],
                        "refusal":null,
                        "audio":null,
                        "future_message":{"keep":true,"null":null},
                        "tool_calls":[{
                            "index":0,
                            "id":"call-extra",
                            "type":"function",
                            "future_tool":{"keep":true,"null":null},
                            "function":{
                                "name":"actual",
                                "arguments":"{\"value\":1}",
                                "future_function":{"keep":true,"null":null}
                            }
                        }]
                    }
                }],
                "usage":{
                    "prompt_tokens":10,
                    "completion_tokens":5,
                    "total_tokens":15,
                    "prompt_tokens_details":{
                        "cached_tokens":2,
                        "cache_creation_input_tokens":1,
                        "audio_tokens":0,
                        "future_prompt":null
                    },
                    "completion_tokens_details":{
                        "reasoning_tokens":2,
                        "future_completion":{"keep":true}
                    },
                    "future_usage":{"keep":true,"null":null},
                    "service_tier":"default"
                },
                "service_tier":"default",
                "system_fingerprint":null,
                "future_after":{"keep":true,"null":null}
            });
        }
        "gpt-chat-response-no-choices" => {
            response.as_object_mut().unwrap().remove("choices");
        }
        "gpt-chat-response-no-id" => {
            response.as_object_mut().unwrap().remove("id");
        }
        "gpt-chat-response-model-null" => response["model"] = Value::Null,
        "gpt-chat-response-object" => response["object"] = json!("wrong"),
        "gpt-chat-response-created" => response["created"] = json!(-1),
        "gpt-chat-response-choices-wrong" => response["choices"] = json!({}),
        "gpt-chat-response-choices-empty" => response["choices"] = json!([]),
        "gpt-chat-response-choices-multiple" => {
            let choice = response["choices"][0].clone();
            response["choices"] = json!([choice.clone(), choice]);
        }
        "gpt-chat-response-choice-wrong" => response["choices"][0] = json!("wrong"),
        "gpt-chat-response-choice-index" => response["choices"][0]["index"] = json!(1),
        "gpt-chat-response-no-message" => {
            response["choices"][0]
                .as_object_mut()
                .unwrap()
                .remove("message");
        }
        "gpt-chat-response-role" => response["choices"][0]["message"]["role"] = json!("user"),
        "gpt-chat-response-content-wrong" => {
            response["choices"][0]["message"]["content"] = json!({})
        }
        "gpt-chat-response-content-part" => {
            response["choices"][0]["message"]["content"] =
                json!([{"type":"image_url","image_url":{"url":"x"}}])
        }
        "gpt-chat-response-no-finish" => {
            response["choices"][0]
                .as_object_mut()
                .unwrap()
                .remove("finish_reason");
        }
        "gpt-chat-response-finish-unknown" => {
            response["choices"][0]["finish_reason"] = json!("unknown")
        }
        "gpt-chat-response-tool-type"
        | "gpt-chat-response-tool-id"
        | "gpt-chat-response-tool-function"
        | "gpt-chat-response-tool-name"
        | "gpt-chat-response-tool-arguments-type"
        | "gpt-chat-response-tool-arguments-json"
        | "gpt-chat-response-tool-arguments-scalar"
        | "gpt-chat-response-tool-collision" => {
            response["choices"][0]["finish_reason"] = json!("tool_calls");
            response["choices"][0]["message"]["content"] = Value::Null;
            response["choices"][0]["message"]["tool_calls"] = json!([{
                "id":"call",
                "type":"function",
                "function":{"name":"actual","arguments":"{}"}
            }]);
            match model {
                "gpt-chat-response-tool-type" => {
                    response["choices"][0]["message"]["tool_calls"][0]["type"] = json!("custom")
                }
                "gpt-chat-response-tool-id" => {
                    response["choices"][0]["message"]["tool_calls"][0]["id"] = Value::Null
                }
                "gpt-chat-response-tool-function" => {
                    response["choices"][0]["message"]["tool_calls"][0]["function"] = json!("wrong")
                }
                "gpt-chat-response-tool-name" => {
                    response["choices"][0]["message"]["tool_calls"][0]["function"]["name"] =
                        json!("")
                }
                "gpt-chat-response-tool-arguments-type" => {
                    response["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
                        json!({})
                }
                "gpt-chat-response-tool-arguments-json" => {
                    response["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
                        json!("{not-json")
                }
                "gpt-chat-response-tool-arguments-scalar" => {
                    response["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
                        json!("[]")
                }
                "gpt-chat-response-tool-collision" => {
                    response["choices"][0]["message"]["tool_calls"][0]["input"] =
                        json!({"override":true})
                }
                _ => unreachable!(),
            }
        }
        "gpt-chat-response-no-usage" => {
            response.as_object_mut().unwrap().remove("usage");
        }
        "gpt-chat-response-usage-null" => response["usage"] = Value::Null,
        "gpt-chat-response-usage-wrong" => response["usage"] = json!([]),
        "gpt-chat-response-usage-negative" => response["usage"]["prompt_tokens"] = json!(-1),
        "gpt-chat-response-usage-total" => response["usage"]["total_tokens"] = json!(99),
        "gpt-chat-response-usage-details" => {
            response["usage"]["prompt_tokens_details"] =
                json!({"cached_tokens":3,"cache_creation_input_tokens":2})
        }
        "gpt-chat-response-usage-overflow" => response["usage"]["prompt_tokens"] = json!(u64::MAX),
        "gpt-chat-response-top-collision" => response["content"] = json!("override"),
        "gpt-chat-response-usage-collision" => response["usage"]["input_tokens"] = json!(99),
        "gpt-chat-response-function-call" => {
            response["choices"][0]["message"]["function_call"] =
                json!({"name":"legacy","arguments":"{}"})
        }
        "gpt-chat-response-reasoning-conflict" => {
            response["choices"][0]["message"]["reasoning_text"] = json!("one");
            response["choices"][0]["message"]["reasoning_content"] = json!("two");
            response["choices"][0]["message"]["reasoning_opaque"] = json!("opaque");
        }
        "gpt-chat-response-reasoning-no-signature" => {
            response["choices"][0]["message"]["reasoning_content"] = json!("reason");
        }
        "gpt-chat-response-logprobs" => {
            response["choices"][0]["logprobs"] = json!({"content":[]});
        }
        "gpt-chat-response-refusal-malformed" => {
            response["choices"][0]["message"]["refusal"] = json!({});
        }
        "gpt-chat-response-tier-valid" => {
            response["service_tier"] = json!("priority");
            response["usage"]["service_tier"] = json!("priority");
        }
        "gpt-chat-response-tier-top-invalid" => {
            response["service_tier"] = json!("invalid");
        }
        "gpt-chat-response-tier-nested-invalid" => {
            response["usage"]["service_tier"] = json!("invalid");
        }
        "gpt-chat-response-tier-conflict" => {
            response["service_tier"] = json!("default");
            response["usage"]["service_tier"] = json!("priority");
        }
        "gpt-chat-response-refusal" | "gpt-direct-chat-response-refusal" => {
            response["choices"][0]["finish_reason"] = json!("content_filter");
            response["choices"][0]["message"]["content"] = Value::Null;
            response["choices"][0]["message"]["refusal"] = json!("blocked");
        }
        "gpt-chat-response-body-error" => {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .header("x-request-id", "chat-body-error")
                .header("retry-after", "2")
                .header("x-ratelimit-remaining", "9")
                .header("x-unsafe-secret", "must-not-propagate")
                .body(Body::from(
                    json!({
                        "error":{
                            "type":"provider_error",
                            "message":"safe upstream failure"
                        }
                    })
                    .to_string(),
                ))
                .expect("Chat body error fixture");
        }
        "gpt-direct-chat-bad-choices" => response["choices"] = json!([]),
        _ => {}
    }
    if model.starts_with("gpt-chat-response-") || model.starts_with("gpt-direct-chat-") {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "chat-response-request")
            .header("retry-after", "2")
            .header("x-ratelimit-remaining", "9")
            .header("x-unsafe-secret", "must-not-propagate")
            .body(Body::from(response.to_string()))
            .expect("Chat response fixture");
    }
    Json(response).into_response()
}

const BUDGET_FIXTURE_FRAGMENT_BYTES: usize = 256 * 1024;

fn ascii_payload_fragments(mut bytes: usize) -> Vec<String> {
    let mut fragments = Vec::new();
    while bytes > 0 {
        let length = bytes.min(BUDGET_FIXTURE_FRAGMENT_BYTES);
        fragments.push("r".repeat(length));
        bytes -= length;
    }
    fragments
}

fn utf8_payload_fragments(mut bytes: usize) -> Vec<String> {
    let mut fragments = Vec::new();
    while bytes > 0 {
        let length = bytes.min(BUDGET_FIXTURE_FRAGMENT_BYTES);
        let mut fragment = "é".repeat(length / 2);
        if length % 2 == 1 {
            fragment.push('x');
        }
        debug_assert_eq!(fragment.len(), length);
        fragments.push(fragment);
        bytes -= length;
    }
    fragments
}

fn opaque_signature_fragments(over_by: usize) -> Vec<String> {
    const PARTS: usize = 128;
    let placeholder_bytes = copilot_api::routes::messages::utils::THINKING_TEXT.len() * PARTS;
    let signature_bytes = copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
        .checked_sub(placeholder_bytes)
        .and_then(|bytes| bytes.checked_add(over_by))
        .expect("opaque fixture budget");
    let base = signature_bytes / PARTS;
    let remainder = signature_bytes % PARTS;
    (0..PARTS)
        .map(|index| "s".repeat(base + usize::from(index < remainder)))
        .collect()
}

fn chat_completions_stream_fixture(model: &str) -> Response {
    let chunk = |choices: Value, usage: Value| {
        json!({
            "id":"chatcmpl-stream",
            "object":"chat.completion.chunk",
            "created":1,
            "model":model,
            "choices":choices,
            "usage":usage
        })
    };
    let text = || {
        chunk(
            json!([{
                "index":0,
                "logprobs":null,
                "delta":{"role":"assistant","content":"Hello"},
                "finish_reason":null
            }]),
            Value::Null,
        )
    };
    let finish = || {
        chunk(
            json!([{
                "index":0,
                "delta":{},
                "finish_reason":"stop"
            }]),
            Value::Null,
        )
    };
    let usage = || {
        chunk(
            json!([]),
            json!({
                "prompt_tokens":4,
                "completion_tokens":2,
                "total_tokens":6,
                "prompt_tokens_details":{
                    "cached_tokens":1,
                    "future_prompt":null
                },
                "future_usage":{"keep":true}
            }),
        )
    };
    let mut chunks = match model {
        "gpt-chat-stream-strict" | "gpt-direct-chat-stream-strict" => {
            let mut first = text();
            first["system_fingerprint"] = Value::Null;
            first["future_chunk"] = json!({"keep":true,"null":null});
            vec![first, finish(), usage()]
        }
        "gpt-chat-stream-no-usage" | "gpt-direct-chat-stream-no-usage" => {
            vec![text(), finish()]
        }
        "gpt-chat-stream-tools" => {
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{
                            "tool_calls":[{
                                "index":0,
                                "id":"call-stream",
                                "type":"function",
                                "future_tool":{"keep":true,"null":null},
                                "function":{
                                    "name":"actual",
                                    "arguments":"{\"value\":",
                                    "future_function":{"keep":true,"null":null}
                                }
                            }]
                        },
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{
                            "tool_calls":[{
                                "index":0,
                                "function":{"arguments":"1}"}
                            }]
                        },
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{},
                        "finish_reason":"tool_calls"
                    }]),
                    json!({
                        "prompt_tokens":3,
                        "completion_tokens":2,
                        "total_tokens":5
                    }),
                ),
            ]
        }
        "gpt-chat-stream-tool-optionals" | "gpt-direct-chat-stream-tool-optionals" => {
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "future_tool":{"keep":true,"null":null}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "function":{
                                "arguments":"{\"value\":",
                                "future_function":{"keep":true,"null":null}
                            }
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "id":"call-late"
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "function":{"name":"actual"}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{"index":0}]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "function":{"arguments":"1}"}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{},
                        "finish_reason":"tool_calls"
                    }]),
                    json!({
                        "prompt_tokens":3,
                        "completion_tokens":2,
                        "total_tokens":5
                    }),
                ),
            ]
        }
        "gpt-chat-stream-refusal" | "gpt-direct-chat-stream-refusal" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"role":"assistant","refusal":"blocked"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-refusal-split" | "gpt-direct-chat-stream-refusal-split" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"role":"assistant","refusal":"blo"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"refusal":"cked"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-refusal-interleaved" | "gpt-direct-chat-stream-refusal-interleaved" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"role":"assistant","content":"blo"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"refusal":"blo"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"cked"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"refusal":"cked"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-refusal-mirror" | "gpt-direct-chat-stream-refusal-mirror" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{
                            "role":"assistant",
                            "content":"blocked",
                            "refusal":"blocked"
                        },
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-refusal-empty" | "gpt-direct-chat-stream-refusal-empty" => vec![
            chunk(
                json!([{
                    "index":0,
                    "delta":{"role":"assistant","refusal":""},
                    "finish_reason":null
                }]),
                Value::Null,
            ),
            text(),
            finish(),
        ],
        "gpt-chat-stream-refusal-repeated" | "gpt-direct-chat-stream-refusal-repeated" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"role":"assistant","refusal":"blocked"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"refusal":"blocked"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-refusal-content-prefix"
        | "gpt-direct-chat-stream-refusal-content-prefix" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"blo"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"refusal":"blocked"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-refusal-partial" | "gpt-direct-chat-stream-refusal-partial" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"blocked"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"refusal":"blo"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-refusal-tool-deferred"
        | "gpt-direct-chat-stream-refusal-tool-deferred" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "id":"call-0",
                            "type":"function",
                            "function":{"name":"actual","arguments":"{}"}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"foo","refusal":"foobar"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-refusal-multiple-tools"
        | "gpt-direct-chat-stream-refusal-multiple-tools" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"pre"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "id":"call-0",
                            "type":"function",
                            "function":{"name":"first","arguments":"{}"}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"mid"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":1,
                            "id":"call-1",
                            "type":"function",
                            "function":{"name":"second","arguments":"{}"}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{
                            "content":"post",
                            "refusal":"premidpost-refused"
                        },
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-budget-reasoning-exact"
        | "gpt-direct-chat-stream-budget-reasoning-exact"
        | "gpt-chat-stream-bad-budget-reasoning-over"
        | "gpt-direct-chat-stream-bad-budget-reasoning-over" => {
            let over_by = usize::from(model.contains("-over"));
            let mut chunks: Vec<Value> = ascii_payload_fragments(
                copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES + over_by,
            )
            .into_iter()
            .enumerate()
            .map(|(index, reasoning)| {
                chunk(
                    json!([{
                        "index":0,
                        "delta":{
                            "role":(index == 0).then_some("assistant"),
                            "reasoning_text":reasoning
                        },
                        "finish_reason":null
                    }]),
                    Value::Null,
                )
            })
            .collect();
            chunks.push(finish());
            chunks
        }
        "gpt-chat-stream-budget-opaque-exact"
        | "gpt-direct-chat-stream-budget-opaque-exact"
        | "gpt-chat-stream-bad-budget-opaque-over"
        | "gpt-direct-chat-stream-bad-budget-opaque-over" => {
            let over_by = usize::from(model.contains("-over"));
            let mut chunks: Vec<Value> = opaque_signature_fragments(over_by)
                .into_iter()
                .enumerate()
                .map(|(index, signature)| {
                    chunk(
                        json!([{
                            "index":0,
                            "delta":{
                                "role":(index == 0).then_some("assistant"),
                                "reasoning_opaque":signature
                            },
                            "finish_reason":null
                        }]),
                        Value::Null,
                    )
                })
                .collect();
            chunks.push(finish());
            chunks
        }
        "gpt-chat-stream-budget-mixed-utf8-exact"
        | "gpt-direct-chat-stream-budget-mixed-utf8-exact"
        | "gpt-chat-stream-bad-budget-mixed-utf8-over"
        | "gpt-direct-chat-stream-bad-budget-mixed-utf8-over" => {
            let over_by = usize::from(model.contains("-over"));
            let tool_id = "budget-call";
            let tool_name = "actual";
            let arguments = "{}";
            let reasoning = "reason";
            let signature = "sig";
            let fixed = reasoning.len()
                + tool_id.len()
                + tool_name.len()
                + arguments.len()
                + copilot_api::routes::messages::utils::THINKING_TEXT.len()
                + signature.len();
            let filler_bytes = copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
                .checked_sub(fixed)
                .and_then(|bytes| bytes.checked_add(over_by))
                .expect("mixed budget fixture");
            let mut chunks = vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"role":"assistant","reasoning_text":reasoning},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "id":tool_id,
                            "type":"function",
                            "function":{"name":tool_name,"arguments":arguments}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"reasoning_opaque":signature},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
            ];
            chunks.extend(
                utf8_payload_fragments(filler_bytes)
                    .into_iter()
                    .map(|content| {
                        chunk(
                            json!([{
                                "index":0,
                                "delta":{"content":content},
                                "finish_reason":null
                            }]),
                            Value::Null,
                        )
                    }),
            );
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("tool_calls");
            chunks.push(terminal);
            chunks
        }
        "gpt-chat-stream-tier-valid" => {
            let mut first = text();
            first["service_tier"] = json!("scale");
            let mut terminal_usage = usage();
            terminal_usage["service_tier"] = json!("scale");
            terminal_usage["usage"]["service_tier"] = json!("scale");
            vec![first, finish(), terminal_usage]
        }
        "gpt-chat-stream-tier-late" | "gpt-direct-chat-stream-tier-late" => {
            let mut terminal = finish();
            terminal["service_tier"] = json!("flex");
            terminal["system_fingerprint"] = json!("late-fingerprint");
            let mut terminal_usage = usage();
            terminal_usage["service_tier"] = json!("flex");
            terminal_usage["usage"]["service_tier"] = json!("flex");
            vec![text(), terminal, terminal_usage]
        }
        "gpt-chat-stream-bad-missing-id" | "gpt-direct-chat-stream-bad-identity" => {
            let mut first = text();
            first.as_object_mut().unwrap().remove("id");
            vec![first]
        }
        "gpt-chat-stream-bad-object" => {
            let mut first = text();
            first["object"] = json!("chat.completion");
            vec![first]
        }
        "gpt-chat-stream-bad-created" => {
            let mut first = text();
            first["created"] = json!(-1);
            vec![first]
        }
        "gpt-chat-stream-bad-model" => {
            let mut first = text();
            first["model"] = json!("");
            vec![first]
        }
        "gpt-chat-stream-bad-id-conflict" => {
            let mut second = finish();
            second["id"] = json!("conflict");
            vec![text(), second]
        }
        "gpt-chat-stream-bad-service" => {
            let mut first = text();
            first["service_tier"] = json!("unsupported");
            vec![first]
        }
        "gpt-chat-stream-bad-service-conflict" => {
            let mut first = text();
            first["service_tier"] = json!("default");
            let mut second = finish();
            second["service_tier"] = json!("priority");
            vec![first, second]
        }
        "gpt-chat-stream-bad-fingerprint" => {
            let mut first = text();
            first["system_fingerprint"] = json!("one");
            let mut second = finish();
            second["system_fingerprint"] = json!("two");
            vec![first, second]
        }
        "gpt-chat-stream-bad-choices" => {
            let mut first = text();
            let choice = first["choices"][0].clone();
            first["choices"] = json!([choice.clone(), choice]);
            vec![first]
        }
        "gpt-chat-stream-bad-choice-index" => {
            let mut first = text();
            first["choices"][0]["index"] = json!(1);
            vec![first]
        }
        "gpt-chat-stream-bad-finish" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("unknown");
            vec![text(), terminal]
        }
        "gpt-chat-stream-bad-function-finish" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("function_call");
            vec![text(), terminal]
        }
        "gpt-chat-stream-bad-tool-finish" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("tool_calls");
            vec![text(), terminal]
        }
        "gpt-chat-stream-bad-stop-tool" => {
            let announced = chunk(
                json!([{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"call",
                        "type":"function",
                        "function":{"name":"actual","arguments":"{}"}
                    }]},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            vec![announced, finish()]
        }
        "gpt-chat-stream-bad-tool-index"
        | "gpt-chat-stream-bad-tool-gap"
        | "gpt-chat-stream-bad-tool-id" => {
            let mut call = json!({
                "index":0,
                "id":"call",
                "type":"function",
                "function":{"name":"actual","arguments":"{}"}
            });
            match model {
                "gpt-chat-stream-bad-tool-index" => {
                    call.as_object_mut().unwrap().remove("index");
                }
                "gpt-chat-stream-bad-tool-gap" => call["index"] = json!(1),
                "gpt-chat-stream-bad-tool-id" => call["id"] = Value::Null,
                _ => unreachable!(),
            }
            vec![chunk(
                json!([{
                    "index":0,
                    "delta":{"tool_calls":[call]},
                    "finish_reason":null
                }]),
                Value::Null,
            )]
        }
        "gpt-chat-stream-bad-tool-duplicate-id" => {
            let announced = chunk(
                json!([{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"conflicting-call",
                        "type":"function",
                        "function":{"name":"actual","arguments":"{"}
                    }]},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            let duplicate = chunk(
                json!([{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"call",
                        "function":{"arguments":"}"}
                    }]},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            vec![announced, duplicate]
        }
        "gpt-chat-stream-bad-tool-incomplete" | "gpt-chat-stream-bad-tool-scalar" => {
            let arguments = if model.ends_with("scalar") {
                "[]"
            } else {
                "{\"x\":"
            };
            let announced = chunk(
                json!([{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"call",
                        "type":"function",
                        "function":{"name":"actual","arguments":arguments}
                    }]},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("tool_calls");
            vec![announced, terminal]
        }
        "gpt-chat-stream-bad-usage-partial"
        | "gpt-chat-stream-bad-usage-total"
        | "gpt-chat-stream-bad-usage-details" => {
            let mut terminal = finish();
            terminal["usage"] = match model {
                "gpt-chat-stream-bad-usage-partial" => json!({"prompt_tokens":1}),
                "gpt-chat-stream-bad-usage-total" => {
                    json!({"prompt_tokens":1,"completion_tokens":1,"total_tokens":9})
                }
                "gpt-chat-stream-bad-usage-details" => json!({
                    "prompt_tokens":1,
                    "completion_tokens":1,
                    "total_tokens":2,
                    "prompt_tokens_details":{"cached_tokens":2}
                }),
                _ => unreachable!(),
            };
            vec![text(), terminal]
        }
        "gpt-chat-stream-bad-usage-orphan" => vec![usage()],
        "gpt-chat-stream-bad-choice-extra" => {
            let mut first = text();
            first["choices"][0]["future_choice"] = json!(true);
            vec![first]
        }
        "gpt-chat-stream-bad-delta-extra" => {
            let mut first = text();
            first["choices"][0]["delta"]["future_delta"] = json!(true);
            vec![first]
        }
        "gpt-chat-stream-bad-later-extra" => {
            let mut second = finish();
            second["future_late"] = json!(true);
            vec![text(), second]
        }
        "gpt-chat-stream-bad-refusal" | "gpt-direct-chat-stream-bad-refusal" => {
            let mut first = text();
            first["choices"][0]["delta"]["refusal"] = json!({});
            vec![first]
        }
        "gpt-chat-stream-bad-logprobs" => {
            let mut first = text();
            first["choices"][0]["logprobs"] = json!({"content":[]});
            vec![first]
        }
        "gpt-chat-stream-bad-tier-nested" | "gpt-direct-chat-stream-bad-tier" => {
            let mut terminal = finish();
            terminal["usage"] = json!({
                "prompt_tokens":1,
                "completion_tokens":1,
                "total_tokens":2,
                "service_tier":"invalid"
            });
            vec![text(), terminal]
        }
        "gpt-chat-stream-bad-tier-conflict" => {
            let mut first = text();
            first["service_tier"] = json!("default");
            let mut terminal = finish();
            terminal["service_tier"] = json!("default");
            terminal["usage"] = json!({
                "prompt_tokens":1,
                "completion_tokens":1,
                "total_tokens":2,
                "service_tier":"priority"
            });
            vec![first, terminal]
        }
        "gpt-chat-stream-bad-refusal-conflict" | "gpt-direct-chat-stream-bad-refusal-conflict" => {
            let mut refusal = text();
            refusal["choices"][0]["delta"]["refusal"] = json!("blocked");
            vec![text(), refusal]
        }
        "gpt-chat-stream-bad-refusal-finish" | "gpt-direct-chat-stream-bad-refusal-finish" => {
            let refusal = chunk(
                json!([{
                    "index":0,
                    "delta":{"refusal":"blocked"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            vec![refusal, finish()]
        }
        "gpt-chat-stream-bad-refusal-late" | "gpt-direct-chat-stream-bad-refusal-late" => {
            let refusal = chunk(
                json!([{
                    "index":0,
                    "delta":{"refusal":"blocked"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            let late = chunk(
                json!([{
                    "index":0,
                    "delta":{"refusal":"late"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            vec![refusal, terminal, late]
        }
        "gpt-chat-stream-bad-refusal-late-finish-usage"
        | "gpt-direct-chat-stream-bad-refusal-late-finish-usage" => {
            let refusal = chunk(
                json!([{
                    "index":0,
                    "delta":{"refusal":"blocked"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            terminal["usage"] = json!({
                "prompt_tokens":3,
                "completion_tokens":2,
                "total_tokens":5
            });
            let late = chunk(
                json!([{
                    "index":0,
                    "delta":{"refusal":"late"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            vec![refusal, terminal, late]
        }
        "gpt-chat-stream-bad-refusal-late-after-usage"
        | "gpt-direct-chat-stream-bad-refusal-late-after-usage" => {
            let refusal = chunk(
                json!([{
                    "index":0,
                    "delta":{"refusal":"blocked"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            let late = chunk(
                json!([{
                    "index":0,
                    "delta":{"refusal":"late"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            vec![refusal, terminal, usage(), late]
        }
        "gpt-chat-stream-bad-refusal-repeated-usage"
        | "gpt-direct-chat-stream-bad-refusal-repeated-usage" => {
            let refusal = chunk(
                json!([{
                    "index":0,
                    "delta":{"refusal":"blocked"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![refusal, terminal, usage(), usage()]
        }
        "gpt-chat-stream-bad-refusal-tool-incomplete"
        | "gpt-direct-chat-stream-bad-refusal-tool-incomplete" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "id":"call-incomplete",
                            "function":{"name":"actual","arguments":"{\"value\":"}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"foo","refusal":"foobar"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
            ]
        }
        "gpt-chat-stream-bad-refusal-tool-late"
        | "gpt-direct-chat-stream-bad-refusal-tool-late" => {
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("content_filter");
            let late = chunk(
                json!([{
                    "index":0,
                    "delta":{"content":"conflict","refusal":"different"},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "id":"call-late",
                            "function":{"name":"actual","arguments":"{}"}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"foo","refusal":"foobar"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                terminal,
                late,
            ]
        }
        "gpt-chat-stream-bad-refusal-tool-eof" | "gpt-direct-chat-stream-bad-refusal-tool-eof" => {
            vec![
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"tool_calls":[{
                            "index":0,
                            "id":"call-eof",
                            "function":{"name":"actual","arguments":"{}"}
                        }]},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
                chunk(
                    json!([{
                        "index":0,
                        "delta":{"content":"foo","refusal":"foobar"},
                        "finish_reason":null
                    }]),
                    Value::Null,
                ),
            ]
        }
        "gpt-chat-stream-bad-tool-late-extra" => {
            let announced = chunk(
                json!([{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"call",
                        "function":{"name":"actual","arguments":"{}"}
                    }]},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            let late = chunk(
                json!([{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "future_late":{"cannot_emit":true}
                    }]},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            vec![announced, late]
        }
        "gpt-chat-stream-bad-tool-missing-terminal" => {
            let announced = chunk(
                json!([{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "function":{"arguments":"{}"}
                    }]},
                    "finish_reason":null
                }]),
                Value::Null,
            );
            let mut terminal = finish();
            terminal["choices"][0]["finish_reason"] = json!("tool_calls");
            vec![announced, terminal]
        }
        _ => vec![text(), finish(), usage()],
    };

    if (model.starts_with("gpt-chat-stream-bad-")
        && model != "gpt-chat-stream-bad-refusal-tool-eof")
        || model == "gpt-direct-chat-stream-bad-identity"
    {
        let mut late = finish();
        late["choices"][0]["delta"]["content"] = json!("late success");
        late["usage"] = json!({
            "prompt_tokens":1,
            "completion_tokens":1,
            "total_tokens":2
        });
        chunks.push(late);
    }
    let mut body: String = chunks
        .into_iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect();
    if !model.ends_with("bad-refusal-tool-eof") {
        body.push_str("data: [DONE]\n\n");
    }
    sse_response(body)
}

fn compact_fixture(body: &Value) -> Response {
    let model = body["model"].as_str().unwrap_or_default();
    if model.starts_with("gpt-provider-compact-") {
        return provider_compact_fixture(model);
    }
    match model {
        "gpt-native-null-shape" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(COMPACT_NULL_SHAPE))
            .expect("compact null fixture"),
        "gpt-direct-compact-success" | "gpt-direct-compact-headers" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "direct-compact-request")
            .header("x-codex-turn-state", "direct-state")
            .header("x-unsafe-secret", "must-not-propagate")
            .body(Body::from(DIRECT_COMPACT_SHAPE))
            .expect("direct compact fixture"),
        "gpt-direct-compact-malformed-json" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "direct-compact-malformed")
            .header("x-unsafe-secret", "must-not-propagate")
            .body(Body::from("{not-json"))
            .expect("malformed compact fixture"),
        "gpt-direct-compact-wrong-output" => {
            Json(json!({"output":"wrong","extension":{"keep":true}})).into_response()
        }
        "gpt-direct-compact-wrong-item" => {
            Json(json!({"output":[{"type":"compaction","id":null}]})).into_response()
        }
        "gpt-direct-compact-wrong-usage" => Json(json!({
            "output":[],
            "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":9}
        }))
        .into_response(),
        "gpt-direct-compact-oversized" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "direct-compact-oversized")
            .header(
                "content-length",
                (copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES + 1).to_string(),
            )
            .body(Body::from(vec![
                b'x';
                copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
                    + 1
            ]))
            .expect("oversized compact fixture"),
        "gpt-direct-compact-400" => (
            StatusCode::BAD_REQUEST,
            [("x-request-id", "direct-compact-400")],
            Json(json!({
                "error":{
                    "message":"direct compact invalid",
                    "type":"invalid_request_error",
                    "code":"compact_invalid"
                }
            })),
        )
            .into_response(),
        "gpt-direct-compact-503" => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("x-request-id", "direct-compact-503"), ("retry-after", "3")],
            Json(json!({
                "error":{
                    "message":"direct compact unavailable",
                    "type":"server_error",
                    "code":"compact_unavailable"
                }
            })),
        )
            .into_response(),
        _ => Json(json!({
            "output": [
                {
                    "type": "compaction",
                    "encrypted_content": "enc_compacted_history",
                    "internal_chat_message_metadata_passthrough": {
                        "turn_id": "turn_compacted"
                    }
                }
            ],
            "fixture_extension": {"preserved": true}
        }))
        .into_response(),
    }
}

fn provider_compact_fixture(model: &str) -> Response {
    match model {
        "gpt-provider-compact-success" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "provider-compact-request")
            .header("openai-request-id", "provider-openai-request")
            .header("x-codex-turn-state", "provider-state")
            .header("x-unsafe-secret", "must-not-propagate")
            .body(Body::from(PROVIDER_COMPACT_SHAPE))
            .expect("provider compact success fixture"),
        "gpt-provider-compact-wrong-item" => {
            Json(json!({"output":[{"type":"compaction","id":null}]})).into_response()
        }
        "gpt-provider-compact-wrong-output" => {
            Json(json!({"output":"wrong","extension":{"keep":true}})).into_response()
        }
        "gpt-provider-compact-usage-malformed" => Json(json!({
            "output":[],
            "usage":{"input_tokens":"5","output_tokens":2,"total_tokens":7}
        }))
        .into_response(),
        "gpt-provider-compact-usage-inconsistent" => Json(json!({
            "output":[],
            "usage":{"input_tokens":5,"output_tokens":2,"total_tokens":8}
        }))
        .into_response(),
        "gpt-provider-compact-usage-negative" => Json(json!({
            "output":[],
            "usage":{"input_tokens":-1,"output_tokens":2,"total_tokens":1}
        }))
        .into_response(),
        "gpt-provider-compact-usage-overflow" => Json(json!({
            "output":[],
            "usage":{"input_tokens":i64::MAX,"output_tokens":1,"total_tokens":i64::MAX}
        }))
        .into_response(),
        "gpt-provider-compact-malformed-json" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "provider-compact-malformed")
            .header("x-unsafe-secret", "must-not-propagate")
            .body(Body::from("{provider-not-json"))
            .expect("provider malformed compact fixture"),
        "gpt-provider-compact-oversized" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("x-request-id", "provider-compact-oversized")
            .header(
                "content-length",
                (copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES + 1).to_string(),
            )
            .body(Body::from(vec![
                b'x';
                copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
                    + 1
            ]))
            .expect("provider oversized compact fixture"),
        "gpt-provider-compact-400" => (
            StatusCode::BAD_REQUEST,
            [
                ("x-request-id", "provider-compact-400"),
                ("x-unsafe-secret", "must-not-propagate"),
            ],
            Json(json!({
                "error":{
                    "message":"provider compact invalid",
                    "type":"invalid_request_error",
                    "code":"provider_compact_invalid",
                    "fixture_extension":{"keep":true}
                }
            })),
        )
            .into_response(),
        "gpt-provider-compact-503" => (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                ("x-request-id", "provider-compact-503"),
                ("retry-after", "4"),
                ("x-unsafe-secret", "must-not-propagate"),
            ],
            Json(json!({
                "error":{
                    "message":"provider compact unavailable",
                    "type":"server_error",
                    "code":"provider_compact_unavailable"
                }
            })),
        )
            .into_response(),
        other => panic!("unexpected provider compact fixture model {other}"),
    }
}

fn anthropic_fixture(body: &Value) -> Response {
    let model = body["model"].as_str().unwrap_or("claude-fixture");
    if model == "claude-rate-limit" {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "7"), ("x-request-id", "claude-rate-1")],
            Json(json!({
                "type":"error",
                "error":{"type":"rate_limit_error","message":"fixture limited"}
            })),
        )
            .into_response();
    }
    if body["stream"] == true {
        let mut frames = vec![
            (
                "message_start",
                json!({
                    "type":"message_start",
                    "message":{
                        "id":"msg_fixture",
                        "type":"message",
                        "role":"assistant",
                        "model":model,
                        "content":[],
                        "stop_reason":Value::Null,
                        "stop_sequence":Value::Null,
                        "usage":{"input_tokens":21,"output_tokens":0}
                    }
                }),
            ),
            (
                "content_block_start",
                json!({
                    "type":"content_block_start",
                    "index":0,
                    "content_block":{"type":"thinking","thinking":"","signature":""}
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type":"content_block_delta",
                    "index":0,
                    "delta":{"type":"thinking_delta","thinking":"inspect"}
                }),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "content_block_start",
                json!({
                    "type":"content_block_start",
                    "index":1,
                    "content_block":{"type":"tool_use","id":"tool_a","name":"read","input":{}}
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type":"content_block_delta",
                    "index":1,
                    "delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}
                }),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "content_block_start",
                json!({
                    "type":"content_block_start",
                    "index":2,
                    "content_block":{"type":"tool_use","id":"tool_b","name":"read","input":{}}
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type":"content_block_delta",
                    "index":2,
                    "delta":{"type":"input_json_delta","partial_json":"{\"path\":\"b\"}"}
                }),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
        ];

        match model {
            "claude-malformed-stream" => {
                let mut text = render_sse(&frames);
                text.push_str("event: content_block_delta\ndata: {not-json\n\n");
                return sse_response(text);
            }
            "claude-premature-eof" => return sse_response(render_sse(&frames)),
            _ => {}
        }

        frames.push((
            "message_delta",
            json!({
                "type":"message_delta",
                "delta":{"stop_reason":"tool_use","stop_sequence":Value::Null},
                "usage":{"output_tokens":9}
            }),
        ));
        frames.push(("message_stop", json!({"type":"message_stop"})));
        return sse_response(render_sse(&frames));
    }

    Json(json!({
        "id":"msg_nonstream",
        "type":"message",
        "role":"assistant",
        "model":model,
        "content":[
            {"type":"thinking","thinking":"inspect","signature":"sig_fixture"},
            {"type":"tool_use","id":"tool_a","name":"read","input":{"path":"a"}},
            {"type":"tool_use","id":"tool_b","name":"read","input":{"path":"b"}}
        ],
        "stop_reason":"tool_use",
        "stop_sequence":Value::Null,
        "usage":{
            "input_tokens":21,
            "output_tokens":9,
            "cache_creation_input_tokens":3,
            "cache_read_input_tokens":5
        },
        "fixture_extension":{"preserved":true}
    }))
    .into_response()
}

fn reasoning_summary_fixture_item(model: &str) -> Option<Value> {
    Some(match model {
        "gpt-reasoning-absent-both" => json!({
            "type":"reasoning",
            "id":"reasoning-absent",
            "encrypted_content":"encrypted-absent",
            "summary":[]
        }),
        "gpt-reasoning-empty-array-id" => json!({
            "type":"reasoning",
            "id":"reasoning-empty-array",
            "summary":[]
        }),
        "gpt-reasoning-empty-text-both" => json!({
            "type":"reasoning",
            "id":"reasoning-empty-text",
            "encrypted_content":"encrypted-empty-text",
            "summary":[{"type":"summary_text","text":""}]
        }),
        "gpt-reasoning-whitespace-encrypted" => json!({
            "type":"reasoning",
            "encrypted_content":"encrypted-whitespace",
            "summary":[
                {"type":"summary_text","text":" \n\t"},
                {"type":"summary_text","text":""}
            ]
        }),
        "gpt-reasoning-empty-carrier-free" => json!({
            "type":"reasoning",
            "summary":[
                {"type":"summary_text","text":""},
                {"type":"summary_text","text":"  "}
            ]
        }),
        "gpt-reasoning-empty-id-value" => json!({
            "type":"reasoning",
            "id":"",
            "summary":[{"type":"summary_text","text":""}]
        }),
        "gpt-reasoning-empty-encrypted-value" => json!({
            "type":"reasoning",
            "encrypted_content":"",
            "summary":[{"type":"summary_text","text":" \n"}]
        }),
        "gpt-reasoning-both-empty-values" => json!({
            "type":"reasoning",
            "id":"",
            "encrypted_content":"",
            "summary":[]
        }),
        "gpt-reasoning-leading-text" => json!({
            "type":"reasoning",
            "id":"reasoning-leading",
            "encrypted_content":"encrypted-leading",
            "summary":[{"type":"summary_text","text":"  analysis  "}]
        }),
        "gpt-reasoning-multiple-parts" => json!({
            "type":"reasoning",
            "id":"reasoning-parts",
            "encrypted_content":"encrypted-parts",
            "summary":[
                {"type":"summary_text","text":"  first "},
                {"type":"summary_text","text":""},
                {"type":"summary_text","text":"\tsecond\n"},
                {"type":"summary_text","text":""}
            ]
        }),
        "gpt-reasoning-summary-content" => json!({
            "type":"reasoning",
            "id":"reasoning-content",
            "encrypted_content":"encrypted-content",
            "summary":[{"type":"summary_text","text":" summary "}],
            "content":[
                {"type":"reasoning_text","text":" raw content "},
                {"type":"reasoning_text","text":"second"}
            ]
        }),
        _ => return None,
    })
}

fn audited_raw_output_variants() -> Vec<(&'static str, Value)> {
    vec![
        (
            "gpt-raw-additional-tools",
            json!({"type":"additional_tools","role":"developer","tools":[]}),
        ),
        (
            "gpt-raw-agent-message",
            json!({
                "type":"agent_message",
                "id":"agent-raw",
                "author":"agent",
                "recipient":"user",
                "content":[{"type":"input_text","text":"agent output"}]
            }),
        ),
        (
            "gpt-raw-local-shell-call",
            json!({
                "type":"local_shell_call",
                "id":"shell-raw",
                "call_id":"shell-call",
                "status":"completed",
                "action":{
                    "type":"exec",
                    "command":["pwd"],
                    "timeout_ms":null,
                    "working_directory":null,
                    "env":null,
                    "user":null
                }
            }),
        ),
        (
            "gpt-raw-function-call-output",
            json!({
                "type":"function_call_output",
                "id":"function-output-raw",
                "call_id":"function-call",
                "output":"done"
            }),
        ),
        (
            "gpt-raw-custom-tool-call-output",
            json!({
                "type":"custom_tool_call_output",
                "id":"custom-output-raw",
                "call_id":"custom-call",
                "name":"freeform",
                "output":"done"
            }),
        ),
        (
            "gpt-raw-web-search-call",
            json!({
                "type":"web_search_call",
                "id":"web-raw",
                "status":"completed",
                "action":{"type":"search","query":"rust"}
            }),
        ),
        (
            "gpt-raw-image-generation-call",
            json!({
                "type":"image_generation_call",
                "id":"image-raw",
                "status":"completed",
                "revised_prompt":"a ferris crab",
                "result":"image-data"
            }),
        ),
        (
            "gpt-raw-context-compaction",
            json!({
                "type":"context_compaction",
                "id":"context-raw",
                "encrypted_content":"opaque"
            }),
        ),
        (
            "gpt-raw-compaction-trigger",
            json!({"type":"compaction_trigger"}),
        ),
        (
            "gpt-raw-future-variant",
            json!({
                "type":"future_response_item",
                "id":"future-raw",
                "future":{"keep":true}
            }),
        ),
    ]
}

fn raw_output_variant_for_model(model: &str) -> Option<Value> {
    audited_raw_output_variants()
        .into_iter()
        .find_map(|(candidate, item)| (candidate == model).then_some(item))
}

fn raw_output_stream_fixture(model: &str) -> Option<Response> {
    let item = raw_output_variant_for_model(model)?;
    Some(sse_response(render_sse(&[
        (
            "response.created",
            json!({
                "type":"response.created",
                "sequence_number":0,
                "response":{"id":"resp_raw_fixture","model":model}
            }),
        ),
        (
            "response.output_item.added",
            json!({
                "type":"response.output_item.added",
                "sequence_number":1,
                "output_index":0,
                "item":item.clone()
            }),
        ),
        (
            "response.output_item.done",
            json!({
                "type":"response.output_item.done",
                "sequence_number":2,
                "output_index":0,
                "item":item.clone()
            }),
        ),
        (
            "response.completed",
            json!({
                "type":"response.completed",
                "sequence_number":3,
                "response":{"id":"resp_raw_fixture","output":[item]}
            }),
        ),
    ])))
}

fn created_output_contract_stream_fixture(model: &str) -> Option<Response> {
    let created_item = json!({
        "type":"message",
        "id":null,
        "role":"assistant",
        "status":"in_progress",
        "content":[]
    });
    let added_item = json!({
        "type":"message",
        "role":"assistant",
        "status":"in_progress",
        "content":[]
    });
    let done_item = json!({
        "type":"message",
        "role":"assistant",
        "status":"completed",
        "content":[{"type":"output_text","text":"created lifecycle"}]
    });
    let events = match model {
        "gpt-contract-created-only-output" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_created_output",
                        "output":[{
                            "type":"message",
                            "role":"assistant",
                            "status":"completed",
                            "content":[{"type":"output_text","text":"unrendered"}]
                        }]
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_created_output"}
                }),
            ),
        ],
        "gpt-contract-created-only-raw-output" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_created_output",
                        "output":[{
                            "type":"web_search_call",
                            "id":"created-raw",
                            "status":"completed",
                            "action":{"type":"search","query":"rust"}
                        }]
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_created_output"}
                }),
            ),
        ],
        "gpt-contract-created-output-lifecycle-match" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{"id":"resp_created_output","output":[created_item]}
                }),
            ),
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":added_item
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":done_item.clone()
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":3,
                    "response":{"id":"resp_created_output","output":[done_item]}
                }),
            ),
        ],
        "gpt-contract-created-output-conflict" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{"id":"resp_created_output","output":[created_item]}
                }),
            ),
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "id":"different",
                        "role":"assistant",
                        "status":"in_progress",
                        "content":[]
                    }
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":done_item
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":3,
                    "response":{"id":"resp_created_output"}
                }),
            ),
        ],
        _ => return None,
    };
    Some(sse_response(render_sse(&events)))
}

fn terminal_contract_stream_fixture(model: &str) -> Option<Response> {
    let created = (
        "response.created",
        json!({
            "type":"response.created",
            "sequence_number":0,
            "response":{
                "id":"resp_terminal_fixture",
                "object":"response"
            }
        }),
    );
    let usage = json!({
        "input_tokens":11,
        "input_tokens_details":{"cached_tokens":3},
        "output_tokens":7,
        "output_tokens_details":{"reasoning_tokens":2},
        "total_tokens":18
    });
    let completed_no_status_usage = (
        "response.completed",
        json!({
            "type":"response.completed",
            "sequence_number":1,
            "response":{"id":"resp_terminal_fixture","usage":usage.clone()}
        }),
    );
    let completed_no_status_no_usage = (
        "response.completed",
        json!({
            "type":"response.completed",
            "sequence_number":1,
            "response":{"id":"resp_terminal_fixture"}
        }),
    );
    let incomplete_no_status_usage = (
        "response.incomplete",
        json!({
            "type":"response.incomplete",
            "sequence_number":1,
            "response":{
                "id":"resp_terminal_fixture",
                "incomplete_details":{"reason":"max_output_tokens"},
                "usage":usage
            }
        }),
    );
    let incomplete_no_status_no_usage = (
        "response.incomplete",
        json!({
            "type":"response.incomplete",
            "sequence_number":1,
            "response":{
                "id":"resp_terminal_fixture",
                "incomplete_details":{"reason":"content_filter"}
            }
        }),
    );
    let pending_item = (
        "response.output_item.added",
        json!({
            "type":"response.output_item.added",
            "sequence_number":1,
            "output_index":0,
            "item":{"type":"reasoning","id":"pending-terminal","summary":[]}
        }),
    );
    let failed = (
        "response.failed",
        json!({
            "type":"response.failed",
            "sequence_number":2,
            "response":{
                "id":"resp_terminal_fixture",
                "error":{"code":"server_error","message":"canonical fixture failure"}
            }
        }),
    );
    let error = (
        "error",
        json!({
            "type":"error",
            "sequence_number":3,
            "code":"stream_error",
            "message":"canonical top-level error"
        }),
    );

    let events = match model {
        "gpt-terminal-completed-no-status-usage" => {
            vec![created, completed_no_status_usage]
        }
        "gpt-terminal-completed-no-status-no-usage" => {
            vec![created, completed_no_status_no_usage]
        }
        "gpt-terminal-completed-matching-status" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_terminal_fixture","status":"completed"}
                }),
            ),
        ],
        "gpt-terminal-completed-null-status" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_terminal_fixture",
                        "status":null,
                        "model":null,
                        "object":42,
                        "created_at":"ignored",
                        "metadata":["ignored"],
                        "instructions":{"ignored":true},
                        "parallel_tool_calls":"ignored",
                        "tools":{"ignored":true},
                        "temperature":"ignored",
                        "top_p":false,
                        "output":null,
                        "output_text":42,
                        "usage":null,
                        "end_turn":null
                    }
                }),
            ),
        ],
        "gpt-terminal-completed-wrong-status-type" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_terminal_fixture","status":42}
                }),
            ),
        ],
        "gpt-terminal-completed-mismatched-status" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_terminal_fixture","status":"incomplete"}
                }),
            ),
        ],
        "gpt-terminal-incomplete-no-status-usage" => {
            vec![created, incomplete_no_status_usage]
        }
        "gpt-terminal-incomplete-no-status-no-usage" => {
            vec![created, incomplete_no_status_no_usage]
        }
        "gpt-terminal-incomplete-matching-status" => vec![
            created,
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_terminal_fixture",
                        "status":"incomplete",
                        "incomplete_details":{"reason":"max_output_tokens"}
                    }
                }),
            ),
        ],
        "gpt-terminal-incomplete-null-status" => vec![
            created,
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_terminal_fixture",
                        "status":null,
                        "incomplete_details":{"reason":"max_output_tokens"}
                    }
                }),
            ),
        ],
        "gpt-terminal-incomplete-wrong-status-type" => vec![
            created,
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_terminal_fixture",
                        "status":false,
                        "incomplete_details":{"reason":"max_output_tokens"}
                    }
                }),
            ),
        ],
        "gpt-terminal-incomplete-mismatched-status" => vec![
            created,
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_terminal_fixture",
                        "status":"completed",
                        "incomplete_details":{"reason":"max_output_tokens"}
                    }
                }),
            ),
        ],
        "gpt-terminal-completed-pending-item" => vec![
            created,
            pending_item,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":2,
                    "response":{"id":"resp_terminal_fixture"}
                }),
            ),
            error,
        ],
        "gpt-terminal-incomplete-pending-item" => vec![
            created,
            pending_item,
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":2,
                    "response":{
                        "id":"resp_terminal_fixture",
                        "incomplete_details":{"reason":"max_output_tokens"}
                    }
                }),
            ),
            error,
        ],
        "gpt-terminal-completed-repeated-later" => vec![
            created,
            completed_no_status_usage.clone(),
            completed_no_status_usage,
            incomplete_no_status_no_usage,
            failed,
            error,
        ],
        "gpt-terminal-incomplete-repeated-later" => vec![
            created,
            incomplete_no_status_usage.clone(),
            incomplete_no_status_usage,
            completed_no_status_no_usage,
            failed,
            error,
        ],
        "gpt-terminal-failed-later" => vec![created, failed, completed_no_status_no_usage, error],
        "gpt-terminal-failed-null-status" => vec![
            created,
            (
                "response.failed",
                json!({
                    "type":"response.failed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_terminal_fixture",
                        "status":null,
                        "error":{"message":"nullable failed status"}
                    }
                }),
            ),
        ],
        "gpt-terminal-error-later" => vec![created, error, failed, completed_no_status_no_usage],
        "gpt-terminal-incomplete-unknown-reason" => vec![
            created,
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_terminal_fixture",
                        "incomplete_details":{"reason":"unknown_fixture_reason"}
                    }
                }),
            ),
            completed_no_status_no_usage,
        ],
        "gpt-terminal-incomplete-missing-response" => vec![
            created,
            (
                "response.incomplete",
                json!({"type":"response.incomplete","sequence_number":1}),
            ),
            completed_no_status_no_usage,
        ],
        "gpt-terminal-completed-missing-id" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{}
                }),
            ),
            error,
        ],
        _ => return None,
    };

    Some(sse_response(render_sse(&events)))
}

fn created_usage_contract_stream_fixture(model: &str) -> Option<Response> {
    let mut include_created = true;
    let mut created_response = json!({"id":"resp_contract_fixture"});
    let mut terminal_type = "response.completed";
    let mut terminal_response = json!({"id":"resp_contract_fixture"});
    let valid_usage = json!({
        "input_tokens":5,
        "input_tokens_details":{"cached_tokens":2},
        "output_tokens":3,
        "output_tokens_details":{"reasoning_tokens":1},
        "total_tokens":8
    });

    match model {
        "gpt-contract-created-model-less" => {}
        "gpt-contract-created-with-model" => {
            created_response["model"] = json!(model);
        }
        "gpt-contract-created-upstream-model" => {
            created_response["model"] = json!("upstream-reported-model");
        }
        "gpt-contract-created-null-optionals" => {
            created_response["model"] = Value::Null;
            created_response["status"] = Value::Null;
            created_response["object"] = json!(42);
            created_response["created_at"] = json!("ignored");
            created_response["metadata"] = json!(["ignored"]);
            created_response["instructions"] = json!({"ignored":true});
            created_response["parallel_tool_calls"] = json!("ignored");
            created_response["tools"] = json!({"ignored":true});
            created_response["temperature"] = json!("ignored");
            created_response["top_p"] = json!(false);
            created_response["output"] = Value::Null;
            created_response["output_text"] = json!(42);
        }
        "gpt-contract-created-empty-id" => created_response["id"] = json!(""),
        "gpt-contract-created-wrong-id" => created_response["id"] = json!(42),
        "gpt-contract-created-missing-id" => {
            created_response.as_object_mut().unwrap().remove("id");
        }
        "gpt-contract-created-empty-model" => created_response["model"] = json!(""),
        "gpt-contract-created-wrong-model" => created_response["model"] = json!(42),
        "gpt-contract-created-status-mismatch" => {
            created_response["status"] = json!("completed");
        }
        "gpt-contract-created-wrong-status-type" => {
            created_response["status"] = json!(42);
        }
        "gpt-contract-created-partial-usage" => {
            created_response["usage"] = json!({"input_tokens":1});
        }
        "gpt-contract-completed-empty-id" => terminal_response["id"] = json!(""),
        "gpt-contract-completed-wrong-id" => terminal_response["id"] = json!(42),
        "gpt-contract-completed-mismatched-id" => {
            terminal_response["id"] = json!("different-response");
        }
        "gpt-contract-incomplete-empty-id" => {
            terminal_type = "response.incomplete";
            terminal_response["id"] = json!("");
            terminal_response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        "gpt-contract-incomplete-wrong-id" => {
            terminal_type = "response.incomplete";
            terminal_response["id"] = json!(42);
            terminal_response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        "gpt-contract-incomplete-mismatched-id" => {
            terminal_type = "response.incomplete";
            terminal_response["id"] = json!("different-response");
            terminal_response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        "gpt-contract-failed-empty-id" => {
            terminal_type = "response.failed";
            terminal_response["id"] = json!("");
            terminal_response["error"] = json!({"message":"failed"});
        }
        "gpt-contract-failed-wrong-id" => {
            terminal_type = "response.failed";
            terminal_response["id"] = json!(42);
            terminal_response["error"] = json!({"message":"failed"});
        }
        "gpt-contract-failed-mismatched-id" => {
            terminal_type = "response.failed";
            terminal_response["id"] = json!("different-response");
            terminal_response["error"] = json!({"message":"failed"});
        }
        "gpt-contract-failed-without-created" => {
            include_created = false;
            terminal_type = "response.failed";
            terminal_response["error"] = json!({"message":"failed before created"});
        }
        "gpt-contract-terminal-wrong-end-turn" => {
            terminal_response["end_turn"] = json!("yes");
        }
        "gpt-contract-terminal-null-end-turn" => {
            terminal_response["end_turn"] = Value::Null;
        }
        "gpt-contract-terminal-true-end-turn" => {
            terminal_response["end_turn"] = json!(true);
        }
        "gpt-contract-terminal-false-end-turn" => {
            terminal_response["end_turn"] = json!(false);
        }
        "gpt-contract-usage-valid-details" => terminal_response["usage"] = valid_usage,
        "gpt-contract-usage-null-details" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "input_tokens_details":null,
                "output_tokens":3,
                "output_tokens_details":null,
                "total_tokens":8
            });
        }
        "gpt-contract-usage-null" => terminal_response["usage"] = Value::Null,
        "gpt-contract-usage-wrong-type" => terminal_response["usage"] = json!("tokens"),
        "gpt-contract-usage-missing-input" => {
            terminal_response["usage"] = json!({"output_tokens":3,"total_tokens":3});
        }
        "gpt-contract-usage-missing-output" => {
            terminal_response["usage"] = json!({"input_tokens":5,"total_tokens":5});
        }
        "gpt-contract-usage-missing-total" => {
            terminal_response["usage"] = json!({"input_tokens":5,"output_tokens":3});
        }
        "gpt-contract-usage-wrong-input" => {
            terminal_response["usage"] =
                json!({"input_tokens":"5","output_tokens":3,"total_tokens":8});
        }
        "gpt-contract-usage-wrong-output" => {
            terminal_response["usage"] =
                json!({"input_tokens":5,"output_tokens":3.5,"total_tokens":8});
        }
        "gpt-contract-usage-null-total" => {
            terminal_response["usage"] =
                json!({"input_tokens":5,"output_tokens":3,"total_tokens":null});
        }
        "gpt-contract-usage-negative-input" => {
            terminal_response["usage"] =
                json!({"input_tokens":-1,"output_tokens":3,"total_tokens":2});
        }
        "gpt-contract-usage-negative-output" => {
            terminal_response["usage"] =
                json!({"input_tokens":5,"output_tokens":-1,"total_tokens":4});
        }
        "gpt-contract-usage-negative-total" => {
            terminal_response["usage"] =
                json!({"input_tokens":5,"output_tokens":3,"total_tokens":-8});
        }
        "gpt-contract-usage-integer-overflow" => {
            terminal_response["usage"] = json!({
                "input_tokens":9223372036854775808_u64,
                "output_tokens":0,
                "total_tokens":9223372036854775808_u64
            });
        }
        "gpt-contract-usage-sum-overflow" => {
            terminal_response["usage"] =
                json!({"input_tokens":i64::MAX,"output_tokens":1,"total_tokens":i64::MAX});
        }
        "gpt-contract-usage-total-mismatch" => {
            terminal_response["usage"] =
                json!({"input_tokens":5,"output_tokens":3,"total_tokens":9});
        }
        "gpt-contract-usage-input-details-wrong" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "input_tokens_details":[],
                "output_tokens":3,
                "total_tokens":8
            });
        }
        "gpt-contract-usage-output-details-wrong" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "output_tokens":3,
                "output_tokens_details":"reasoning",
                "total_tokens":8
            });
        }
        "gpt-contract-usage-missing-cached" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "input_tokens_details":{},
                "output_tokens":3,
                "total_tokens":8
            });
        }
        "gpt-contract-usage-missing-reasoning" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "output_tokens":3,
                "output_tokens_details":{},
                "total_tokens":8
            });
        }
        "gpt-contract-usage-negative-cached" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "input_tokens_details":{"cached_tokens":-1},
                "output_tokens":3,
                "total_tokens":8
            });
        }
        "gpt-contract-usage-negative-reasoning" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "output_tokens":3,
                "output_tokens_details":{"reasoning_tokens":-1},
                "total_tokens":8
            });
        }
        "gpt-contract-usage-cached-exceeds-input" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "input_tokens_details":{"cached_tokens":6},
                "output_tokens":3,
                "total_tokens":8
            });
        }
        "gpt-contract-usage-reasoning-exceeds-output" => {
            terminal_response["usage"] = json!({
                "input_tokens":5,
                "output_tokens":3,
                "output_tokens_details":{"reasoning_tokens":4},
                "total_tokens":8
            });
        }
        _ => return None,
    }

    let mut events = Vec::new();
    if include_created {
        events.push((
            "response.created",
            json!({
                "type":"response.created",
                "sequence_number":0,
                "response":created_response
            }),
        ));
    }
    events.push((
        terminal_type,
        json!({
            "type":terminal_type,
            "sequence_number":1,
            "response":terminal_response
        }),
    ));
    events.push((
        "error",
        json!({"type":"error","sequence_number":2,"message":"later terminal"}),
    ));

    Some(sse_response(render_sse(&events)))
}

fn malformed_scalar_stream_fixture(model: &str) -> Option<Response> {
    let created = (
        "response.created",
        json!({
            "type":"response.created",
            "sequence_number":0,
            "response":{"id":"resp_scalar_fixture"}
        }),
    );
    let later_terminal = (
        "response.completed",
        json!({
            "type":"response.completed",
            "sequence_number":99,
            "response":{"id":"resp_scalar_fixture"}
        }),
    );
    let function_added = (
        "response.output_item.added",
        json!({
            "type":"response.output_item.added",
            "sequence_number":1,
            "output_index":0,
            "item":{
                "type":"function_call",
                "id":"function-scalar",
                "call_id":"call-scalar",
                "name":"read",
                "arguments":""
            }
        }),
    );
    let function_done_item = json!({
        "type":"function_call",
        "id":"function-scalar",
        "call_id":"call-scalar",
        "name":"read",
        "arguments":"{\"path\":\"a\"}"
    });
    let message_added = (
        "response.output_item.added",
        json!({
            "type":"response.output_item.added",
            "sequence_number":1,
            "output_index":0,
            "item":{
                "type":"message",
                "id":"message-scalar",
                "role":"assistant",
                "content":[]
            }
        }),
    );
    let reasoning_added = (
        "response.output_item.added",
        json!({
            "type":"response.output_item.added",
            "sequence_number":1,
            "output_index":0,
            "item":{"type":"reasoning","id":"reasoning-scalar","summary":[]}
        }),
    );
    let completed_with = |sequence_number: i64, output: Vec<Value>| {
        (
            "response.completed",
            json!({
                "type":"response.completed",
                "sequence_number":sequence_number,
                "response":{
                    "id":"resp_scalar_fixture",
                    "output":output,
                    "usage":{
                        "input_tokens":5,
                        "input_tokens_details":{"cached_tokens":2},
                        "output_tokens":3,
                        "output_tokens_details":{"reasoning_tokens":1},
                        "total_tokens":8
                    }
                }
            }),
        )
    };

    let events = match model {
        "gpt-scalar-function-valid" => vec![
            created,
            function_added,
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "item_id":"function-scalar",
                    "call_id":"call-scalar",
                    "delta":"{\"path\":"
                }),
            ),
            (
                "response.function_call_arguments.done",
                json!({
                    "type":"response.function_call_arguments.done",
                    "sequence_number":3,
                    "output_index":0,
                    "item_id":"function-scalar",
                    "call_id":"call-scalar",
                    "arguments":"{\"path\":\"a\"}"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":4,
                    "output_index":0,
                    "item":function_done_item.clone()
                }),
            ),
            completed_with(5, vec![function_done_item]),
        ],
        "gpt-scalar-custom-tool-valid" => {
            let added = json!({
                "type":"custom_tool_call",
                "id":"custom-scalar",
                "call_id":"custom-call",
                "name":"freeform",
                "input":""
            });
            let done = json!({
                "type":"custom_tool_call",
                "id":"custom-scalar",
                "call_id":"custom-call",
                "name":"freeform",
                "input":"payload"
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":added
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":2,
                        "output_index":0,
                        "item":done.clone()
                    }),
                ),
                completed_with(3, vec![done]),
            ]
        }
        "gpt-scalar-function-whitespace-namespace-valid" => {
            let added = json!({
                "type":"function_call",
                "id":"function-namespace",
                "call_id":"call-namespace",
                "name":"read",
                "namespace":" ",
                "arguments":""
            });
            let done = json!({
                "type":"function_call",
                "id":"function-namespace",
                "call_id":"call-namespace",
                "name":"read",
                "namespace":" ",
                "arguments":"{}"
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":added
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":2,
                        "output_index":0,
                        "item":done.clone()
                    }),
                ),
                completed_with(3, vec![done]),
            ]
        }
        "gpt-scalar-tool-search-valid" => {
            let item = json!({
                "type":"tool_search_call",
                "call_id":null,
                "execution":"client",
                "arguments":{"query":"calendar","limit":1}
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":2,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                completed_with(3, vec![item]),
            ]
        }
        "gpt-scalar-tool-search-item-id-valid" => {
            let item = json!({
                "type":"tool_search_call",
                "id":"search-item-only",
                "execution":"client",
                "arguments":{"query":"calendar"}
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":2,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                completed_with(3, vec![item]),
            ]
        }
        "gpt-scalar-tool-search-late-id-valid" => {
            let added = json!({
                "type":"tool_search_call",
                "id":"search-late-item",
                "call_id":null,
                "execution":"client",
                "arguments":{"query":"calendar"}
            });
            let done = json!({
                "type":"tool_search_call",
                "id":"search-late-item",
                "call_id":"late-search-call",
                "execution":"client",
                "arguments":{"query":"calendar"}
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":added
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":2,
                        "output_index":0,
                        "item":done.clone()
                    }),
                ),
                completed_with(3, vec![done]),
            ]
        }
        "gpt-scalar-tool-search-conflicting-id" => {
            let added = json!({
                "type":"tool_search_call",
                "id":"search-conflict-item",
                "call_id":"search-call-a",
                "execution":"client",
                "arguments":{"query":"calendar"}
            });
            let done = json!({
                "type":"tool_search_call",
                "id":"search-conflict-item",
                "call_id":"search-call-b",
                "execution":"client",
                "arguments":{"query":"calendar"}
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":added
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":2,
                        "output_index":0,
                        "item":done
                    }),
                ),
                later_terminal,
            ]
        }
        "gpt-scalar-tool-search-removed-id" => {
            let added = json!({
                "type":"tool_search_call",
                "id":"search-removed-item",
                "call_id":"search-call-present",
                "execution":"client",
                "arguments":{"query":"calendar"}
            });
            let done = json!({
                "type":"tool_search_call",
                "id":"search-removed-item",
                "call_id":null,
                "execution":"client",
                "arguments":{"query":"calendar"}
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":added
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":2,
                        "output_index":0,
                        "item":done
                    }),
                ),
                later_terminal,
            ]
        }
        "gpt-scalar-tool-search-unexpected-delta" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"tool_search_call",
                        "call_id":null,
                        "execution":"client",
                        "arguments":{"query":"calendar"}
                    }
                }),
            ),
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "delta":"{}"
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-empty-call-id" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"tool_search_call",
                        "call_id":" ",
                        "execution":"client",
                        "arguments":{"query":"calendar"}
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-output-valid" => {
            let item = json!({
                "type":"tool_search_output",
                "status":"completed",
                "execution":"client",
                "tools":[{"name":"calendar"}]
            });
            vec![
                created,
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":1,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                completed_with(2, vec![item]),
            ]
        }
        "gpt-scalar-message-valid" => {
            let annotations = json!([{"type":"url_citation","url":"https://example.test"}]);
            let item = json!({
                "type":"message",
                "id":"message-scalar",
                "role":"assistant",
                "content":[{
                    "type":"output_text",
                    "text":"AB",
                    "annotations":annotations.clone()
                }],
                "internal_chat_message_metadata_passthrough":{"turn_id":"turn-scalar"}
            });
            vec![
                created,
                message_added,
                (
                    "response.output_text.delta",
                    json!({
                        "type":"response.output_text.delta",
                        "sequence_number":2,
                        "output_index":0,
                        "item_id":"message-scalar",
                        "content_index":0,
                        "delta":"A",
                        "annotations":[]
                    }),
                ),
                (
                    "response.output_text.done",
                    json!({
                        "type":"response.output_text.done",
                        "sequence_number":3,
                        "output_index":0,
                        "item_id":"message-scalar",
                        "content_index":0,
                        "text":"AB",
                        "annotations":annotations
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":4,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                completed_with(5, vec![item]),
            ]
        }
        "gpt-scalar-message-incomplete-on-completed" => {
            let item = json!({
                "type":"message",
                "id":"message-incomplete",
                "role":"assistant",
                "status":"incomplete",
                "content":[{"type":"output_text","text":"partial"}]
            });
            vec![
                created,
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":1,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                completed_with(2, vec![item]),
            ]
        }
        "gpt-scalar-reasoning-valid" => {
            let item = json!({
                "type":"reasoning",
                "id":"reasoning-scalar",
                "summary":[{"type":"summary_text","text":"summary"}],
                "content":[{"type":"reasoning_text","text":"content"}],
                "encrypted_content":"opaque"
            });
            vec![
                created,
                reasoning_added,
                (
                    "response.reasoning_summary_part.added",
                    json!({
                        "type":"response.reasoning_summary_part.added",
                        "sequence_number":2,
                        "output_index":0,
                        "summary_index":0,
                        "part":{"type":"summary_text","text":""}
                    }),
                ),
                (
                    "response.reasoning_summary_text.delta",
                    json!({
                        "type":"response.reasoning_summary_text.delta",
                        "sequence_number":3,
                        "output_index":0,
                        "summary_index":0,
                        "delta":"summary"
                    }),
                ),
                (
                    "response.reasoning_summary_text.done",
                    json!({
                        "type":"response.reasoning_summary_text.done",
                        "sequence_number":4,
                        "item_id":"reasoning-scalar",
                        "output_index":0,
                        "summary_index":0,
                        "text":"summary"
                    }),
                ),
                (
                    "response.reasoning_text.delta",
                    json!({
                        "type":"response.reasoning_text.delta",
                        "sequence_number":5,
                        "output_index":0,
                        "content_index":0,
                        "delta":"content"
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":6,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                completed_with(7, vec![item]),
            ]
        }
        "gpt-scalar-compaction-valid" => {
            let item = json!({"type":"compaction","encrypted_content":"opaque-compaction"});
            vec![
                created,
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":1,
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                completed_with(2, vec![item]),
            ]
        }
        "gpt-scalar-function-added-missing" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"function_call"}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-custom-tool-malformed" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"custom_tool_call","call_id":42,"name":null}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-added-missing-call-id" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"function_call","name":"read","arguments":""}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-added-missing-name" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call-scalar","arguments":""}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-added-missing-arguments" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call-scalar","name":"read"}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-added-wrong" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "call_id":42,
                        "name":null,
                        "arguments":{}
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-added-wrong-name" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "call_id":"call-scalar",
                        "name":42,
                        "arguments":""
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-added-wrong-arguments" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "call_id":"call-scalar",
                        "name":"read",
                        "arguments":{}
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-added-empty" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"","name":" ","arguments":""}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-added-invalid-json" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "call_id":"call-scalar",
                        "name":"read",
                        "arguments":"{bad"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-done-missing" => vec![
            created,
            function_added,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":{"type":"function_call","id":"function-scalar"}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-done-missing-call-id" => vec![
            created,
            function_added,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-scalar",
                        "name":"read",
                        "arguments":"{}"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-done-missing-name" => vec![
            created,
            function_added,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-scalar",
                        "call_id":"call-scalar",
                        "arguments":"{}"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-done-missing-arguments" => vec![
            created,
            function_added,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-scalar",
                        "call_id":"call-scalar",
                        "name":"read"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-done-invalid-json" => vec![
            created,
            function_added,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-scalar",
                        "call_id":"call-scalar",
                        "name":"read",
                        "arguments":"not-json"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-done-wrong-call-id" => vec![
            created,
            function_added,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-scalar",
                        "call_id":42,
                        "name":"read",
                        "arguments":"{}"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-done-wrong-name" => vec![
            created,
            function_added,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-scalar",
                        "call_id":"call-scalar",
                        "name":42,
                        "arguments":"{}"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-done-wrong-arguments" => vec![
            created,
            function_added,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":2,
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-scalar",
                        "call_id":"call-scalar",
                        "name":"read",
                        "arguments":{}
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-delta-wrong" => vec![
            created,
            function_added,
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "delta":42
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-delta-empty" => vec![
            created,
            function_added,
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "delta":""
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-arguments-done-invalid" => vec![
            created,
            function_added,
            (
                "response.function_call_arguments.done",
                json!({
                    "type":"response.function_call_arguments.done",
                    "sequence_number":2,
                    "output_index":0,
                    "arguments":"{bad"
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-arguments-done-duplicate" => vec![
            created,
            function_added,
            (
                "response.function_call_arguments.done",
                json!({
                    "type":"response.function_call_arguments.done",
                    "sequence_number":2,
                    "output_index":0,
                    "arguments":"{}"
                }),
            ),
            (
                "response.function_call_arguments.done",
                json!({
                    "type":"response.function_call_arguments.done",
                    "sequence_number":3,
                    "output_index":0,
                    "arguments":"{}"
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-function-delta-after-done" => vec![
            created,
            function_added,
            (
                "response.function_call_arguments.done",
                json!({
                    "type":"response.function_call_arguments.done",
                    "sequence_number":2,
                    "output_index":0,
                    "arguments":"{}"
                }),
            ),
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "sequence_number":3,
                    "output_index":0,
                    "delta":" "
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-missing-execution" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"tool_search_call","arguments":{}}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-wrong" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"tool_search_call",
                        "call_id":42,
                        "execution":[],
                        "status":{},
                        "arguments":{}
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-wrong-execution" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"tool_search_call",
                        "execution":42,
                        "arguments":{}
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-wrong-status" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"tool_search_call",
                        "execution":"client",
                        "status":[],
                        "arguments":{}
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-missing-arguments" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"tool_search_call","execution":"client"}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-output-malformed" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"tool_search_output","tools":"not-an-array"}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-output-wrong-execution" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"tool_search_output",
                        "status":"completed",
                        "execution":42,
                        "tools":[]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-tool-search-output-wrong-tools" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"tool_search_output",
                        "status":"completed",
                        "execution":"client",
                        "tools":{}
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-message-missing" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"message"}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-message-wrong-content" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"message","role":42,"content":{}}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-message-wrong-role" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"message","role":42,"content":[]}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-message-content-not-array" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"message","role":"assistant","content":{}}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-message-block-malformed" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "role":"assistant",
                        "content":[{"type":"output_text","text":42}]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-message-annotations-malformed" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "role":"assistant",
                        "content":[{
                            "type":"output_text",
                            "text":"text",
                            "annotations":"bad"
                        }]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-message-refusal-unsupported" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "role":"assistant",
                        "content":[{"type":"refusal","refusal":"blocked"}]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-message-image-unsupported" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "role":"assistant",
                        "content":[{
                            "type":"input_image",
                            "image_url":"https://example.test/image.png",
                            "detail":"high"
                        }]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-output-annotations-late" => vec![
            created,
            message_added,
            (
                "response.output_text.delta",
                json!({
                    "type":"response.output_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "delta":"partial"
                }),
            ),
            (
                "response.output_text.done",
                json!({
                    "type":"response.output_text.done",
                    "sequence_number":3,
                    "output_index":0,
                    "content_index":0,
                    "text":"partial",
                    "annotations":[42]
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-output-done-duplicate" => vec![
            created,
            message_added,
            (
                "response.output_text.done",
                json!({
                    "type":"response.output_text.done",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "text":"text"
                }),
            ),
            (
                "response.output_text.done",
                json!({
                    "type":"response.output_text.done",
                    "sequence_number":3,
                    "output_index":0,
                    "content_index":0,
                    "text":"text"
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-output-delta-after-done" => vec![
            created,
            message_added,
            (
                "response.output_text.done",
                json!({
                    "type":"response.output_text.done",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "text":"text"
                }),
            ),
            (
                "response.output_text.delta",
                json!({
                    "type":"response.output_text.delta",
                    "sequence_number":3,
                    "output_index":0,
                    "content_index":0,
                    "delta":"late"
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-output-index-mismatch" => vec![
            created,
            message_added,
            (
                "response.output_text.done",
                json!({
                    "type":"response.output_text.done",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":1,
                    "text":"extra"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":3,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "id":"message-scalar",
                        "role":"assistant",
                        "content":[{"type":"output_text","text":"only"}]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-wrong" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":42,
                        "summary":{},
                        "content":"bad",
                        "encrypted_content":[]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-missing-summary" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-scalar",
                        "encrypted_content":"opaque"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-wrong-id" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"reasoning","id":42,"summary":[]}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-wrong-encrypted" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "summary":[],
                        "encrypted_content":[]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-summary-not-array" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"reasoning","summary":{}}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-content-not-array" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[],"content":{}}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-summary-malformed" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "summary":[{"type":"summary_text"}]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-content-malformed" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "summary":[],
                        "content":[{"type":"reasoning_text","text":42}]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-event-id-wrong" => vec![
            created,
            reasoning_added,
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "sequence_number":2,
                    "item_id":42,
                    "output_index":0,
                    "content_index":0,
                    "delta":"reasoning"
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-part-malformed" => vec![
            created,
            reasoning_added,
            (
                "response.reasoning_summary_part.added",
                json!({
                    "type":"response.reasoning_summary_part.added",
                    "sequence_number":2,
                    "output_index":0,
                    "summary_index":0,
                    "part":{"type":"summary_text","text":42}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-done-missing-item-id" => vec![
            created,
            reasoning_added,
            (
                "response.reasoning_summary_text.done",
                json!({
                    "type":"response.reasoning_summary_text.done",
                    "sequence_number":2,
                    "output_index":0,
                    "summary_index":0,
                    "text":"summary"
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-summary-conflict" => vec![
            created,
            reasoning_added,
            (
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "summary_index":0,
                    "delta":"A"
                }),
            ),
            (
                "response.reasoning_summary_text.done",
                json!({
                    "type":"response.reasoning_summary_text.done",
                    "sequence_number":3,
                    "item_id":"reasoning-scalar",
                    "output_index":0,
                    "summary_index":0,
                    "text":"B"
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-reasoning-content-conflict" => vec![
            created,
            reasoning_added,
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "delta":"A"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":3,
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-scalar",
                        "summary":[],
                        "content":[{"type":"reasoning_text","text":"B"}]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-compaction-missing" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"compaction"}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-compaction-wrong" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"compaction","id":42,"encrypted_content":[]}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-compaction-wrong-id" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"compaction",
                        "id":42,
                        "encrypted_content":"opaque"
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-compaction-wrong-encrypted" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"compaction","encrypted_content":[]}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-output-index-wrong" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":"zero",
                    "item":{
                        "type":"message",
                        "role":"assistant",
                        "content":[]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-output-index-sparse" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":1,
                    "item":{"type":"message","role":"assistant","content":[]}
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-output-item-id-wrong" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item_id":42,
                    "item":{
                        "type":"message",
                        "id":[],
                        "role":"assistant",
                        "content":[]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-output-wrapper-id-mismatch" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item_id":"outer-id",
                    "item":{
                        "type":"message",
                        "id":"inner-id",
                        "role":"assistant",
                        "content":[]
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-metadata-wrong" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "role":"assistant",
                        "content":[],
                        "internal_chat_message_metadata_passthrough":42
                    }
                }),
            ),
            later_terminal,
        ],
        "gpt-scalar-metadata-turn-id-wrong" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "role":"assistant",
                        "content":[],
                        "internal_chat_message_metadata_passthrough":{"turn_id":42}
                    }
                }),
            ),
            later_terminal,
        ],
        _ => return None,
    };

    let mut events = events;
    events.push((
        "error",
        json!({
            "type":"error",
            "sequence_number":100,
            "message":"later scalar fixture terminal"
        }),
    ));
    Some(sse_response(render_sse(&events)))
}

fn scalar_nonstream_fixture(model: &str) -> Option<Response> {
    let output = match model {
        "gpt-scalar-function-valid" => vec![json!({
            "type":"function_call",
            "id":"function-scalar",
            "call_id":"call-scalar",
            "name":"read",
            "arguments":"{\"path\":\"a\"}"
        })],
        "gpt-scalar-custom-tool-valid" => vec![json!({
            "type":"custom_tool_call",
            "id":"custom-scalar",
            "call_id":"custom-call",
            "name":"freeform",
            "input":"payload"
        })],
        "gpt-scalar-tool-search-valid" => vec![json!({
            "type":"tool_search_call",
            "call_id":null,
            "execution":"client",
            "arguments":{"query":"calendar","limit":1}
        })],
        "gpt-scalar-tool-search-item-id-valid" => vec![json!({
            "type":"tool_search_call",
            "id":"search-item-only",
            "execution":"client",
            "arguments":{"query":"calendar"}
        })],
        "gpt-scalar-tool-search-late-id-valid" => vec![json!({
            "type":"tool_search_call",
            "id":"search-late-item",
            "call_id":"late-search-call",
            "execution":"client",
            "arguments":{"query":"calendar"}
        })],
        "gpt-scalar-tool-search-output-valid" => vec![json!({
            "type":"tool_search_output",
            "status":"completed",
            "execution":"client",
            "tools":[{"name":"calendar"}]
        })],
        "gpt-scalar-message-valid" => vec![json!({
            "type":"message",
            "id":"message-scalar",
            "role":"assistant",
            "content":[{
                "type":"output_text",
                "text":"AB",
                "annotations":[{"type":"url_citation","url":"https://example.test"}]
            }],
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn-scalar"}
        })],
        "gpt-scalar-reasoning-valid" => vec![json!({
            "type":"reasoning",
            "id":"reasoning-scalar",
            "summary":[{"type":"summary_text","text":"summary"}],
            "content":[{"type":"reasoning_text","text":"content"}],
            "encrypted_content":"opaque"
        })],
        "gpt-scalar-compaction-valid" => {
            vec![json!({"type":"compaction","encrypted_content":"opaque-compaction"})]
        }
        "gpt-scalar-function-added-missing" => vec![json!({"type":"function_call"})],
        "gpt-scalar-custom-tool-malformed" => {
            vec![json!({"type":"custom_tool_call","call_id":42,"name":null})]
        }
        "gpt-scalar-tool-search-missing-execution" => vec![json!({
            "type":"tool_search_call",
            "arguments":{}
        })],
        "gpt-scalar-tool-search-wrong" => vec![json!({
            "type":"tool_search_call",
            "call_id":42,
            "execution":"client",
            "arguments":{}
        })],
        "gpt-scalar-tool-search-empty-call-id" => vec![json!({
            "type":"tool_search_call",
            "call_id":" ",
            "execution":"client",
            "arguments":{}
        })],
        "gpt-scalar-tool-search-output-malformed" => vec![json!({
            "type":"tool_search_output",
            "tools":"not-an-array"
        })],
        "gpt-scalar-message-wrong-role" => vec![json!({
            "type":"message",
            "role":"user",
            "content":[]
        })],
        "gpt-scalar-message-incomplete-on-completed" => vec![json!({
            "type":"message",
            "id":"message-incomplete",
            "role":"assistant",
            "status":"incomplete",
            "content":[{"type":"output_text","text":"partial"}]
        })],
        "gpt-scalar-message-block-malformed" => vec![json!({
            "type":"message",
            "role":"assistant",
            "content":[{"type":"output_text","text":42}]
        })],
        "gpt-scalar-reasoning-missing-summary" => vec![json!({
            "type":"reasoning",
            "id":"reasoning-scalar",
            "encrypted_content":"opaque"
        })],
        "gpt-scalar-reasoning-wrong-id" => vec![json!({
            "type":"reasoning",
            "id":42,
            "summary":[]
        })],
        "gpt-scalar-compaction-missing" => vec![json!({"type":"compaction"})],
        "gpt-scalar-metadata-wrong" => vec![json!({
            "type":"message",
            "role":"assistant",
            "content":[],
            "internal_chat_message_metadata_passthrough":"wrong"
        })],
        "gpt-contract-completed-empty-id"
        | "gpt-contract-created-wrong-model"
        | "gpt-contract-terminal-wrong-end-turn"
        | "gpt-contract-usage-wrong-input"
        | "gpt-contract-usage-total-mismatch" => vec![],
        _ => vec![raw_output_variant_for_model(model)?],
    };
    let mut response = json!({
        "id":"resp_scalar_fixture",
        "object":"response",
        "created_at":1,
        "model":model,
        "status":"completed",
        "output":output,
        "output_text":"",
        "usage":{
            "input_tokens":5,
            "input_tokens_details":{"cached_tokens":2},
            "output_tokens":3,
            "output_tokens_details":{"reasoning_tokens":1},
            "total_tokens":8
        }
    });
    if model == "gpt-contract-usage-wrong-input" {
        response["usage"] = json!({"input_tokens":"5","output_tokens":3,"total_tokens":8});
    } else if model == "gpt-contract-usage-total-mismatch" {
        response["usage"] = json!({"input_tokens":5,"output_tokens":3,"total_tokens":9});
    } else if model == "gpt-contract-completed-empty-id" {
        response["id"] = json!("");
    } else if model == "gpt-contract-created-wrong-model" {
        response["model"] = json!(42);
    } else if model == "gpt-contract-terminal-wrong-end-turn" {
        response["end_turn"] = json!("wrong");
    }
    Some(Json(response).into_response())
}

fn web_search_authority_fixture(model: &str) -> Option<Response> {
    let required_usage = json!({"input_tokens":6,"output_tokens":4,"total_tokens":10});
    let detailed_usage = json!({
        "input_tokens":6,
        "input_tokens_details":{"cached_tokens":1},
        "output_tokens":4,
        "output_tokens_details":{"reasoning_tokens":1},
        "total_tokens":10
    });
    let output = json!([
        {
            "type":"web_search_call",
            "id":"authority-web",
            "status":"completed",
            "action":{"type":"search","query":"rust async"}
        },
        {
            "type":"message",
            "id":"authority-message",
            "role":"assistant",
            "status":"completed",
            "content":[{
                "type":"output_text",
                "text":"Grounded answer.",
                "annotations":[{
                    "type":"url_citation",
                    "url":"https://example.test/source",
                    "title":"Source"
                }]
            }]
        }
    ]);
    let mut created = json!({
        "id":"resp_web_partial",
        "status":null,
        "output":null,
        "usage":null
    });
    let mut terminal = json!({
        "id":"resp_web_partial",
        "status":null,
        "output":output,
        "usage":detailed_usage.clone()
    });
    match model {
        "gpt-web-model-requested-fallback" => {}
        "gpt-web-usage-details-created-only" => {
            created["usage"] = detailed_usage;
            terminal["usage"] = required_usage;
        }
        "gpt-web-usage-details-terminal-only" => {
            created["usage"] = json!({
                "input_tokens":6,
                "input_tokens_details":null,
                "output_tokens":4,
                "output_tokens_details":null,
                "total_tokens":10
            });
        }
        "gpt-web-created-usage-details-malformed" => {
            created["usage"] = json!({
                "input_tokens":6,
                "input_tokens_details":{},
                "output_tokens":4,
                "total_tokens":10
            });
        }
        "gpt-web-terminal-usage-details-malformed" => {
            terminal["usage"] = json!({
                "input_tokens":6,
                "output_tokens":4,
                "output_tokens_details":{"reasoning_tokens":"bad"},
                "total_tokens":10
            });
        }
        "gpt-web-incomplete-details-created-only" => {
            created["incomplete_details"] = json!({"reason":"created-only","nested":{"x":1}});
        }
        "gpt-web-incomplete-details-terminal-only" => {
            terminal["incomplete_details"] = json!({"reason":"terminal-only","nested":{"x":1}});
        }
        "gpt-web-incomplete-details-matching" => {
            created["incomplete_details"] = json!({"reason":"matching","nested":{"x":1}});
            terminal["incomplete_details"] = json!({"reason":"matching","nested":{"x":1}});
        }
        "gpt-web-incomplete-details-null-absent" => {
            created["incomplete_details"] = Value::Null;
        }
        "gpt-web-incomplete-details-conflict" => {
            created["incomplete_details"] = json!({"reason":"created","nested":{"x":1}});
            terminal["incomplete_details"] = json!({"reason":"terminal","nested":{"x":2}});
        }
        "gpt-web-metadata-created-only" => {
            created["metadata"] = json!({"source":"created","nested":{"x":1}});
        }
        "gpt-web-metadata-terminal-only" => {
            terminal["metadata"] = json!({"source":"terminal","nested":{"x":1}});
        }
        "gpt-web-metadata-matching" => {
            created["metadata"] = json!({"source":"matching","nested":{"x":1}});
            terminal["metadata"] = json!({"source":"matching","nested":{"x":1}});
        }
        "gpt-web-metadata-null-absent" => {
            terminal["metadata"] = Value::Null;
        }
        "gpt-web-created-metadata-malformed" => {
            created["metadata"] = json!("malformed");
        }
        "gpt-web-terminal-metadata-malformed" => {
            terminal["metadata"] = json!(["malformed"]);
        }
        "gpt-web-created-incomplete-details-malformed" => {
            created["incomplete_details"] = json!("malformed");
        }
        "gpt-web-terminal-incomplete-details-malformed" => {
            terminal["incomplete_details"] = json!(["malformed"]);
        }
        "gpt-web-end-turn-created-only" => {
            created["end_turn"] = json!(false);
        }
        "gpt-web-end-turn-terminal-only" => {
            terminal["end_turn"] = json!(false);
        }
        "gpt-web-end-turn-conflict" => {
            created["end_turn"] = json!(false);
            terminal["end_turn"] = json!(true);
        }
        "gpt-web-output-text-conflict" => {
            created["output_text"] = json!("created text");
            terminal["output_text"] = json!("terminal text");
        }
        "gpt-web-ignored-extra-conflict" => {
            created["future_snapshot_field"] = json!({"side":"created"});
            terminal["future_snapshot_field"] = json!({"side":"terminal"});
        }
        _ => return None,
    }

    let events = vec![
        (
            "response.created",
            json!({
                "type":"response.created",
                "sequence_number":0,
                "response":created
            }),
        ),
        (
            "response.completed",
            json!({
                "type":"response.completed",
                "sequence_number":1,
                "response":terminal
            }),
        ),
    ];
    Some(sse_response(render_sse(&events)))
}

fn web_annotation_output(annotations: Option<Value>, status: &str) -> Vec<Value> {
    let mut text = json!({
        "type":"output_text",
        "text":"Grounded answer."
    });
    if let Some(annotations) = annotations {
        text["annotations"] = annotations;
    }
    vec![
        json!({
            "type":"web_search_call",
            "id":"annotation-web",
            "status":status,
            "action":{"type":"search","query":"rust async"}
        }),
        json!({
            "type":"message",
            "id":"annotation-message",
            "role":"assistant",
            "status":status,
            "content":[text]
        }),
    ]
}

fn web_search_annotation_lifecycle_fixture(model: &str) -> Option<Response> {
    let citation = json!({
        "type":"url_citation",
        "url":"https://example.test/source",
        "title":"Source"
    });
    let conflicting = json!({
        "type":"url_citation",
        "url":"https://example.test/other",
        "title":"Other"
    });
    let unknown = json!({"type":"future_annotation","opaque":{"keep":true}});
    let (added_annotations, done_annotations, terminal_annotations) = match model {
        "gpt-web-annotations-lifecycle-empty-unknown" => {
            (Some(json!([unknown])), Some(json!([])), None)
        }
        "gpt-web-annotations-lifecycle-mixed-known" => (
            Some(json!([citation.clone(), unknown])),
            Some(json!([citation.clone()])),
            Some(json!([citation])),
        ),
        "gpt-web-annotations-lifecycle-conflict-known" => (
            Some(json!([citation])),
            Some(json!([conflicting.clone()])),
            Some(json!([conflicting])),
        ),
        _ => return None,
    };
    let added = web_annotation_output(added_annotations, "in_progress");
    let done = web_annotation_output(done_annotations, "completed");
    let terminal = web_annotation_output(terminal_annotations, "completed");
    Some(sse_response(render_sse(&[
        (
            "response.created",
            json!({
                "type":"response.created",
                "sequence_number":0,
                "response":{"id":"resp_web_annotations","status":null,"output":null}
            }),
        ),
        (
            "response.output_item.added",
            json!({
                "type":"response.output_item.added",
                "sequence_number":1,
                "output_index":0,
                "item":added[0]
            }),
        ),
        (
            "response.output_item.done",
            json!({
                "type":"response.output_item.done",
                "sequence_number":2,
                "output_index":0,
                "item":done[0]
            }),
        ),
        (
            "response.output_item.added",
            json!({
                "type":"response.output_item.added",
                "sequence_number":3,
                "output_index":1,
                "item":added[1]
            }),
        ),
        (
            "response.output_item.done",
            json!({
                "type":"response.output_item.done",
                "sequence_number":4,
                "output_index":1,
                "item":done[1]
            }),
        ),
        (
            "response.completed",
            json!({
                "type":"response.completed",
                "sequence_number":5,
                "response":{
                    "id":"resp_web_annotations",
                    "status":null,
                    "output":terminal,
                    "usage":{"input_tokens":6,"output_tokens":4,"total_tokens":10}
                }
            }),
        ),
    ])))
}

fn web_search_annotation_fixture(model: &str) -> Option<Response> {
    if let Some(response) = web_search_annotation_lifecycle_fixture(model) {
        return Some(response);
    }
    let citation = json!({
        "type":"url_citation",
        "url":"https://example.test/source",
        "title":"Source"
    });
    let unknown = json!({"type":"future_annotation","opaque":{"keep":true}});
    let mixed = json!([citation.clone(), unknown.clone()]);
    let (created_annotations, terminal_annotations) = match model {
        "gpt-web-annotations-created-empty-terminal-absent" => (Some(json!([])), None),
        "gpt-web-annotations-created-absent-terminal-empty" => (None, Some(json!([]))),
        "gpt-web-annotations-created-unknown-terminal-null" => {
            (Some(json!([unknown.clone()])), Some(Value::Null))
        }
        "gpt-web-annotations-created-null-terminal-unknown" => {
            (Some(Value::Null), Some(json!([unknown])))
        }
        "gpt-web-annotations-created-mixed-terminal-known" => {
            (Some(mixed.clone()), Some(json!([citation.clone()])))
        }
        "gpt-web-annotations-created-known-terminal-mixed" => {
            (Some(json!([citation.clone()])), Some(mixed))
        }
        "gpt-web-annotations-duplicate-known" => (
            Some(json!([
                citation.clone(),
                {
                    "type":"url_citation",
                    "url":"https://example.test/source",
                    "title":"Ignored duplicate title"
                }
            ])),
            Some(json!([citation.clone()])),
        ),
        "gpt-web-annotations-default-title" => (
            Some(json!([{
                "type":"url_citation",
                "url":"https://example.test/source"
            }])),
            Some(json!([{
                "type":"url_citation",
                "url":"https://example.test/source",
                "title":"https://example.test/source"
            }])),
        ),
        "gpt-web-annotations-known-extensions" => (
            Some(json!([{
                "type":"url_citation",
                "url":"https://example.test/source",
                "title":"Source",
                "start_index":0,
                "end_index":4,
                "future":{"keep":true}
            }])),
            Some(json!([citation.clone()])),
        ),
        "gpt-web-annotations-conflict-known" => (
            Some(json!([citation])),
            Some(json!([{
                "type":"url_citation",
                "url":"https://example.test/other",
                "title":"Other"
            }])),
        ),
        "gpt-web-annotations-malformed-field" => (Some(json!("wrong")), None),
        "gpt-web-annotations-malformed-entry" => (None, Some(json!([1]))),
        "gpt-web-annotations-malformed-type" => (Some(json!([{"type":1,"opaque":true}])), None),
        "gpt-web-annotations-malformed-known-missing-url" => (
            None,
            Some(json!([{"type":"url_citation","title":"Missing"}])),
        ),
        "gpt-web-annotations-malformed-known-url" => (
            Some(json!([{
                "type":"url_citation",
                "url":42,
                "title":"Wrong URL"
            }])),
            None,
        ),
        "gpt-web-annotations-malformed-known-title" => (
            None,
            Some(json!([{
                "type":"url_citation",
                "url":"https://example.test/source",
                "title":42
            }])),
        ),
        _ => return None,
    };
    let created_output = web_annotation_output(created_annotations, "completed");
    let terminal_output = web_annotation_output(terminal_annotations, "completed");
    Some(sse_response(render_sse(&[
        (
            "response.created",
            json!({
                "type":"response.created",
                "sequence_number":0,
                "response":{
                    "id":"resp_web_annotations",
                    "status":null,
                    "output":created_output
                }
            }),
        ),
        (
            "response.completed",
            json!({
                "type":"response.completed",
                "sequence_number":1,
                "response":{
                    "id":"resp_web_annotations",
                    "status":null,
                    "output":terminal_output,
                    "usage":{"input_tokens":6,"output_tokens":4,"total_tokens":10}
                }
            }),
        ),
    ])))
}

fn web_search_item_lifecycle_fixture(model: &str) -> Option<Response> {
    let (added, done) = match model {
        "gpt-web-lifecycle-item-fields-added-only" => (
            json!({
                "type":"web_search_call",
                "id":"lifecycle-web-id",
                "status":"in_progress",
                "action":{"type":"search","query":"rust async"}
            }),
            json!({"type":"web_search_call"}),
        ),
        "gpt-web-lifecycle-item-fields-done-only" => (
            json!({"type":"web_search_call","id":null,"status":null}),
            json!({
                "type":"web_search_call",
                "id":"lifecycle-web-id",
                "status":"completed",
                "action":{"type":"search","query":"rust async"}
            }),
        ),
        "gpt-web-lifecycle-item-id-conflict" => (
            json!({
                "type":"web_search_call",
                "id":"lifecycle-web-a",
                "status":"in_progress",
                "action":{"type":"search","query":"rust async"}
            }),
            json!({
                "type":"web_search_call",
                "id":"lifecycle-web-b",
                "status":"completed",
                "action":{"type":"search","query":"rust async"}
            }),
        ),
        "gpt-web-lifecycle-item-action-conflict" => (
            json!({
                "type":"web_search_call",
                "status":"in_progress",
                "action":{"type":"search","query":"rust async"}
            }),
            json!({
                "type":"web_search_call",
                "status":"completed",
                "action":{"type":"search","query":"different"}
            }),
        ),
        _ => return None,
    };
    Some(sse_response(render_sse(&[
        (
            "response.created",
            json!({
                "type":"response.created",
                "sequence_number":0,
                "response":{"id":"resp_web_partial","model":model,"output":[]}
            }),
        ),
        (
            "response.output_item.added",
            json!({
                "type":"response.output_item.added",
                "sequence_number":1,
                "output_index":0,
                "item":added
            }),
        ),
        (
            "response.output_item.done",
            json!({
                "type":"response.output_item.done",
                "sequence_number":2,
                "output_index":0,
                "item":done
            }),
        ),
        (
            "response.completed",
            json!({
                "type":"response.completed",
                "sequence_number":3,
                "response":{
                    "id":"resp_web_partial",
                    "usage":{"input_tokens":6,"output_tokens":4,"total_tokens":10}
                }
            }),
        ),
    ])))
}

fn web_search_partial_terminal_fixture(model: &str) -> Option<Response> {
    if let Some(response) = web_search_annotation_fixture(model) {
        return Some(response);
    }
    if let Some(response) = web_search_authority_fixture(model) {
        return Some(response);
    }
    if let Some(response) = web_search_item_lifecycle_fixture(model) {
        return Some(response);
    }
    let usage = json!({
        "input_tokens":6,
        "input_tokens_details":{"cached_tokens":1},
        "output_tokens":4,
        "output_tokens_details":{"reasoning_tokens":1},
        "total_tokens":10
    });
    let created = (
        "response.created",
        json!({
            "type":"response.created",
            "sequence_number":0,
            "response":{
                "id":"resp_web_partial",
                "object":"response",
                "created_at":1,
                "model":model,
                "status":"in_progress",
                "output":[],
                "output_text":null,
                "usage":usage.clone(),
                "metadata":{"snapshot":"matching"}
            }
        }),
    );
    let web_search_item = json!({
        "type":"web_search_call",
        "id":"web-search-item",
        "status":"completed",
        "action":{"type":"search","query":"rust async"}
    });
    let message_item = json!({
        "type":"message",
        "id":"web-message",
        "role":"assistant",
        "status":"completed",
        "content":[{
            "type":"output_text",
            "text":"Grounded answer.",
            "annotations":[{
                "type":"url_citation",
                "url":"https://example.test/source",
                "title":"Source",
                "page_age":null,
                "ignored_extension":{"side":"created"}
            }]
        }]
    });
    let output_events = vec![
        (
            "response.output_item.done",
            json!({
                "type":"response.output_item.done",
                "sequence_number":1,
                "output_index":0,
                "item":web_search_item.clone()
            }),
        ),
        (
            "response.output_item.added",
            json!({
                "type":"response.output_item.added",
                "sequence_number":2,
                "output_index":1,
                "item":{
                    "type":"message",
                    "id":"web-message",
                    "role":"assistant",
                    "status":"in_progress",
                    "content":[]
                }
            }),
        ),
        (
            "response.output_text.delta",
            json!({
                "type":"response.output_text.delta",
                "sequence_number":3,
                "output_index":1,
                "content_index":0,
                "item_id":"web-message",
                "delta":"Grounded "
            }),
        ),
        (
            "response.output_text.annotation.added",
            json!({
                "type":"response.output_text.annotation.added",
                "sequence_number":4,
                "output_index":1,
                "content_index":0,
                "item_id":"web-message",
                "annotation":{
                    "type":"url_citation",
                    "url":"https://example.test/source",
                    "title":"Source",
                    "ignored_extension":{"side":"terminal"}
                }
            }),
        ),
        (
            "response.output_text.done",
            json!({
                "type":"response.output_text.done",
                "sequence_number":5,
                "output_index":1,
                "content_index":0,
                "item_id":"web-message",
                "text":"Grounded answer."
            }),
        ),
        (
            "response.output_item.done",
            json!({
                "type":"response.output_item.done",
                "sequence_number":6,
                "output_index":1,
                "item":message_item.clone()
            }),
        ),
    ];

    let events = match model {
        "gpt-web-partial-completed" => {
            let mut events = vec![created];
            events.extend(output_events);
            events.push((
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":7,
                    "response":{
                        "id":"resp_web_partial",
                        "status":null,
                        "usage":usage,
                        "metadata":{"snapshot":"matching"}
                    }
                }),
            ));
            events
        }
        "gpt-web-created-lifecycle-equivalent" => {
            let message_added = json!({
                "type":"message",
                "id":"web-message",
                "role":"assistant",
                "status":"in_progress",
                "content":[]
            });
            let mut events = vec![(
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "object":"response",
                        "model":model,
                        "status":"in_progress",
                        "output":[web_search_item.clone(),message_added],
                        "usage":usage.clone()
                    }
                }),
            )];
            events.extend(output_events);
            events.push((
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":7,
                    "response":{
                        "id":"resp_web_partial",
                        "output":[web_search_item,message_item],
                        "usage":usage
                    }
                }),
            ));
            events
        }
        "gpt-web-terminal-output-completed" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":usage,
                        "output":[web_search_item,message_item]
                    }
                }),
            ),
        ],
        "gpt-web-created-only-completed" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "object":"response",
                        "model":model,
                        "status":"in_progress",
                        "output":[web_search_item,message_item],
                        "output_text":"Grounded answer.",
                        "usage":usage,
                        "metadata":{"snapshot":"created-only"}
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_web_partial","status":null}
                }),
            ),
        ],
        "gpt-web-terminal-only-completed" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "object":null,
                        "model":null,
                        "status":null,
                        "output":null,
                        "output_text":null,
                        "usage":null,
                        "metadata":null
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "object":"response",
                        "model":model,
                        "status":null,
                        "output":[web_search_item,message_item],
                        "output_text":"Grounded answer.",
                        "usage":usage,
                        "metadata":{"snapshot":"terminal-only"}
                    }
                }),
            ),
        ],
        "gpt-web-matching-duplicate-completed" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "object":"response",
                        "model":model,
                        "status":"in_progress",
                        "output":[web_search_item.clone(),message_item.clone()],
                        "output_text":"Grounded answer.",
                        "usage":usage.clone(),
                        "metadata":{"snapshot":"duplicate"}
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "object":"response",
                        "model":model,
                        "status":"completed",
                        "output":[web_search_item,message_item],
                        "output_text":"Grounded answer.",
                        "usage":usage,
                        "metadata":{"snapshot":"duplicate"}
                    }
                }),
            ),
        ],
        "gpt-web-usage-null-details-match" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "model":model,
                        "status":null,
                        "output":[],
                        "usage":{
                            "input_tokens":6,
                            "input_tokens_details":null,
                            "output_tokens":4,
                            "output_tokens_details":null,
                            "total_tokens":10
                        }
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "status":null,
                        "output":[web_search_item,message_item],
                        "usage":{"input_tokens":6,"output_tokens":4,"total_tokens":10}
                    }
                }),
            ),
        ],
        "gpt-web-output-null-optional-equivalent" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "model":model,
                        "status":null,
                        "output":[
                            {
                                "type":"web_search_call",
                                "id":null,
                                "status":null,
                                "ignored_extension":{"side":"created"},
                                "action":{"type":"search","query":"rust async"}
                            },
                            {
                                "type":"message",
                                "id":null,
                                "role":"assistant",
                                "status":null,
                                "ignored_extension":{"side":"created"},
                                "content":[{
                                    "type":"output_text",
                                    "text":"Grounded answer.",
                                    "annotations":[{
                                        "type":"url_citation",
                                        "url":"https://example.test/source",
                                        "title":"Source",
                                        "page_age":null,
                                        "ignored_extension":{"side":"created"}
                                    }]
                                }]
                            }
                        ],
                        "usage":usage.clone()
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "status":null,
                        "output":[
                            {
                                "type":"web_search_call",
                                "ignored_extension":{"side":"terminal"},
                                "action":{"type":"search","query":"rust async"}
                            },
                            {
                                "type":"message",
                                "role":"assistant",
                                "ignored_extension":{"side":"terminal"},
                                "content":[{
                                    "type":"output_text",
                                    "text":"Grounded answer.",
                                    "annotations":[{
                                        "type":"url_citation",
                                        "url":"https://example.test/source",
                                        "title":"Source",
                                        "ignored_extension":{"side":"terminal"}
                                    }]
                                }]
                            }
                        ],
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-item-id-created-only" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "model":model,
                        "output":[web_search_item.clone(),message_item.clone()],
                        "usage":usage.clone()
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "output":[
                            {
                                "type":"web_search_call",
                                "status":"completed",
                                "action":{"type":"search","query":"rust async"}
                            },
                            {
                                "type":"message",
                                "role":"assistant",
                                "status":"completed",
                                "content":[{
                                    "type":"output_text",
                                    "text":"Grounded answer.",
                                    "annotations":null
                                }]
                            }
                        ],
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-item-id-terminal-only" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "model":model,
                        "output":[
                            {
                                "type":"web_search_call",
                                "status":null,
                                "action":{"type":"search","query":"rust async"}
                            },
                            {
                                "type":"message",
                                "role":"assistant",
                                "status":null,
                                "content":message_item["content"].clone()
                            }
                        ],
                        "usage":usage.clone()
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "output":[web_search_item,message_item],
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-item-id-conflict" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "model":model,
                        "output":[web_search_item.clone(),message_item.clone()],
                        "usage":usage.clone()
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "output":[
                            {
                                "type":"web_search_call",
                                "id":"different-web-id",
                                "status":"completed",
                                "action":{"type":"search","query":"rust async"}
                            },
                            message_item
                        ],
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-message-id-conflict" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "model":model,
                        "output":[web_search_item.clone(),message_item.clone()],
                        "usage":usage.clone()
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "output":[
                            web_search_item,
                            {
                                "type":"message",
                                "id":"different-message-id",
                                "role":"assistant",
                                "status":"completed",
                                "content":message_item["content"].clone()
                            }
                        ],
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-item-status-conflict" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "model":model,
                        "output":[web_search_item.clone(),message_item.clone()],
                        "usage":usage.clone()
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "output":[
                            {
                                "type":"web_search_call",
                                "id":"web-search-item",
                                "status":"incomplete",
                                "action":{"type":"search","query":"rust async"}
                            },
                            message_item
                        ],
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-terminal-id-conflict" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_web_other","usage":usage}
                }),
            ),
        ],
        "gpt-web-terminal-model-conflict" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "model":"different-model",
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-terminal-object-conflict" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "object":"different",
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-terminal-status-conflict" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "status":"incomplete",
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-terminal-usage-conflict" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":{
                            "input_tokens":7,
                            "input_tokens_details":{"cached_tokens":1},
                            "output_tokens":4,
                            "output_tokens_details":{"reasoning_tokens":1},
                            "total_tokens":11
                        }
                    }
                }),
            ),
        ],
        "gpt-web-terminal-cached-usage-conflict" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":{
                            "input_tokens":6,
                            "input_tokens_details":{"cached_tokens":2},
                            "output_tokens":4,
                            "output_tokens_details":{"reasoning_tokens":1},
                            "total_tokens":10
                        }
                    }
                }),
            ),
        ],
        "gpt-web-terminal-reasoning-usage-conflict" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":{
                            "input_tokens":6,
                            "input_tokens_details":{"cached_tokens":1},
                            "output_tokens":4,
                            "output_tokens_details":{"reasoning_tokens":2},
                            "total_tokens":10
                        }
                    }
                }),
            ),
        ],
        "gpt-web-terminal-metadata-conflict" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":usage,
                        "metadata":{"snapshot":"different"}
                    }
                }),
            ),
        ],
        "gpt-web-terminal-output-conflict" => vec![
            (
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_web_partial",
                        "object":"response",
                        "model":model,
                        "status":"in_progress",
                        "output":[web_search_item.clone(),message_item.clone()],
                        "usage":usage.clone()
                    }
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "output":[
                            web_search_item,
                            {
                                "type":"message",
                                "id":"web-message",
                                "role":"assistant",
                                "status":"completed",
                                "content":[{
                                    "type":"output_text",
                                    "text":"Different answer.",
                                    "annotations":[]
                                }]
                            }
                        ],
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-terminal-lifecycle-output-conflict" => {
            let mut events = vec![created];
            events.extend(output_events);
            events.push((
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":7,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":usage,
                        "output":[web_search_item]
                    }
                }),
            ));
            events
        }
        "gpt-web-terminal-output-malformed" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":usage,
                        "output":[{
                            "type":"message",
                            "role":"assistant",
                            "content":[{"type":"output_text","text":42}]
                        }]
                    }
                }),
            ),
        ],
        "gpt-web-unsupported-raw-output" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":usage,
                        "output":[{
                            "type":"image_generation_call",
                            "id":"image-web",
                            "status":"completed",
                            "result":"image-data"
                        }]
                    }
                }),
            ),
        ],
        "gpt-web-unrepresentable-search-call" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":usage,
                        "output":[{
                            "type":"web_search_call",
                            "id":"web-multiple",
                            "status":"completed",
                            "action":{"type":"search","queries":["one","two"]}
                        }]
                    }
                }),
            ),
        ],
        "gpt-web-incomplete-search-call" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":usage,
                        "output":[{
                            "type":"web_search_call",
                            "id":"web-incomplete",
                            "status":"in_progress",
                            "action":{"type":"search","query":"rust"}
                        }]
                    }
                }),
            ),
        ],
        "gpt-web-empty-query-entry" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "usage":usage,
                        "output":[{
                            "type":"web_search_call",
                            "id":"web-empty-query",
                            "status":"completed",
                            "action":{"type":"search","queries":["","rust"]}
                        }]
                    }
                }),
            ),
        ],
        "gpt-web-late-text-conflict" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "id":"web-message",
                        "role":"assistant",
                        "status":"in_progress",
                        "content":[]
                    }
                }),
            ),
            (
                "response.output_text.delta",
                json!({
                    "type":"response.output_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "item_id":"web-message",
                    "delta":"conflicting"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":3,
                    "output_index":0,
                    "item":message_item
                }),
            ),
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":4,
                    "response":{"id":"resp_web_partial","usage":usage}
                }),
            ),
        ],
        "gpt-web-delta-after-item-done" => vec![
            created,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":1,
                    "output_index":0,
                    "item":message_item
                }),
            ),
            (
                "response.output_text.delta",
                json!({
                    "type":"response.output_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "item_id":"web-message",
                    "delta":"late"
                }),
            ),
        ],
        "gpt-web-terminal-failed" => vec![
            created,
            (
                "response.failed",
                json!({
                    "type":"response.failed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "error":{"message":"web fixture failed"}
                    }
                }),
            ),
        ],
        "gpt-web-terminal-incomplete" => vec![
            created,
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_web_partial",
                        "incomplete_details":{"reason":"max_output_tokens"},
                        "usage":usage
                    }
                }),
            ),
        ],
        "gpt-web-later-terminal" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_web_partial","usage":usage}
                }),
            ),
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":2,
                    "response":{
                        "id":"resp_web_partial",
                        "incomplete_details":{"reason":"max_output_tokens"}
                    }
                }),
            ),
        ],
        _ => return None,
    };
    Some(sse_response(render_sse(&events)))
}

fn reasoning_lifecycle_stream_fixture(model: &str) -> Option<Response> {
    let created = (
        "response.created",
        json!({
            "type":"response.created",
            "sequence_number":0,
            "response":{
                "id":"resp_lifecycle_fixture",
                "object":"response",
                "created_at":1,
                "status":"in_progress",
                "model":model,
                "output":[]
            }
        }),
    );
    let added = (
        "response.output_item.added",
        json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"reasoning","id":"reasoning-life","summary":[]}
        }),
    );
    let done_item = json!({
        "type":"reasoning",
        "id":"reasoning-life",
        "encrypted_content":"encrypted-life",
        "summary":[{"type":"summary_text","text":"once"}]
    });
    let done = (
        "response.output_item.done",
        json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":done_item
        }),
    );
    let completed = (
        "response.completed",
        json!({
            "type":"response.completed",
            "sequence_number":99,
            "response":{
                "id":"resp_lifecycle_fixture",
                "object":"response",
                "created_at":1,
                "model":model,
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
            }
        }),
    );

    let events = match model {
        "gpt-reasoning-summary-content" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"reasoning","id":"reasoning-content","summary":[]}
                }),
            ),
            (
                "response.reasoning_summary_part.added",
                json!({
                    "type":"response.reasoning_summary_part.added",
                    "output_index":0,
                    "summary_index":0
                }),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "item_id":"reasoning-content",
                    "output_index":0,
                    "summary_index":0,
                    "delta":" summary "
                }),
            ),
            (
                "response.reasoning_summary_text.done",
                json!({
                    "type":"response.reasoning_summary_text.done",
                    "item_id":"reasoning-content",
                    "output_index":0,
                    "summary_index":0,
                    "text":" summary "
                }),
            ),
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "output_index":0,
                    "content_index":0,
                    "delta":" raw"
                }),
            ),
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "output_index":0,
                    "content_index":0,
                    "delta":" content "
                }),
            ),
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "output_index":0,
                    "content_index":1,
                    "delta":"second"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-content",
                        "encrypted_content":"encrypted-content",
                        "summary":[{"type":"summary_text","text":" summary "}]
                    }
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-missing-reasoning-done" => vec![
            created,
            added,
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "output_index":0,
                    "content_index":0,
                    "delta":"unfinished"
                }),
            ),
            completed,
            done,
        ],
        "gpt-lifecycle-duplicate-reasoning-done" => {
            vec![created, added.clone(), done.clone(), done, completed]
        }
        "gpt-lifecycle-conflicting-reasoning-done" => vec![
            created,
            added,
            done,
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-life",
                        "encrypted_content":"encrypted-life",
                        "summary":[{"type":"summary_text","text":"different"}]
                    }
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-summary-before-added" => vec![
            created,
            (
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "output_index":0,
                    "summary_index":0,
                    "delta":"early"
                }),
            ),
            added,
            done,
            completed,
        ],
        "gpt-lifecycle-summary-after-done" => vec![
            created,
            added,
            done,
            (
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "item_id":"reasoning-life",
                    "output_index":0,
                    "summary_index":0,
                    "delta":"late"
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-summary-delta-after-text-done" => vec![
            created,
            added,
            (
                "response.reasoning_summary_text.done",
                json!({
                    "type":"response.reasoning_summary_text.done",
                    "item_id":"reasoning-life",
                    "output_index":0,
                    "summary_index":0,
                    "text":"done"
                }),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "item_id":"reasoning-life",
                    "output_index":0,
                    "summary_index":0,
                    "delta":"late"
                }),
            ),
            done,
            completed,
        ],
        "gpt-lifecycle-duplicate-added" => {
            vec![created, added.clone(), added, done, completed]
        }
        "gpt-lifecycle-summary-done-without-part" => vec![
            created,
            added,
            (
                "response.reasoning_summary_text.done",
                json!({
                    "type":"response.reasoning_summary_text.done",
                    "item_id":"reasoning-life",
                    "output_index":0,
                    "summary_index":0,
                    "text":"buffered-authoritative"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-life",
                        "encrypted_content":"encrypted-life",
                        "summary":[]
                    }
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-late-reasoning-id" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"reasoning","summary":[]}
                }),
            ),
            (
                "response.reasoning_summary_text.done",
                json!({
                    "type":"response.reasoning_summary_text.done",
                    "item_id":"reasoning-late",
                    "output_index":0,
                    "summary_index":0,
                    "text":"late-id"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-late",
                        "encrypted_content":"encrypted-late",
                        "summary":[]
                    }
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-duplicate-delta-sequence" => {
            let delta = (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "delta":"sequence-once"
                }),
            );
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":{"type":"reasoning","id":"reasoning-life","summary":[]}
                    }),
                ),
                delta.clone(),
                delta,
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":3,
                        "output_index":0,
                        "item":{
                            "type":"reasoning",
                            "id":"reasoning-life",
                            "encrypted_content":"encrypted-life",
                            "summary":[]
                        }
                    }),
                ),
                completed,
            ]
        }
        "gpt-lifecycle-empty-content-part" => vec![
            created,
            added,
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "output_index":0,
                    "content_index":0,
                    "delta":""
                }),
            ),
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "output_index":0,
                    "content_index":1,
                    "delta":"second"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-life",
                        "encrypted_content":"encrypted-life",
                        "summary":[]
                    }
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-conflicting-delta-sequence" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"reasoning","id":"reasoning-life","summary":[]}
                }),
            ),
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "delta":"first"
                }),
            ),
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "content_index":0,
                    "delta":"conflict"
                }),
            ),
            done,
            completed,
        ],
        "gpt-lifecycle-out-of-order-sequence" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"type":"reasoning","id":"reasoning-life","summary":[]}
                }),
            ),
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "sequence_number":3,
                    "output_index":0,
                    "content_index":0,
                    "delta":"newer"
                }),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "sequence_number":2,
                    "output_index":0,
                    "summary_index":0,
                    "delta":"older"
                }),
            ),
            done,
            completed,
        ],
        "gpt-lifecycle-reused-reasoning-id" => vec![
            created,
            added,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":1,
                    "item":{"type":"reasoning","id":"reasoning-life","summary":[]}
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-sparse-summary-index" => vec![
            created,
            added,
            (
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "output_index":0,
                    "summary_index":1,
                    "delta":"missing zero"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-life",
                        "encrypted_content":"encrypted-life",
                        "summary":[]
                    }
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-sparse-content-index" => vec![
            created,
            added,
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "output_index":0,
                    "content_index":1,
                    "delta":"missing zero"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-life",
                        "encrypted_content":"encrypted-life",
                        "summary":[]
                    }
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-terminal-untracked-output" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{
                        "id":"resp_lifecycle_fixture",
                        "status":"completed",
                        "model":model,
                        "output":[{
                            "type":"reasoning",
                            "id":"untracked",
                            "encrypted_content":"opaque",
                            "summary":[{"type":"summary_text","text":"lost"}]
                        }],
                        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                    }
                }),
            ),
        ],
        "gpt-lifecycle-terminal-omitted-output" => vec![
            created,
            added,
            done,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":99,
                    "response":{
                        "id":"resp_lifecycle_fixture",
                        "status":"completed",
                        "model":model,
                        "output":[],
                        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                    }
                }),
            ),
        ],
        "gpt-lifecycle-terminal-mismatched-output" => vec![
            created,
            added,
            done,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":99,
                    "response":{
                        "id":"resp_lifecycle_fixture",
                        "status":"completed",
                        "model":model,
                        "output":[{
                            "type":"reasoning",
                            "id":"reasoning-life",
                            "encrypted_content":"encrypted-life",
                            "summary":[{"type":"summary_text","text":"different"}]
                        }],
                        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                    }
                }),
            ),
        ],
        "gpt-lifecycle-standalone-message-done" => {
            let item = json!({
                "type":"message",
                "id":"standalone-message",
                "role":"assistant",
                "status":"completed",
                "content":[{"type":"output_text","text":"standalone text","annotations":[]}]
            });
            vec![
                created,
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                (
                    "response.completed",
                    json!({
                        "type":"response.completed",
                        "sequence_number":99,
                        "response":{
                            "id":"resp_lifecycle_fixture",
                            "status":"completed",
                            "model":model,
                            "output":[item],
                            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                        }
                    }),
                ),
            ]
        }
        "gpt-lifecycle-standalone-function-done" => {
            let item = json!({
                "type":"function_call",
                "id":"standalone-function",
                "call_id":"standalone-call",
                "name":"read",
                "arguments":"{\"path\":\"standalone\"}",
                "status":"completed"
            });
            vec![
                created,
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                (
                    "response.completed",
                    json!({
                        "type":"response.completed",
                        "sequence_number":99,
                        "response":{
                            "id":"resp_lifecycle_fixture",
                            "status":"completed",
                            "model":model,
                            "output":[item],
                            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                        }
                    }),
                ),
            ]
        }
        "gpt-lifecycle-partial-message-completion" => {
            let item = json!({
                "type":"message",
                "id":"partial-message",
                "role":"assistant",
                "status":"completed",
                "content":[{"type":"output_text","text":"AB","annotations":[]}]
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{
                            "type":"message",
                            "id":"partial-message",
                            "role":"assistant",
                            "status":"in_progress",
                            "content":[]
                        }
                    }),
                ),
                (
                    "response.output_text.delta",
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "content_index":0,
                        "delta":"A"
                    }),
                ),
                (
                    "response.output_text.done",
                    json!({
                        "type":"response.output_text.done",
                        "output_index":0,
                        "content_index":0,
                        "text":"AB"
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                (
                    "response.completed",
                    json!({
                        "type":"response.completed",
                        "sequence_number":99,
                        "response":{
                            "id":"resp_lifecycle_fixture",
                            "status":"completed",
                            "model":model,
                            "output":[item],
                            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                        }
                    }),
                ),
            ]
        }
        "gpt-lifecycle-partial-function-completion" => {
            let item = json!({
                "type":"function_call",
                "id":"partial-function",
                "call_id":"partial-call",
                "name":"read",
                "arguments":"{\"path\":\"x\"}",
                "status":"completed"
            });
            vec![
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{
                            "type":"function_call",
                            "id":"partial-function",
                            "call_id":"partial-call",
                            "name":"read",
                            "arguments":"",
                            "status":"in_progress"
                        }
                    }),
                ),
                (
                    "response.function_call_arguments.delta",
                    json!({
                        "type":"response.function_call_arguments.delta",
                        "output_index":0,
                        "delta":"{\"path\":\""
                    }),
                ),
                (
                    "response.function_call_arguments.done",
                    json!({
                        "type":"response.function_call_arguments.done",
                        "output_index":0,
                        "arguments":"{\"path\":\"x\"}"
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "output_index":0,
                        "item":item.clone()
                    }),
                ),
                (
                    "response.completed",
                    json!({
                        "type":"response.completed",
                        "sequence_number":99,
                        "response":{
                            "id":"resp_lifecycle_fixture",
                            "status":"completed",
                            "model":model,
                            "output":[item],
                            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                        }
                    }),
                ),
            ]
        }
        "gpt-lifecycle-conflicting-message-completion" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "id":"conflicting-message",
                        "role":"assistant",
                        "content":[]
                    }
                }),
            ),
            (
                "response.output_text.delta",
                json!({
                    "type":"response.output_text.delta",
                    "output_index":0,
                    "content_index":0,
                    "delta":"A"
                }),
            ),
            (
                "response.output_text.done",
                json!({
                    "type":"response.output_text.done",
                    "output_index":0,
                    "content_index":0,
                    "text":"X"
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-conflicting-function-completion" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"conflicting-function",
                        "call_id":"conflicting-call",
                        "name":"read",
                        "arguments":""
                    }
                }),
            ),
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "output_index":0,
                    "delta":"{\"a\":"
                }),
            ),
            (
                "response.function_call_arguments.done",
                json!({
                    "type":"response.function_call_arguments.done",
                    "output_index":0,
                    "arguments":"{\"b\":1}"
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-missing-reasoning-delta" => vec![
            created,
            added,
            (
                "response.reasoning_text.delta",
                json!({
                    "type":"response.reasoning_text.delta",
                    "output_index":0,
                    "content_index":0
                }),
            ),
            done,
            completed,
        ],
        "gpt-lifecycle-missing-summary-done-text" => vec![
            created,
            added,
            (
                "response.reasoning_summary_text.done",
                json!({
                    "type":"response.reasoning_summary_text.done",
                    "output_index":0,
                    "summary_index":0
                }),
            ),
            done,
            completed,
        ],
        "gpt-lifecycle-conflicting-function-identity" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-identity",
                        "call_id":"call-a",
                        "name":"read",
                        "arguments":""
                    }
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-identity",
                        "call_id":"call-b",
                        "name":"write",
                        "arguments":"{}"
                    }
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-missing-terminal-response" => vec![
            created,
            (
                "response.completed",
                json!({"type":"response.completed","sequence_number":1}),
            ),
        ],
        "gpt-lifecycle-terminal-status-mismatch" => vec![
            created,
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":1,
                    "response":{"id":"resp_lifecycle_fixture","status":"incomplete"}
                }),
            ),
        ],
        "gpt-lifecycle-missing-function-done" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{
                        "type":"function_call",
                        "id":"function-life",
                        "call_id":"call-life",
                        "name":"read",
                        "arguments":""
                    }
                }),
            ),
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "output_index":0,
                    "delta":"{\"path\":\"unfinished\"}"
                }),
            ),
            completed,
        ],
        "gpt-lifecycle-missing-message-done" => vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{
                        "type":"message",
                        "id":"message-life",
                        "role":"assistant",
                        "content":[]
                    }
                }),
            ),
            (
                "response.output_text.delta",
                json!({
                    "type":"response.output_text.delta",
                    "output_index":0,
                    "content_index":0,
                    "delta":"unfinished"
                }),
            ),
            completed,
        ],
        _ => return None,
    };
    Some(sse_response(render_sse(&events)))
}

fn responses_state_budget_stream_fixture(model: &str) -> Option<Response> {
    let text_mode = model.ends_with("responses-state-exact")
        || model.ends_with("responses-state-over")
        || model.ends_with("responses-state-utf8-exact");
    let function_mode = model.ends_with("responses-function-state-exact")
        || model.ends_with("responses-function-state-over");
    let mixed_mode =
        model.ends_with("responses-mixed-budget") || model.ends_with("responses-mixed-budget-over");
    if !text_mode && !function_mode && !mixed_mode {
        return None;
    }

    let created = (
        "response.created",
        json!({
            "type":"response.created",
            "response":{
                "id":"resp_budget",
                "object":"response",
                "created_at":1,
                "status":"in_progress",
                "model":model
            }
        }),
    );
    let terminal = (
        "response.completed",
        json!({
            "type":"response.completed",
            "response":{
                "id":"resp_budget",
                "status":"completed",
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
            }
        }),
    );

    if text_mode {
        let utf8 = model.ends_with("utf8-exact");
        let over = usize::from(model.ends_with("state-over"));
        let mut remaining = copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES + over;
        let mut events = vec![created];
        let mut index = 0usize;
        while remaining > 0 {
            let id = format!("message-{index}");
            let filler_bytes = remaining.min(BUDGET_FIXTURE_FRAGMENT_BYTES);
            let text = if utf8 {
                utf8_payload_fragments(filler_bytes)
                    .into_iter()
                    .collect::<String>()
            } else {
                "t".repeat(filler_bytes)
            };
            let added = json!({
                "type":"message",
                "id":id,
                "role":"assistant",
                "content":[]
            });
            let done = json!({
                "type":"message",
                "id":format!("message-{index}"),
                "role":"assistant",
                "content":[{"type":"output_text","text":text}]
            });
            events.extend([
                (
                    "response.output_item.added",
                    json!({"type":"response.output_item.added","output_index":index,"item":added}),
                ),
                (
                    "response.output_text.delta",
                    json!({"type":"response.output_text.delta","output_index":index,"content_index":0,"delta":text}),
                ),
                (
                    "response.output_text.done",
                    json!({"type":"response.output_text.done","output_index":index,"content_index":0,"text":text}),
                ),
                (
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":index,"item":done}),
                ),
            ]);
            remaining -= filler_bytes;
            index += 1;
        }
        events.push(terminal);
        return Some(sse_response(render_sse(&events)));
    }

    if function_mode {
        let over = usize::from(model.ends_with("state-over"));
        const COUNT: usize = 64;
        let mut retained_metadata = "resp_budget".len();
        for index in 0..COUNT {
            let id = format!("function-{index}");
            let call_id = format!("call-{index}");
            let added = json!({
                "type":"function_call",
                "id":id,
                "call_id":call_id,
                "name":"actual",
                "arguments":""
            });
            retained_metadata += serde_json::to_vec(&added).unwrap().len()
                + "function_call".len()
                + id.len()
                + id.len()
                + call_id.len()
                + "actual".len();
        }
        let argument_total = copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
            .checked_add(over)
            .and_then(|limit| limit.checked_sub(retained_metadata))
            .expect("function retained-budget fixture metadata fits");
        assert!(argument_total >= COUNT * 8);
        let base_argument_len = argument_total / COUNT;
        let argument_remainder = argument_total % COUNT;
        let mut added_events = Vec::new();
        let mut argument_events = Vec::new();
        let mut done_events = Vec::new();
        for index in 0..COUNT {
            let id = format!("function-{index}");
            let call_id = format!("call-{index}");
            let argument_bytes = base_argument_len + usize::from(index < argument_remainder);
            let value_bytes = argument_bytes - 8;
            let arguments = format!(r#"{{"v":"{}"}}"#, "a".repeat(value_bytes));
            debug_assert_eq!(arguments.len(), argument_bytes);
            let streamed_prefix = &arguments[..arguments.len() - 1];
            let added = json!({
                "type":"function_call",
                "id":id,
                "call_id":call_id,
                "name":"actual",
                "arguments":""
            });
            let done = json!({
                "type":"function_call",
                "id":format!("function-{index}"),
                "call_id":format!("call-{index}"),
                "name":"actual",
                "arguments":arguments
            });
            added_events.push((
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":index,"item":added}),
            ));
            argument_events.extend([
                (
                    "response.function_call_arguments.delta",
                    json!({"type":"response.function_call_arguments.delta","output_index":index,"delta":streamed_prefix}),
                ),
                (
                    "response.function_call_arguments.done",
                    json!({"type":"response.function_call_arguments.done","output_index":index,"arguments":arguments}),
                ),
            ]);
            done_events.push((
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":index,"item":done}),
            ));
        }
        let mut events = vec![created];
        events.extend(added_events);
        events.extend(argument_events);
        events.extend(done_events);
        events.push(terminal);
        return Some(sse_response(render_sse(&events)));
    }

    const REASONING_TEXT: &str = "inspect";
    const MIXED_ARGUMENTS: &str = "{\"value\":1}";
    let signature =
        copilot_api::routes::messages::responses_translation::encode_reasoning_signature(
            Some("enc"),
            Some("reasoning-mixed"),
        );
    let fixed_output_bytes = REASONING_TEXT.len()
        + signature.len()
        + (0..2)
            .map(|index| {
                format!("mixed-call-{index}").len() + "actual".len() + MIXED_ARGUMENTS.len()
            })
            .sum::<usize>();
    let mixed_over = usize::from(model.ends_with("mixed-budget-over"));
    let mut remaining_text = copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
        .checked_add(mixed_over)
        .and_then(|limit| limit.checked_sub(fixed_output_bytes))
        .expect("mixed Responses budget fixture fits");
    let mut events = vec![created];
    let mut output_index = 0usize;
    while remaining_text > 0 {
        let text_bytes = remaining_text.min(BUDGET_FIXTURE_FRAGMENT_BYTES);
        let text = utf8_payload_fragments(text_bytes)
            .into_iter()
            .collect::<String>();
        let id = format!("mixed-message-{output_index}");
        events.extend([
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":output_index,
                    "item":{"type":"message","id":id,"role":"assistant","content":[]}
                }),
            ),
            (
                "response.output_text.delta",
                json!({
                    "type":"response.output_text.delta",
                    "output_index":output_index,
                    "content_index":0,
                    "delta":text
                }),
            ),
            (
                "response.output_text.done",
                json!({
                    "type":"response.output_text.done",
                    "output_index":output_index,
                    "content_index":0,
                    "text":text
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "output_index":output_index,
                    "item":{
                        "type":"message",
                        "id":format!("mixed-message-{output_index}"),
                        "role":"assistant",
                        "status":"completed",
                        "content":[{"type":"output_text","text":text}]
                    }
                }),
            ),
        ]);
        remaining_text -= text_bytes;
        output_index += 1;
    }

    let reasoning = json!({
        "type":"reasoning",
        "id":"reasoning-mixed",
        "summary":[{"type":"summary_text","text":REASONING_TEXT}],
        "encrypted_content":"enc",
        "status":"completed"
    });
    events.push((
        "response.output_item.done",
        json!({"type":"response.output_item.done","output_index":output_index,"item":reasoning}),
    ));
    output_index += 1;
    for index in 0..2 {
        events.push((
            "response.output_item.added",
            json!({
                "type":"response.output_item.added",
                "output_index":output_index + index,
                "item":{
                    "type":"function_call",
                    "id":format!("mixed-function-{index}"),
                    "call_id":format!("mixed-call-{index}"),
                    "name":"actual",
                    "arguments":""
                }
            }),
        ));
    }
    for index in 0..2 {
        events.push((
            "response.function_call_arguments.delta",
            json!({"type":"response.function_call_arguments.delta","output_index":output_index + index,"delta":"{\"value\":"}),
        ));
        events.push((
            "response.function_call_arguments.done",
            json!({"type":"response.function_call_arguments.done","output_index":output_index + index,"arguments":MIXED_ARGUMENTS}),
        ));
    }
    for index in 0..2 {
        events.push((
            "response.output_item.done",
            json!({
                "type":"response.output_item.done",
                "output_index":output_index + index,
                "item":{
                    "type":"function_call",
                    "id":format!("mixed-function-{index}"),
                    "call_id":format!("mixed-call-{index}"),
                    "name":"actual",
                    "arguments":MIXED_ARGUMENTS
                }
            }),
        ));
    }
    events.push(terminal);
    Some(sse_response(render_sse(&events)))
}

fn web_search_reconstructed_overflow_fixture(model: &str) -> Response {
    let mut result = json!({
        "id":"resp_web_budget",
        "object":"response",
        "created_at":1,
        "status":"completed",
        "model":model,
        "output":[
            {
                "type":"web_search_call",
                "status":"completed",
                "action":{"type":"search","query":"budget"}
            },
            {
                "type":"message",
                "role":"assistant",
                "status":"completed",
                "content":[{
                    "type":"output_text",
                    "text":"",
                    "annotations":[]
                }]
            }
        ],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
    });
    let base = serde_json::to_vec(&result).expect("web overflow fixture");
    let filler_bytes = copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
        .checked_sub(base.len())
        .expect("web overflow fixture base fits");
    result["output"][1]["content"][0]["text"] = json!("w".repeat(filler_bytes));
    let body = serde_json::to_vec(&result).expect("web overflow fixture body");
    assert_eq!(
        body.len(),
        copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header("content-length", body.len().to_string())
        .body(Body::from(body))
        .unwrap()
}

fn responses_fixture(body: &Value) -> Response {
    let model = body["model"].as_str().unwrap_or("gpt-fixture");
    if model.starts_with("gpt-direct-compact-") {
        return compact_fixture(body);
    }
    if model == "gpt-web-reconstructed-overflow" {
        return web_search_reconstructed_overflow_fixture(model);
    }
    match model {
        "gpt-rate-limit" => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "5"), ("x-request-id", "responses-rate-1")],
                Json(json!({
                    "error":{
                        "message":"fixture limited",
                        "type":"rate_limit_error",
                        "param":Value::Null,
                        "code":"rate_limit_exceeded",
                        "fixture_extension":true
                    }
                })),
            )
                .into_response()
        }
        "gpt-upstream-500" => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message":"fixture upstream unavailable"})),
            )
                .into_response()
        }
        "gpt-forbidden" => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error":{
                        "message":"fixture permission denied",
                        "type":"permission_error",
                        "param":Value::Null,
                        "code":"insufficient_permissions"
                    }
                })),
            )
                .into_response()
        }
        "gpt-overload" => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "1")],
                Json(json!({
                    "error":{
                        "message":"fixture overloaded",
                        "type":"server_error",
                        "param":Value::Null,
                        "code":"server_overloaded"
                    }
                })),
            )
                .into_response()
        }
        "gpt-unknown" => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error":{
                        "message":"fixture model not found",
                        "type":"invalid_request_error",
                        "param":"model",
                        "code":"model_not_found"
                    }
                })),
            )
                .into_response()
        }
        "gpt-malformed-json" => {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from("{not-json"))
                .unwrap()
        }
        "gpt-oversized-response" => {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .header(
                    "content-length",
                    (copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES + 1).to_string(),
                )
                .body(Body::empty())
                .unwrap()
        }
        _ => {}
    }

    match model {
        "gpt-direct-response-raw" => {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .header("openai-request-id", "direct-response-request")
                .header("x-unsafe-secret", "must-not-propagate")
                .body(Body::from(DIRECT_RESPONSES_SHAPE))
                .expect("direct response fixture")
        }
        "gpt-direct-response-malformed" => {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .header("x-request-id", "direct-response-malformed")
                .header("x-unsafe-secret", "must-not-propagate")
                .body(Body::from("{not-json"))
                .expect("malformed direct response")
        }
        "gpt-direct-response-wrong-shape" => {
            return Json(json!({
                "model":"gpt-direct-response-wrong-shape",
                "status":"completed",
                "output":"wrong"
            }))
            .into_response()
        }
        "gpt-direct-response-wrong-item" => {
            return Json(json!({
                "id":"resp_direct_wrong_item",
                "model":"gpt-direct-response-wrong-item",
                "status":"completed",
                "output":[{
                    "type":"function_call",
                    "name":"missing_call_id",
                    "arguments":"{}"
                }]
            }))
            .into_response()
        }
        "gpt-direct-response-wrong-usage" => {
            return Json(json!({
                "id":"resp_direct_wrong_usage",
                "model":"gpt-direct-response-wrong-usage",
                "status":"completed",
                "output":[],
                "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":99}
            }))
            .into_response()
        }
        "gpt-direct-response-oversized" => {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .header("x-request-id", "direct-response-oversized")
                .header(
                    "content-length",
                    (copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES + 1).to_string(),
                )
                .body(Body::from(vec![
                    b'x';
                    copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
                        + 1
                ]))
                .expect("oversized direct response")
        }
        "gpt-direct-response-400" => {
            return (
                StatusCode::BAD_REQUEST,
                [("x-request-id", "direct-response-400")],
                Json(json!({
                    "error":{
                        "message":"direct response invalid",
                        "type":"invalid_request_error",
                        "code":"direct_invalid",
                        "fixture_extension":{"keep":true}
                    }
                })),
            )
                .into_response()
        }
        "gpt-direct-response-500" => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [
                    ("x-request-id", "direct-response-500"),
                    ("retry-after", "2"),
                ],
                Json(json!({
                    "error":{
                        "message":"direct response unavailable",
                        "type":"server_error",
                        "code":"direct_unavailable"
                    }
                })),
            )
                .into_response()
        }
        _ => {}
    }

    if model == "gpt-native-null-shape" && body["stream"] != true {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(NATIVE_NULL_SHAPE))
            .expect("native null fixture");
    }

    if model == "gpt-native-raw-variants" {
        let output: Vec<Value> = audited_raw_output_variants()
            .into_iter()
            .map(|(_, item)| item)
            .collect();
        if body["stream"] == true {
            let mut events = vec![(
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{"id":"resp_native_raw","model":model}
                }),
            )];
            for (index, item) in output.iter().enumerate() {
                events.push((
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":index + 1,
                        "output_index":index,
                        "item":item
                    }),
                ));
            }
            events.push((
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":output.len() + 1,
                    "response":{"id":"resp_native_raw","output":output}
                }),
            ));
            return sse_response(render_sse(&events));
        }
        return Json(json!({
            "id":"resp_native_raw",
            "object":"response",
            "model":model,
            "status":"completed",
            "output":output
        }))
        .into_response();
    }

    if body["stream"] == true {
        if let Some(response) = web_search_partial_terminal_fixture(model) {
            return response;
        }
    }

    if body["stream"] != true {
        if let Some(response) = scalar_nonstream_fixture(model) {
            return response;
        }
    }

    if body["stream"] == true {
        if let Some(response) = responses_state_budget_stream_fixture(model) {
            return response;
        }
        if let Some(response) = created_output_contract_stream_fixture(model) {
            return response;
        }
        if let Some(response) = raw_output_stream_fixture(model) {
            return response;
        }
        if let Some(response) = malformed_scalar_stream_fixture(model) {
            return response;
        }
        if let Some(response) = created_usage_contract_stream_fixture(model) {
            return response;
        }
        if let Some(response) = terminal_contract_stream_fixture(model) {
            return response;
        }
        if let Some(response) = reasoning_lifecycle_stream_fixture(model) {
            return response;
        }
    }

    if let Some(reasoning) = reasoning_summary_fixture_item(model) {
        if body["stream"] == true {
            let mut events = vec![(
                "response.created",
                json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{
                        "id":"resp_reasoning_fixture",
                        "object":"response",
                        "created_at":1,
                        "status":"in_progress",
                        "model":model,
                        "output":[]
                    }
                }),
            )];
            if matches!(
                model,
                "gpt-reasoning-leading-text" | "gpt-reasoning-multiple-parts"
            ) {
                events.push((
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "output_index":0,
                        "item":{
                            "type":"reasoning",
                            "id":if model == "gpt-reasoning-leading-text" {
                                "reasoning-leading"
                            } else {
                                "reasoning-parts"
                            },
                            "summary":[]
                        }
                    }),
                ));
            }
            if model == "gpt-reasoning-leading-text" {
                events.extend([
                    (
                        "response.reasoning_summary_part.added",
                        json!({
                            "type":"response.reasoning_summary_part.added",
                            "output_index":0,
                            "summary_index":0,
                            "part":{"type":"summary_text","text":""}
                        }),
                    ),
                    (
                        "response.reasoning_summary_text.delta",
                        json!({
                            "type":"response.reasoning_summary_text.delta",
                            "item_id":"reasoning-leading",
                            "output_index":0,
                            "summary_index":0,
                            "delta":"  "
                        }),
                    ),
                    (
                        "response.reasoning_summary_text.delta",
                        json!({
                            "type":"response.reasoning_summary_text.delta",
                            "item_id":"reasoning-leading",
                            "output_index":0,
                            "summary_index":0,
                            "delta":"analysis"
                        }),
                    ),
                    (
                        "response.reasoning_summary_text.delta",
                        json!({
                            "type":"response.reasoning_summary_text.delta",
                            "item_id":"reasoning-leading",
                            "output_index":0,
                            "summary_index":0,
                            "delta":"  "
                        }),
                    ),
                    (
                        "response.reasoning_summary_text.done",
                        json!({
                            "type":"response.reasoning_summary_text.done",
                            "item_id":"reasoning-leading",
                            "output_index":0,
                            "summary_index":0,
                            "text":"  analysis  "
                        }),
                    ),
                ]);
            } else if model == "gpt-reasoning-multiple-parts" {
                events.extend([
                    (
                        "response.reasoning_summary_part.added",
                        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
                    ),
                    (
                        "response.reasoning_summary_text.delta",
                        json!({"type":"response.reasoning_summary_text.delta","item_id":"reasoning-parts","output_index":0,"summary_index":0,"delta":"  first "}),
                    ),
                    (
                        "response.reasoning_summary_text.done",
                        json!({"type":"response.reasoning_summary_text.done","item_id":"reasoning-parts","output_index":0,"summary_index":0,"text":"  first "}),
                    ),
                    (
                        "response.reasoning_summary_part.added",
                        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1,"part":{"type":"summary_text","text":""}}),
                    ),
                    // A duplicate boundary event must not duplicate the separator.
                    (
                        "response.reasoning_summary_part.added",
                        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1,"part":{"type":"summary_text","text":""}}),
                    ),
                    (
                        "response.reasoning_summary_text.done",
                        json!({"type":"response.reasoning_summary_text.done","item_id":"reasoning-parts","output_index":0,"summary_index":1,"text":""}),
                    ),
                    (
                        "response.reasoning_summary_part.added",
                        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":2,"part":{"type":"summary_text","text":""}}),
                    ),
                    (
                        "response.reasoning_summary_text.delta",
                        json!({"type":"response.reasoning_summary_text.delta","item_id":"reasoning-parts","output_index":0,"summary_index":2,"delta":"\tsecond"}),
                    ),
                    (
                        "response.reasoning_summary_text.delta",
                        json!({"type":"response.reasoning_summary_text.delta","item_id":"reasoning-parts","output_index":0,"summary_index":2,"delta":"\n"}),
                    ),
                    (
                        "response.reasoning_summary_text.done",
                        json!({"type":"response.reasoning_summary_text.done","item_id":"reasoning-parts","output_index":0,"summary_index":2,"text":"\tsecond\n"}),
                    ),
                    (
                        "response.reasoning_summary_part.added",
                        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":3,"part":{"type":"summary_text","text":""}}),
                    ),
                    (
                        "response.reasoning_summary_text.done",
                        json!({"type":"response.reasoning_summary_text.done","item_id":"reasoning-parts","output_index":0,"summary_index":3,"text":""}),
                    ),
                ]);
            }
            events.push((
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":98,
                    "output_index":0,
                    "item":reasoning.clone()
                }),
            ));
            events.push((
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":99,
                    "response":{
                        "id":"resp_reasoning_fixture",
                        "object":"response",
                        "created_at":1,
                        "status":"completed",
                        "model":model,
                        "output":[reasoning],
                        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                    }
                }),
            ));
            return sse_response(render_sse(&events));
        }
        return Json(json!({
            "id":"resp_reasoning_fixture",
            "object":"response",
            "created_at":1,
            "status":"completed",
            "model":model,
            "output":[reasoning],
            "output_text":"",
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }))
        .into_response();
    }

    if body["stream"] == true {
        let created = (
            "response.created",
            json!({
                "type":"response.created",
                "sequence_number":0,
                "response":{
                    "id":"resp_fixture",
                    "object":"response",
                    "created_at":1,
                    "status":"in_progress",
                    "model":model,
                    "output":[]
                }
            }),
        );
        if model == "gpt-cli-smoke" {
            return sse_response(render_sse(&[
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":{
                            "id":"msg_cli",
                            "type":"message",
                            "role":"assistant",
                            "status":"in_progress",
                            "content":[]
                        }
                    }),
                ),
                (
                    "response.output_text.delta",
                    json!({
                        "type":"response.output_text.delta",
                        "sequence_number":2,
                        "output_index":0,
                        "item_id":"msg_cli",
                        "content_index":0,
                        "delta":"OK"
                    }),
                ),
                (
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":3,
                        "output_index":0,
                        "item":{
                            "id":"msg_cli",
                            "type":"message",
                            "role":"assistant",
                            "status":"completed",
                            "content":[{"type":"output_text","text":"OK","annotations":[]}]
                        }
                    }),
                ),
                (
                    "response.completed",
                    json!({
                        "type":"response.completed",
                        "sequence_number":4,
                        "response":{
                            "id":"resp_fixture",
                            "object":"response",
                            "created_at":1,
                            "status":"completed",
                            "model":model,
                            "output":[{
                                "id":"msg_cli",
                                "type":"message",
                                "role":"assistant",
                                "status":"completed",
                                "content":[{"type":"output_text","text":"OK","annotations":[]}]
                            }],
                            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                        }
                    }),
                ),
            ]));
        }
        if model == "gpt-malformed-stream" {
            let mut text = render_sse(&[created]);
            text.push_str("event: response.output_text.delta\ndata: {not-json\n\n");
            return sse_response(text);
        }
        if model == "gpt-premature-eof" {
            return sse_response(render_sse(&[
                created,
                (
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "sequence_number":1,
                        "output_index":0,
                        "item":{
                            "id":"msg_partial",
                            "type":"message",
                            "role":"assistant",
                            "status":"in_progress",
                            "content":[]
                        }
                    }),
                ),
                (
                    "response.output_text.delta",
                    json!({
                        "type":"response.output_text.delta",
                        "sequence_number":2,
                        "output_index":0,
                        "item_id":"msg_partial",
                        "content_index":0,
                        "delta":"partial"
                    }),
                ),
            ]));
        }

        let terminal = if model == "gpt-incomplete" {
            (
                "response.incomplete",
                json!({
                    "type":"response.incomplete",
                    "sequence_number":11,
                    "response":{
                        "id":"resp_fixture",
                        "status":"incomplete",
                        "incomplete_details":{"reason":"max_output_tokens"},
                        "output":[
                            {
                                "id":"rs_1",
                                "type":"reasoning",
                                "summary":[{"type":"summary_text","text":"inspect"}],
                                "encrypted_content":"enc_fixture",
                                "status":"completed"
                            },
                            {
                                "id":"fc_a",
                                "type":"function_call",
                                "call_id":"call_a",
                                "name":"read",
                                "arguments":"{\"path\":\"a\"}",
                                "status":"completed"
                            },
                            {
                                "id":"fc_b",
                                "type":"function_call",
                                "call_id":"call_b",
                                "name":"read",
                                "arguments":"{\"path\":\"b\"}",
                                "status":"completed"
                            }
                        ],
                        "usage":{"input_tokens":13,"output_tokens":2,"total_tokens":15}
                    }
                }),
            )
        } else {
            (
                "response.completed",
                json!({
                    "type":"response.completed",
                    "sequence_number":12,
                    "response":{
                        "id":"resp_fixture",
                        "object":"response",
                        "created_at":1,
                        "status":"completed",
                        "model":model,
                        "output":[
                            {
                                "id":"rs_1",
                                "type":"reasoning",
                                "summary":[{"type":"summary_text","text":"inspect"}],
                                "encrypted_content":"enc_fixture",
                                "status":"completed"
                            },
                            {
                                "id":"fc_a",
                                "type":"function_call",
                                "call_id":"call_a",
                                "name":"read",
                                "arguments":"{\"path\":\"a\"}",
                                "status":"completed"
                            },
                            {
                                "id":"fc_b",
                                "type":"function_call",
                                "call_id":"call_b",
                                "name":"read",
                                "arguments":"{\"path\":\"b\"}",
                                "status":"completed"
                            }
                        ],
                        "usage":{
                            "input_tokens":13,
                            "input_tokens_details":{"cached_tokens":4},
                            "output_tokens":8,
                            "output_tokens_details":{"reasoning_tokens":3},
                            "total_tokens":21
                        }
                    }
                }),
            )
        };
        let frames = vec![
            created,
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":1,
                    "output_index":0,
                    "item":{"id":"rs_1","type":"reasoning","summary":[],"status":"in_progress"}
                }),
            ),
            (
                "response.reasoning_summary_part.added",
                json!({
                    "type":"response.reasoning_summary_part.added",
                    "sequence_number":2,
                    "output_index":0,
                    "item_id":"rs_1",
                    "summary_index":0,
                    "part":{"type":"summary_text","text":""}
                }),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "sequence_number":3,
                    "output_index":0,
                    "item_id":"rs_1",
                    "summary_index":0,
                    "delta":"inspect"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":4,
                    "output_index":0,
                    "item":{
                        "id":"rs_1",
                        "type":"reasoning",
                        "summary":[{"type":"summary_text","text":"inspect"}],
                        "encrypted_content":"enc_fixture",
                        "status":"completed"
                    }
                }),
            ),
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":5,
                    "output_index":1,
                    "item":{
                        "id":"fc_a",
                        "type":"function_call",
                        "call_id":"call_a",
                        "name":"read",
                        "arguments":"",
                        "status":"in_progress"
                    }
                }),
            ),
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "sequence_number":6,
                    "output_index":1,
                    "item_id":"fc_a",
                    "delta":"{\"path\":\"a\"}"
                }),
            ),
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "sequence_number":7,
                    "output_index":2,
                    "item":{
                        "id":"fc_b",
                        "type":"function_call",
                        "call_id":"call_b",
                        "name":"read",
                        "arguments":"",
                        "status":"in_progress"
                    }
                }),
            ),
            (
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "sequence_number":8,
                    "output_index":2,
                    "item_id":"fc_b",
                    "delta":"{\"path\":\"b\"}"
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":9,
                    "output_index":1,
                    "item":{
                        "id":"fc_a",
                        "type":"function_call",
                        "call_id":"call_a",
                        "name":"read",
                        "arguments":"{\"path\":\"a\"}",
                        "status":"completed"
                    }
                }),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "sequence_number":10,
                    "output_index":2,
                    "item":{
                        "id":"fc_b",
                        "type":"function_call",
                        "call_id":"call_b",
                        "name":"read",
                        "arguments":"{\"path\":\"b\"}",
                        "status":"completed"
                    }
                }),
            ),
            terminal,
        ];
        return sse_response(render_sse(&frames));
    }

    Json(json!({
        "id":"resp_nonstream",
        "object":"response",
        "created_at":1,
        "status":"completed",
        "model":model,
        "output":[
            {
                "id":"rs_1",
                "type":"reasoning",
                "summary":[{"type":"summary_text","text":"inspect"}],
                "encrypted_content":"enc_fixture",
                "status":"completed"
            },
            {
                "id":"fc_a",
                "type":"function_call",
                "call_id":"call_a",
                "name":"read",
                "arguments":"{\"path\":\"a\"}",
                "status":"completed"
            },
            {
                "id":"fc_b",
                "type":"function_call",
                "call_id":"call_b",
                "name":"read",
                "arguments":"{\"path\":\"b\"}",
                "status":"completed"
            }
        ],
        "parallel_tool_calls":true,
        "usage":{
            "input_tokens":13,
            "input_tokens_details":{"cached_tokens":4},
            "output_tokens":8,
            "output_tokens_details":{"reasoning_tokens":3},
            "total_tokens":21
        },
        "fixture_extension":{"preserved":true}
    }))
    .into_response()
}

fn render_sse(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(name, value)| format!("event: {name}\ndata: {value}\n\n"))
        .collect()
}

fn sse_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(body))
        .unwrap()
}

fn configure(fixture: &Fixture) {
    configure_with_web_search_model(fixture, None);
}

fn configure_direct_copilot(fixture: &Fixture) {
    configure(fixture);
    let model_ids = [
        "gpt-direct-compact-success",
        "gpt-direct-compact-malformed-json",
        "gpt-direct-compact-wrong-output",
        "gpt-direct-compact-wrong-item",
        "gpt-direct-compact-wrong-usage",
        "gpt-direct-compact-oversized",
        "gpt-direct-compact-400",
        "gpt-direct-compact-503",
        "gpt-direct-compact-headers",
        "gpt-direct-response-raw",
        "gpt-direct-response-malformed",
        "gpt-direct-response-wrong-shape",
        "gpt-direct-response-wrong-item",
        "gpt-direct-response-wrong-usage",
        "gpt-direct-response-oversized",
        "gpt-direct-response-400",
        "gpt-direct-response-500",
        "gpt-direct-responses-state-exact",
        "gpt-direct-responses-state-over",
        "gpt-direct-responses-state-utf8-exact",
        "gpt-direct-responses-function-state-exact",
        "gpt-direct-responses-function-state-over",
        "gpt-direct-responses-mixed-budget",
        "gpt-direct-responses-mixed-budget-over",
        "gpt-web-reconstructed-overflow",
        "gpt-direct-chat-extensions",
        "gpt-direct-chat-response-extras",
        "gpt-direct-chat-response-refusal",
        "gpt-direct-chat-malformed-json",
        "gpt-direct-chat-bad-choices",
        "gpt-direct-chat-429",
        "gpt-direct-chat-stream-strict",
        "gpt-direct-chat-stream-no-usage",
        "gpt-direct-chat-stream-bad-identity",
        "gpt-direct-chat-stream-tool-optionals",
        "gpt-direct-chat-stream-refusal",
        "gpt-direct-chat-stream-refusal-split",
        "gpt-direct-chat-stream-refusal-interleaved",
        "gpt-direct-chat-stream-refusal-mirror",
        "gpt-direct-chat-stream-refusal-empty",
        "gpt-direct-chat-stream-refusal-repeated",
        "gpt-direct-chat-stream-refusal-content-prefix",
        "gpt-direct-chat-stream-refusal-partial",
        "gpt-direct-chat-stream-refusal-tool-deferred",
        "gpt-direct-chat-stream-refusal-multiple-tools",
        "gpt-direct-chat-stream-budget-reasoning-exact",
        "gpt-direct-chat-stream-budget-opaque-exact",
        "gpt-direct-chat-stream-budget-mixed-utf8-exact",
        "gpt-direct-chat-stream-tier-late",
        "gpt-direct-chat-stream-bad-tier",
        "gpt-direct-chat-stream-bad-refusal",
        "gpt-direct-chat-stream-bad-refusal-conflict",
        "gpt-direct-chat-stream-bad-refusal-finish",
        "gpt-direct-chat-stream-bad-refusal-late",
        "gpt-direct-chat-stream-bad-refusal-late-finish-usage",
        "gpt-direct-chat-stream-bad-refusal-late-after-usage",
        "gpt-direct-chat-stream-bad-refusal-repeated-usage",
        "gpt-direct-chat-stream-bad-refusal-tool-incomplete",
        "gpt-direct-chat-stream-bad-refusal-tool-late",
        "gpt-direct-chat-stream-bad-refusal-tool-eof",
        "gpt-direct-chat-stream-bad-budget-reasoning-over",
        "gpt-direct-chat-stream-bad-budget-opaque-over",
        "gpt-direct-chat-stream-bad-budget-mixed-utf8-over",
    ];
    let models = ModelsResponse {
        object: "list".to_string(),
        data: model_ids
            .into_iter()
            .map(|id| Model {
                id: id.to_string(),
                name: id.to_string(),
                supported_endpoints: Some(vec![if id.starts_with("gpt-direct-chat-") {
                    "/chat/completions".to_string()
                } else {
                    "/responses".to_string()
                }]),
                ..Default::default()
            })
            .collect(),
    };
    copilot_api::libs::state::with_state_mut(|state| {
        state.provider_only = None;
        state.copilot_token = Some("direct-copilot-token".to_string());
        state.copilot_api_url = Some(format!("{}/v1", fixture.base_url));
        state.account_type = "individual".to_string();
        state.models = Some(Arc::new(models));
        state.premium_interactions = None;
    });
}

fn configure_direct_web_search(fixture: &Fixture, model: &str) {
    configure_direct_copilot(fixture);
    let mut config = (*copilot_api::libs::config::get_config()).clone();
    config.message_api_web_search_model = Some(model.to_string());
    config.use_responses_api_web_search = Some(true);
    set_cached_config_for_test(config);
}

fn configure_with_web_search_model(fixture: &Fixture, web_search_model: Option<&str>) {
    copilot_api::libs::state::with_state_mut(|state| {
        state.provider_only = Some("responses-fixture".to_string());
        state.models = None;
    });
    let models = [
        "claude-sonnet-4-6",
        "claude-malformed-stream",
        "claude-premature-eof",
        "claude-rate-limit",
    ]
    .into_iter()
    .map(|model| (model.to_string(), ModelConfig::default()))
    .collect();
    let response_models = [
        "gpt-fixture",
        "gpt-5.4",
        "gpt-rate-limit",
        "gpt-upstream-500",
        "gpt-forbidden",
        "gpt-overload",
        "gpt-unknown",
        "gpt-malformed-json",
        "gpt-oversized-response",
        "gpt-malformed-stream",
        "gpt-premature-eof",
        "gpt-incomplete",
        "gpt-cli-smoke",
        "gpt-provider-compact-success",
        "gpt-provider-compact-wrong-item",
        "gpt-provider-compact-wrong-output",
        "gpt-provider-compact-usage-malformed",
        "gpt-provider-compact-usage-inconsistent",
        "gpt-provider-compact-usage-negative",
        "gpt-provider-compact-usage-overflow",
        "gpt-provider-compact-malformed-json",
        "gpt-provider-compact-oversized",
        "gpt-provider-compact-400",
        "gpt-provider-compact-503",
        "gpt-scalar-function-valid",
        "gpt-scalar-custom-tool-valid",
        "gpt-scalar-function-whitespace-namespace-valid",
        "gpt-scalar-tool-search-valid",
        "gpt-scalar-tool-search-item-id-valid",
        "gpt-scalar-tool-search-late-id-valid",
        "gpt-scalar-tool-search-output-valid",
        "gpt-scalar-message-valid",
        "gpt-scalar-reasoning-valid",
        "gpt-scalar-compaction-valid",
        "gpt-scalar-function-added-missing",
        "gpt-scalar-custom-tool-malformed",
        "gpt-scalar-function-added-missing-call-id",
        "gpt-scalar-function-added-missing-name",
        "gpt-scalar-function-added-missing-arguments",
        "gpt-scalar-function-added-wrong",
        "gpt-scalar-function-added-wrong-name",
        "gpt-scalar-function-added-wrong-arguments",
        "gpt-scalar-function-added-empty",
        "gpt-scalar-function-added-invalid-json",
        "gpt-scalar-function-done-missing",
        "gpt-scalar-function-done-missing-call-id",
        "gpt-scalar-function-done-missing-name",
        "gpt-scalar-function-done-missing-arguments",
        "gpt-scalar-function-done-invalid-json",
        "gpt-scalar-function-done-wrong-call-id",
        "gpt-scalar-function-done-wrong-name",
        "gpt-scalar-function-done-wrong-arguments",
        "gpt-scalar-function-delta-wrong",
        "gpt-scalar-function-delta-empty",
        "gpt-scalar-function-arguments-done-invalid",
        "gpt-scalar-function-arguments-done-duplicate",
        "gpt-scalar-function-delta-after-done",
        "gpt-scalar-tool-search-conflicting-id",
        "gpt-scalar-tool-search-removed-id",
        "gpt-scalar-tool-search-unexpected-delta",
        "gpt-scalar-tool-search-empty-call-id",
        "gpt-scalar-tool-search-missing-execution",
        "gpt-scalar-tool-search-wrong",
        "gpt-scalar-tool-search-wrong-execution",
        "gpt-scalar-tool-search-wrong-status",
        "gpt-scalar-tool-search-missing-arguments",
        "gpt-scalar-tool-search-output-malformed",
        "gpt-scalar-tool-search-output-wrong-execution",
        "gpt-scalar-tool-search-output-wrong-tools",
        "gpt-scalar-message-missing",
        "gpt-scalar-message-incomplete-on-completed",
        "gpt-scalar-message-wrong-content",
        "gpt-scalar-message-wrong-role",
        "gpt-scalar-message-content-not-array",
        "gpt-scalar-message-block-malformed",
        "gpt-scalar-message-annotations-malformed",
        "gpt-scalar-message-refusal-unsupported",
        "gpt-scalar-message-image-unsupported",
        "gpt-scalar-output-annotations-late",
        "gpt-scalar-output-done-duplicate",
        "gpt-scalar-output-delta-after-done",
        "gpt-scalar-output-index-mismatch",
        "gpt-scalar-reasoning-wrong",
        "gpt-scalar-reasoning-missing-summary",
        "gpt-scalar-reasoning-wrong-id",
        "gpt-scalar-reasoning-wrong-encrypted",
        "gpt-scalar-reasoning-summary-not-array",
        "gpt-scalar-reasoning-content-not-array",
        "gpt-scalar-reasoning-summary-malformed",
        "gpt-scalar-reasoning-content-malformed",
        "gpt-scalar-reasoning-event-id-wrong",
        "gpt-scalar-reasoning-part-malformed",
        "gpt-scalar-reasoning-done-missing-item-id",
        "gpt-scalar-reasoning-summary-conflict",
        "gpt-scalar-reasoning-content-conflict",
        "gpt-scalar-compaction-missing",
        "gpt-scalar-compaction-wrong",
        "gpt-scalar-compaction-wrong-id",
        "gpt-scalar-compaction-wrong-encrypted",
        "gpt-scalar-output-index-wrong",
        "gpt-scalar-output-index-sparse",
        "gpt-scalar-output-item-id-wrong",
        "gpt-scalar-output-wrapper-id-mismatch",
        "gpt-scalar-metadata-wrong",
        "gpt-scalar-metadata-turn-id-wrong",
        "gpt-web-partial-completed",
        "gpt-web-model-requested-fallback",
        "gpt-web-usage-details-created-only",
        "gpt-web-usage-details-terminal-only",
        "gpt-web-created-usage-details-malformed",
        "gpt-web-terminal-usage-details-malformed",
        "gpt-web-reconstructed-overflow",
        "gpt-web-incomplete-details-created-only",
        "gpt-web-incomplete-details-terminal-only",
        "gpt-web-incomplete-details-matching",
        "gpt-web-incomplete-details-null-absent",
        "gpt-web-incomplete-details-conflict",
        "gpt-web-metadata-created-only",
        "gpt-web-metadata-terminal-only",
        "gpt-web-metadata-matching",
        "gpt-web-metadata-null-absent",
        "gpt-web-annotations-created-empty-terminal-absent",
        "gpt-web-annotations-created-absent-terminal-empty",
        "gpt-web-annotations-created-unknown-terminal-null",
        "gpt-web-annotations-created-null-terminal-unknown",
        "gpt-web-annotations-created-mixed-terminal-known",
        "gpt-web-annotations-created-known-terminal-mixed",
        "gpt-web-annotations-duplicate-known",
        "gpt-web-annotations-default-title",
        "gpt-web-annotations-known-extensions",
        "gpt-web-annotations-lifecycle-empty-unknown",
        "gpt-web-annotations-lifecycle-mixed-known",
        "gpt-web-annotations-conflict-known",
        "gpt-web-annotations-malformed-field",
        "gpt-web-annotations-malformed-entry",
        "gpt-web-annotations-malformed-type",
        "gpt-web-annotations-malformed-known-missing-url",
        "gpt-web-annotations-malformed-known-url",
        "gpt-web-annotations-malformed-known-title",
        "gpt-web-annotations-lifecycle-conflict-known",
        "gpt-web-created-metadata-malformed",
        "gpt-web-terminal-metadata-malformed",
        "gpt-web-created-incomplete-details-malformed",
        "gpt-web-terminal-incomplete-details-malformed",
        "gpt-web-end-turn-conflict",
        "gpt-web-output-text-conflict",
        "gpt-web-ignored-extra-conflict",
        "gpt-web-end-turn-created-only",
        "gpt-web-end-turn-terminal-only",
        "gpt-web-lifecycle-item-fields-added-only",
        "gpt-web-lifecycle-item-fields-done-only",
        "gpt-web-lifecycle-item-id-conflict",
        "gpt-web-lifecycle-item-action-conflict",
        "gpt-web-created-lifecycle-equivalent",
        "gpt-web-terminal-output-completed",
        "gpt-web-created-only-completed",
        "gpt-web-terminal-only-completed",
        "gpt-web-matching-duplicate-completed",
        "gpt-web-usage-null-details-match",
        "gpt-web-output-null-optional-equivalent",
        "gpt-web-item-id-created-only",
        "gpt-web-item-id-terminal-only",
        "gpt-web-item-id-conflict",
        "gpt-web-message-id-conflict",
        "gpt-web-item-status-conflict",
        "gpt-web-terminal-id-conflict",
        "gpt-web-terminal-model-conflict",
        "gpt-web-terminal-object-conflict",
        "gpt-web-terminal-status-conflict",
        "gpt-web-terminal-usage-conflict",
        "gpt-web-terminal-cached-usage-conflict",
        "gpt-web-terminal-reasoning-usage-conflict",
        "gpt-web-terminal-metadata-conflict",
        "gpt-web-terminal-output-conflict",
        "gpt-web-terminal-lifecycle-output-conflict",
        "gpt-web-terminal-output-malformed",
        "gpt-web-unsupported-raw-output",
        "gpt-web-unrepresentable-search-call",
        "gpt-web-incomplete-search-call",
        "gpt-web-empty-query-entry",
        "gpt-web-late-text-conflict",
        "gpt-web-delta-after-item-done",
        "gpt-web-terminal-failed",
        "gpt-web-terminal-incomplete",
        "gpt-web-later-terminal",
        "gpt-native-raw-variants",
        "gpt-native-null-shape",
        "gpt-raw-additional-tools",
        "gpt-raw-agent-message",
        "gpt-raw-local-shell-call",
        "gpt-raw-function-call-output",
        "gpt-raw-custom-tool-call-output",
        "gpt-raw-web-search-call",
        "gpt-raw-image-generation-call",
        "gpt-raw-context-compaction",
        "gpt-raw-compaction-trigger",
        "gpt-raw-future-variant",
        "gpt-contract-created-model-less",
        "gpt-contract-created-with-model",
        "gpt-contract-created-upstream-model",
        "gpt-contract-created-null-optionals",
        "gpt-contract-created-only-output",
        "gpt-contract-created-only-raw-output",
        "gpt-contract-created-output-lifecycle-match",
        "gpt-contract-created-output-conflict",
        "gpt-contract-created-empty-id",
        "gpt-contract-created-wrong-id",
        "gpt-contract-created-missing-id",
        "gpt-contract-created-empty-model",
        "gpt-contract-created-wrong-model",
        "gpt-contract-created-status-mismatch",
        "gpt-contract-created-wrong-status-type",
        "gpt-contract-created-partial-usage",
        "gpt-contract-completed-empty-id",
        "gpt-contract-completed-wrong-id",
        "gpt-contract-completed-mismatched-id",
        "gpt-contract-incomplete-empty-id",
        "gpt-contract-incomplete-wrong-id",
        "gpt-contract-incomplete-mismatched-id",
        "gpt-contract-failed-empty-id",
        "gpt-contract-failed-wrong-id",
        "gpt-contract-failed-mismatched-id",
        "gpt-contract-failed-without-created",
        "gpt-contract-terminal-wrong-end-turn",
        "gpt-contract-terminal-null-end-turn",
        "gpt-contract-terminal-true-end-turn",
        "gpt-contract-terminal-false-end-turn",
        "gpt-contract-usage-valid-details",
        "gpt-contract-usage-null-details",
        "gpt-contract-usage-null",
        "gpt-contract-usage-wrong-type",
        "gpt-contract-usage-missing-input",
        "gpt-contract-usage-missing-output",
        "gpt-contract-usage-missing-total",
        "gpt-contract-usage-wrong-input",
        "gpt-contract-usage-wrong-output",
        "gpt-contract-usage-null-total",
        "gpt-contract-usage-negative-input",
        "gpt-contract-usage-negative-output",
        "gpt-contract-usage-negative-total",
        "gpt-contract-usage-integer-overflow",
        "gpt-contract-usage-sum-overflow",
        "gpt-contract-usage-total-mismatch",
        "gpt-contract-usage-input-details-wrong",
        "gpt-contract-usage-output-details-wrong",
        "gpt-contract-usage-missing-cached",
        "gpt-contract-usage-missing-reasoning",
        "gpt-contract-usage-negative-cached",
        "gpt-contract-usage-negative-reasoning",
        "gpt-contract-usage-cached-exceeds-input",
        "gpt-contract-usage-reasoning-exceeds-output",
        "gpt-terminal-completed-no-status-usage",
        "gpt-terminal-completed-no-status-no-usage",
        "gpt-terminal-completed-matching-status",
        "gpt-terminal-completed-null-status",
        "gpt-terminal-completed-wrong-status-type",
        "gpt-terminal-completed-mismatched-status",
        "gpt-terminal-incomplete-no-status-usage",
        "gpt-terminal-incomplete-no-status-no-usage",
        "gpt-terminal-incomplete-matching-status",
        "gpt-terminal-incomplete-null-status",
        "gpt-terminal-incomplete-wrong-status-type",
        "gpt-terminal-incomplete-mismatched-status",
        "gpt-terminal-completed-pending-item",
        "gpt-terminal-incomplete-pending-item",
        "gpt-terminal-completed-repeated-later",
        "gpt-terminal-incomplete-repeated-later",
        "gpt-terminal-failed-later",
        "gpt-terminal-failed-null-status",
        "gpt-terminal-error-later",
        "gpt-terminal-incomplete-unknown-reason",
        "gpt-terminal-incomplete-missing-response",
        "gpt-terminal-completed-missing-id",
        "gpt-reasoning-absent-both",
        "gpt-reasoning-empty-array-id",
        "gpt-reasoning-empty-text-both",
        "gpt-reasoning-whitespace-encrypted",
        "gpt-reasoning-empty-carrier-free",
        "gpt-reasoning-empty-id-value",
        "gpt-reasoning-empty-encrypted-value",
        "gpt-reasoning-both-empty-values",
        "gpt-reasoning-leading-text",
        "gpt-reasoning-multiple-parts",
        "gpt-reasoning-summary-content",
        "gpt-lifecycle-missing-reasoning-done",
        "gpt-lifecycle-duplicate-reasoning-done",
        "gpt-lifecycle-conflicting-reasoning-done",
        "gpt-lifecycle-summary-before-added",
        "gpt-lifecycle-summary-after-done",
        "gpt-lifecycle-summary-delta-after-text-done",
        "gpt-lifecycle-duplicate-added",
        "gpt-lifecycle-summary-done-without-part",
        "gpt-lifecycle-late-reasoning-id",
        "gpt-lifecycle-duplicate-delta-sequence",
        "gpt-lifecycle-empty-content-part",
        "gpt-lifecycle-conflicting-delta-sequence",
        "gpt-lifecycle-out-of-order-sequence",
        "gpt-lifecycle-reused-reasoning-id",
        "gpt-lifecycle-sparse-summary-index",
        "gpt-lifecycle-sparse-content-index",
        "gpt-lifecycle-terminal-untracked-output",
        "gpt-lifecycle-terminal-omitted-output",
        "gpt-lifecycle-terminal-mismatched-output",
        "gpt-lifecycle-standalone-message-done",
        "gpt-lifecycle-standalone-function-done",
        "gpt-lifecycle-partial-message-completion",
        "gpt-lifecycle-partial-function-completion",
        "gpt-lifecycle-conflicting-message-completion",
        "gpt-lifecycle-conflicting-function-completion",
        "gpt-lifecycle-missing-reasoning-delta",
        "gpt-lifecycle-missing-summary-done-text",
        "gpt-lifecycle-conflicting-function-identity",
        "gpt-lifecycle-missing-terminal-response",
        "gpt-lifecycle-terminal-status-mismatch",
        "gpt-lifecycle-missing-function-done",
        "gpt-lifecycle-missing-message-done",
    ]
    .into_iter()
    .map(|model| (model.to_string(), ModelConfig::default()))
    .collect();
    let chat_models = [
        "gpt-chat-fixture",
        "gpt-chat-scalar-fixture",
        "gpt-chat-response-extras",
        "gpt-chat-response-malformed-json",
        "gpt-chat-response-oversized",
        "gpt-chat-response-429",
        "gpt-chat-response-503",
        "gpt-chat-response-body-error",
        "gpt-chat-response-no-choices",
        "gpt-chat-response-no-id",
        "gpt-chat-response-model-null",
        "gpt-chat-response-object",
        "gpt-chat-response-created",
        "gpt-chat-response-choices-wrong",
        "gpt-chat-response-choices-empty",
        "gpt-chat-response-choices-multiple",
        "gpt-chat-response-choice-wrong",
        "gpt-chat-response-choice-index",
        "gpt-chat-response-no-message",
        "gpt-chat-response-role",
        "gpt-chat-response-content-wrong",
        "gpt-chat-response-content-part",
        "gpt-chat-response-no-finish",
        "gpt-chat-response-finish-unknown",
        "gpt-chat-response-tool-type",
        "gpt-chat-response-tool-id",
        "gpt-chat-response-tool-function",
        "gpt-chat-response-tool-name",
        "gpt-chat-response-tool-arguments-type",
        "gpt-chat-response-tool-arguments-json",
        "gpt-chat-response-tool-arguments-scalar",
        "gpt-chat-response-tool-collision",
        "gpt-chat-response-no-usage",
        "gpt-chat-response-usage-null",
        "gpt-chat-response-usage-wrong",
        "gpt-chat-response-usage-negative",
        "gpt-chat-response-usage-total",
        "gpt-chat-response-usage-details",
        "gpt-chat-response-usage-overflow",
        "gpt-chat-response-top-collision",
        "gpt-chat-response-usage-collision",
        "gpt-chat-response-function-call",
        "gpt-chat-response-reasoning-conflict",
        "gpt-chat-response-reasoning-no-signature",
        "gpt-chat-response-logprobs",
        "gpt-chat-response-refusal-malformed",
        "gpt-chat-response-tier-valid",
        "gpt-chat-response-tier-top-invalid",
        "gpt-chat-response-tier-nested-invalid",
        "gpt-chat-response-tier-conflict",
        "gpt-chat-response-refusal",
        "gpt-chat-stream-strict",
        "gpt-chat-stream-no-usage",
        "gpt-chat-stream-tools",
        "gpt-chat-stream-tool-optionals",
        "gpt-chat-stream-refusal",
        "gpt-chat-stream-refusal-split",
        "gpt-chat-stream-refusal-interleaved",
        "gpt-chat-stream-refusal-mirror",
        "gpt-chat-stream-refusal-empty",
        "gpt-chat-stream-refusal-repeated",
        "gpt-chat-stream-refusal-content-prefix",
        "gpt-chat-stream-refusal-partial",
        "gpt-chat-stream-refusal-tool-deferred",
        "gpt-chat-stream-refusal-multiple-tools",
        "gpt-chat-stream-budget-reasoning-exact",
        "gpt-chat-stream-budget-opaque-exact",
        "gpt-chat-stream-budget-mixed-utf8-exact",
        "gpt-chat-stream-tier-valid",
        "gpt-chat-stream-tier-late",
        "gpt-chat-stream-bad-missing-id",
        "gpt-chat-stream-bad-object",
        "gpt-chat-stream-bad-created",
        "gpt-chat-stream-bad-model",
        "gpt-chat-stream-bad-id-conflict",
        "gpt-chat-stream-bad-service",
        "gpt-chat-stream-bad-service-conflict",
        "gpt-chat-stream-bad-fingerprint",
        "gpt-chat-stream-bad-choices",
        "gpt-chat-stream-bad-choice-index",
        "gpt-chat-stream-bad-finish",
        "gpt-chat-stream-bad-function-finish",
        "gpt-chat-stream-bad-tool-finish",
        "gpt-chat-stream-bad-stop-tool",
        "gpt-chat-stream-bad-tool-index",
        "gpt-chat-stream-bad-tool-gap",
        "gpt-chat-stream-bad-tool-id",
        "gpt-chat-stream-bad-tool-duplicate-id",
        "gpt-chat-stream-bad-tool-incomplete",
        "gpt-chat-stream-bad-tool-scalar",
        "gpt-chat-stream-bad-usage-partial",
        "gpt-chat-stream-bad-usage-total",
        "gpt-chat-stream-bad-usage-details",
        "gpt-chat-stream-bad-usage-orphan",
        "gpt-chat-stream-bad-choice-extra",
        "gpt-chat-stream-bad-delta-extra",
        "gpt-chat-stream-bad-later-extra",
        "gpt-chat-stream-bad-refusal",
        "gpt-chat-stream-bad-logprobs",
        "gpt-chat-stream-bad-tier-nested",
        "gpt-chat-stream-bad-tier-conflict",
        "gpt-chat-stream-bad-refusal-conflict",
        "gpt-chat-stream-bad-refusal-finish",
        "gpt-chat-stream-bad-refusal-late",
        "gpt-chat-stream-bad-refusal-late-finish-usage",
        "gpt-chat-stream-bad-refusal-late-after-usage",
        "gpt-chat-stream-bad-refusal-repeated-usage",
        "gpt-chat-stream-bad-refusal-tool-incomplete",
        "gpt-chat-stream-bad-refusal-tool-late",
        "gpt-chat-stream-bad-refusal-tool-eof",
        "gpt-chat-stream-bad-budget-reasoning-over",
        "gpt-chat-stream-bad-budget-opaque-over",
        "gpt-chat-stream-bad-budget-mixed-utf8-over",
        "gpt-chat-stream-bad-tool-late-extra",
        "gpt-chat-stream-bad-tool-missing-terminal",
        "gpt-responses-state-exact",
        "gpt-responses-state-over",
        "gpt-responses-state-utf8-exact",
        "gpt-responses-function-state-exact",
        "gpt-responses-function-state-over",
        "gpt-responses-mixed-budget",
        "gpt-responses-mixed-budget-over",
    ]
    .into_iter()
    .map(|model| {
        let config = if model == "gpt-chat-fixture" {
            ModelConfig {
                tool_content_support_type: Some(vec!["array".to_string(), "image".to_string()]),
                ..Default::default()
            }
        } else {
            ModelConfig::default()
        };
        (model.to_string(), config)
    })
    .collect();
    let providers = BTreeMap::from([
        (
            "anthropic-fixture".to_string(),
            ProviderConfig {
                provider_type: Some("anthropic".to_string()),
                enabled: Some(true),
                base_url: Some(fixture.base_url.clone()),
                api_key: Some(UPSTREAM_KEY.to_string()),
                auth_type: Some("x-api-key".to_string()),
                models: Some(models),
                adjust_input_tokens: Some(false),
                extra: Map::new(),
            },
        ),
        (
            "responses-fixture".to_string(),
            ProviderConfig {
                provider_type: Some("openai-responses".to_string()),
                enabled: Some(true),
                base_url: Some(fixture.base_url.clone()),
                api_key: Some(UPSTREAM_KEY.to_string()),
                auth_type: Some("authorization".to_string()),
                models: Some(response_models),
                adjust_input_tokens: None,
                extra: Map::new(),
            },
        ),
        (
            "chat-fixture".to_string(),
            ProviderConfig {
                provider_type: Some("openai-compatible".to_string()),
                enabled: Some(true),
                base_url: Some(fixture.base_url.clone()),
                api_key: Some(UPSTREAM_KEY.to_string()),
                auth_type: Some("authorization".to_string()),
                models: Some(chat_models),
                adjust_input_tokens: None,
                extra: Map::new(),
            },
        ),
    ]);
    set_cached_config_for_test(AppConfig {
        auth: Some(AuthConfig {
            api_keys: Some(vec![json!(CLIENT_KEY)]),
            admin_api_key: None,
        }),
        providers: Some(providers),
        model_mappings: Some(BTreeMap::from([(
            "coding-default".to_string(),
            json!("responses-fixture/gpt-fixture"),
        )])),
        message_api_web_search_model: web_search_model.map(str::to_string),
        use_responses_api_context_management: Some(false),
        use_responses_api_web_search: Some(false),
        ..Default::default()
    });
}

fn post_json(path: &str, value: Value, key: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    request.body(Body::from(value.to_string())).unwrap()
}

fn data_events(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| !data.is_empty() && *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("SSE data is JSON"))
        .collect()
}

fn chat_event_schedule(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event["type"].as_str()? {
            "message_start" => Some("message_start".to_string()),
            "content_block_start" => Some(format!(
                "start:{}:{}:{}",
                event["index"],
                event["content_block"]["type"].as_str().unwrap_or_default(),
                event["content_block"]["name"].as_str().unwrap_or_default()
            )),
            "content_block_delta" => Some(format!(
                "delta:{}:{}",
                event["index"],
                event
                    .pointer("/delta/text")
                    .or_else(|| event.pointer("/delta/partial_json"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )),
            "content_block_stop" => Some(format!("stop:{}", event["index"])),
            "message_delta" => Some(format!(
                "terminal:{}",
                event["delta"]["stop_reason"].as_str().unwrap_or_default()
            )),
            "message_stop" => Some("message_stop".to_string()),
            "error" => Some("error".to_string()),
            _ => None,
        })
        .collect()
}

fn translated_payload_bytes(events: &[Value]) -> usize {
    events
        .iter()
        .map(|event| match event["type"].as_str() {
            Some("content_block_start") if event["content_block"]["type"] == "tool_use" => {
                event["content_block"]["id"].as_str().map_or(0, str::len)
                    + event["content_block"]["name"].as_str().map_or(0, str::len)
            }
            Some("content_block_delta") => [
                event.pointer("/delta/text"),
                event.pointer("/delta/thinking"),
                event.pointer("/delta/signature"),
                event.pointer("/delta/partial_json"),
            ]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::len)
            .sum(),
            _ => 0,
        })
        .sum()
}

fn codex_request(model: &str, stream: bool) -> Value {
    json!({
        "model": format!("responses-fixture/{model}"),
        "instructions": "You are Codex.",
        "input": [
            {
                "type":"message",
                "role":"user",
                "content":[{"type":"input_text","text":"inspect"}],
                "fixture_item_extension":{"keep":true}
            },
            {
                "id":"rs_previous",
                "type":"reasoning",
                "summary":[{"type":"summary_text","text":"prior"}],
                "encrypted_content":"enc_previous"
            },
            {
                "type":"function_call",
                "call_id":"call_previous",
                "name":"read",
                "arguments":"{\"path\":\"old\"}"
            },
            {
                "type":"function_call_output",
                "call_id":"call_previous",
                "output":"old contents"
            }
        ],
        "tools":[
            {
                "type":"function",
                "name":"read",
                "description":"Read a file",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}}}
            }
        ],
        "tool_choice":"auto",
        "parallel_tool_calls":true,
        "reasoning":{"effort":"high","summary":"auto"},
        "store":false,
        "stream":stream,
        "include":["reasoning.encrypted_content"],
        "prompt_cache_key":"thread-fixture",
        "service_tier":"default",
        "previous_response_id":"resp_previous",
        "conversation":"conv_fixture",
        "client_metadata":{"terminal":"fixture"},
        "fixture_top_level_extension":{"keep":true}
    })
}

/// Every Codex 0.144.1 `ResponseItem` variant whose optional fields can appear
/// in HTTP continuation history. Typed proxy variants and raw forward-compatible
/// variants are both represented so adding a stricter local field cannot regress
/// provider/model dispatch again.
fn codex_optional_response_item_cases() -> Vec<(&'static str, Value)> {
    vec![
        (
            "message-image-without-detail",
            json!({
                "type":"message",
                "role":"user",
                "content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]
            }),
        ),
        (
            "reasoning-without-encrypted-content",
            json!({
                "type":"reasoning",
                "summary":[],
                "content":[{"type":"reasoning_text","text":"preserve me"}]
            }),
        ),
        (
            "reasoning-with-null-encrypted-content",
            json!({
                "type":"reasoning",
                "summary":[],
                "encrypted_content":Value::Null
            }),
        ),
        (
            "local-shell-without-ids",
            json!({
                "type":"local_shell_call",
                "status":"completed",
                "action":{
                    "type":"exec",
                    "command":["pwd"],
                    "timeout_ms":Value::Null,
                    "working_directory":Value::Null,
                    "env":Value::Null,
                    "user":Value::Null
                }
            }),
        ),
        (
            "function-call-without-item-id",
            json!({
                "type":"function_call",
                "name":"read",
                "arguments":"{}",
                "call_id":"call_function"
            }),
        ),
        (
            "function-output-image-without-detail",
            json!({
                "type":"function_call_output",
                "call_id":"call_function",
                "output":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]
            }),
        ),
        (
            "tool-search-call-without-ids",
            json!({
                "type":"tool_search_call",
                "call_id":Value::Null,
                "execution":"client",
                "arguments":{"query":"read"}
            }),
        ),
        (
            "tool-search-output-without-ids",
            json!({
                "type":"tool_search_output",
                "call_id":Value::Null,
                "status":"completed",
                "execution":"client",
                "tools":[]
            }),
        ),
        (
            "custom-tool-call-optional-fields",
            json!({
                "type":"custom_tool_call",
                "call_id":"call_custom",
                "name":"freeform",
                "input":"payload"
            }),
        ),
        (
            "custom-tool-output-optional-fields",
            json!({
                "type":"custom_tool_call_output",
                "call_id":"call_custom",
                "output":"done"
            }),
        ),
        (
            "web-search-all-optional-fields",
            json!({"type":"web_search_call"}),
        ),
        (
            "image-generation-optional-fields",
            json!({
                "type":"image_generation_call",
                "status":"completed",
                "result":"image-data"
            }),
        ),
        (
            "compaction-without-id",
            json!({
                "type":"compaction",
                "encrypted_content":"enc_compact"
            }),
        ),
        (
            "legacy-compaction-summary-without-id",
            json!({
                "type":"compaction_summary",
                "encrypted_content":"enc_legacy_compact"
            }),
        ),
        (
            "context-compaction-all-optional-fields",
            json!({"type":"context_compaction"}),
        ),
        (
            "additional-tools-without-id",
            json!({
                "type":"additional_tools",
                "role":"developer",
                "tools":[]
            }),
        ),
        ("compaction-trigger", json!({"type":"compaction_trigger"})),
        (
            "agent-message-without-id",
            json!({
                "type":"agent_message",
                "author":"agent",
                "recipient":"user",
                "content":[{"type":"input_text","text":"hi"}]
            }),
        ),
    ]
}

fn normalized_optional_item(mut item: Value) -> Value {
    let item_type = item.get("type").and_then(Value::as_str).map(str::to_owned);
    let optional_null = match item_type.as_deref() {
        Some("reasoning") => Some("encrypted_content"),
        Some("tool_search_call" | "tool_search_output") => Some("call_id"),
        _ => None,
    };
    if let (Some(key), Some(object)) = (optional_null, item.as_object_mut()) {
        if object.get(key).is_some_and(Value::is_null) {
            object.remove(key);
        }
    }
    if item_type.as_deref() == Some("compaction_summary") {
        item["type"] = json!("compaction");
    }
    item
}

fn aggregate_empty_reasoning_cases(
) -> [(&'static str, Option<&'static str>, Option<&'static str>); 8] {
    [
        (
            "gpt-reasoning-absent-both",
            Some("encrypted-absent"),
            Some("reasoning-absent"),
        ),
        (
            "gpt-reasoning-empty-array-id",
            None,
            Some("reasoning-empty-array"),
        ),
        (
            "gpt-reasoning-empty-text-both",
            Some("encrypted-empty-text"),
            Some("reasoning-empty-text"),
        ),
        (
            "gpt-reasoning-whitespace-encrypted",
            Some("encrypted-whitespace"),
            None,
        ),
        ("gpt-reasoning-empty-carrier-free", None, None),
        ("gpt-reasoning-empty-id-value", None, Some("")),
        ("gpt-reasoning-empty-encrypted-value", Some(""), None),
        ("gpt-reasoning-both-empty-values", Some(""), Some("")),
    ]
}

fn reasoning_content_framing_cases() -> Vec<(&'static str, String, &'static str)> {
    vec![
        (
            "gpt-reasoning-leading-text",
            "  analysis  ".to_string(),
            "encrypted-leading@reasoning-leading",
        ),
        (
            "gpt-reasoning-multiple-parts",
            ["  first ", "", "\tsecond\n", ""].join(REASONING_SUMMARY_SEPARATOR),
            "encrypted-parts@reasoning-parts",
        ),
    ]
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_code_2_1_207_contract_crosses_public_axum_boundary() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let request = json!({
        "model":"anthropic-fixture/claude-sonnet-4-6",
        "max_tokens":1024,
        "system":[
            {"type":"text","text":"You are Claude Code.","cache_control":{"type":"ephemeral"}}
        ],
        "messages":[
            {"role":"user","content":[{"type":"text","text":"inspect"}]},
            {
                "role":"assistant",
                "content":[
                    {"type":"tool_use","id":"prior_a","name":"read","input":{"path":"a"}},
                    {"type":"tool_use","id":"prior_b","name":"read","input":{"path":"b"}}
                ]
            },
            {
                "role":"user",
                "content":[
                    {"type":"tool_result","tool_use_id":"prior_a","content":"A"},
                    {"type":"tool_result","tool_use_id":"prior_b","content":"B"}
                ]
            }
        ],
        "tools":[
            {
                "name":"read",
                "description":"Read a file",
                "input_schema":{"type":"object","properties":{"path":{"type":"string"}}}
            }
        ],
        "thinking":{"type":"enabled","budget_tokens":256},
        "metadata":{"user_id":"session_fixture","fixture_extension":true},
        "stream":false,
        "fixture_top_level_extension":{"keep":true}
    });
    let mut req = post_json("/v1/messages", request, Some(CLIENT_KEY));
    req.headers_mut()
        .insert("anthropic-version", "2023-06-01".parse().unwrap());
    req.headers_mut().insert(
        "anthropic-beta",
        "prompt-caching-2024-07-31,interleaved-thinking-2025-05-14"
            .parse()
            .unwrap(),
    );
    req.headers_mut()
        .insert("user-agent", "claude-code/2.1.207".parse().unwrap());
    let (status, body) = send(req).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["type"], "thinking");
    assert_eq!(body["content"][1]["id"], "tool_a");
    assert_eq!(body["content"][2]["id"], "tool_b");
    assert_eq!(body["fixture_extension"]["preserved"], true);

    let mut streaming = post_json(
        "/v1/messages",
        json!({
            "model":"anthropic-fixture/claude-sonnet-4-6",
            "max_tokens":1024,
            "messages":[{"role":"user","content":"parallel tools"}],
            "stream":true
        }),
        Some(CLIENT_KEY),
    );
    streaming
        .headers_mut()
        .insert("anthropic-version", "2023-06-01".parse().unwrap());
    streaming.headers_mut().insert(
        "anthropic-beta",
        "interleaved-thinking-2025-05-14".parse().unwrap(),
    );
    let (status, headers, body) = send_full(streaming).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let events = data_events(&body);
    let types: Vec<&str> = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert_eq!(types.first().copied(), Some("message_start"));
    assert_eq!(types.last().copied(), Some("message_stop"));
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );
    let tool_ids: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .filter_map(|event| event["content_block"]["id"].as_str())
        .collect();
    assert_eq!(tool_ids, ["tool_a", "tool_b"]);

    let captures = fixture.requests();
    let first = captures
        .iter()
        .find(|capture| capture.path == "/v1/messages" && capture.body["stream"] == false)
        .expect("captured non-streaming Messages request");
    assert_eq!(first.body["model"], "claude-sonnet-4-6");
    assert_eq!(first.body["fixture_top_level_extension"]["keep"], true);
    assert_eq!(first.headers["x-api-key"], UPSTREAM_KEY);
    assert_eq!(first.headers["anthropic-version"], "2023-06-01");
    assert!(first.headers["anthropic-beta"]
        .to_str()
        .unwrap()
        .contains("prompt-caching"));
    assert!(first.headers.get("authorization").is_none());
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_optional_reasoning_carriers_cross_public_messages_boundary() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (case, encrypted_content, id) in [
        (
            "legacy-both",
            Some("enc-public-both"),
            Some("reasoning-public-both"),
        ),
        ("versioned-encrypted-only", Some("enc-public-only"), None),
        ("versioned-id-only", None, Some("reasoning-public-only")),
        ("versioned-neither", None, None),
        ("versioned-empty-encrypted", Some(""), None),
        ("versioned-empty-id", None, Some("")),
        ("versioned-both-empty", Some(""), Some("")),
    ] {
        let signature = encode_reasoning_signature(encrypted_content, id);
        if case == "legacy-both" {
            assert_eq!(signature, "enc-public-both@reasoning-public-both");
        } else {
            assert!(
                signature.starts_with("rs1#"),
                "{case}: expected versioned carrier"
            );
        }

        let request = json!({
            "model":"responses-fixture/gpt-fixture",
            "max_tokens":128,
            "messages":[
                {
                    "role":"assistant",
                    "content":[{
                        "type":"thinking",
                        "thinking":"preserve optional reasoning",
                        "signature":signature
                    }]
                },
                {"role":"user","content":"continue"}
            ],
            "stream":false
        });
        let (status, body) = send(post_json("/v1/messages", request, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{case}: {}",
            String::from_utf8_lossy(&body)
        );

        let captures = fixture.requests();
        let captured = captures
            .iter()
            .rev()
            .find(|capture| capture.path == "/v1/responses")
            .unwrap_or_else(|| panic!("{case}: translated Responses request not captured"));
        let reasoning = captured.body["input"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["type"] == "reasoning"))
            .unwrap_or_else(|| panic!("{case}: reasoning item was dropped"));
        assert_eq!(
            reasoning.get("encrypted_content").and_then(Value::as_str),
            encrypted_content,
            "{case}: encrypted_content changed"
        );
        assert_eq!(
            reasoning.get("id").and_then(Value::as_str),
            id,
            "{case}: id changed"
        );
        assert_eq!(
            reasoning["summary"][0]["text"],
            "preserve optional reasoning"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_aggregate_empty_reasoning_nonstream_preserves_only_real_carriers() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, encrypted_content, id) in aggregate_empty_reasoning_cases() {
        let request = json!({
            "model":format!("responses-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"reason"}],
            "stream":false
        });
        let (status, body) = send(post_json("/v1/messages", request, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{model}: {}",
            String::from_utf8_lossy(&body)
        );
        let response = json_body(&body);
        let thinking = response["content"]
            .as_array()
            .and_then(|content| content.iter().find(|block| block["type"] == "thinking"));

        if encrypted_content.is_some() || id.is_some() {
            let thinking = thinking.unwrap_or_else(|| panic!("{model}: thinking carrier dropped"));
            assert_eq!(thinking["thinking"], THINKING_TEXT, "{model}");
            assert_eq!(
                thinking["signature"],
                encode_reasoning_signature(encrypted_content, id),
                "{model}"
            );
        } else {
            assert!(
                thinking.is_none(),
                "{model}: carrier-free empty reasoning invented {thinking:?}"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_aggregate_empty_reasoning_stream_matches_nonstream_rules() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, encrypted_content, id) in aggregate_empty_reasoning_cases() {
        let request = json!({
            "model":format!("responses-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"reason"}],
            "stream":true
        });
        let (status, body) = send(post_json("/v1/messages", request, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{model}: {}",
            String::from_utf8_lossy(&body)
        );
        let events = data_events(&body);
        let thinking_start = events.iter().find(|event| {
            event["type"] == "content_block_start" && event["content_block"]["type"] == "thinking"
        });
        let thinking_delta = events.iter().find(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "thinking_delta"
        });
        let signature_delta = events.iter().find(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "signature_delta"
        });

        if encrypted_content.is_some() || id.is_some() {
            assert!(thinking_start.is_some(), "{model}: thinking block missing");
            assert_eq!(
                thinking_delta.and_then(|event| event["delta"]["thinking"].as_str()),
                Some(THINKING_TEXT),
                "{model}"
            );
            let expected_signature = encode_reasoning_signature(encrypted_content, id);
            assert_eq!(
                signature_delta.and_then(|event| event["delta"]["signature"].as_str()),
                Some(expected_signature.as_str()),
                "{model}"
            );
        } else {
            assert!(thinking_start.is_none(), "{model}: invented thinking block");
            assert!(thinking_delta.is_none(), "{model}: invented thinking delta");
            assert!(
                signature_delta.is_none(),
                "{model}: invented signature delta"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_reasoning_content_framing_nonstream_is_lossless() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, expected_thinking, expected_signature) in reasoning_content_framing_cases() {
        let request = json!({
            "model":format!("responses-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"reason"}],
            "stream":false
        });
        let (status, body) = send(post_json("/v1/messages", request, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let response = json_body(&body);
        let thinking = response["content"]
            .as_array()
            .and_then(|content| content.iter().find(|block| block["type"] == "thinking"))
            .unwrap_or_else(|| panic!("{model}: thinking block missing"));
        assert_eq!(thinking["thinking"], expected_thinking, "{model}");
        assert_eq!(thinking["signature"], expected_signature, "{model}");
        if model == "gpt-reasoning-multiple-parts" {
            assert_eq!(
                expected_thinking
                    .matches(REASONING_SUMMARY_SEPARATOR)
                    .count(),
                3,
                "one separator per semantic part boundary"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_reasoning_content_framing_stream_matches_nonstream_exactly() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, expected_thinking, expected_signature) in reasoning_content_framing_cases() {
        let request = json!({
            "model":format!("responses-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"reason"}],
            "stream":true
        });
        let (status, body) = send(post_json("/v1/messages", request, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        let thinking: String = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta" && event["delta"]["type"] == "thinking_delta"
            })
            .filter_map(|event| event["delta"]["thinking"].as_str())
            .collect();
        let signatures: Vec<&str> = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta"
                    && event["delta"]["type"] == "signature_delta"
            })
            .filter_map(|event| event["delta"]["signature"].as_str())
            .collect();
        assert_eq!(thinking, expected_thinking, "{model}");
        assert_eq!(signatures, [expected_signature], "{model}");
        if model == "gpt-reasoning-multiple-parts" {
            assert_eq!(
                thinking.matches(REASONING_SUMMARY_SEPARATOR).count(),
                3,
                "duplicate part.added must not duplicate separators"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_reasoning_content_deltas_cross_public_stream_losslessly() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let expected = [" summary ", " raw content ", "second"].join(REASONING_SUMMARY_SEPARATOR);

    let nonstream = json!({
        "model":"responses-fixture/gpt-reasoning-summary-content",
        "max_tokens":128,
        "messages":[{"role":"user","content":"reason"}],
        "stream":false
    });
    let (status, body) = send(post_json("/v1/messages", nonstream, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let response = json_body(&body);
    assert_eq!(response["content"][0]["type"], "thinking");
    assert_eq!(response["content"][0]["thinking"], expected);
    assert_eq!(
        response["content"][0]["signature"],
        "encrypted-content@reasoning-content"
    );

    let stream = json!({
        "model":"responses-fixture/gpt-reasoning-summary-content",
        "max_tokens":128,
        "messages":[{"role":"user","content":"reason"}],
        "stream":true
    });
    let (status, body) = send(post_json("/v1/messages", stream, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let thinking: String = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "thinking_delta"
        })
        .filter_map(|event| event["delta"]["thinking"].as_str())
        .collect();
    assert_eq!(thinking, expected);
    let signatures: Vec<&str> = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "signature_delta"
        })
        .filter_map(|event| event["delta"]["signature"].as_str())
        .collect();
    assert_eq!(
        signatures,
        ["encrypted-content@reasoning-content"],
        "reasoning carrier must be emitted exactly once"
    );

    let position = |event_type: &str, delta_type: Option<&str>| {
        events
            .iter()
            .position(|event| {
                event["type"] == event_type
                    && delta_type.is_none_or(|kind| event["delta"]["type"] == kind)
            })
            .unwrap_or_else(|| panic!("missing {event_type}/{delta_type:?}"))
    };
    let thinking_position = position("content_block_delta", Some("thinking_delta"));
    let signature_position = position("content_block_delta", Some("signature_delta"));
    let stop_position = position("content_block_stop", None);
    let message_delta_position = position("message_delta", None);
    let message_stop_position = position("message_stop", None);
    assert!(
        thinking_position < signature_position
            && signature_position < stop_position
            && stop_position < message_delta_position
            && message_delta_position < message_stop_position
    );
    assert_eq!(
        events.last().and_then(|event| event["type"].as_str()),
        Some("message_stop")
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_reasoning_lifecycle_replays_and_adjacent_variants_are_deterministic() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, expected_thinking, expected_signature) in [
        (
            "gpt-lifecycle-duplicate-reasoning-done",
            "once",
            "encrypted-life@reasoning-life",
        ),
        (
            "gpt-lifecycle-duplicate-added",
            "once",
            "encrypted-life@reasoning-life",
        ),
        (
            "gpt-lifecycle-summary-done-without-part",
            "buffered-authoritative",
            "encrypted-life@reasoning-life",
        ),
        (
            "gpt-lifecycle-late-reasoning-id",
            "late-id",
            "encrypted-late@reasoning-late",
        ),
        (
            "gpt-lifecycle-duplicate-delta-sequence",
            "sequence-once",
            "encrypted-life@reasoning-life",
        ),
        (
            "gpt-lifecycle-empty-content-part",
            "\u{2063}\n\nsecond",
            "encrypted-life@reasoning-life",
        ),
    ] {
        let request = json!({
            "model":format!("responses-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"reason"}],
            "stream":true
        });
        let (status, body) = send(post_json("/v1/messages", request, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            0,
            "{model}: valid replay/variant failed"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}: terminal success count"
        );
        let thinking: Vec<&str> = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta" && event["delta"]["type"] == "thinking_delta"
            })
            .filter_map(|event| event["delta"]["thinking"].as_str())
            .collect();
        assert_eq!(thinking, [expected_thinking], "{model}");
        let signatures: Vec<&str> = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta"
                    && event["delta"]["type"] == "signature_delta"
            })
            .filter_map(|event| event["delta"]["signature"].as_str())
            .collect();
        assert_eq!(signatures, [expected_signature], "{model}");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_standalone_done_items_render_complete_text_and_function_calls() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let request = |model: &str| {
        post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"complete item"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        )
    };

    let (status, body) = send(request("gpt-lifecycle-standalone-message-done")).await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let text: String = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
        })
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "standalone text");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );

    let (status, body) = send(request("gpt-lifecycle-partial-message-completion")).await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let text: String = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
        })
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect();
    assert_eq!(
        text, "AB",
        "output_text.done must append the verified suffix"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );

    let (status, body) = send(request("gpt-lifecycle-standalone-function-done")).await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let tool_starts: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_start" && event["content_block"]["type"] == "tool_use"
        })
        .collect();
    assert_eq!(tool_starts.len(), 1);
    assert_eq!(tool_starts[0]["content_block"]["id"], "standalone-call");
    assert_eq!(tool_starts[0]["content_block"]["name"], "read");
    let arguments: String = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "input_json_delta"
        })
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(arguments, "{\"path\":\"standalone\"}");
    assert_eq!(
        events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .and_then(|event| event["delta"]["stop_reason"].as_str()),
        Some("tool_use")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );

    let (status, body) = send(request("gpt-lifecycle-partial-function-completion")).await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let arguments: String = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "input_json_delta"
        })
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(
        arguments, "{\"path\":\"x\"}",
        "arguments.done must append the verified suffix"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_incomplete_or_out_of_order_response_items_fail_once_without_success() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for model in [
        "gpt-lifecycle-missing-reasoning-done",
        "gpt-lifecycle-conflicting-reasoning-done",
        "gpt-lifecycle-summary-before-added",
        "gpt-lifecycle-summary-after-done",
        "gpt-lifecycle-summary-delta-after-text-done",
        "gpt-lifecycle-conflicting-delta-sequence",
        "gpt-lifecycle-out-of-order-sequence",
        "gpt-lifecycle-reused-reasoning-id",
        "gpt-lifecycle-sparse-summary-index",
        "gpt-lifecycle-sparse-content-index",
        "gpt-lifecycle-terminal-untracked-output",
        "gpt-lifecycle-terminal-omitted-output",
        "gpt-lifecycle-terminal-mismatched-output",
        "gpt-lifecycle-missing-terminal-response",
        "gpt-lifecycle-terminal-status-mismatch",
        "gpt-lifecycle-conflicting-message-completion",
        "gpt-lifecycle-conflicting-function-completion",
        "gpt-lifecycle-missing-reasoning-delta",
        "gpt-lifecycle-missing-summary-done-text",
        "gpt-lifecycle-conflicting-function-identity",
        "gpt-lifecycle-missing-function-done",
        "gpt-lifecycle-missing-message-done",
    ] {
        let request = json!({
            "model":format!("responses-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"reason"}],
            "stream":true
        });
        let (status, body) = send(post_json("/v1/messages", request, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        let errors: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "error")
            .collect();
        assert_eq!(errors.len(), 1, "{model}: {events:#?}");
        assert_eq!(errors[0]["error"]["type"], "api_error", "{model}");
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("error"),
            "{model}: terminal error must be final"
        );
        assert!(
            !events.iter().any(|event| event["type"] == "message_delta"),
            "{model}: fabricated Anthropic success delta"
        );
        assert!(
            !events.iter().any(|event| event["type"] == "message_stop"),
            "{model}: fabricated Anthropic success stop"
        );
        assert!(
            events
                .iter()
                .filter(|event| {
                    event["type"] == "content_block_delta"
                        && event["delta"]["type"] == "signature_delta"
                })
                .count()
                <= 1,
            "{model}: malformed lifecycle duplicated a carrier"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_completed_terminals_follow_codex_event_discriminator() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, input_tokens, output_tokens, cached_tokens) in [
        (
            "gpt-terminal-completed-no-status-usage",
            8_i64,
            7_i64,
            Some(3_i64),
        ),
        ("gpt-terminal-completed-no-status-no-usage", 0, 0, None),
        ("gpt-terminal-completed-matching-status", 0, 0, None),
        ("gpt-terminal-completed-null-status", 0, 0, None),
        ("gpt-terminal-completed-repeated-later", 8, 7, Some(3)),
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"complete"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            0,
            "{model}: {events:#?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_delta")
                .count(),
            1,
            "{model}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("message_stop"),
            "{model}"
        );
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("completed terminal emits message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "end_turn", "{model}");
        assert_eq!(delta["usage"]["input_tokens"], input_tokens, "{model}");
        assert_eq!(delta["usage"]["output_tokens"], output_tokens, "{model}");
        assert_eq!(
            delta["usage"]
                .get("cache_read_input_tokens")
                .and_then(Value::as_i64),
            cached_tokens,
            "{model}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_completed_end_turn_false_maps_to_pause_turn() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, expected_stop) in [
        ("gpt-contract-terminal-true-end-turn", "end_turn"),
        ("gpt-contract-terminal-false-end-turn", "pause_turn"),
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"end turn contract"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("valid completed event emits message_delta");
        assert_eq!(delta["delta"]["stop_reason"], expected_stop, "{model}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_incomplete_terminals_preserve_truncation_semantics_without_status() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, stop_reason, input_tokens, output_tokens, cached_tokens) in [
        (
            "gpt-terminal-incomplete-no-status-usage",
            "max_tokens",
            8_i64,
            7_i64,
            Some(3_i64),
        ),
        (
            "gpt-terminal-incomplete-no-status-no-usage",
            "refusal",
            0,
            0,
            None,
        ),
        (
            "gpt-terminal-incomplete-matching-status",
            "max_tokens",
            0,
            0,
            None,
        ),
        (
            "gpt-terminal-incomplete-null-status",
            "max_tokens",
            0,
            0,
            None,
        ),
        (
            "gpt-terminal-incomplete-repeated-later",
            "max_tokens",
            8,
            7,
            Some(3),
        ),
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"truncate"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            0,
            "{model}: {events:#?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("incomplete terminal emits truncation delta");
        assert_eq!(delta["delta"]["stop_reason"], stop_reason, "{model}");
        assert_eq!(delta["usage"]["input_tokens"], input_tokens, "{model}");
        assert_eq!(delta["usage"]["output_tokens"], output_tokens, "{model}");
        assert_eq!(
            delta["usage"]
                .get("cache_read_input_tokens")
                .and_then(Value::as_i64),
            cached_tokens,
            "{model}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_terminal_contradictions_and_pending_state_fail_once() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for model in [
        "gpt-terminal-completed-mismatched-status",
        "gpt-terminal-completed-wrong-status-type",
        "gpt-terminal-incomplete-mismatched-status",
        "gpt-terminal-incomplete-wrong-status-type",
        "gpt-terminal-completed-pending-item",
        "gpt-terminal-incomplete-pending-item",
        "gpt-terminal-incomplete-unknown-reason",
        "gpt-terminal-incomplete-missing-response",
        "gpt-terminal-completed-missing-id",
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"terminal error"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {events:#?}"
        );
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("error"),
            "{model}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event["type"].as_str(),
                Some("message_delta" | "message_stop")
            )),
            "{model}: contradictory terminal fabricated success"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_failed_and_error_terminals_suppress_all_later_terminals() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, expected_message) in [
        ("gpt-terminal-failed-later", "canonical fixture failure"),
        ("gpt-terminal-error-later", "canonical top-level error"),
        ("gpt-terminal-failed-null-status", "nullable failed status"),
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"fail"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        let errors: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "error")
            .collect();
        assert_eq!(errors.len(), 1, "{model}: {events:#?}");
        assert_eq!(errors[0]["error"]["message"], expected_message, "{model}");
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("error"),
            "{model}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event["type"].as_str(),
                Some("message_delta" | "message_stop")
            )),
            "{model}: failure terminal fabricated success"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_model_less_created_uses_resolved_model_context() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, expected_message_model) in [
        (
            "gpt-contract-created-model-less",
            "gpt-contract-created-model-less",
        ),
        (
            "gpt-contract-created-with-model",
            "gpt-contract-created-with-model",
        ),
        (
            "gpt-contract-created-upstream-model",
            "upstream-reported-model",
        ),
        (
            "gpt-contract-created-null-optionals",
            "gpt-contract-created-null-optionals",
        ),
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"created contract"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            0,
            "{model}: {events:#?}"
        );
        let start = events
            .iter()
            .find(|event| event["type"] == "message_start")
            .expect("model contract emits message_start");
        assert_eq!(start["message"]["id"], "resp_contract_fixture", "{model}");
        assert_eq!(
            start["message"]["model"], expected_message_model,
            "{model}: fallback/reported model changed"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_created_output_requires_matching_rendered_lifecycle() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let request = |model: &str| {
        post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"created output"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        )
    };

    let (status, body) = send(request("gpt-contract-created-output-lifecycle-match")).await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let text: String = events
        .iter()
        .filter(|event| event["delta"]["type"] == "text_delta")
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "created lifecycle");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );

    for model in [
        "gpt-contract-created-only-output",
        "gpt-contract-created-only-raw-output",
        "gpt-contract-created-output-conflict",
    ] {
        let (status, body) = send(request(model)).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {events:#?}"
        );
        assert!(!events.iter().any(|event| {
            matches!(
                event["type"].as_str(),
                Some("message_delta" | "message_stop")
            )
        }));
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_created_and_terminal_identity_fields_fail_closed() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for model in [
        "gpt-contract-created-empty-id",
        "gpt-contract-created-wrong-id",
        "gpt-contract-created-missing-id",
        "gpt-contract-created-empty-model",
        "gpt-contract-created-wrong-model",
        "gpt-contract-created-status-mismatch",
        "gpt-contract-created-wrong-status-type",
        "gpt-contract-completed-empty-id",
        "gpt-contract-completed-wrong-id",
        "gpt-contract-completed-mismatched-id",
        "gpt-contract-incomplete-empty-id",
        "gpt-contract-incomplete-wrong-id",
        "gpt-contract-incomplete-mismatched-id",
        "gpt-contract-failed-empty-id",
        "gpt-contract-failed-wrong-id",
        "gpt-contract-failed-mismatched-id",
        "gpt-contract-terminal-wrong-end-turn",
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"identity contract"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {events:#?}"
        );
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("error"),
            "{model}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event["type"].as_str(),
                Some("message_delta" | "message_stop")
            )),
            "{model}: invalid identity/scalar fabricated success"
        );
    }

    let (status, body) = send(post_json(
        "/v1/messages",
        json!({
            "model":"responses-fixture/gpt-contract-failed-without-created",
            "max_tokens":128,
            "messages":[{"role":"user","content":"failed contract"}],
            "stream":true
        }),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let errors: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "error")
        .collect();
    assert_eq!(errors.len(), 1, "{events:#?}");
    assert_eq!(errors[0]["error"]["message"], "failed before created");
    assert!(!events.iter().any(|event| event["type"] == "message_stop"));
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_usage_contract_preserves_valid_details_and_omission() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, input_tokens, output_tokens, cached_tokens) in [
        (
            "gpt-contract-usage-valid-details",
            3_i64,
            3_i64,
            Some(2_i64),
        ),
        ("gpt-contract-usage-null-details", 5, 3, None),
        ("gpt-contract-usage-null", 0, 0, None),
        ("gpt-contract-terminal-null-end-turn", 0, 0, None),
        ("gpt-contract-created-model-less", 0, 0, None),
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"usage contract"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            0,
            "{model}: {events:#?}"
        );
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("valid usage emits terminal delta");
        assert_eq!(delta["usage"]["input_tokens"], input_tokens, "{model}");
        assert_eq!(delta["usage"]["output_tokens"], output_tokens, "{model}");
        assert_eq!(
            delta["usage"]
                .get("cache_read_input_tokens")
                .and_then(Value::as_i64),
            cached_tokens,
            "{model}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_malformed_usage_never_coerces_to_success() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for model in [
        "gpt-contract-created-partial-usage",
        "gpt-contract-usage-wrong-type",
        "gpt-contract-usage-missing-input",
        "gpt-contract-usage-missing-output",
        "gpt-contract-usage-missing-total",
        "gpt-contract-usage-wrong-input",
        "gpt-contract-usage-wrong-output",
        "gpt-contract-usage-null-total",
        "gpt-contract-usage-negative-input",
        "gpt-contract-usage-negative-output",
        "gpt-contract-usage-negative-total",
        "gpt-contract-usage-integer-overflow",
        "gpt-contract-usage-sum-overflow",
        "gpt-contract-usage-total-mismatch",
        "gpt-contract-usage-input-details-wrong",
        "gpt-contract-usage-output-details-wrong",
        "gpt-contract-usage-missing-cached",
        "gpt-contract-usage-missing-reasoning",
        "gpt-contract-usage-negative-cached",
        "gpt-contract-usage-negative-reasoning",
        "gpt-contract-usage-cached-exceeds-input",
        "gpt-contract-usage-reasoning-exceeds-output",
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"invalid usage"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {events:#?}"
        );
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("error"),
            "{model}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event["type"].as_str(),
                Some("message_delta" | "message_stop")
            )),
            "{model}: malformed usage was coerced to success"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_handled_scalar_families_accept_source_valid_shapes() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for model in [
        "gpt-scalar-function-valid",
        "gpt-scalar-custom-tool-valid",
        "gpt-scalar-function-whitespace-namespace-valid",
        "gpt-scalar-tool-search-valid",
        "gpt-scalar-tool-search-item-id-valid",
        "gpt-scalar-tool-search-late-id-valid",
        "gpt-scalar-tool-search-output-valid",
        "gpt-scalar-message-valid",
        "gpt-scalar-reasoning-valid",
        "gpt-scalar-compaction-valid",
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"valid scalar family"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            0,
            "{model}: {events:#?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
        for start in events
            .iter()
            .filter(|event| event["type"] == "content_block_start")
        {
            if start["content_block"]["type"] == "tool_use" {
                assert!(
                    start["content_block"]["id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty()),
                    "{model}: empty tool id"
                );
                assert!(
                    start["content_block"]["name"]
                        .as_str()
                        .is_some_and(|name| !name.is_empty()),
                    "{model}: empty tool name"
                );
                if model == "gpt-scalar-function-whitespace-namespace-valid" {
                    assert_eq!(
                        start["content_block"]["name"], "read",
                        "trimmed-empty namespace must fall back to required name"
                    );
                }
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_malformed_handled_scalars_fail_once_without_empty_blocks() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for model in [
        "gpt-scalar-function-added-missing",
        "gpt-scalar-custom-tool-malformed",
        "gpt-scalar-function-added-missing-call-id",
        "gpt-scalar-function-added-missing-name",
        "gpt-scalar-function-added-missing-arguments",
        "gpt-scalar-function-added-wrong",
        "gpt-scalar-function-added-wrong-name",
        "gpt-scalar-function-added-wrong-arguments",
        "gpt-scalar-function-added-empty",
        "gpt-scalar-function-added-invalid-json",
        "gpt-scalar-function-done-missing",
        "gpt-scalar-function-done-missing-call-id",
        "gpt-scalar-function-done-missing-name",
        "gpt-scalar-function-done-missing-arguments",
        "gpt-scalar-function-done-invalid-json",
        "gpt-scalar-function-done-wrong-call-id",
        "gpt-scalar-function-done-wrong-name",
        "gpt-scalar-function-done-wrong-arguments",
        "gpt-scalar-function-delta-wrong",
        "gpt-scalar-function-delta-empty",
        "gpt-scalar-function-arguments-done-invalid",
        "gpt-scalar-function-arguments-done-duplicate",
        "gpt-scalar-function-delta-after-done",
        "gpt-scalar-tool-search-unexpected-delta",
        "gpt-scalar-tool-search-empty-call-id",
        "gpt-scalar-tool-search-missing-execution",
        "gpt-scalar-tool-search-wrong",
        "gpt-scalar-tool-search-wrong-execution",
        "gpt-scalar-tool-search-wrong-status",
        "gpt-scalar-tool-search-missing-arguments",
        "gpt-scalar-tool-search-output-malformed",
        "gpt-scalar-tool-search-output-wrong-execution",
        "gpt-scalar-tool-search-output-wrong-tools",
        "gpt-scalar-message-missing",
        "gpt-scalar-message-incomplete-on-completed",
        "gpt-scalar-message-wrong-content",
        "gpt-scalar-message-wrong-role",
        "gpt-scalar-message-content-not-array",
        "gpt-scalar-message-block-malformed",
        "gpt-scalar-message-annotations-malformed",
        "gpt-scalar-message-refusal-unsupported",
        "gpt-scalar-message-image-unsupported",
        "gpt-scalar-output-annotations-late",
        "gpt-scalar-output-done-duplicate",
        "gpt-scalar-output-delta-after-done",
        "gpt-scalar-output-index-mismatch",
        "gpt-scalar-reasoning-wrong",
        "gpt-scalar-reasoning-missing-summary",
        "gpt-scalar-reasoning-wrong-id",
        "gpt-scalar-reasoning-wrong-encrypted",
        "gpt-scalar-reasoning-summary-not-array",
        "gpt-scalar-reasoning-content-not-array",
        "gpt-scalar-reasoning-summary-malformed",
        "gpt-scalar-reasoning-content-malformed",
        "gpt-scalar-reasoning-event-id-wrong",
        "gpt-scalar-reasoning-part-malformed",
        "gpt-scalar-reasoning-done-missing-item-id",
        "gpt-scalar-reasoning-summary-conflict",
        "gpt-scalar-reasoning-content-conflict",
        "gpt-scalar-compaction-missing",
        "gpt-scalar-compaction-wrong",
        "gpt-scalar-compaction-wrong-id",
        "gpt-scalar-compaction-wrong-encrypted",
        "gpt-scalar-output-index-wrong",
        "gpt-scalar-output-index-sparse",
        "gpt-scalar-output-item-id-wrong",
        "gpt-scalar-output-wrapper-id-mismatch",
        "gpt-scalar-metadata-wrong",
        "gpt-scalar-metadata-turn-id-wrong",
    ] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"malformed scalar family"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {events:#?}"
        );
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("error"),
            "{model}: later frames escaped terminal cleanup"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event["type"].as_str(),
                Some("message_delta" | "message_stop")
            )),
            "{model}: malformed scalar fabricated success"
        );
        for start in events
            .iter()
            .filter(|event| event["type"] == "content_block_start")
        {
            if start["content_block"]["type"] == "tool_use" {
                assert!(
                    start["content_block"]["id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty()),
                    "{model}: malformed input opened an empty tool id"
                );
                assert!(
                    start["content_block"]["name"]
                        .as_str()
                        .is_some_and(|name| !name.is_empty()),
                    "{model}: malformed input opened an empty tool name"
                );
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_json_and_sse_outputs_match_for_valid_families() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for model in [
        "gpt-scalar-function-valid",
        "gpt-scalar-custom-tool-valid",
        "gpt-scalar-tool-search-valid",
        "gpt-scalar-tool-search-item-id-valid",
        "gpt-scalar-tool-search-late-id-valid",
        "gpt-scalar-tool-search-output-valid",
        "gpt-scalar-message-valid",
        "gpt-scalar-reasoning-valid",
        "gpt-scalar-compaction-valid",
    ] {
        let request = |stream| {
            post_json(
                "/v1/messages",
                json!({
                    "model":format!("responses-fixture/{model}"),
                    "max_tokens":128,
                    "messages":[{"role":"user","content":"paired output"}],
                    "stream":stream
                }),
                Some(CLIENT_KEY),
            )
        };
        let (json_status, json_body_bytes) = send(request(false)).await;
        assert_eq!(
            json_status,
            StatusCode::OK,
            "{model}: {}",
            String::from_utf8_lossy(&json_body_bytes)
        );
        let json_response = json_body(&json_body_bytes);

        let (sse_status, sse_body) = send(request(true)).await;
        assert_eq!(sse_status, StatusCode::OK, "{model}");
        let events = data_events(&sse_body);
        assert!(!events.iter().any(|event| event["type"] == "error"));
        assert_eq!(
            events.first().and_then(|event| event["type"].as_str()),
            Some("message_start")
        );
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("message_stop")
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_start")
                .count(),
            1,
            "{model}: message_start count"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_delta")
                .count(),
            1,
            "{model}: message_delta count"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}: message_stop count"
        );
        let stream_stop = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .and_then(|event| event["delta"]["stop_reason"].as_str());
        assert_eq!(
            json_response["stop_reason"].as_str(),
            stream_stop,
            "{model}: stop reason diverged"
        );
        let expected_stop = if matches!(
            model,
            "gpt-scalar-function-valid"
                | "gpt-scalar-custom-tool-valid"
                | "gpt-scalar-tool-search-valid"
                | "gpt-scalar-tool-search-item-id-valid"
                | "gpt-scalar-tool-search-late-id-valid"
        ) {
            "tool_use"
        } else {
            "end_turn"
        };
        assert_eq!(json_response["stop_reason"], expected_stop, "{model}");
        assert_eq!(json_response["usage"]["input_tokens"], 3, "{model}");
        assert_eq!(json_response["usage"]["output_tokens"], 3, "{model}");
        assert_eq!(
            json_response["usage"]["cache_read_input_tokens"], 2,
            "{model}"
        );
        let stream_usage = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("terminal usage");
        assert_eq!(stream_usage["usage"]["input_tokens"], 3, "{model}");
        assert_eq!(stream_usage["usage"]["output_tokens"], 3, "{model}");
        assert_eq!(
            stream_usage["usage"]["cache_read_input_tokens"], 2,
            "{model}"
        );

        let block_starts = events
            .iter()
            .filter(|event| event["type"] == "content_block_start")
            .count();
        let block_stops = events
            .iter()
            .filter(|event| event["type"] == "content_block_stop")
            .count();
        assert_eq!(block_starts, block_stops, "{model}: unbalanced blocks");
        assert_eq!(
            block_starts,
            json_response["content"].as_array().map_or(0, Vec::len),
            "{model}: block count diverged"
        );

        match model {
            "gpt-scalar-function-valid"
            | "gpt-scalar-custom-tool-valid"
            | "gpt-scalar-tool-search-valid"
            | "gpt-scalar-tool-search-item-id-valid"
            | "gpt-scalar-tool-search-late-id-valid" => {
                let json_tool = json_response["content"]
                    .as_array()
                    .and_then(|blocks| blocks.iter().find(|block| block["type"] == "tool_use"))
                    .expect("JSON tool block");
                let stream_tool = events
                    .iter()
                    .find(|event| {
                        event["type"] == "content_block_start"
                            && event["content_block"]["type"] == "tool_use"
                    })
                    .expect("SSE tool block");
                assert_eq!(
                    json_tool["id"], stream_tool["content_block"]["id"],
                    "{model}: deterministic tool id diverged"
                );
                assert_eq!(
                    json_tool["name"], stream_tool["content_block"]["name"],
                    "{model}: tool name diverged"
                );
                let partial_json: String = events
                    .iter()
                    .filter(|event| event["delta"]["type"] == "input_json_delta")
                    .filter_map(|event| event["delta"]["partial_json"].as_str())
                    .collect();
                let stream_input: Value =
                    serde_json::from_str(&partial_json).expect("complete streamed tool input");
                assert_eq!(json_tool["input"], stream_input, "{model}: tool input");
                if model == "gpt-scalar-tool-search-valid" {
                    assert_eq!(json_tool["id"], "tool_call_0");
                } else if model == "gpt-scalar-tool-search-item-id-valid" {
                    assert_eq!(json_tool["id"], "search-item-only");
                } else if model == "gpt-scalar-tool-search-late-id-valid" {
                    assert_eq!(json_tool["id"], "late-search-call");
                }
                match model {
                    "gpt-scalar-function-valid" => {
                        assert_eq!(json_tool["id"], "call-scalar");
                        assert_eq!(json_tool["name"], "read");
                        assert_eq!(json_tool["input"], json!({"path":"a"}));
                    }
                    "gpt-scalar-custom-tool-valid" => {
                        assert_eq!(json_tool["id"], "custom-call");
                        assert_eq!(json_tool["name"], "freeform");
                        assert_eq!(json_tool["input"], json!({"input":"payload"}));
                    }
                    _ => assert_eq!(json_tool["name"], "mcp__tool_search__search"),
                }
            }
            "gpt-scalar-message-valid" => {
                let stream_text: String = events
                    .iter()
                    .filter(|event| event["delta"]["type"] == "text_delta")
                    .filter_map(|event| event["delta"]["text"].as_str())
                    .collect();
                assert_eq!(json_response["content"][0]["text"], stream_text);
                assert_eq!(stream_text, "AB");
            }
            "gpt-scalar-reasoning-valid" | "gpt-scalar-compaction-valid" => {
                let stream_thinking: String = events
                    .iter()
                    .filter(|event| event["delta"]["type"] == "thinking_delta")
                    .filter_map(|event| event["delta"]["thinking"].as_str())
                    .collect();
                let stream_signature = events
                    .iter()
                    .find(|event| event["delta"]["type"] == "signature_delta")
                    .and_then(|event| event["delta"]["signature"].as_str());
                assert_eq!(json_response["content"][0]["thinking"], stream_thinking);
                assert_eq!(
                    json_response["content"][0]["signature"].as_str(),
                    stream_signature
                );
                if model == "gpt-scalar-reasoning-valid" {
                    assert_eq!(
                        json_response["content"][0]["thinking"],
                        ["summary", "content"].join(REASONING_SUMMARY_SEPARATOR)
                    );
                    assert_eq!(
                        json_response["content"][0]["signature"],
                        "opaque@reasoning-scalar"
                    );
                } else {
                    assert_eq!(json_response["content"][0]["thinking"], THINKING_TEXT);
                    assert_eq!(
                        json_response["content"][0]["signature"],
                        "cm1#opaque-compaction@"
                    );
                }
            }
            "gpt-scalar-tool-search-output-valid" => {
                assert_eq!(json_response["content"], json!([]));
                assert!(!events
                    .iter()
                    .any(|event| event["type"] == "content_block_start"));
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_json_and_sse_reject_equivalent_malformed_outputs() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for model in [
        "gpt-scalar-function-added-missing",
        "gpt-scalar-custom-tool-malformed",
        "gpt-scalar-tool-search-missing-execution",
        "gpt-scalar-tool-search-empty-call-id",
        "gpt-scalar-tool-search-wrong",
        "gpt-scalar-tool-search-output-malformed",
        "gpt-scalar-message-incomplete-on-completed",
        "gpt-scalar-message-wrong-role",
        "gpt-scalar-message-block-malformed",
        "gpt-scalar-reasoning-missing-summary",
        "gpt-scalar-reasoning-wrong-id",
        "gpt-scalar-compaction-missing",
        "gpt-scalar-metadata-wrong",
        "gpt-contract-completed-empty-id",
        "gpt-contract-created-wrong-model",
        "gpt-contract-terminal-wrong-end-turn",
        "gpt-contract-usage-wrong-input",
        "gpt-contract-usage-total-mismatch",
    ] {
        let request = |stream| {
            post_json(
                "/v1/messages",
                json!({
                    "model":format!("responses-fixture/{model}"),
                    "max_tokens":128,
                    "messages":[{"role":"user","content":"paired malformed output"}],
                    "stream":stream
                }),
                Some(CLIENT_KEY),
            )
        };
        let (json_status, json_body_bytes) = send(request(false)).await;
        assert!(
            json_status.is_server_error(),
            "{model}: malformed JSON result returned {json_status}: {}",
            String::from_utf8_lossy(&json_body_bytes)
        );
        let json_error = json_body(&json_body_bytes);
        assert_eq!(json_error["type"], "error", "{model}");

        let (sse_status, sse_body) = send(request(true)).await;
        assert_eq!(sse_status, StatusCode::OK, "{model}");
        let events = data_events(&sse_body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {events:#?}"
        );
        assert!(!events.iter().any(|event| {
            matches!(
                event["type"].as_str(),
                Some("message_delta" | "message_stop")
            )
        }));
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_raw_output_variants_fail_explicitly_in_json_and_sse() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, _) in audited_raw_output_variants() {
        let request = |stream| {
            post_json(
                "/v1/messages",
                json!({
                    "model":format!("responses-fixture/{model}"),
                    "max_tokens":128,
                    "messages":[{"role":"user","content":"raw output"}],
                    "stream":stream
                }),
                Some(CLIENT_KEY),
            )
        };
        let (json_status, json_bytes) = send(request(false)).await;
        assert!(
            json_status.is_server_error(),
            "{model}: {json_status} {}",
            String::from_utf8_lossy(&json_bytes)
        );
        let json_error = json_body(&json_bytes);
        assert_eq!(json_error["type"], "error", "{model}");
        assert_eq!(json_error["error"]["type"], "api_error", "{model}");
        assert!(json_error.get("stop_reason").is_none(), "{model}");

        let (sse_status, sse_bytes) = send(request(true)).await;
        assert_eq!(sse_status, StatusCode::OK, "{model}");
        let events = data_events(&sse_bytes);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {events:#?}"
        );
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("error"),
            "{model}"
        );
        assert!(!events.iter().any(|event| {
            matches!(
                event["type"].as_str(),
                Some("content_block_start" | "message_delta" | "message_stop")
            )
        }));
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_tool_search_identity_merges_omission_and_rejects_conflicts() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, initial_call_id) in [("gpt-scalar-tool-search-conflicting-id", "search-call-a")] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("responses-fixture/{model}"),
                "max_tokens":128,
                "messages":[{"role":"user","content":"late identity corruption"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        let event_types: Vec<&str> = events
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect();
        assert_eq!(
            event_types,
            [
                "message_start",
                "content_block_start",
                "content_block_stop",
                "error"
            ],
            "{model}: {events:#?}"
        );
        assert_eq!(events[1]["content_block"]["id"], initial_call_id, "{model}");
        assert_eq!(events[3]["error"]["type"], "api_error", "{model}");
    }

    let (status, body) = send(post_json(
        "/v1/messages",
        json!({
            "model":"responses-fixture/gpt-scalar-tool-search-removed-id",
            "max_tokens":128,
            "messages":[{"role":"user","content":"partial identity"}],
            "stream":true
        }),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    assert!(!events.iter().any(|event| event["type"] == "error"));
    let start = events
        .iter()
        .find(|event| event["type"] == "content_block_start")
        .expect("tool block");
    assert_eq!(start["content_block"]["id"], "search-call-present");
    assert_eq!(
        events.last().and_then(|event| event["type"].as_str()),
        Some("message_stop")
    );
}

fn web_search_messages_request(stream: bool) -> Request<Body> {
    post_json(
        "/v1/messages",
        json!({
            "model":"responses-fixture/gpt-fixture",
            "max_tokens":128,
            "messages":[{"role":"user","content":"Search for Rust async sources"}],
            "tools":[{
                "type":"web_search_20250305",
                "name":"web_search",
                "max_uses":3
            }],
            "stream":stream
        }),
        Some(CLIENT_KEY),
    )
}

fn web_search_policy_request(tool: Value) -> Request<Body> {
    post_json(
        "/v1/messages",
        json!({
            "model":"claude-sonnet-4-6",
            "max_tokens":128,
            "messages":[{"role":"user","content":"Search with this policy"}],
            "tools":[tool],
            "stream":false
        }),
        Some(CLIENT_KEY),
    )
}

fn assert_anthropic_invalid_request(body: &[u8], label: &str) {
    let error = json_body(body);
    assert_eq!(error.as_object().map(Map::len), Some(2), "{label}");
    assert_eq!(error["type"], "error", "{label}");
    assert_eq!(error["error"].as_object().map(Map::len), Some(2), "{label}");
    assert_eq!(error["error"]["type"], "invalid_request_error", "{label}");
    assert!(error["error"]["message"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
}

fn assert_anthropic_upstream_error(body: &[u8], label: &str) {
    let error = json_body(body);
    assert_eq!(error["type"], "error", "{label}");
    assert_eq!(error["error"]["type"], "api_error", "{label}");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "{label}"
    );
    assert!(error.get("id").is_none(), "{label} fabricated success");
    assert!(error.get("content").is_none(), "{label} fabricated content");
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_request_policy_rejects_malformed_before_dispatch() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure_with_web_search_model(
        &fixture,
        Some("responses-fixture/gpt-web-partial-completed"),
    );

    for (label, tool) in [
        (
            "allowed scalar",
            json!({"type":"web_search_20250305","name":"web_search","allowed_domains":"x.test"}),
        ),
        (
            "allowed mixed",
            json!({"type":"web_search_20250305","name":"web_search","allowed_domains":[42,"x.test"]}),
        ),
        (
            "allowed blank",
            json!({"type":"web_search_20250305","name":"web_search","allowed_domains":["  "]}),
        ),
        (
            "blocked scalar",
            json!({"type":"web_search_20250305","name":"web_search","blocked_domains":{"x":true}}),
        ),
        (
            "blocked mixed",
            json!({"type":"web_search_20250305","name":"web_search","blocked_domains":["x.test",null]}),
        ),
        (
            "both domain policies",
            json!({
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["allowed.test"],
                "blocked_domains":["blocked.test"]
            }),
        ),
        (
            "location scalar",
            json!({"type":"web_search_20250305","name":"web_search","user_location":"US"}),
        ),
        (
            "location missing type",
            json!({"type":"web_search_20250305","name":"web_search","user_location":{"country":"US"}}),
        ),
        (
            "location wrong type",
            json!({"type":"web_search_20250305","name":"web_search","user_location":{"type":"exact","country":"US"}}),
        ),
        (
            "location empty",
            json!({"type":"web_search_20250305","name":"web_search","user_location":{"type":"approximate"}}),
        ),
        (
            "location field type",
            json!({"type":"web_search_20250305","name":"web_search","user_location":{"type":"approximate","city":42}}),
        ),
        (
            "location blank",
            json!({"type":"web_search_20250305","name":"web_search","user_location":{"type":"approximate","city":"  "}}),
        ),
        (
            "country code",
            json!({"type":"web_search_20250305","name":"web_search","user_location":{"type":"approximate","country":"USA"}}),
        ),
        (
            "allowed callers mixed",
            json!({"type":"web_search_20250305","name":"web_search","allowed_callers":["direct",7]}),
        ),
        (
            "allowed callers unsupported",
            json!({"type":"web_search_20250305","name":"web_search","allowed_callers":["direct"]}),
        ),
        (
            "response inclusion unsupported",
            json!({"type":"web_search_20250305","name":"web_search","response_inclusion":"full"}),
        ),
        (
            "strict type",
            json!({"type":"web_search_20250305","name":"web_search","strict":"yes"}),
        ),
        (
            "max uses",
            json!({"type":"web_search_20250305","name":"web_search","max_uses":0}),
        ),
    ] {
        let before = fixture.requests().len();
        let (status, body) = send(web_search_policy_request(tool)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_anthropic_invalid_request(&body, label);
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_request_policy_preserves_valid_empty_duplicate_and_unknown_values() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure_with_web_search_model(
        &fixture,
        Some("responses-fixture/gpt-web-partial-completed"),
    );

    let (status, _) = send(web_search_policy_request(json!({
        "type":"web_search_20250305",
        "name":"web_search",
        "allowed_domains":["b.example/path","a.example","b.example/path"],
        "blocked_domains":null,
        "user_location":{
            "type":"approximate",
            "city":"Seattle",
            "country":"US",
            "future_location_key":{"keep":true}
        },
        "future_tool_key":{"keep":true}
    })))
    .await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("web-search policy capture");
    assert_eq!(
        capture.body["tools"][0]["filters"]["allowed_domains"],
        json!(["b.example/path", "a.example", "b.example/path"])
    );
    assert_eq!(capture.body["tools"][0]["user_location"]["city"], "Seattle");
    assert_eq!(capture.body["tools"][0]["user_location"]["country"], "US");
    assert_eq!(
        capture.body["tools"][0]["user_location"]["future_location_key"]["keep"],
        true
    );
    assert_eq!(capture.body["tools"][0]["future_tool_key"]["keep"], true);

    let before = fixture.requests().len();
    let (status, _) = send(web_search_policy_request(json!({
        "type":"web_search_20250305",
        "name":"web_search",
        "allowed_domains":[],
        "blocked_domains":[],
        "user_location":null
    })))
    .await;
    assert_eq!(status, StatusCode::OK);
    let captures = fixture.requests();
    assert_eq!(captures.len(), before + 1);
    let capture = captures.last().expect("empty web-search policy capture");
    assert!(capture.body["tools"][0].get("filters").is_none());
    assert!(capture.body["tools"][0].get("user_location").is_none());

    let (status, _) = send(web_search_policy_request(json!({
        "type":"web_search_20250305",
        "name":"web_search",
        "allowed_domains":null,
        "blocked_domains":["b.example","a.example","b.example"]
    })))
    .await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("blocked-domain policy capture");
    assert_eq!(
        capture.body["tools"][0]["filters"]["blocked_domains"],
        json!(["b.example", "a.example", "b.example"])
    );
}

fn base_provider_messages_body() -> Value {
    json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "messages":[{"role":"user","content":"Validate this request"}],
        "stream":false
    })
}

fn tool_result_messages_body(tool_result: Value) -> Value {
    json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "messages":[
            {
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":"call",
                    "name":"tool",
                    "input":{}
                }]
            },
            {"role":"user","content":[tool_result]}
        ],
        "stream":false
    })
}

fn deferred_reference_body(content: Option<Value>, defer_loading: bool) -> Value {
    let mut tool_result = json!({
        "type":"tool_result",
        "tool_use_id":"search-call",
        "is_error":false,
        "future_tool_result":{"keep":true}
    });
    if let Some(content) = content {
        tool_result["content"] = content;
    }
    json!({
        "model":"responses-fixture/gpt-5.4",
        "max_tokens":128,
        "tools":[
            {
                "name":"mcp__tool_search__search",
                "description":"Load deferred tools",
                "input_schema":{
                    "type":"object",
                    "properties":{"names":{"type":"array","items":{"type":"string"}}},
                    "required":["names"]
                }
            },
            {
                "name":"mcp__weather",
                "description":"Get weather",
                "defer_loading":defer_loading,
                "input_schema":{
                    "type":"object",
                    "properties":{"city":{"type":"string"}},
                    "required":["city"],
                    "future_schema_key":{"keep":true}
                },
                "future_tool_key":{"keep":true}
            },
            {
                "name":"mcp__forecast",
                "description":"Get forecast",
                "defer_loading":true,
                "input_schema":{
                    "type":"object",
                    "properties":{"days":{"type":"integer"}},
                    "required":["days"]
                }
            }
        ],
        "messages":[
            {
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":"search-call",
                    "name":"mcp__tool_search__search",
                    "input":{"names":["mcp__weather"]},
                    "future_tool_use":{"keep":true}
                }]
            },
            {"role":"user","content":[tool_result]}
        ],
        "stream":false
    })
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_known_request_collections_fail_closed_before_provider_dispatch() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let mut invalid = Vec::new();
    let mut body = base_provider_messages_body();
    body["tools"] = json!("wrong");
    invalid.push(("tools scalar", body));
    let mut body = base_provider_messages_body();
    body["tools"] = json!([1]);
    invalid.push(("tool entry scalar", body));
    let mut body = base_provider_messages_body();
    body["tools"] = json!([{"input_schema":{"type":"object"}}]);
    invalid.push(("tool missing name", body));
    let mut body = base_provider_messages_body();
    body["tools"] = json!([{"name":"broken","input_schema":"wrong"}]);
    invalid.push(("tool schema scalar", body));
    let mut body = base_provider_messages_body();
    body["tools"] = json!([{
        "name":"broken",
        "input_schema":{"type":"object","required":["ok",4]}
    }]);
    invalid.push(("schema required mixed", body));
    let mut body = base_provider_messages_body();
    body["tools"] = json!([{
        "name":"broken",
        "input_schema":{"type":"object","required":null}
    }]);
    invalid.push(("schema required null", body));
    let mut body = base_provider_messages_body();
    body["tools"] = json!([
        {"name":"duplicate","input_schema":{"type":"object"}},
        {"name":"duplicate","input_schema":{"type":"object"}}
    ]);
    invalid.push(("duplicate tool names", body));
    let mut body = base_provider_messages_body();
    body["tools"] = json!([{
        "name":"broken",
        "defer_loading":"yes",
        "input_schema":{"type":"object"}
    }]);
    invalid.push(("defer loading type", body));
    let mut body = base_provider_messages_body();
    body["tool_choice"] = json!({"type":"future"});
    invalid.push(("tool choice type", body));
    let mut body = base_provider_messages_body();
    body["tool_choice"] = json!({"type":"tool"});
    invalid.push(("tool choice missing name", body));
    let mut body = base_provider_messages_body();
    body["system"] = json!({"type":"text","text":"wrong container"});
    invalid.push(("system object", body));
    let mut body = base_provider_messages_body();
    body["system"] = json!([{"type":"text","text":7}]);
    invalid.push(("system text type", body));
    let mut body = base_provider_messages_body();
    body["metadata"] = json!("wrong");
    invalid.push(("metadata scalar", body));
    let mut body = base_provider_messages_body();
    body["metadata"] = json!({"user_id":7});
    invalid.push(("metadata user id", body));
    let mut body = base_provider_messages_body();
    body["thinking"] = json!("wrong");
    invalid.push(("thinking scalar", body));
    let mut body = base_provider_messages_body();
    body["thinking"] = json!({"type":"future"});
    invalid.push(("thinking type", body));
    let mut body = base_provider_messages_body();
    body["thinking"] = json!({"type":"enabled","budget_tokens":0});
    invalid.push(("thinking budget", body));
    let mut body = base_provider_messages_body();
    body["output_config"] = json!({"effort":7});
    invalid.push(("output config effort", body));
    let mut body = base_provider_messages_body();
    body["stop_sequences"] = json!(["stop", 7]);
    invalid.push(("stop sequences mixed", body));
    let mut body = base_provider_messages_body();
    body["cache_control"] = json!("wrong");
    invalid.push(("top cache control", body));
    let mut body = base_provider_messages_body();
    body["messages"][0]["content"] = json!({"type":"text","text":"wrong container"});
    invalid.push(("message content object", body));
    let mut body = base_provider_messages_body();
    body["messages"][0]["content"] = json!([1]);
    invalid.push(("content block scalar", body));
    let mut body = base_provider_messages_body();
    body["messages"][0]["content"] = json!([{"type":"text","text":7}]);
    invalid.push(("text value type", body));
    let mut body = base_provider_messages_body();
    body["messages"][0]["content"] = json!([{"type":"future_content","value":true}]);
    invalid.push(("unsupported content variant", body));
    let mut body = base_provider_messages_body();
    body["messages"][0]["content"] = json!([{"type":"image","source":"not-an-object"}]);
    invalid.push(("image source scalar", body));
    let mut body = base_provider_messages_body();
    body["messages"][0]["content"] = json!([{
        "type":"document",
        "title":7,
        "source":{"type":"url","url":"https://example.test/doc.pdf"}
    }]);
    invalid.push(("document title type", body));
    let mut body = base_provider_messages_body();
    body["messages"] = json!([{
        "role":"assistant",
        "content":[{"type":"tool_use","name":"tool","input":{}}]
    }]);
    invalid.push(("tool use missing id", body));
    let mut body = base_provider_messages_body();
    body["messages"] = json!([{
        "role":"assistant",
        "content":[{"type":"tool_use","id":"call","name":"tool","input":"wrong"}]
    }]);
    invalid.push(("tool use input type", body));
    let body = tool_result_messages_body(json!({
        "type":"tool_result",
        "content":"missing id"
    }));
    invalid.push(("tool result missing id", body));
    let body = tool_result_messages_body(json!({
        "type":"tool_result",
        "tool_use_id":"call",
        "is_error":"wrong",
        "content":"result"
    }));
    invalid.push(("tool result error type", body));
    let body = tool_result_messages_body(json!({
        "type":"tool_result",
        "tool_use_id":"call",
        "content":42
    }));
    invalid.push(("tool result content type", body));
    let body = tool_result_messages_body(json!({
        "type":"tool_result",
        "tool_use_id":"call",
        "content":[{"type":"future_result","value":true}]
    }));
    invalid.push(("tool result content variant", body));
    let body = tool_result_messages_body(json!({
        "type":"tool_result",
        "tool_use_id":"unknown",
        "content":"result"
    }));
    invalid.push(("tool result unknown id", body));
    let mut body = base_provider_messages_body();
    body["messages"][0]["content"] = json!([{
        "type":"text",
        "text":"cache",
        "cache_control":{"type":"wrong"}
    }]);
    invalid.push(("cache control type", body));
    let mut body = base_provider_messages_body();
    body["messages"] = json!([{
        "role":"assistant",
        "content":[{"type":"thinking","thinking":"x","signature":7}]
    }]);
    invalid.push(("thinking signature type", body));

    for (label, body) in invalid {
        let before = fixture.requests().len();
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_anthropic_invalid_request(&response, label);
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

fn tool_schema_messages_body(model: &str, schema: Value, tool_choice: Option<Value>) -> Value {
    let mut body = json!({
        "model":format!("responses-fixture/{model}"),
        "max_tokens":128,
        "tools":[{
            "name":"selected_tool",
            "description":"Selected tool",
            "input_schema":schema,
            "future_tool_key":{"keep":true}
        }],
        "messages":[{"role":"user","content":"Use the selected tool"}],
        "stream":false
    });
    if let Some(tool_choice) = tool_choice {
        body["tool_choice"] = tool_choice;
    }
    body
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_tool_choice_must_resolve_to_one_compatible_declared_tool() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let function_tool = json!({
        "name":"actual",
        "input_schema":{"type":"object"}
    });
    for (label, body) in [
        (
            "undefined function",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[function_tool.clone()],
                "tool_choice":{"type":"tool","name":"missing"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "deferred function",
            json!({
                "model":"responses-fixture/gpt-5.4",
                "max_tokens":128,
                "tools":[{
                    "name":"mcp__deferred",
                    "defer_loading":true,
                    "input_schema":{"type":"object"}
                }],
                "tool_choice":{"type":"tool","name":"mcp__deferred"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "server tool",
            json!({
                "model":"claude-sonnet-4-6",
                "max_tokens":128,
                "tools":[{"type":"web_search_20250305","name":"web_search"}],
                "tool_choice":{"type":"tool","name":"web_search"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "unknown server kind",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[{"type":"future_server","name":"future"}],
                "tool_choice":{"type":"tool","name":"future"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "bridge without deferred tool",
            json!({
                "model":"responses-fixture/gpt-5.4",
                "max_tokens":128,
                "tools":[{
                    "name":"mcp__tool_search__search",
                    "input_schema":{"type":"object"}
                }],
                "tool_choice":{"type":"tool","name":"mcp__tool_search__search"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "bridge unsupported model",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[
                    {
                        "name":"mcp__tool_search__search",
                        "input_schema":{"type":"object"},
                        "future_bridge":{"keep":true}
                    },
                    {
                        "name":"mcp__deferred",
                        "defer_loading":true,
                        "input_schema":{"type":"object"}
                    }
                ],
                "tool_choice":{"type":"tool","name":"mcp__tool_search__search"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "bridge choice extension",
            json!({
                "model":"responses-fixture/gpt-5.4",
                "max_tokens":128,
                "tools":[
                    {
                        "name":"mcp__tool_search__search",
                        "input_schema":{"type":"object"},
                        "future_bridge":{"keep":true}
                    },
                    {
                        "name":"mcp__deferred",
                        "defer_loading":true,
                        "input_schema":{"type":"object"}
                    }
                ],
                "tool_choice":{
                    "type":"tool",
                    "name":"mcp__tool_search__search",
                    "future_choice":true
                },
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "auto with name",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[function_tool.clone()],
                "tool_choice":{"type":"auto","name":"actual"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "any with name",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[function_tool.clone()],
                "tool_choice":{"type":"any","name":"actual"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "none with name",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[function_tool.clone()],
                "tool_choice":{"type":"none","name":"actual"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
        (
            "duplicate ambiguous catalog",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[function_tool.clone(),function_tool],
                "tool_choice":{"type":"tool","name":"actual"},
                "messages":[{"role":"user","content":"choose"}]
            }),
        ),
    ] {
        let before = fixture.requests().len();
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_anthropic_invalid_request(&response, label);
        let expected_path = if label == "duplicate ambiguous catalog" {
            "tools[1].name"
        } else {
            "tool_choice"
        };
        assert!(json_body(&response)["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(expected_path)));
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_open_object_extension_collisions_fail_before_provider_dispatch() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let mut cases = Vec::new();
    let mut body = tool_schema_messages_body("gpt-fixture", json!({"type":"object"}), None);
    body["tools"][0]["parameters"] = json!({"override":true});
    cases.push(("function parameters collision", body));
    let body = json!({
        "model":"responses-fixture/gpt-5.4",
        "max_tokens":128,
        "tools":[{
            "name":"mcp__deferred",
            "defer_loading":true,
            "input_schema":{"type":"object"},
            "tools":[]
        }],
        "messages":[{"role":"user","content":"collision"}]
    });
    cases.push(("deferred tools collision", body));
    let body = json!({
        "model":"responses-fixture/gpt-5.4",
        "max_tokens":128,
        "tools":[{
            "name":"mcp__tool_search__search",
            "input_schema":{"type":"object"},
            "execution":"server"
        }],
        "messages":[{"role":"user","content":"collision"}]
    });
    cases.push(("bridge execution collision", body));
    let body = json!({
        "model":"claude-sonnet-4-6",
        "max_tokens":128,
        "tools":[{
            "type":"web_search_20250305",
            "name":"web_search",
            "filters":{"override":true}
        }],
        "messages":[{"role":"user","content":"collision"}]
    });
    cases.push(("web filters collision", body));
    let body = tool_schema_messages_body(
        "gpt-fixture",
        json!({"type":"object"}),
        Some(json!({"type":"auto","future_choice":true})),
    );
    cases.push(("scalar choice extension", body));

    let body = json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "messages":[{
            "role":"user",
            "content":[{
                "type":"image",
                "image_url":"collision",
                "source":{"type":"url","url":"https://example.test/image.png"}
            }]
        }]
    });
    cases.push(("image canonical collision", body));
    let body = json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "messages":[{
            "role":"assistant",
            "content":[{
                "type":"tool_use",
                "id":"call",
                "name":"tool",
                "input":{},
                "call_id":"collision"
            }]
        }]
    });
    cases.push(("tool use canonical collision", body));
    let body = tool_result_messages_body(json!({
        "type":"tool_result",
        "tool_use_id":"call",
        "content":"result",
        "output":"collision"
    }));
    cases.push(("tool result canonical collision", body));
    let body = json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "messages":[{
            "role":"assistant",
            "content":[{
                "type":"thinking",
                "thinking":"analysis",
                "signature":"enc@id",
                "summary":"collision"
            }]
        }]
    });
    cases.push(("thinking block collision", body));
    let mut body = base_provider_messages_body();
    body["thinking"] = json!({"type":"enabled","budget_tokens":1024,"effort":"collision"});
    cases.push(("thinking config collision", body));
    let mut body = base_provider_messages_body();
    body["output_config"] = json!({"effort":"high","summary":"collision"});
    cases.push(("output config collision", body));
    let mut body = base_provider_messages_body();
    body["system"] = json!([{
        "type":"text",
        "text":"system",
        "future_system":{"cannot":"represent"}
    }]);
    cases.push(("system extension", body));

    for (label, body) in cases {
        let before = fixture.requests().len();
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_anthropic_invalid_request(&response, label);
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_payload_and_message_extensions_survive_split_responses_translation() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let body = json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "future_request_extension":{
            "first":1,
            "nested":{"keep":true,"null":null},
            "array":[null,{"x":1}],
            "last":2
        },
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[
            {
                "role":"assistant",
                "future_assistant_extension":{"keep":"assistant","null":null},
                "content":[
                    {"type":"text","text":"before tool"},
                    {"type":"tool_use","id":"call","name":"actual","input":{}},
                    {"type":"text","text":"after tool"}
                ]
            },
            {
                "role":"user",
                "future_user_extension":{"keep":"user","null":null},
                "content":[
                    {"type":"text","text":"before result"},
                    {"type":"tool_result","tool_use_id":"call","content":"done"},
                    {"type":"text","text":"after result"}
                ]
            }
        ]
    });
    let (status, _) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("split extension capture");
    assert_eq!(
        capture.body["future_request_extension"],
        json!({
            "first":1,
            "nested":{"keep":true,"null":null},
            "array":[null,{"x":1}],
            "last":2
        })
    );
    let request_keys: Vec<&str> = capture
        .body
        .as_object()
        .expect("captured Responses request object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        request_keys.last().copied(),
        Some("future_request_extension"),
        "open request extensions append after canonical Responses fields"
    );
    let extension_keys: Vec<&str> = capture.body["future_request_extension"]
        .as_object()
        .expect("nested request extension object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(extension_keys, ["first", "nested", "array", "last"]);
    let nested_extension_keys: Vec<&str> = capture.body["future_request_extension"]["nested"]
        .as_object()
        .expect("nested extension value")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(nested_extension_keys, ["keep", "null"]);
    let input = capture.body["input"].as_array().expect("translated input");
    let assistant_messages: Vec<&Value> = input
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "assistant")
        .collect();
    let user_messages: Vec<&Value> = input
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .collect();
    assert_eq!(assistant_messages.len(), 2);
    assert_eq!(user_messages.len(), 2);
    assert!(assistant_messages.iter().all(|message| {
        message["future_assistant_extension"] == json!({"keep":"assistant","null":null})
    }));
    assert!(user_messages
        .iter()
        .all(|message| { message["future_user_extension"] == json!({"keep":"user","null":null}) }));
    assert!(assistant_messages.iter().all(|message| {
        message
            .as_object()
            .and_then(|message| message.keys().next_back())
            .is_some_and(|key| key == "future_assistant_extension")
    }));
    assert!(user_messages.iter().all(|message| {
        message
            .as_object()
            .and_then(|message| message.keys().next_back())
            .is_some_and(|key| key == "future_user_extension")
    }));
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_payload_and_message_extension_collisions_fail_without_dispatch() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    for (label, body) in [
        (
            "request input collision",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "input":{"override":true},
                "messages":[{"role":"user","content":"collision"}]
            }),
        ),
        (
            "request stop bypass",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "stop":["bypass"],
                "messages":[{"role":"user","content":"collision"}]
            }),
        ),
        (
            "user message phase collision",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "messages":[{
                    "role":"user",
                    "content":"collision",
                    "phase":"override"
                }]
            }),
        ),
        (
            "assistant message status collision",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "messages":[{
                    "role":"assistant",
                    "content":"collision",
                    "status":"override"
                }]
            }),
        ),
        (
            "tool-only assistant extension",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[{
                    "name":"actual",
                    "input_schema":{"type":"object"}
                }],
                "messages":[{
                    "role":"assistant",
                    "future_assistant_extension":{"cannot_move":true},
                    "content":[{
                        "type":"tool_use",
                        "id":"call",
                        "name":"actual",
                        "input":{}
                    }]
                }]
            }),
        ),
        (
            "tool-result-only user extension",
            json!({
                "model":"responses-fixture/gpt-fixture",
                "max_tokens":128,
                "tools":[{
                    "name":"actual",
                    "input_schema":{"type":"object"}
                }],
                "messages":[
                    {
                        "role":"assistant",
                        "content":[{
                            "type":"tool_use",
                            "id":"call",
                            "name":"actual",
                            "input":{}
                        }]
                    },
                    {
                        "role":"user",
                        "future_user_extension":{"cannot_move":true},
                        "content":[{
                            "type":"tool_result",
                            "tool_use_id":"call",
                            "content":"done"
                        }]
                    }
                ]
            }),
        ),
    ] {
        let before = fixture.requests().len();
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
        assert_anthropic_invalid_request(&response, label);
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_stop_sequences_reject_responses_but_preserve_native_anthropic_support() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let responses_body = |stop_sequences: Value| {
        json!({
            "model":"responses-fixture/gpt-fixture",
            "max_tokens":128,
            "stop_sequences":stop_sequences,
            "messages":[{"role":"user","content":"stop policy"}]
        })
    };
    let before = fixture.requests().len();
    let (status, response) = send(post_json(
        "/v1/messages",
        responses_body(json!(["END"])),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_anthropic_invalid_request(&response, "Responses stop_sequences");
    assert!(json_body(&response)["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("stop_sequences")));
    assert_eq!(fixture.requests().len(), before);

    let before = fixture.requests().len();
    let (status, response) = send(post_json(
        "/responses-fixture/v1/messages",
        responses_body(json!(["DIRECT-END"])),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_anthropic_invalid_request(&response, "direct provider stop_sequences");
    assert_eq!(fixture.requests().len(), before);

    let mut codex_alias_body = responses_body(json!(["CODEX-END"]));
    codex_alias_body["model"] = json!("codex/gpt-fixture");
    let before = fixture.requests().len();
    let (status, response) = send(post_json(
        "/v1/messages",
        codex_alias_body,
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_anthropic_invalid_request(&response, "Codex alias stop_sequences");
    assert_eq!(fixture.requests().len(), before);

    let chat_body = json!({
        "model":"chat-fixture/gpt-chat-fixture",
        "max_tokens":128,
        "stop_sequences":["z-stop","a-stop","z-stop"],
        "messages":[{"role":"user","content":"chat stop policy"}],
        "stream":false
    });
    let (status, _) = send(post_json("/v1/messages", chat_body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/chat/completions")
        .expect("Chat Completions stop capture");
    assert_eq!(capture.body["stop"], json!(["z-stop", "a-stop", "z-stop"]));

    for empty in [Value::Null, json!([])] {
        let before = fixture.requests().len();
        let (status, _) = send(post_json(
            "/v1/messages",
            responses_body(empty),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let captures = fixture.requests();
        assert_eq!(captures.len(), before + 1);
        assert!(captures
            .last()
            .expect("Responses empty-stop capture")
            .body
            .get("stop_sequences")
            .is_none());
    }

    let native_body = json!({
        "model":"anthropic-fixture/claude-sonnet-4-6",
        "max_tokens":128,
        "stop_sequences":["z-stop","a-stop","z-stop"],
        "top_k":17,
        "cache_control":{"type":"ephemeral"},
        "service_tier":"standard_only",
        "temperature":0.25,
        "top_p":0.75,
        "messages":[{"role":"user","content":"native stop policy"}],
        "stream":false
    });
    let (status, _) = send(post_json("/v1/messages", native_body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/messages")
        .expect("native Anthropic stop capture");
    assert_eq!(
        capture.body["stop_sequences"],
        json!(["z-stop", "a-stop", "z-stop"])
    );
    assert_eq!(capture.body["top_k"], 17);
    assert_eq!(capture.body["cache_control"], json!({"type":"ephemeral"}));
    assert_eq!(capture.body["service_tier"], "standard_only");
    assert_eq!(capture.body["temperature"], 0.25);
    assert_eq!(capture.body["top_p"], 0.75);

    configure_with_web_search_model(&fixture, Some("anthropic-fixture/claude-sonnet-4-6"));
    let native_web_search_body = json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "stop_sequences":["NATIVE-WEB-END"],
        "tools":[{
            "type":"web_search_20250305",
            "name":"web_search"
        }],
        "messages":[{"role":"user","content":"native search"}]
    });
    let (status, _) = send(post_json(
        "/v1/messages",
        native_web_search_body,
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/messages")
        .expect("native web-search stop capture");
    assert_eq!(capture.body["stop_sequences"], json!(["NATIVE-WEB-END"]));

    configure_with_web_search_model(&fixture, Some("responses-fixture/gpt-fixture"));
    let web_search_body = json!({
        "model":"anthropic-fixture/claude-sonnet-4-6",
        "max_tokens":128,
        "stop_sequences":["WEB-END"],
        "tools":[{
            "type":"web_search_20250305",
            "name":"web_search"
        }],
        "messages":[{"role":"user","content":"search"}]
    });
    let before = fixture.requests().len();
    let (status, response) =
        send(post_json("/v1/messages", web_search_body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_anthropic_invalid_request(&response, "web-search stop_sequences");
    assert_eq!(fixture.requests().len(), before);
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_responses_controls_preserve_supported_and_reject_unrepresentable_values() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let base = || {
        json!({
            "model":"responses-fixture/gpt-fixture",
            "max_tokens":128,
            "messages":[{"role":"user","content":"control policy"}]
        })
    };

    for (label, field, value) in [
        ("top_k", "top_k", json!(17)),
        (
            "top-level cache_control",
            "cache_control",
            json!({"type":"ephemeral"}),
        ),
        ("service_tier", "service_tier", json!("standard_only")),
    ] {
        let mut body = base();
        body[field] = value;
        let before = fixture.requests().len();
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
        assert_anthropic_invalid_request(&response, label);
        assert!(json_body(&response)["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(field)));
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }

    let mut supported = base();
    supported["temperature"] = json!(0.25);
    supported["top_p"] = json!(0.75);
    let (status, _) = send(post_json("/v1/messages", supported, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("supported Responses control capture");
    assert_eq!(capture.body["temperature"], 0.25);
    assert_eq!(capture.body["top_p"], 0.75);

    for field in ["temperature", "top_p"] {
        let mut body = base();
        body["model"] = json!("codex/gpt-fixture");
        body[field] = json!(0.25);
        let before = fixture.requests().len();
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "Codex {field}");
        assert_anthropic_invalid_request(&response, &format!("Codex {field}"));
        assert!(json_body(&response)["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(field)));
        assert_eq!(
            fixture.requests().len(),
            before,
            "Codex {field} reached upstream"
        );
    }

    let mut direct_codex_body = base();
    direct_codex_body["temperature"] = json!(0.25);
    let before = fixture.requests().len();
    let (status, response) = send(post_json(
        "/codex/v1/messages",
        direct_codex_body,
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_anthropic_invalid_request(&response, "direct Codex temperature");
    assert_eq!(fixture.requests().len(), before);

    let mut null_controls = base();
    null_controls["top_k"] = Value::Null;
    null_controls["cache_control"] = Value::Null;
    null_controls["service_tier"] = Value::Null;
    null_controls["temperature"] = Value::Null;
    let (status, _) = send(post_json("/v1/messages", null_controls, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("null Responses controls capture");
    assert_eq!(capture.body["temperature"], 1.0);
    assert!(capture.body.get("top_k").is_none());
    assert!(capture.body.get("cache_control").is_none());
    assert!(capture.body.get("service_tier").is_none());
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_chat_extensions_preserve_scope_nulls_order_and_split_messages() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let body = json!({
        "model":"chat-fixture/gpt-chat-fixture",
        "max_tokens":128,
        "top_k":4,
        "service_tier":"auto",
        "future_request_extension":{
            "first":1,
            "nested":{"keep":true,"null":null},
            "last":2
        },
        "system":[
            {
                "type":"text",
                "text":"system one",
                "future_system_extension":{"keep":"system","null":null}
            },
            {
                "type":"text",
                "text":"system two",
                "cache_control":{"type":"ephemeral","ttl":"1h"}
            }
        ],
        "metadata":{"user_id":"chat-user"},
        "output_config":{"effort":"high"},
        "tools":[{
            "name":"actual",
            "description":"actual tool",
            "input_schema":{"type":"object","properties":{}},
            "strict":true,
            "cache_control":{"type":"ephemeral"},
            "future_tool_extension":{"keep":"tool","null":null}
        }],
        "tool_choice":{
            "type":"tool",
            "name":"actual",
            "future_choice_extension":{"keep":"choice","null":null}
        },
        "messages":[
            {
                "role":"user",
                "content":"plain user",
                "future_plain_user":{"keep":"plain","null":null}
            },
            {
                "role":"assistant",
                "future_assistant_message":{"keep":"assistant","null":null},
                "content":[
                    {
                        "type":"text",
                        "text":"before tool",
                        "future_assistant_text":{"keep":"text","null":null}
                    },
                    {
                        "type":"tool_use",
                        "id":"call",
                        "name":"actual",
                        "input":{"value":1},
                        "cache_control":{"type":"ephemeral"},
                        "future_tool_use":{"keep":"call","null":null}
                    }
                ]
            },
            {
                "role":"user",
                "future_split_user":{"keep":"split","null":null},
                "content":[
                    {
                        "type":"tool_result",
                        "tool_use_id":"call",
                        "is_error":false,
                        "cache_control":{"type":"ephemeral"},
                        "future_tool_result":{"keep":"result","null":null},
                        "content":[{
                            "type":"text",
                            "text":"done",
                            "future_result_text":{"keep":"result-text","null":null}
                        }]
                    },
                    {
                        "type":"text",
                        "text":"after result",
                        "future_user_text":{"keep":"user-text","null":null}
                    }
                ]
            }
        ]
    });
    let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response)
    );
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/chat/completions")
        .expect("Chat extension capture");

    assert_eq!(
        capture.body["future_request_extension"],
        json!({
            "first":1,
            "nested":{"keep":true,"null":null},
            "last":2
        })
    );
    let request_extension_keys: Vec<&str> = capture.body["future_request_extension"]
        .as_object()
        .expect("request extension object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(request_extension_keys, ["first", "nested", "last"]);
    let request_keys: Vec<&str> = capture
        .body
        .as_object()
        .expect("Chat request object")
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        request_keys
            .iter()
            .position(|key| *key == "future_request_extension")
            > request_keys.iter().position(|key| *key == "service_tier")
    );
    assert_eq!(capture.body["top_k"], 4);
    assert_eq!(capture.body["service_tier"], "auto");
    assert_eq!(capture.body["reasoning_effort"], "high");
    assert_eq!(capture.body["user"], "chat-user");
    assert_eq!(
        capture.body["tools"][0]["future_tool_extension"],
        json!({"keep":"tool","null":null})
    );
    assert_eq!(capture.body["tools"][0]["function"]["strict"], true);
    assert_eq!(
        capture.body["tool_choice"]["future_choice_extension"],
        json!({"keep":"choice","null":null})
    );

    let messages = capture.body["messages"]
        .as_array()
        .expect("Chat message array");
    let system = messages
        .iter()
        .find(|message| message["role"] == "system")
        .expect("structured system message");
    assert!(system["content"].is_array());
    assert_eq!(
        system["content"][0]["future_system_extension"],
        json!({"keep":"system","null":null})
    );
    assert_eq!(
        system["content"][1]["cache_control"],
        json!({"type":"ephemeral","ttl":"1h"})
    );

    let plain_user = messages
        .iter()
        .find(|message| message.get("future_plain_user").is_some())
        .expect("plain user extension");
    assert_eq!(
        plain_user["future_plain_user"],
        json!({"keep":"plain","null":null})
    );
    assert_eq!(
        plain_user
            .as_object()
            .and_then(|message| message.keys().next_back())
            .map(String::as_str),
        Some("future_plain_user")
    );

    let assistant = messages
        .iter()
        .find(|message| message.get("future_assistant_message").is_some())
        .expect("assistant extension");
    assert_eq!(
        assistant["future_assistant_message"],
        json!({"keep":"assistant","null":null})
    );
    assert_eq!(
        assistant["content"][0]["future_assistant_text"],
        json!({"keep":"text","null":null})
    );
    assert_eq!(
        assistant["tool_calls"][0]["future_tool_use"],
        json!({"keep":"call","null":null})
    );
    assert_eq!(
        assistant["tool_calls"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );

    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool result message");
    assert_eq!(tool["tool_call_id"], "call");
    assert_eq!(tool["is_error"], false);
    assert_eq!(
        tool["future_tool_result"],
        json!({"keep":"result","null":null})
    );
    assert_eq!(
        tool["content"][0]["future_result_text"],
        json!({"keep":"result-text","null":null})
    );
    assert!(
        tool.get("future_split_user").is_none(),
        "wrapper extension must not move onto the tool result"
    );

    let split_user = messages
        .iter()
        .find(|message| message.get("future_split_user").is_some())
        .expect("split user extension");
    assert_eq!(
        split_user["future_split_user"],
        json!({"keep":"split","null":null})
    );
    assert_eq!(
        split_user["content"][0]["future_user_text"],
        json!({"keep":"user-text","null":null})
    );

    let rich_body = json!({
        "model":"chat-fixture/gpt-chat-scalar-fixture",
        "max_tokens":128,
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[
            {
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":"rich-call",
                    "name":"actual",
                    "input":{}
                }]
            },
            {
                "role":"user",
                "future_rich_wrapper":{"keep":"moved","null":null},
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":"rich-call",
                    "future_rich_result":{"keep":"tool","null":null},
                    "content":[{
                        "type":"image",
                        "source":{
                            "type":"base64",
                            "media_type":"image/png",
                            "data":"AAAA"
                        }
                    }]
                }]
            }
        ]
    });
    let (status, response) = send(post_json("/v1/messages", rich_body, Some(CLIENT_KEY))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response)
    );
    let rich_capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/chat/completions")
        .expect("rich Chat split capture");
    let rich_messages = rich_capture.body["messages"]
        .as_array()
        .expect("rich Chat messages");
    let rich_tool = rich_messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("rich tool message");
    assert_eq!(
        rich_tool["future_rich_result"],
        json!({"keep":"tool","null":null})
    );
    assert!(rich_tool.get("future_rich_wrapper").is_none());
    let moved_user = rich_messages
        .iter()
        .find(|message| message.get("future_rich_wrapper").is_some())
        .expect("single moved user carrier");
    assert_eq!(
        moved_user["future_rich_wrapper"],
        json!({"keep":"moved","null":null})
    );
    assert_eq!(moved_user["content"][1]["type"], "image_url");
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_direct_chat_preprocessing_keeps_split_message_extension_carrier() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure_direct_copilot(&fixture);
    let body = json!({
        "model":"gpt-direct-chat-extensions",
        "max_tokens":128,
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[
            {
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":"call",
                    "name":"actual",
                    "input":{}
                }]
            },
            {
                "role":"user",
                "future_direct_split":{"keep":true,"null":null},
                "content":[
                    {
                        "type":"tool_result",
                        "tool_use_id":"call",
                        "content":"done"
                    },
                    {"type":"text","text":"after result"}
                ]
            }
        ]
    });
    let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response)
    );
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/chat/completions")
        .expect("direct Chat split capture");
    let messages = capture.body["messages"]
        .as_array()
        .expect("direct Chat messages");
    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("direct tool result");
    assert!(tool.get("future_direct_split").is_none());
    let user = messages
        .iter()
        .find(|message| message.get("future_direct_split").is_some())
        .expect("direct user extension carrier");
    assert_eq!(
        user["future_direct_split"],
        json!({"keep":true,"null":null})
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_chat_extensions_reject_collisions_and_unrepresentable_scopes() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let base = || {
        json!({
            "model":"chat-fixture/gpt-chat-fixture",
            "max_tokens":128,
            "messages":[{"role":"user","content":"hello"}]
        })
    };

    let mut cases = Vec::new();
    let mut body = base();
    body["stop"] = json!(["bypass"]);
    cases.push(("request stop collision", body));

    let mut body = base();
    body["messages"][0]["tool_call_id"] = json!("override");
    cases.push(("user message collision", body));

    let mut body = base();
    body["messages"][0] = json!({
        "role":"assistant",
        "content":"answer",
        "tool_calls":[]
    });
    cases.push(("assistant message collision", body));

    let mut body = base();
    body["metadata"] = json!({"user_id":"user","future_metadata":{"drop":true}});
    cases.push(("metadata extension", body));

    let mut body = base();
    body["thinking"] =
        json!({"type":"enabled","budget_tokens":1024,"future_thinking":{"drop":true}});
    cases.push(("thinking config extension", body));

    let mut body = base();
    body["output_config"] = json!({"effort":"high","future_output":{"drop":true}});
    cases.push(("output config extension", body));

    let mut body = base();
    body["cache_control"] = json!({"type":"ephemeral"});
    cases.push(("top-level cache control", body));

    let mut body = base();
    body["system"] = json!([{
        "type":"text",
        "text":"ordinary system",
        "future_system":{"keep":true}
    }]);
    body["messages"][0]["content"] = json!([{
        "type":"document",
        "source":{
            "type":"base64",
            "media_type":"application/pdf",
            "data":"AAAA"
        },
        "future_document":{"cannot_flatten":true}
    }]);
    cases.push(("document fallback extension", body));

    let mut body = base();
    body["tools"] = json!([{
        "name":"actual",
        "input_schema":{"type":"object"},
        "function":{"override":true}
    }]);
    cases.push(("tool canonical collision", body));

    let mut body = base();
    body["tools"] = json!([{
        "name":"actual",
        "input_schema":{"type":"object"}
    }]);
    body["tool_choice"] = json!({
        "type":"tool",
        "name":"actual",
        "function":{"override":true}
    });
    cases.push(("tool choice canonical collision", body));

    let mut body = base();
    body["tools"] = json!([{
        "name":"actual",
        "input_schema":{"type":"object"},
        "defer_loading":true
    }]);
    cases.push(("deferred tool", body));

    let mut body = base();
    body["messages"][0]["content"] = json!([{
        "type":"image",
        "source":{
            "type":"base64",
            "media_type":"image/png",
            "data":"AAAA",
            "url":"https://invalid.example/image.png"
        }
    }]);
    cases.push(("inconsistent image source", body));

    let body = json!({
        "model":"chat-fixture/gpt-chat-fixture",
        "max_tokens":128,
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[
            {
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":"call",
                    "name":"actual",
                    "input":{}
                }]
            },
            {
                "role":"user",
                "future_tool_result_wrapper":{"cannot_move":true},
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":"call",
                    "content":"done"
                }]
            }
        ]
    });
    cases.push(("tool-result-only wrapper extension", body));

    let body = json!({
        "model":"chat-fixture/gpt-chat-fixture",
        "max_tokens":128,
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[
            {
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":"call",
                    "name":"actual",
                    "input":{}
                }]
            },
            {
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":"call",
                    "role":"assistant",
                    "content":"done"
                }]
            }
        ]
    });
    cases.push(("tool result message collision", body));

    let body = json!({
        "model":"chat-fixture/gpt-chat-fixture",
        "max_tokens":128,
        "messages":[{
            "role":"assistant",
            "content":[{
                "type":"thinking",
                "thinking":"reason",
                "signature":"sig",
                "future_thinking_block":{"cannot_move":true}
            }]
        }]
    });
    cases.push(("thinking block extension", body));

    let body = json!({
        "model":"chat-fixture/gpt-chat-scalar-fixture",
        "max_tokens":128,
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[
            {
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":"call",
                    "name":"actual",
                    "input":{}
                }]
            },
            {
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":"call",
                    "content":[{
                        "type":"text",
                        "text":"done",
                        "future_result_text":{"cannot_flatten":true}
                    }]
                }]
            }
        ]
    });
    cases.push(("scalar tool content extension", body));

    for (label, body) in cases {
        let before = fixture.requests().len();
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_anthropic_invalid_request(&response, label);
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_chat_response_extensions_and_usage_survive_provider_boundary() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let body = json!({
        "model":"chat-fixture/gpt-chat-response-extras",
        "max_tokens":128,
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[{"role":"user","content":"respond with extras"}]
    });
    let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response)
    );
    let response = json_body(&response);
    assert_eq!(response["id"], "chatcmpl-extras");
    assert_eq!(response["type"], "message");
    assert_eq!(response["stop_reason"], "tool_use");
    assert_eq!(response["object"], "chat.completion");
    assert_eq!(response["created"], 7);
    assert_eq!(response["future_before"], Value::Null);
    assert_eq!(response["service_tier"], "default");
    assert_eq!(response["system_fingerprint"], Value::Null);
    assert_eq!(response["future_after"], json!({"keep":true,"null":null}));
    assert_eq!(
        response["chat_choice_extensions"],
        json!({"future_choice":{"keep":true,"null":null}})
    );
    assert_eq!(
        response["chat_message_extensions"],
        json!({
            "annotations":[{
                "type":"url_citation",
                "url":"https://example.test"
            }],
            "audio":null,
            "future_message":{"keep":true,"null":null}
        })
    );
    let keys: Vec<&str> = response
        .as_object()
        .expect("Anthropic Chat response object")
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        keys.iter().position(|key| *key == "object")
            < keys.iter().position(|key| *key == "future_after")
    );
    assert!(
        keys.iter().position(|key| *key == "future_after")
            < keys.iter().position(|key| *key == "chat_choice_extensions")
    );

    assert_eq!(response["content"][0]["type"], "thinking");
    assert_eq!(response["content"][0]["thinking"], "reason");
    assert_eq!(response["content"][0]["signature"], "opaque");
    assert_eq!(response["content"][1]["type"], "text");
    assert_eq!(response["content"][1]["text"], "chat text");
    assert_eq!(
        response["content"][1]["future_content"],
        json!({"keep":true,"null":null})
    );
    assert_eq!(response["content"][2]["type"], "tool_use");
    assert_eq!(response["content"][2]["id"], "call-extra");
    assert_eq!(response["content"][2]["name"], "actual");
    assert_eq!(response["content"][2]["input"], json!({"value":1}));
    assert_eq!(response["content"][2]["index"], 0);
    assert_eq!(
        response["content"][2]["future_tool"],
        json!({"keep":true,"null":null})
    );
    assert_eq!(
        response["content"][2]["chat_function_extensions"],
        json!({"future_function":{"keep":true,"null":null}})
    );

    assert_eq!(response["usage"]["input_tokens"], 7);
    assert_eq!(response["usage"]["output_tokens"], 5);
    assert_eq!(response["usage"]["cache_read_input_tokens"], 2);
    assert_eq!(response["usage"]["cache_creation_input_tokens"], 1);
    assert_eq!(response["usage"]["service_tier"], "default");
    assert_eq!(
        response["usage"]["completion_tokens_details"],
        json!({
            "reasoning_tokens":2,
            "future_completion":{"keep":true}
        })
    );
    assert_eq!(
        response["usage"]["future_usage"],
        json!({"keep":true,"null":null})
    );
    assert_eq!(
        response["usage"]["chat_prompt_tokens_details"],
        json!({"audio_tokens":0,"future_prompt":null})
    );

    let refusal_body = json!({
        "model":"chat-fixture/gpt-chat-response-refusal",
        "max_tokens":128,
        "messages":[{"role":"user","content":"blocked"}]
    });
    let (status, refusal) = send(post_json("/v1/messages", refusal_body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let refusal = json_body(&refusal);
    assert_eq!(refusal["stop_reason"], "refusal");
    assert_eq!(
        refusal["content"],
        json!([{"type":"text","text":"blocked"}])
    );
    assert!(refusal.get("chat_message_extensions").is_none());

    let tier_body = json!({
        "model":"chat-fixture/gpt-chat-response-tier-valid",
        "max_tokens":128,
        "messages":[{"role":"user","content":"tier"}]
    });
    let (status, tier) = send(post_json("/v1/messages", tier_body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let tier = json_body(&tier);
    assert_eq!(tier["service_tier"], "priority");
    assert_eq!(tier["usage"]["service_tier"], "priority");

    for model in ["gpt-chat-response-no-usage", "gpt-chat-response-usage-null"] {
        let body = json!({
            "model":format!("chat-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"optional usage"}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let response = json_body(&response);
        assert_eq!(response["usage"]["input_tokens"], 0, "{model}");
        assert_eq!(response["usage"]["output_tokens"], 0, "{model}");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_chat_malformed_provider_responses_fail_as_sanitized_bad_gateway() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let cases = [
        "gpt-chat-response-malformed-json",
        "gpt-chat-response-oversized",
        "gpt-chat-response-body-error",
        "gpt-chat-response-no-choices",
        "gpt-chat-response-no-id",
        "gpt-chat-response-model-null",
        "gpt-chat-response-object",
        "gpt-chat-response-created",
        "gpt-chat-response-choices-wrong",
        "gpt-chat-response-choices-empty",
        "gpt-chat-response-choices-multiple",
        "gpt-chat-response-choice-wrong",
        "gpt-chat-response-choice-index",
        "gpt-chat-response-no-message",
        "gpt-chat-response-role",
        "gpt-chat-response-content-wrong",
        "gpt-chat-response-content-part",
        "gpt-chat-response-no-finish",
        "gpt-chat-response-finish-unknown",
        "gpt-chat-response-tool-type",
        "gpt-chat-response-tool-id",
        "gpt-chat-response-tool-function",
        "gpt-chat-response-tool-name",
        "gpt-chat-response-tool-arguments-type",
        "gpt-chat-response-tool-arguments-json",
        "gpt-chat-response-tool-arguments-scalar",
        "gpt-chat-response-tool-collision",
        "gpt-chat-response-usage-wrong",
        "gpt-chat-response-usage-negative",
        "gpt-chat-response-usage-total",
        "gpt-chat-response-usage-details",
        "gpt-chat-response-usage-overflow",
        "gpt-chat-response-top-collision",
        "gpt-chat-response-usage-collision",
        "gpt-chat-response-function-call",
        "gpt-chat-response-reasoning-conflict",
        "gpt-chat-response-reasoning-no-signature",
        "gpt-chat-response-logprobs",
        "gpt-chat-response-refusal-malformed",
        "gpt-chat-response-tier-top-invalid",
        "gpt-chat-response-tier-nested-invalid",
        "gpt-chat-response-tier-conflict",
    ];
    for model in cases {
        let body = json!({
            "model":format!("chat-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"malformed response"}]
        });
        let (status, headers, response) =
            send_full(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{model}");
        assert_anthropic_upstream_error(&response, model);
        let expected_request_id = match model {
            "gpt-chat-response-malformed-json" => "chat-malformed-json",
            "gpt-chat-response-oversized" => "chat-oversized",
            "gpt-chat-response-body-error" => "chat-body-error",
            _ => "chat-response-request",
        };
        assert_eq!(
            headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            Some(expected_request_id),
            "{model}"
        );
        assert_eq!(
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("2"),
            "{model}"
        );
        assert_eq!(
            headers
                .get("x-ratelimit-remaining")
                .and_then(|value| value.to_str().ok()),
            Some("9"),
            "{model}"
        );
        assert!(headers.get("x-unsafe-secret").is_none(), "{model}");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_chat_upstream_status_and_direct_failures_preserve_safe_semantics() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (model, expected_status, request_id, retry_after) in [
        (
            "gpt-chat-response-429",
            StatusCode::TOO_MANY_REQUESTS,
            "chat-rate-limit",
            Some("4"),
        ),
        (
            "gpt-chat-response-503",
            StatusCode::SERVICE_UNAVAILABLE,
            "chat-unavailable",
            None,
        ),
    ] {
        let body = json!({
            "model":format!("chat-fixture/{model}"),
            "max_tokens":128,
            "messages":[{"role":"user","content":"upstream status"}]
        });
        let (status, headers, response) =
            send_full(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, expected_status, "{model}");
        let error = json_body(&response);
        assert_eq!(error["type"], "error", "{model}");
        assert_eq!(
            headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            Some(request_id),
            "{model}"
        );
        assert_eq!(
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            retry_after,
            "{model}"
        );
        assert!(headers.get("x-unsafe-secret").is_none(), "{model}");
    }

    configure_direct_copilot(&fixture);
    let extras = json!({
        "model":"gpt-direct-chat-response-extras",
        "max_tokens":128,
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[{"role":"user","content":"direct extras"}]
    });
    let (status, response) = send(post_json("/v1/messages", extras, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let response = json_body(&response);
    assert_eq!(response["id"], "chatcmpl-extras");
    assert_eq!(response["future_after"], json!({"keep":true,"null":null}));
    assert!(response["chat_message_extensions"].get("refusal").is_none());
    assert_eq!(
        response["content"][2]["future_tool"],
        json!({"keep":true,"null":null})
    );

    let refusal = json!({
        "model":"gpt-direct-chat-response-refusal",
        "max_tokens":128,
        "messages":[{"role":"user","content":"direct refusal"}]
    });
    let (status, refusal) = send(post_json("/v1/messages", refusal, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let refusal = json_body(&refusal);
    assert_eq!(refusal["stop_reason"], "refusal");
    assert_eq!(
        refusal["content"],
        json!([{"type":"text","text":"blocked"}])
    );
    assert!(refusal.get("chat_message_extensions").is_none());

    for model in [
        "gpt-direct-chat-malformed-json",
        "gpt-direct-chat-bad-choices",
    ] {
        let body = json!({
            "model":model,
            "max_tokens":128,
            "messages":[{"role":"user","content":"direct malformed"}]
        });
        let (status, headers, response) =
            send_full(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{model}");
        assert_anthropic_upstream_error(&response, model);
        assert!(headers.get("x-request-id").is_some(), "{model}");
        assert_eq!(
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("2"),
            "{model}"
        );
        assert_eq!(
            headers
                .get("x-ratelimit-remaining")
                .and_then(|value| value.to_str().ok()),
            Some("9"),
            "{model}"
        );
        assert!(headers.get("x-unsafe-secret").is_none(), "{model}");
    }

    let rate_limit = json!({
        "model":"gpt-direct-chat-429",
        "max_tokens":128,
        "messages":[{"role":"user","content":"direct rate limit"}]
    });
    let (status, headers, response) =
        send_full(post_json("/v1/messages", rate_limit, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_body(&response)["error"]["type"], "rate_limit_error");
    assert_eq!(
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("4")
    );
    assert!(headers.get("x-unsafe-secret").is_none());
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_chat_sse_strict_identity_usage_extras_and_tools_cross_public_boundary() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let request = |model: &str| {
        json!({
            "model":format!("chat-fixture/{model}"),
            "max_tokens":128,
            "stream":true,
            "tools":[
                {"name":"actual","input_schema":{"type":"object"}},
                {"name":"first","input_schema":{"type":"object"}},
                {"name":"second","input_schema":{"type":"object"}}
            ],
            "messages":[{"role":"user","content":"stream"}]
        })
    };
    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-strict"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    assert_eq!(
        events[0]["message"]["future_chunk"],
        json!({"keep":true,"null":null})
    );
    assert_eq!(events[0]["message"]["system_fingerprint"], Value::Null);
    let terminal_usage = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .expect("strict Chat terminal usage");
    assert_eq!(terminal_usage["usage"]["input_tokens"], 3);
    assert_eq!(terminal_usage["usage"]["output_tokens"], 2);
    assert_eq!(terminal_usage["usage"]["cache_read_input_tokens"], 1);
    assert_eq!(
        terminal_usage["usage"]["future_usage"],
        json!({"keep":true})
    );
    assert_eq!(
        terminal_usage["usage"]["chat_prompt_tokens_details"],
        json!({"future_prompt":null})
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );
    assert!(!events.iter().any(|event| event["type"] == "error"));

    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-no-usage"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let terminal = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .expect("usage-optional terminal");
    assert_eq!(terminal["usage"]["input_tokens"], 0);
    assert_eq!(terminal["usage"]["output_tokens"], 0);
    assert_eq!(events.last().unwrap()["type"], "message_stop");

    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-tools"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let start = events
        .iter()
        .find(|event| {
            event["type"] == "content_block_start" && event["content_block"]["type"] == "tool_use"
        })
        .expect("stream tool start");
    assert_eq!(start["content_block"]["id"], "call-stream");
    assert_eq!(
        start["content_block"]["future_tool"],
        json!({"keep":true,"null":null})
    );
    assert_eq!(
        start["content_block"]["chat_function_extensions"],
        json!({"future_function":{"keep":true,"null":null}})
    );
    let arguments: String = events
        .iter()
        .filter_map(|event| event.pointer("/delta/partial_json").and_then(Value::as_str))
        .collect();
    assert_eq!(arguments, "{\"value\":1}");
    assert_eq!(events.last().unwrap()["type"], "message_stop");

    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-tool-optionals"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let start = events
        .iter()
        .find(|event| event["type"] == "content_block_start")
        .expect("late-identity tool start");
    assert_eq!(start["content_block"]["id"], "call-late");
    assert_eq!(start["content_block"]["name"], "actual");
    assert_eq!(
        start["content_block"]["future_tool"],
        json!({"keep":true,"null":null})
    );
    assert_eq!(
        start["content_block"]["chat_function_extensions"],
        json!({"future_function":{"keep":true,"null":null}})
    );
    let arguments: String = events
        .iter()
        .filter_map(|event| event.pointer("/delta/partial_json").and_then(Value::as_str))
        .collect();
    assert_eq!(arguments, "{\"value\":1}");
    assert_eq!(events.last().unwrap()["type"], "message_stop");

    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-refusal"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    assert!(events
        .iter()
        .any(|event| { event.pointer("/delta/text").and_then(Value::as_str) == Some("blocked") }));
    let terminal = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .expect("refusal terminal");
    assert_eq!(terminal["delta"]["stop_reason"], "refusal");

    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-refusal-split"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let text: String = events
        .iter()
        .filter_map(|event| event.pointer("/delta/text").and_then(Value::as_str))
        .collect();
    assert_eq!(text, "blocked");
    assert_eq!(
        events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("split refusal terminal")["delta"]["stop_reason"],
        "refusal"
    );

    for (model, expected) in [
        ("gpt-chat-stream-refusal-interleaved", "blocked"),
        ("gpt-chat-stream-refusal-mirror", "blocked"),
        ("gpt-chat-stream-refusal-repeated", "blockedblocked"),
        ("gpt-chat-stream-refusal-content-prefix", "blocked"),
        ("gpt-chat-stream-refusal-partial", "blocked"),
    ] {
        let (status, body) =
            send(post_json("/v1/messages", request(model), Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        let text: String = events
            .iter()
            .filter_map(|event| event.pointer("/delta/text").and_then(Value::as_str))
            .collect();
        assert_eq!(text, expected, "{model}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
    }

    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-refusal-empty"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let text: String = events
        .iter()
        .filter_map(|event| event.pointer("/delta/text").and_then(Value::as_str))
        .collect();
    assert_eq!(text, "Hello");
    assert_eq!(
        events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("empty refusal terminal")["delta"]["stop_reason"],
        "end_turn"
    );

    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-refusal-tool-deferred"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    assert_eq!(
        chat_event_schedule(&events),
        vec![
            "message_start",
            "start:0:tool_use:actual",
            "delta:0:{}",
            "stop:0",
            "start:1:text:",
            "delta:1:foo",
            "delta:1:bar",
            "stop:1",
            "terminal:refusal",
            "message_stop",
        ]
    );

    let (status, body) = send(post_json(
        "/v1/messages",
        request("gpt-chat-stream-refusal-multiple-tools"),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    assert_eq!(
        chat_event_schedule(&events),
        vec![
            "message_start",
            "start:0:text:",
            "delta:0:pre",
            "stop:0",
            "start:1:tool_use:first",
            "delta:1:{}",
            "stop:1",
            "start:2:text:",
            "delta:2:mid",
            "stop:2",
            "start:3:tool_use:second",
            "delta:3:{}",
            "stop:3",
            "start:4:text:",
            "delta:4:post",
            "delta:4:-refused",
            "stop:4",
            "terminal:refusal",
            "message_stop",
        ]
    );

    for model in [
        "gpt-chat-stream-budget-reasoning-exact",
        "gpt-chat-stream-budget-opaque-exact",
        "gpt-chat-stream-budget-mixed-utf8-exact",
    ] {
        let (status, body) =
            send(post_json("/v1/messages", request(model), Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        assert_eq!(
            translated_payload_bytes(&events),
            copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES,
            "{model}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
        assert!(!events.iter().any(|event| event["type"] == "error"));
    }

    for (model, tier, fingerprint) in [
        ("gpt-chat-stream-tier-valid", "scale", None),
        (
            "gpt-chat-stream-tier-late",
            "flex",
            Some("late-fingerprint"),
        ),
    ] {
        let (status, body) =
            send(post_json("/v1/messages", request(model), Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&body);
        let terminal = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("tier terminal");
        assert_eq!(terminal["usage"]["service_tier"], tier, "{model}");
        if let Some(fingerprint) = fingerprint {
            assert_eq!(
                terminal["usage"]["chat_system_fingerprint"], fingerprint,
                "{model}"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_chat_sse_malformed_matrix_errors_once_without_later_success() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let models = [
        "gpt-chat-stream-bad-missing-id",
        "gpt-chat-stream-bad-object",
        "gpt-chat-stream-bad-created",
        "gpt-chat-stream-bad-model",
        "gpt-chat-stream-bad-id-conflict",
        "gpt-chat-stream-bad-service",
        "gpt-chat-stream-bad-service-conflict",
        "gpt-chat-stream-bad-fingerprint",
        "gpt-chat-stream-bad-choices",
        "gpt-chat-stream-bad-choice-index",
        "gpt-chat-stream-bad-finish",
        "gpt-chat-stream-bad-function-finish",
        "gpt-chat-stream-bad-tool-finish",
        "gpt-chat-stream-bad-stop-tool",
        "gpt-chat-stream-bad-tool-index",
        "gpt-chat-stream-bad-tool-gap",
        "gpt-chat-stream-bad-tool-id",
        "gpt-chat-stream-bad-tool-duplicate-id",
        "gpt-chat-stream-bad-tool-incomplete",
        "gpt-chat-stream-bad-tool-scalar",
        "gpt-chat-stream-bad-usage-partial",
        "gpt-chat-stream-bad-usage-total",
        "gpt-chat-stream-bad-usage-details",
        "gpt-chat-stream-bad-usage-orphan",
        "gpt-chat-stream-bad-choice-extra",
        "gpt-chat-stream-bad-delta-extra",
        "gpt-chat-stream-bad-later-extra",
        "gpt-chat-stream-bad-refusal",
        "gpt-chat-stream-bad-logprobs",
        "gpt-chat-stream-bad-tier-nested",
        "gpt-chat-stream-bad-tier-conflict",
        "gpt-chat-stream-bad-refusal-conflict",
        "gpt-chat-stream-bad-refusal-finish",
        "gpt-chat-stream-bad-refusal-late",
        "gpt-chat-stream-bad-refusal-late-finish-usage",
        "gpt-chat-stream-bad-refusal-late-after-usage",
        "gpt-chat-stream-bad-refusal-repeated-usage",
        "gpt-chat-stream-bad-refusal-tool-incomplete",
        "gpt-chat-stream-bad-refusal-tool-late",
        "gpt-chat-stream-bad-refusal-tool-eof",
        "gpt-chat-stream-bad-budget-reasoning-over",
        "gpt-chat-stream-bad-budget-opaque-over",
        "gpt-chat-stream-bad-budget-mixed-utf8-over",
        "gpt-chat-stream-bad-tool-late-extra",
        "gpt-chat-stream-bad-tool-missing-terminal",
    ];
    for model in models {
        let body = json!({
            "model":format!("chat-fixture/{model}"),
            "max_tokens":128,
            "stream":true,
            "tools":[{
                "name":"actual",
                "input_schema":{"type":"object"}
            }],
            "messages":[{"role":"user","content":"malformed stream"}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&response);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            0,
            "{model}"
        );
        assert!(
            !events.iter().any(|event| {
                event.pointer("/delta/text").and_then(Value::as_str) == Some("late success")
            }),
            "{model}"
        );
        assert_eq!(events.last().unwrap()["type"], "error", "{model}");
        if model.contains("bad-budget-") {
            assert!(
                translated_payload_bytes(&events)
                    <= copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES,
                "{model}"
            );
        }
        let expected_schedule = match model {
            "gpt-chat-stream-bad-refusal-tool-incomplete" => Some(vec![
                "message_start",
                "start:0:tool_use:actual",
                "delta:0:{\"value\":",
                "stop:0",
                "error",
            ]),
            "gpt-chat-stream-bad-refusal-tool-late" => Some(vec![
                "message_start",
                "start:0:tool_use:actual",
                "delta:0:{}",
                "stop:0",
                "start:1:text:",
                "delta:1:foo",
                "delta:1:bar",
                "stop:1",
                "error",
            ]),
            "gpt-chat-stream-bad-refusal-tool-eof" => Some(vec![
                "message_start",
                "start:0:tool_use:actual",
                "delta:0:{}",
                "stop:0",
                "error",
            ]),
            _ => None,
        };
        if let Some(expected_schedule) = expected_schedule {
            assert_eq!(chat_event_schedule(&events), expected_schedule, "{model}");
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_direct_chat_sse_matches_provider_identity_and_optional_usage_policy() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure_direct_copilot(&fixture);
    for model in [
        "gpt-direct-chat-stream-strict",
        "gpt-direct-chat-stream-no-usage",
        "gpt-direct-chat-stream-tool-optionals",
        "gpt-direct-chat-stream-refusal",
        "gpt-direct-chat-stream-refusal-split",
        "gpt-direct-chat-stream-refusal-interleaved",
        "gpt-direct-chat-stream-refusal-mirror",
        "gpt-direct-chat-stream-refusal-empty",
        "gpt-direct-chat-stream-refusal-repeated",
        "gpt-direct-chat-stream-refusal-content-prefix",
        "gpt-direct-chat-stream-refusal-partial",
        "gpt-direct-chat-stream-refusal-tool-deferred",
        "gpt-direct-chat-stream-refusal-multiple-tools",
        "gpt-direct-chat-stream-budget-reasoning-exact",
        "gpt-direct-chat-stream-budget-opaque-exact",
        "gpt-direct-chat-stream-budget-mixed-utf8-exact",
        "gpt-direct-chat-stream-tier-late",
    ] {
        let body = json!({
            "model":model,
            "max_tokens":128,
            "stream":true,
            "tools":[
                {"name":"actual","input_schema":{"type":"object"}},
                {"name":"first","input_schema":{"type":"object"}},
                {"name":"second","input_schema":{"type":"object"}}
            ],
            "messages":[{"role":"user","content":"direct stream"}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&response);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
        assert!(
            !events.iter().any(|event| event["type"] == "error"),
            "{model}"
        );
        let expected_text = match model {
            "gpt-direct-chat-stream-refusal"
            | "gpt-direct-chat-stream-refusal-split"
            | "gpt-direct-chat-stream-refusal-interleaved"
            | "gpt-direct-chat-stream-refusal-mirror"
            | "gpt-direct-chat-stream-refusal-content-prefix"
            | "gpt-direct-chat-stream-refusal-partial" => Some(("blocked", "refusal")),
            "gpt-direct-chat-stream-refusal-empty" => Some(("Hello", "end_turn")),
            "gpt-direct-chat-stream-refusal-repeated" => Some(("blockedblocked", "refusal")),
            "gpt-direct-chat-stream-refusal-tool-deferred" => Some(("foobar", "refusal")),
            "gpt-direct-chat-stream-refusal-multiple-tools" => {
                Some(("premidpost-refused", "refusal"))
            }
            _ => None,
        };
        if let Some((expected_text, expected_stop)) = expected_text {
            let text: String = events
                .iter()
                .filter_map(|event| event.pointer("/delta/text").and_then(Value::as_str))
                .collect();
            assert_eq!(text, expected_text, "{model}");
            assert_eq!(
                events
                    .iter()
                    .find(|event| event["type"] == "message_delta")
                    .expect("direct refusal terminal")["delta"]["stop_reason"],
                expected_stop,
                "{model}"
            );
        }
        let expected_schedule = match model {
            "gpt-direct-chat-stream-refusal-tool-deferred" => Some(vec![
                "message_start",
                "start:0:tool_use:actual",
                "delta:0:{}",
                "stop:0",
                "start:1:text:",
                "delta:1:foo",
                "delta:1:bar",
                "stop:1",
                "terminal:refusal",
                "message_stop",
            ]),
            "gpt-direct-chat-stream-refusal-multiple-tools" => Some(vec![
                "message_start",
                "start:0:text:",
                "delta:0:pre",
                "stop:0",
                "start:1:tool_use:first",
                "delta:1:{}",
                "stop:1",
                "start:2:text:",
                "delta:2:mid",
                "stop:2",
                "start:3:tool_use:second",
                "delta:3:{}",
                "stop:3",
                "start:4:text:",
                "delta:4:post",
                "delta:4:-refused",
                "stop:4",
                "terminal:refusal",
                "message_stop",
            ]),
            _ => None,
        };
        if let Some(expected_schedule) = expected_schedule {
            assert_eq!(chat_event_schedule(&events), expected_schedule, "{model}");
        }
        if model.contains("-budget-") {
            assert_eq!(
                translated_payload_bytes(&events),
                copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES,
                "{model}"
            );
        }
    }

    for model in [
        "gpt-direct-chat-stream-bad-identity",
        "gpt-direct-chat-stream-bad-tier",
        "gpt-direct-chat-stream-bad-refusal",
        "gpt-direct-chat-stream-bad-refusal-conflict",
        "gpt-direct-chat-stream-bad-refusal-finish",
        "gpt-direct-chat-stream-bad-refusal-late",
        "gpt-direct-chat-stream-bad-refusal-late-finish-usage",
        "gpt-direct-chat-stream-bad-refusal-late-after-usage",
        "gpt-direct-chat-stream-bad-refusal-repeated-usage",
        "gpt-direct-chat-stream-bad-refusal-tool-incomplete",
        "gpt-direct-chat-stream-bad-refusal-tool-late",
        "gpt-direct-chat-stream-bad-refusal-tool-eof",
        "gpt-direct-chat-stream-bad-budget-reasoning-over",
        "gpt-direct-chat-stream-bad-budget-opaque-over",
        "gpt-direct-chat-stream-bad-budget-mixed-utf8-over",
    ] {
        let body = json!({
            "model":model,
            "max_tokens":128,
            "stream":true,
            "messages":[{"role":"user","content":"direct malformed stream"}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&response);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}"
        );
        assert!(
            !events.iter().any(|event| event["type"] == "message_stop"),
            "{model}"
        );
        if model.contains("bad-budget-") {
            assert!(
                translated_payload_bytes(&events)
                    <= copilot_api::libs::http::MAX_UPSTREAM_RESPONSE_BYTES,
                "{model}"
            );
        }
        let expected_schedule = match model {
            "gpt-direct-chat-stream-bad-refusal-tool-incomplete" => Some(vec![
                "message_start",
                "start:0:tool_use:actual",
                "delta:0:{\"value\":",
                "stop:0",
                "error",
            ]),
            "gpt-direct-chat-stream-bad-refusal-tool-late" => Some(vec![
                "message_start",
                "start:0:tool_use:actual",
                "delta:0:{}",
                "stop:0",
                "start:1:text:",
                "delta:1:foo",
                "delta:1:bar",
                "stop:1",
                "error",
            ]),
            "gpt-direct-chat-stream-bad-refusal-tool-eof" => Some(vec![
                "message_start",
                "start:0:tool_use:actual",
                "delta:0:{}",
                "stop:0",
                "error",
            ]),
            _ => None,
        };
        if let Some(expected_schedule) = expected_schedule {
            assert_eq!(chat_event_schedule(&events), expected_schedule, "{model}");
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_responses_state_budgets_cross_provider_and_direct_boundaries() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;

    configure(&fixture);
    for model in [
        "gpt-responses-state-exact",
        "gpt-responses-state-utf8-exact",
        "gpt-responses-function-state-exact",
        "gpt-responses-mixed-budget",
    ] {
        let body = json!({
            "model":format!("responses-fixture/{model}"),
            "max_tokens":128,
            "stream":true,
            "messages":[{"role":"user","content":"responses budget"}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&response);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}: {:?}",
            events.last()
        );
        assert!(!events.iter().any(|event| event["type"] == "error"));
        if model.ends_with("utf8-exact") {
            assert!(events.iter().any(|event| {
                event
                    .pointer("/delta/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains('é'))
            }));
        }
        if model.ends_with("mixed-budget") {
            assert!(events.iter().any(|event| {
                event["delta"]["type"] == "text_delta"
                    && event["delta"]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains('é'))
            }));
            assert!(events
                .iter()
                .any(|event| event["delta"]["type"] == "thinking_delta"));
            assert!(events
                .iter()
                .any(|event| event["delta"]["type"] == "signature_delta"));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["content_block"]["type"] == "tool_use")
                    .count(),
                2
            );
        }
    }
    for model in [
        "gpt-responses-state-over",
        "gpt-responses-function-state-over",
        "gpt-responses-mixed-budget-over",
    ] {
        let body = json!({
            "model":format!("responses-fixture/{model}"),
            "max_tokens":128,
            "stream":true,
            "messages":[{"role":"user","content":"responses overflow"}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&response);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}"
        );
        assert!(!events.iter().any(|event| event["type"] == "message_stop"));
    }

    configure_direct_copilot(&fixture);
    for model in [
        "gpt-direct-responses-state-exact",
        "gpt-direct-responses-state-utf8-exact",
        "gpt-direct-responses-function-state-exact",
        "gpt-direct-responses-mixed-budget",
    ] {
        let body = json!({
            "model":model,
            "max_tokens":128,
            "stream":true,
            "messages":[{"role":"user","content":"direct responses budget"}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&response);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1,
            "{model}"
        );
        assert!(!events.iter().any(|event| event["type"] == "error"));
        if model.ends_with("mixed-budget") {
            assert!(events.iter().any(|event| {
                event["delta"]["type"] == "text_delta"
                    && event["delta"]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains('é'))
            }));
            assert!(events
                .iter()
                .any(|event| event["delta"]["type"] == "thinking_delta"));
            assert!(events
                .iter()
                .any(|event| event["delta"]["type"] == "signature_delta"));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["content_block"]["type"] == "tool_use")
                    .count(),
                2
            );
        }
    }
    for model in [
        "gpt-direct-responses-state-over",
        "gpt-direct-responses-function-state-over",
        "gpt-direct-responses-mixed-budget-over",
    ] {
        let body = json!({
            "model":model,
            "max_tokens":128,
            "stream":true,
            "messages":[{"role":"user","content":"direct responses overflow"}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        let events = data_events(&response);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}"
        );
        assert!(!events.iter().any(|event| event["type"] == "message_stop"));
    }
}

fn binary_schema(depth: usize) -> Value {
    if depth == 0 {
        return Value::Bool(true);
    }
    let child = binary_schema(depth - 1);
    json!({"allOf":[child.clone(),child]})
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_recursive_schema_shape_and_bounds_fail_before_dispatch() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let mut too_deep = Value::Bool(true);
    for _ in 0..=copilot_api::routes::messages::request_validation::MAX_REQUEST_SCHEMA_DEPTH {
        too_deep = json!({"not":too_deep});
    }
    let oversized_properties: Map<String, Value> = (0
        ..=copilot_api::routes::messages::request_validation::MAX_REQUEST_SCHEMA_COLLECTION_ITEMS)
        .map(|index| (format!("p{index}"), Value::Bool(true)))
        .collect();
    let cases = [
        ("property scalar", json!({"properties":{"path":42}})),
        (
            "pattern property scalar",
            json!({"patternProperties":{"^x":42}}),
        ),
        ("defs scalar", json!({"$defs":{"entry":"wrong"}})),
        ("definitions scalar", json!({"definitions":{"entry":7}})),
        ("items scalar", json!({"items":"wrong"})),
        ("items schema array scalar", json!({"items":[true,7]})),
        ("prefix items container", json!({"prefixItems":{"x":true}})),
        ("prefix items scalar", json!({"prefixItems":[true,7]})),
        (
            "additional properties scalar",
            json!({"additionalProperties":"wrong"}),
        ),
        (
            "unevaluated properties scalar",
            json!({"unevaluatedProperties":7}),
        ),
        ("all of container", json!({"allOf":{"x":true}})),
        ("any of scalar", json!({"anyOf":[true,"wrong"]})),
        ("one of scalar", json!({"oneOf":[7]})),
        ("not scalar", json!({"not":"wrong"})),
        ("if scalar", json!({"if":7,"then":true,"else":false})),
        (
            "dependent schemas scalar",
            json!({"dependentSchemas":{"x":"wrong"}}),
        ),
        ("contains scalar", json!({"contains":"wrong"})),
        ("property names scalar", json!({"propertyNames":7})),
        ("required mixed", json!({"required":["x",7]})),
        (
            "dependent required container",
            json!({"dependentRequired":["x"]}),
        ),
        (
            "dependent required mixed",
            json!({"dependentRequired":{"x":["y",7]}}),
        ),
        ("dependencies container", json!({"dependencies":[] })),
        ("dependencies scalar", json!({"dependencies":{"x":7}})),
        ("reference type", json!({"$ref":7})),
        ("type unknown", json!({"type":"future"})),
        ("type empty", json!({"type":""})),
        ("type array empty", json!({"type":[]})),
        ("type array mixed", json!({"type":["object",7]})),
        ("type array unknown", json!({"type":["object","future"]})),
        ("type array duplicate", json!({"type":["string","string"]})),
        ("required empty name", json!({"required":[""]})),
        ("required blank name", json!({"required":["  "]})),
        ("required duplicate", json!({"required":["x","x"]})),
        (
            "dependent required empty",
            json!({"dependentRequired":{"x":[""]}}),
        ),
        (
            "dependent required duplicate",
            json!({"dependentRequired":{"x":["y","y"]}}),
        ),
        (
            "dependencies duplicate",
            json!({"dependencies":{"x":["y","y"]}}),
        ),
        ("depth bound", too_deep),
        ("node bound", binary_schema(12)),
        (
            "collection bound",
            Value::Object(Map::from_iter([(
                "properties".to_string(),
                Value::Object(oversized_properties),
            )])),
        ),
    ];
    for (label, schema) in cases {
        let before = fixture.requests().len();
        let body = tool_schema_messages_body("gpt-fixture", schema, None);
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_anthropic_invalid_request(&response, label);
        assert!(json_body(&response)["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("tools[0].input_schema")));
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_complex_boolean_schemas_choices_and_sources_preserve_supported_shape() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let complex_schema = json!({
        "type":["object","null"],
        "properties":{
            "path":{"type":"string"},
            "flags":{"type":"array","items":[true,{"not":false}]},
            "all_types":{
                "type":["null","boolean","object","array","number","string","integer"]
            },
            "empty_constraints":{
                "type":"object",
                "required":[],
                "dependentRequired":{"x":[]},
                "dependencies":{"legacy":[]}
            }
        },
        "patternProperties":{"^x-":false},
        "$defs":{"name":{"type":"string"}},
        "definitions":{"legacy":true},
        "items":true,
        "prefixItems":[true,{"type":"integer"}],
        "additionalProperties":false,
        "unevaluatedProperties":{"type":"string"},
        "allOf":[true,{"required":["path"]}],
        "anyOf":[false,{"$ref":"#/$defs/name"}],
        "oneOf":[true,false],
        "not":false,
        "if":{"properties":{"path":true}},
        "then":true,
        "else":false,
        "dependentSchemas":{"path":{"properties":{"other":true}}},
        "contains":true,
        "propertyNames":{"type":"string"},
        "required":["path"],
        "dependentRequired":{"path":["flags"]},
        "dependencies":{"legacy":["path"],"schema":true},
        "enum":[null,1,"x",{"unknown":true}],
        "const":{"unknown":["value"]},
        "future_keyword":{"opaque":[1,2,3]}
    });
    let mut complex_body = tool_schema_messages_body(
        "gpt-fixture",
        complex_schema.clone(),
        Some(json!({"type":"tool","name":"selected_tool","future_choice":true})),
    );
    complex_body["metadata"] = json!({"user_id":"session-1","future_metadata":{"keep":true}});
    complex_body["tools"][0]["strict"] = json!(true);
    complex_body["thinking"] =
        json!({"type":"enabled","budget_tokens":1024,"future_thinking":{"keep":true}});
    complex_body["output_config"] = json!({"effort":"high","future_output_config":{"keep":true}});
    let (status, _) = send(post_json("/v1/messages", complex_body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("complex schema capture");
    assert_eq!(capture.body["tools"][0]["parameters"], complex_schema);
    assert_eq!(
        capture.body["tool_choice"],
        json!({"type":"function","name":"selected_tool","future_choice":true})
    );
    assert_eq!(capture.body["tools"][0]["future_tool_key"]["keep"], true);
    assert_eq!(capture.body["tools"][0]["strict"], true);
    assert_eq!(capture.body["metadata"]["future_metadata"]["keep"], true);
    assert_eq!(capture.body["reasoning"]["effort"], "high");
    assert_eq!(capture.body["reasoning"]["future_thinking"]["keep"], true);
    assert_eq!(
        capture.body["reasoning"]["future_output_config"]["keep"],
        true
    );

    let (status, _) = send(post_json(
        "/v1/messages",
        tool_schema_messages_body(
            "gpt-fixture",
            Value::Bool(true),
            Some(json!({"type":"tool","name":"selected_tool"})),
        ),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("boolean schema capture");
    assert_eq!(capture.body["tools"][0]["parameters"], true);

    for (choice, expected) in [
        (json!({"type":"auto"}), json!("auto")),
        (json!({"type":"any"}), json!("required")),
        (json!({"type":"none"}), json!("none")),
    ] {
        let (status, _) = send(post_json(
            "/v1/messages",
            tool_schema_messages_body("gpt-fixture", json!({"type":"object"}), Some(choice)),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let capture = fixture
            .requests()
            .into_iter()
            .rev()
            .find(|capture| capture.path == "/v1/responses")
            .expect("tool-choice capture");
        assert_eq!(capture.body["tool_choice"], expected);
    }

    let bridge_body = json!({
        "model":"responses-fixture/gpt-5.4",
        "max_tokens":128,
        "tools":[
            {
                "name":"mcp__tool_search__search",
                "input_schema":{"type":"object"},
                "future_bridge":{"keep":true}
            },
            {
                "name":"mcp__weather",
                "defer_loading":true,
                "input_schema":true
            },
            {
                "name":"ordinary_tool",
                "defer_loading":false,
                "input_schema":{"type":"object"},
                "future_ordinary":{"keep":true}
            }
        ],
        "tool_choice":{"type":"tool","name":"mcp__tool_search__search"},
        "messages":[{"role":"user","content":"choose through bridge"}]
    });
    let (status, _) = send(post_json("/v1/messages", bridge_body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("bridge choice capture");
    assert_eq!(capture.body["tool_choice"], "auto");
    assert!(capture.body["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["type"] == "tool_search")));
    assert!(capture.body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["type"] == "function" && tool["name"] == "ordinary_tool")
    }));
    let tools = capture.body["tools"].as_array().expect("translated tools");
    assert!(tools
        .iter()
        .any(|tool| { tool["type"] == "tool_search" && tool["future_bridge"]["keep"] == true }));
    assert!(tools.iter().any(|tool| {
        tool["name"] == "ordinary_tool" && tool["future_ordinary"]["keep"] == true
    }));

    let source_body = json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "messages":[{
            "role":"user",
            "content":[
                {
                    "type":"image",
                    "future_image_block":{"keep":true},
                    "source":{
                        "type":"base64",
                        "media_type":"image/png",
                        "data":"aGVsbG8=",
                        "future_image_key":{"keep":true}
                    }
                },
                {
                    "type":"document",
                    "title":"doc.pdf",
                    "future_document_block":{"keep":true},
                    "source":{
                        "type":"url",
                        "url":"https://example.test/doc.pdf",
                        "future_document_key":{"keep":true}
                    }
                }
            ]
        }]
    });
    let (status, _) = send(post_json("/v1/messages", source_body, Some(CLIENT_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("supported source capture");
    let content = &capture.body["input"][0]["content"];
    assert_eq!(
        content[0]["anthropic_source_extensions"]["future_image_key"]["keep"],
        true
    );
    assert_eq!(content[0]["future_image_block"]["keep"], true);
    assert_eq!(
        content[1]["anthropic_source_extensions"]["future_document_key"]["keep"],
        true
    );
    assert_eq!(content[1]["future_document_block"]["keep"], true);

    let open_blocks_body = json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "messages":[{
            "role":"assistant",
            "content":[
                {
                    "type":"thinking",
                    "thinking":"analysis",
                    "signature":"enc@id",
                    "future_thinking_block":{"keep":true}
                },
                {
                    "type":"text",
                    "text":"answer",
                    "future_text_block":{"keep":true}
                }
            ]
        }]
    });
    let (status, _) = send(post_json(
        "/v1/messages",
        open_blocks_body,
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("open content capture");
    let input = capture.body["input"].as_array().expect("translated input");
    assert!(input.iter().any(|item| {
        item["type"] == "reasoning" && item["future_thinking_block"]["keep"] == true
    }));
    assert!(input.iter().any(|item| {
        item["type"] == "message" && item["content"][0]["future_text_block"]["keep"] == true
    }));

    let function_blocks_body = json!({
        "model":"responses-fixture/gpt-fixture",
        "max_tokens":128,
        "tools":[{
            "name":"actual",
            "input_schema":{"type":"object"}
        }],
        "messages":[
            {
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":"call",
                    "name":"actual",
                    "input":{},
                    "future_tool_use":{"keep":true}
                }]
            },
            {
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":"call",
                    "content":"done",
                    "future_tool_result":{"keep":true}
                }]
            }
        ]
    });
    let (status, _) = send(post_json(
        "/v1/messages",
        function_blocks_body,
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("open tool block capture");
    let input = capture.body["input"].as_array().expect("translated input");
    assert!(input.iter().any(|item| {
        item["type"] == "function_call" && item["future_tool_use"]["keep"] == true
    }));
    assert!(input.iter().any(|item| {
        item["type"] == "function_call_output" && item["future_tool_result"]["keep"] == true
    }));
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_unsupported_source_types_fail_before_admission_or_dispatch() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    for (label, block) in [
        (
            "image file source",
            json!({"type":"image","source":{"type":"file","file_id":"file-1"}}),
        ),
        (
            "document file source",
            json!({"type":"document","source":{"type":"file","file_id":"file-2"}}),
        ),
        (
            "image unknown source",
            json!({"type":"image","source":{"type":"future","opaque":true}}),
        ),
        (
            "document unknown source",
            json!({"type":"document","source":{"type":"future","opaque":true}}),
        ),
    ] {
        let before = fixture.requests().len();
        let body = json!({
            "model":"responses-fixture/gpt-fixture",
            "max_tokens":128,
            "messages":[{"role":"user","content":[block]}]
        });
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
        assert_anthropic_invalid_request(&response, label);
        assert!(json_body(&response)["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("source.type")));
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_deferred_tool_references_reject_malformed_collections_before_dispatch() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (label, body) in [
        (
            "missing tool name",
            deferred_reference_body(Some(json!([{"type":"tool_reference"}])), true),
        ),
        (
            "null tool name",
            deferred_reference_body(
                Some(json!([{"type":"tool_reference","tool_name":null}])),
                true,
            ),
        ),
        (
            "wrong tool name",
            deferred_reference_body(Some(json!([{"type":"tool_reference","tool_name":7}])), true),
        ),
        (
            "empty tool name",
            deferred_reference_body(
                Some(json!([{"type":"tool_reference","tool_name":"  "}])),
                true,
            ),
        ),
        (
            "unknown tool name",
            deferred_reference_body(
                Some(json!([{"type":"tool_reference","tool_name":"mcp__unknown"}])),
                true,
            ),
        ),
        (
            "non-deferred tool",
            deferred_reference_body(
                Some(json!([{"type":"tool_reference","tool_name":"mcp__weather"}])),
                false,
            ),
        ),
        (
            "mixed reference collection",
            deferred_reference_body(
                Some(json!([
                    {"type":"tool_reference","tool_name":"mcp__weather"},
                    {"type":"tool_reference","tool_name":7}
                ])),
                true,
            ),
        ),
        (
            "sentinel names scalar",
            deferred_reference_body(
                Some(Value::String(
                    json!({"type":"copilot_api_tool_search","names":"mcp__weather"}).to_string(),
                )),
                true,
            ),
        ),
        (
            "sentinel names mixed",
            deferred_reference_body(
                Some(Value::String(
                    json!({"type":"copilot_api_tool_search","names":["mcp__weather",7]})
                        .to_string(),
                )),
                true,
            ),
        ),
        (
            "sentinel names empty",
            deferred_reference_body(
                Some(Value::String(
                    json!({"type":"copilot_api_tool_search","names":[]}).to_string(),
                )),
                true,
            ),
        ),
        (
            "sentinel unknown tool",
            deferred_reference_body(
                Some(Value::String(
                    json!({"type":"copilot_api_tool_search","names":["mcp__unknown"]}).to_string(),
                )),
                true,
            ),
        ),
    ] {
        let before = fixture.requests().len();
        let (status, response) = send(post_json("/v1/messages", body, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_anthropic_invalid_request(&response, label);
        assert_eq!(fixture.requests().len(), before, "{label} reached upstream");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_deferred_tool_empty_duplicate_and_unknown_extensions_are_explicit() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let duplicate_references = deferred_reference_body(
        Some(json!([
            {
                "type":"tool_reference",
                "tool_name":"mcp__weather",
                "future_reference_key":{"keep":true}
            },
            {"type":"tool_reference","tool_name":"mcp__forecast"},
            {"type":"tool_reference","tool_name":"mcp__weather"}
        ])),
        true,
    );
    let (status, _) = send(post_json(
        "/v1/messages",
        duplicate_references,
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("duplicate tool-reference capture");
    let output = capture.body["input"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["type"] == "tool_search_output")
        })
        .expect("tool-search output");
    let call = capture.body["input"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["type"] == "tool_search_call"))
        .expect("tool-search call");
    assert_eq!(call["future_tool_use"]["keep"], true);
    assert_eq!(output["future_tool_result"]["keep"], true);
    assert_eq!(
        output["anthropic_tool_reference_extensions"][0]["future_reference_key"]["keep"],
        true
    );
    assert_eq!(output["tools"].as_array().map(Vec::len), Some(3));
    assert_eq!(output["tools"][0]["name"], "mcp__weather");
    assert_eq!(output["tools"][1]["name"], "mcp__forecast");
    assert_eq!(output["tools"][2]["name"], "mcp__weather");
    assert_eq!(
        output["tools"][0]["tools"][0]["parameters"]["future_schema_key"]["keep"],
        true
    );
    assert_eq!(output["tools"][0]["future_tool_key"]["keep"], true);

    let (status, _) = send(post_json(
        "/v1/messages",
        deferred_reference_body(
            Some(Value::String(
                json!({
                    "type":"copilot_api_tool_search",
                    "names":["mcp__weather","mcp__weather"]
                })
                .to_string(),
            )),
            true,
        ),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("duplicate sentinel capture");
    let output = capture.body["input"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["type"] == "tool_search_output")
        })
        .expect("sentinel tool-search output");
    assert_eq!(output["tools"].as_array().map(Vec::len), Some(2));

    for content in [Some(json!([])), None] {
        let before = fixture.requests().len();
        let (status, _) = send(post_json(
            "/v1/messages",
            deferred_reference_body(content, true),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let captures = fixture.requests();
        assert_eq!(captures.len(), before + 1);
        let output = captures
            .last()
            .and_then(|capture| capture.body["input"].as_array())
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["type"] == "tool_search_output")
            })
            .expect("empty tool-search output");
        assert_eq!(output["tools"], json!([]));
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_partial_terminals_preserve_output_in_json_and_sse() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;

    for model in [
        "gpt-web-partial-completed",
        "gpt-web-created-lifecycle-equivalent",
        "gpt-web-terminal-output-completed",
        "gpt-web-created-only-completed",
        "gpt-web-terminal-only-completed",
        "gpt-web-matching-duplicate-completed",
        "gpt-web-usage-null-details-match",
        "gpt-web-output-null-optional-equivalent",
        "gpt-web-model-requested-fallback",
        "gpt-web-usage-details-created-only",
        "gpt-web-usage-details-terminal-only",
        "gpt-web-incomplete-details-created-only",
        "gpt-web-incomplete-details-terminal-only",
        "gpt-web-incomplete-details-matching",
        "gpt-web-incomplete-details-null-absent",
        "gpt-web-metadata-created-only",
        "gpt-web-metadata-terminal-only",
        "gpt-web-metadata-matching",
        "gpt-web-metadata-null-absent",
        "gpt-web-ignored-extra-conflict",
        "gpt-web-item-id-created-only",
        "gpt-web-item-id-terminal-only",
    ] {
        configure_with_web_search_model(&fixture, Some(&format!("responses-fixture/{model}")));
        let (json_status, json_bytes) = send(web_search_messages_request(false)).await;
        assert_eq!(
            json_status,
            StatusCode::OK,
            "{model}: {}",
            String::from_utf8_lossy(&json_bytes)
        );
        let json_response = json_body(&json_bytes);
        assert_eq!(json_response["id"], "resp_web_partial", "{model}");
        assert_eq!(json_response["stop_reason"], "end_turn", "{model}");
        assert_eq!(json_response["usage"]["input_tokens"], 6, "{model}");
        assert_eq!(json_response["usage"]["output_tokens"], 4, "{model}");
        assert_eq!(
            json_response["usage"]["server_tool_use"]["web_search_requests"], 1,
            "{model}"
        );
        assert_eq!(json_response["content"].as_array().map(Vec::len), Some(3));
        assert_eq!(json_response["content"][0]["type"], "server_tool_use");
        if model == "gpt-web-output-null-optional-equivalent" {
            assert!(json_response["content"][0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("srvtoolu_")));
        } else if matches!(
            model,
            "gpt-web-model-requested-fallback"
                | "gpt-web-usage-details-created-only"
                | "gpt-web-usage-details-terminal-only"
                | "gpt-web-incomplete-details-created-only"
                | "gpt-web-incomplete-details-terminal-only"
                | "gpt-web-incomplete-details-matching"
                | "gpt-web-incomplete-details-null-absent"
                | "gpt-web-metadata-created-only"
                | "gpt-web-metadata-terminal-only"
                | "gpt-web-metadata-matching"
                | "gpt-web-metadata-null-absent"
                | "gpt-web-ignored-extra-conflict"
        ) {
            assert_eq!(json_response["content"][0]["id"], "authority-web");
        } else {
            assert_eq!(json_response["content"][0]["id"], "web-search-item");
        }
        assert_eq!(json_response["content"][0]["name"], "web_search");
        assert_eq!(json_response["content"][0]["input"]["query"], "rust async");
        assert_eq!(
            json_response["content"][1]["type"],
            "web_search_tool_result"
        );
        assert_eq!(
            json_response["content"][1]["tool_use_id"],
            json_response["content"][0]["id"]
        );
        assert_eq!(
            json_response["content"][1]["content"][0]["url"],
            "https://example.test/source"
        );
        assert_eq!(json_response["content"][2]["text"], "Grounded answer.");

        let (sse_status, sse_bytes) = send(web_search_messages_request(true)).await;
        assert_eq!(sse_status, StatusCode::OK, "{model}");
        let events = data_events(&sse_bytes);
        let event_types: Vec<&str> = events
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect();
        assert_eq!(
            event_types,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{model}"
        );
        assert_eq!(events[0]["message"]["id"], json_response["id"], "{model}");
        assert_eq!(
            events[1]["content_block"]["id"], json_response["content"][0]["id"],
            "{model}"
        );
        assert_eq!(
            events[4]["content_block"]["tool_use_id"], json_response["content"][1]["tool_use_id"],
            "{model}"
        );
        assert_eq!(
            events[4]["content_block"]["content"][0]["url"], "https://example.test/source",
            "{model}"
        );
        assert_eq!(events[7]["delta"]["text"], "Grounded answer.", "{model}");
        assert_eq!(events[9]["delta"]["stop_reason"], "end_turn", "{model}");
        assert_eq!(events[9]["usage"]["output_tokens"], 4, "{model}");
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_overflow_precedes_usage_recording() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    let model = "gpt-web-reconstructed-overflow";
    configure_direct_web_search(&fixture, model);
    let before = copilot_api::libs::token_usage::get_token_usage_events_page(1, 500, "day")
        .items
        .into_iter()
        .filter(|event| event.model == model)
        .count();
    let metric_value = |status: &str| {
        copilot_api::libs::metrics::render()
            .lines()
            .find(|line| {
                line.starts_with("http_requests_total{")
                    && line.contains("method=\"POST\"")
                    && line.contains(&format!("status=\"{status}\""))
            })
            .and_then(|line| line.split_whitespace().last())
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let success_before = metric_value("200");
    let error_before = metric_value("500");
    let in_flight_before = copilot_api::libs::metrics::render()
        .lines()
        .find(|line| line.starts_with("http_requests_in_flight "))
        .and_then(|line| line.split_whitespace().last())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0);

    for stream in [false, true] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":"claude-sonnet-4-6",
                "max_tokens":128,
                "messages":[{"role":"user","content":"overflow web reconstruction"}],
                "tools":[{"type":"web_search_20250305","name":"web_search"}],
                "stream":stream
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert!(
            status.is_server_error(),
            "unexpected status {status}: {}",
            String::from_utf8_lossy(&body)
        );
        let error = json_body(&body);
        assert_eq!(
            error.as_object().map(Map::len),
            Some(2),
            "overflow returns one native Anthropic error envelope"
        );
        assert_eq!(error["type"], "error");
        assert!(error["error"].is_object());
        assert!(error["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.contains("w".repeat(128).as_str())));
    }

    let after = copilot_api::libs::token_usage::get_token_usage_events_page(1, 500, "day")
        .items
        .into_iter()
        .filter(|event| event.model == model)
        .count();
    assert_eq!(after, before, "overflow must not record token usage");
    assert_eq!(
        metric_value("200"),
        success_before,
        "overflow must not record reconstruction success"
    );
    assert!(metric_value("500") >= error_before + 2.0);
    let in_flight_after = copilot_api::libs::metrics::render()
        .lines()
        .find(|line| line.starts_with("http_requests_in_flight "))
        .and_then(|line| line.split_whitespace().last())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0);
    assert_eq!(
        in_flight_after, in_flight_before,
        "overflow request finalization must release in-flight accounting"
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_annotations_canonicalize_across_all_snapshots() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;

    for model in [
        "gpt-web-annotations-created-empty-terminal-absent",
        "gpt-web-annotations-created-absent-terminal-empty",
        "gpt-web-annotations-created-unknown-terminal-null",
        "gpt-web-annotations-created-null-terminal-unknown",
        "gpt-web-annotations-created-mixed-terminal-known",
        "gpt-web-annotations-created-known-terminal-mixed",
        "gpt-web-annotations-duplicate-known",
        "gpt-web-annotations-default-title",
        "gpt-web-annotations-known-extensions",
        "gpt-web-annotations-lifecycle-empty-unknown",
        "gpt-web-annotations-lifecycle-mixed-known",
    ] {
        configure_with_web_search_model(&fixture, Some(&format!("responses-fixture/{model}")));
        let expects_source = matches!(
            model,
            "gpt-web-annotations-created-mixed-terminal-known"
                | "gpt-web-annotations-created-known-terminal-mixed"
                | "gpt-web-annotations-duplicate-known"
                | "gpt-web-annotations-default-title"
                | "gpt-web-annotations-known-extensions"
                | "gpt-web-annotations-lifecycle-mixed-known"
        );

        let (json_status, json_bytes) = send(web_search_messages_request(false)).await;
        assert_eq!(
            json_status,
            StatusCode::OK,
            "{model}: {}",
            String::from_utf8_lossy(&json_bytes)
        );
        let json_response = json_body(&json_bytes);
        assert_eq!(json_response["id"], "resp_web_annotations", "{model}");
        assert_eq!(json_response["stop_reason"], "end_turn", "{model}");
        let sources = json_response["content"][1]["content"]
            .as_array()
            .expect("web-search source array");
        assert_eq!(sources.len(), usize::from(expects_source), "{model}");
        if expects_source {
            assert_eq!(sources[0]["url"], "https://example.test/source", "{model}");
            assert_eq!(
                sources[0]["title"],
                if model == "gpt-web-annotations-default-title" {
                    "https://example.test/source"
                } else {
                    "Source"
                },
                "{model}"
            );
        }

        let (sse_status, sse_bytes) = send(web_search_messages_request(true)).await;
        assert_eq!(sse_status, StatusCode::OK, "{model}");
        let events = data_events(&sse_bytes);
        assert!(
            !events.iter().any(|event| event["type"] == "error"),
            "{model}"
        );
        assert_eq!(
            events.last().and_then(|event| event["type"].as_str()),
            Some("message_stop"),
            "{model}"
        );
        let tool_result = events
            .iter()
            .find(|event| event["content_block"]["type"] == "web_search_tool_result")
            .expect("synthetic web-search tool-result block");
        assert_eq!(
            tool_result["content_block"]["content"]
                .as_array()
                .map(Vec::len),
            Some(usize::from(expects_source)),
            "{model}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_malformed_or_conflicting_annotations_fail_once() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;

    for model in [
        "gpt-web-annotations-conflict-known",
        "gpt-web-annotations-malformed-field",
        "gpt-web-annotations-malformed-entry",
        "gpt-web-annotations-malformed-type",
        "gpt-web-annotations-malformed-known-missing-url",
        "gpt-web-annotations-malformed-known-url",
        "gpt-web-annotations-malformed-known-title",
        "gpt-web-annotations-lifecycle-conflict-known",
    ] {
        configure_with_web_search_model(&fixture, Some(&format!("responses-fixture/{model}")));
        for stream in [false, true] {
            let (status, body) = send(web_search_messages_request(stream)).await;
            assert!(
                status.is_server_error(),
                "{model}/{stream}: {status} {}",
                String::from_utf8_lossy(&body)
            );
            let error = json_body(&body);
            assert_eq!(error["type"], "error", "{model}/{stream}");
            assert_eq!(error["error"]["type"], "api_error", "{model}/{stream}");
            assert!(error.get("content").is_none(), "{model}/{stream}");
            assert!(error.get("stop_reason").is_none(), "{model}/{stream}");
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_lifecycle_optionals_merge_in_both_directions() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    for model in [
        "gpt-web-lifecycle-item-fields-added-only",
        "gpt-web-lifecycle-item-fields-done-only",
    ] {
        configure_with_web_search_model(&fixture, Some(&format!("responses-fixture/{model}")));
        for stream in [false, true] {
            let (status, body) = send(web_search_messages_request(stream)).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{model}/{stream}: {}",
                String::from_utf8_lossy(&body)
            );
            if stream {
                let events = data_events(&body);
                let start = events
                    .iter()
                    .find(|event| {
                        event["type"] == "content_block_start"
                            && event["content_block"]["type"] == "server_tool_use"
                    })
                    .expect("server tool start");
                assert_eq!(start["content_block"]["id"], "lifecycle-web-id");
                let delta = events
                    .iter()
                    .find(|event| event["delta"]["type"] == "input_json_delta")
                    .expect("server tool input");
                assert_eq!(delta["delta"]["partial_json"], "{\"query\":\"rust async\"}");
            } else {
                let response = json_body(&body);
                assert_eq!(response["content"][0]["id"], "lifecycle-web-id");
                assert_eq!(response["content"][0]["input"]["query"], "rust async");
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_end_turn_assertions_merge_in_both_directions() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    for model in [
        "gpt-web-end-turn-created-only",
        "gpt-web-end-turn-terminal-only",
    ] {
        configure_with_web_search_model(&fixture, Some(&format!("responses-fixture/{model}")));
        for stream in [false, true] {
            let (status, body) = send(web_search_messages_request(stream)).await;
            assert_eq!(status, StatusCode::OK, "{model}/{stream}");
            if stream {
                let events = data_events(&body);
                assert_eq!(
                    events
                        .iter()
                        .find(|event| event["type"] == "message_delta")
                        .and_then(|event| event["delta"]["stop_reason"].as_str()),
                    Some("pause_turn"),
                    "{model}"
                );
            } else {
                assert_eq!(json_body(&body)["stop_reason"], "pause_turn", "{model}");
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn claude_web_search_terminal_conflicts_fail_before_json_or_sse_success() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;

    for model in [
        "gpt-web-terminal-id-conflict",
        "gpt-web-terminal-model-conflict",
        "gpt-web-terminal-object-conflict",
        "gpt-web-terminal-status-conflict",
        "gpt-web-terminal-usage-conflict",
        "gpt-web-terminal-cached-usage-conflict",
        "gpt-web-terminal-reasoning-usage-conflict",
        "gpt-web-created-usage-details-malformed",
        "gpt-web-terminal-usage-details-malformed",
        "gpt-web-terminal-metadata-conflict",
        "gpt-web-incomplete-details-conflict",
        "gpt-web-created-metadata-malformed",
        "gpt-web-terminal-metadata-malformed",
        "gpt-web-created-incomplete-details-malformed",
        "gpt-web-terminal-incomplete-details-malformed",
        "gpt-web-end-turn-conflict",
        "gpt-web-output-text-conflict",
        "gpt-web-terminal-output-conflict",
        "gpt-web-terminal-lifecycle-output-conflict",
        "gpt-web-terminal-output-malformed",
        "gpt-web-unsupported-raw-output",
        "gpt-web-unrepresentable-search-call",
        "gpt-web-incomplete-search-call",
        "gpt-web-empty-query-entry",
        "gpt-web-item-id-conflict",
        "gpt-web-message-id-conflict",
        "gpt-web-item-status-conflict",
        "gpt-web-lifecycle-item-id-conflict",
        "gpt-web-lifecycle-item-action-conflict",
        "gpt-web-late-text-conflict",
        "gpt-web-delta-after-item-done",
        "gpt-web-terminal-failed",
        "gpt-web-terminal-incomplete",
        "gpt-web-later-terminal",
    ] {
        configure_with_web_search_model(&fixture, Some(&format!("responses-fixture/{model}")));
        for stream in [false, true] {
            let (status, body) = send(web_search_messages_request(stream)).await;
            assert!(
                status.is_server_error(),
                "{model}/{stream}: {status} {}",
                String::from_utf8_lossy(&body)
            );
            let error = json_body(&body);
            assert_eq!(error["type"], "error", "{model}/{stream}");
            assert_eq!(error["error"]["type"], "api_error", "{model}/{stream}");
            assert!(error.get("content").is_none(), "{model}/{stream}");
            assert!(error.get("stop_reason").is_none(), "{model}/{stream}");
        }
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn native_responses_preserves_all_raw_output_variants() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);
    let expected: Vec<Value> = audited_raw_output_variants()
        .into_iter()
        .map(|(_, item)| item)
        .collect();

    let request = |stream| {
        post_json(
            "/v1/responses",
            json!({
                "model":"responses-fixture/gpt-native-raw-variants",
                "input":"preserve raw output",
                "stream":stream
            }),
            Some(CLIENT_KEY),
        )
    };

    let (json_status, json_bytes) = send(request(false)).await;
    assert_eq!(json_status, StatusCode::OK);
    let response = json_body(&json_bytes);
    assert_eq!(response["output"], Value::Array(expected.clone()));

    let (sse_status, sse_bytes) = send(request(true)).await;
    assert_eq!(sse_status, StatusCode::OK);
    let events = data_events(&sse_bytes);
    let done_items: Vec<Value> = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.done")
        .map(|event| event["item"].clone())
        .collect();
    assert_eq!(done_items, expected);
    let terminal = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("native raw terminal");
    assert_eq!(terminal["response"]["output"], Value::Array(expected));
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn native_nonstream_responses_preserves_exact_null_and_unknown_shape() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let (status, body) = send(post_json(
        "/v1/responses",
        json!({
            "model":"responses-fixture/gpt-native-null-shape",
            "input":"preserve exact shape",
            "stream":false
        }),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_slice(), NATIVE_NULL_SHAPE.as_bytes());

    let (status, body) = send(post_json(
        "/v1/responses/compact",
        json!({
            "model":"responses-fixture/gpt-native-null-shape",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"compact"}]}]
        }),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_slice(), COMPACT_NULL_SHAPE.as_bytes());

    let (status, body) = send(post_json(
        "/v1/responses",
        json!({
            "model":"responses-fixture/gpt-malformed-json",
            "input":"reject malformed response",
            "stream":false
        }),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let error = json_body(&body);
    assert_eq!(error["error"]["type"], "server_error");
}

fn assert_upstream_metric(metric: &str, endpoint: &str, status: &str) {
    let rendered = copilot_api::libs::metrics::render();
    let prefix = format!("{metric}_count{{");
    assert!(
        rendered.lines().any(|line| {
            line.starts_with(&prefix)
                && line.contains(&format!("endpoint=\"{endpoint}\""))
                && line.contains(&format!("status=\"{status}\""))
                && line
                    .split_whitespace()
                    .last()
                    .and_then(|value| value.parse::<f64>().ok())
                    .is_some_and(|value| value >= 1.0)
        }),
        "missing bounded {metric} metric for {endpoint}/{status}:\n{rendered}"
    );
}

async fn await_usage_event(model: &str) -> copilot_api::libs::token_usage::TokenUsageEventRecord {
    for _ in 0..100 {
        if let Some(event) =
            copilot_api::libs::token_usage::get_token_usage_events_page(1, 100, "day")
                .items
                .into_iter()
                .find(|event| event.model == model)
        {
            return event;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for token usage event for {model}");
}

fn direct_compact_request(model: &str) -> Request<Body> {
    post_json(
        "/v1/responses/compact",
        json!({
            "model":model,
            "instructions":"Keep decisions.",
            "input":[{
                "type":"message",
                "role":"user",
                "content":[{"type":"input_text","text":"compact direct history"}]
            }],
            "fixture_extension":{"keep":true}
        }),
        Some(CLIENT_KEY),
    )
}

fn provider_compact_request(model: &str) -> Request<Body> {
    post_json(
        "/v1/responses/compact",
        json!({
            "model":format!("responses-fixture/{model}"),
            "instructions":"Keep provider decisions.",
            "input":[{
                "type":"message",
                "role":"user",
                "content":[{"type":"input_text","text":"compact provider history"}]
            }],
            "fixture_extension":{"keep":true}
        }),
        Some(CLIENT_KEY),
    )
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn provider_compact_uses_shared_output_contract_and_records_usage() {
    let _ = copilot_api::libs::metrics::metrics_handle();
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let (status, headers, body) =
        send_full(provider_compact_request("gpt-provider-compact-success")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_slice(), PROVIDER_COMPACT_SHAPE.as_bytes());
    assert_eq!(
        headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("provider-compact-request")
    );
    assert_eq!(
        headers
            .get("openai-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("provider-openai-request")
    );
    assert_eq!(
        headers
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok()),
        Some("provider-state")
    );
    assert!(headers.get("x-unsafe-secret").is_none());

    let compacted = serde_json::from_str::<Value>(PROVIDER_COMPACT_SHAPE)
        .expect("provider compact fixture JSON")["output"][1]
        .clone();
    let (status, _) = send(post_json(
        "/v1/responses",
        json!({
            "model":"responses-fixture/gpt-fixture",
            "input":[
                compacted,
                {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
            ],
            "stream":false
        }),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);

    let captures = fixture.requests();
    let compact_capture = captures
        .iter()
        .find(|capture| {
            capture.path == "/v1/responses/compact"
                && capture.body["model"] == "gpt-provider-compact-success"
        })
        .expect("provider compact capture");
    assert_eq!(
        compact_capture.headers["authorization"],
        format!("Bearer {UPSTREAM_KEY}")
    );
    assert_ne!(
        compact_capture.headers["authorization"],
        format!("Bearer {CLIENT_KEY}")
    );
    let continuation = captures
        .iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("provider compact continuation capture");
    let continued_compaction = continuation.body["input"]
        .as_array()
        .and_then(|input| input.iter().find(|item| item["type"] == "compaction"))
        .expect("id-less provider compaction continuation");
    assert!(continued_compaction.get("id").is_none());
    assert_eq!(
        continued_compaction["internal_chat_message_metadata_passthrough"]["turn_id"],
        "provider-turn"
    );

    let usage = await_usage_event("gpt-provider-compact-success").await;
    assert_eq!(usage.endpoint, "responses_compact");
    assert_eq!(usage.source, "provider");
    assert_eq!(usage.provider_name.as_deref(), Some("responses-fixture"));
    assert_eq!(usage.cache_read_input_tokens, 1);
    assert_eq!(usage.input_tokens, 4);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(usage.total_tokens, 7);
    assert_upstream_metric(
        "provider_upstream_request_seconds",
        "responses_compact",
        "ok",
    );
    assert!(
        copilot_api::libs::metrics::render()
            .lines()
            .filter(|line| line.starts_with("provider_upstream_request_seconds_count{"))
            .all(|line| !line.contains("provider=")),
        "configured provider aliases must not become metric labels"
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn provider_compact_failures_match_direct_native_semantics() {
    let _ = copilot_api::libs::metrics::metrics_handle();
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let invalid_models = [
        "gpt-provider-compact-wrong-item",
        "gpt-provider-compact-wrong-output",
        "gpt-provider-compact-usage-malformed",
        "gpt-provider-compact-usage-inconsistent",
        "gpt-provider-compact-usage-negative",
        "gpt-provider-compact-usage-overflow",
        "gpt-provider-compact-malformed-json",
        "gpt-provider-compact-oversized",
    ];
    for model in invalid_models {
        let (status, headers, body) = send_full(provider_compact_request(model)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{model}");
        let error = json_body(&body);
        assert_eq!(error["error"]["type"], "server_error", "{model}");
        assert_eq!(error["error"]["code"], "server_error", "{model}");
        assert_eq!(error["error"]["param"], Value::Null, "{model}");
        assert!(!body
            .windows(b"provider-not-json".len())
            .any(|window| window == b"provider-not-json"));
        let expected_request_id = match model {
            "gpt-provider-compact-malformed-json" => Some("provider-compact-malformed"),
            "gpt-provider-compact-oversized" => Some("provider-compact-oversized"),
            _ => None,
        };
        assert_eq!(
            headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            expected_request_id,
            "{model}"
        );
        assert!(headers.get("x-unsafe-secret").is_none(), "{model}");
    }

    for (model, expected_status, request_id, retry_after, expected_code) in [
        (
            "gpt-provider-compact-400",
            StatusCode::BAD_REQUEST,
            "provider-compact-400",
            None,
            "provider_compact_invalid",
        ),
        (
            "gpt-provider-compact-503",
            StatusCode::SERVICE_UNAVAILABLE,
            "provider-compact-503",
            Some("4"),
            "provider_compact_unavailable",
        ),
    ] {
        let (status, headers, body) = send_full(provider_compact_request(model)).await;
        assert_eq!(status, expected_status, "{model}");
        assert_eq!(
            headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            Some(request_id),
            "{model}"
        );
        assert_eq!(
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            retry_after,
            "{model}"
        );
        assert!(headers.get("x-unsafe-secret").is_none(), "{model}");
        let error = json_body(&body);
        assert_eq!(error["error"]["code"], expected_code, "{model}");
        assert_eq!(
            error["error"]["type"],
            if expected_status == StatusCode::BAD_REQUEST {
                "invalid_request_error"
            } else {
                "server_error"
            },
            "{model}"
        );
        if expected_status == StatusCode::BAD_REQUEST {
            assert_eq!(error["error"]["fixture_extension"]["keep"], true);
        }
        assert!(error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("provider compact")));
    }

    let events = copilot_api::libs::token_usage::get_token_usage_events_page(1, 100, "day").items;
    assert!(
        !events
            .iter()
            .any(|event| invalid_models.contains(&event.model.as_str())),
        "invalid compact responses must not emit usage: {events:#?}"
    );
    assert_upstream_metric(
        "provider_upstream_request_seconds",
        "responses_compact",
        "ok",
    );
    assert_upstream_metric(
        "provider_upstream_request_seconds",
        "responses_compact",
        "client_error",
    );
    assert_upstream_metric(
        "provider_upstream_request_seconds",
        "responses_compact",
        "server_error",
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn direct_copilot_compact_preserves_output_only_contract_and_continuation() {
    let _ = copilot_api::libs::metrics::metrics_handle();
    let fixture = Fixture::start().await;
    configure_direct_copilot(&fixture);

    for model in ["gpt-direct-compact-success", "gpt-direct-compact-headers"] {
        let (status, headers, body) = send_full(direct_compact_request(model)).await;
        assert_eq!(status, StatusCode::OK, "{model}");
        assert_eq!(body.as_slice(), DIRECT_COMPACT_SHAPE.as_bytes(), "{model}");
        assert_eq!(
            headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("direct-compact-request")
        );
        assert_eq!(
            headers
                .get("x-codex-turn-state")
                .and_then(|value| value.to_str().ok()),
            Some("direct-state")
        );
        assert!(headers.get("x-unsafe-secret").is_none());
    }

    let compacted = serde_json::from_str::<Value>(DIRECT_COMPACT_SHAPE)
        .expect("compact fixture JSON")["output"][0]
        .clone();
    let (status, body) = send(post_json(
        "/v1/responses",
        json!({
            "model":"gpt-direct-response-raw",
            "input":[
                compacted,
                {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
            ],
            "stream":false
        }),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_slice(), DIRECT_RESPONSES_SHAPE.as_bytes());
    let capture = fixture
        .requests()
        .into_iter()
        .rev()
        .find(|capture| capture.path == "/v1/responses")
        .expect("direct continuation capture");
    assert_eq!(capture.body["input"][0]["type"], "compaction");
    assert!(capture.body["input"][0].get("id").is_none());
    assert_eq!(capture.body["input"][0]["encrypted_content"], "enc_direct");
    assert_eq!(
        capture.body["input"][0]["internal_chat_message_metadata_passthrough"]["turn_id"],
        "direct-turn"
    );
    let usage = await_usage_event("gpt-direct-compact-success").await;
    assert_eq!(usage.endpoint, "responses_compact");
    assert_eq!(usage.source, "copilot");
    assert_eq!(usage.input_tokens, 2);
    assert_eq!(usage.output_tokens, 1);
    assert_eq!(usage.total_tokens, 3);
    assert_upstream_metric(
        "copilot_upstream_request_seconds",
        "responses_compact",
        "ok",
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn direct_copilot_compact_failures_use_native_bad_gateway_semantics() {
    let _ = copilot_api::libs::metrics::metrics_handle();
    let fixture = Fixture::start().await;
    configure_direct_copilot(&fixture);

    for (model, expected_request_id) in [
        (
            "gpt-direct-compact-malformed-json",
            Some("direct-compact-malformed"),
        ),
        ("gpt-direct-compact-wrong-output", None),
        ("gpt-direct-compact-wrong-item", None),
        ("gpt-direct-compact-wrong-usage", None),
        (
            "gpt-direct-compact-oversized",
            Some("direct-compact-oversized"),
        ),
    ] {
        let (status, headers, body) = send_full(direct_compact_request(model)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{model}");
        let error = json_body(&body);
        assert_eq!(error["error"]["type"], "server_error", "{model}");
        assert_eq!(error["error"]["code"], "server_error", "{model}");
        assert_eq!(error["error"]["param"], Value::Null, "{model}");
        assert!(!body.windows(8).any(|window| window == b"not-json"));
        assert_eq!(
            headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            expected_request_id,
            "{model}"
        );
        assert!(headers.get("x-unsafe-secret").is_none(), "{model}");
    }

    for (model, expected_status, request_id, retry_after) in [
        (
            "gpt-direct-compact-400",
            StatusCode::BAD_REQUEST,
            "direct-compact-400",
            None,
        ),
        (
            "gpt-direct-compact-503",
            StatusCode::SERVICE_UNAVAILABLE,
            "direct-compact-503",
            Some("3"),
        ),
    ] {
        let (status, headers, body) = send_full(direct_compact_request(model)).await;
        assert_eq!(status, expected_status, "{model}");
        assert_eq!(
            headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            Some(request_id)
        );
        assert_eq!(
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            retry_after
        );
        let error = json_body(&body);
        assert!(error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("direct compact")));
        assert_eq!(
            error["error"]["code"],
            if expected_status == StatusCode::BAD_REQUEST {
                "compact_invalid"
            } else {
                "compact_unavailable"
            },
            "{model}"
        );
    }
    assert_upstream_metric(
        "copilot_upstream_request_seconds",
        "responses_compact",
        "ok",
    );
    assert_upstream_metric(
        "copilot_upstream_request_seconds",
        "responses_compact",
        "client_error",
    );
    assert_upstream_metric(
        "copilot_upstream_request_seconds",
        "responses_compact",
        "server_error",
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn direct_copilot_regular_responses_preserve_bytes_headers_and_errors() {
    let _ = copilot_api::libs::metrics::metrics_handle();
    let fixture = Fixture::start().await;
    configure_direct_copilot(&fixture);
    let request = |model: &str| {
        post_json(
            "/v1/responses",
            json!({"model":model,"input":"direct response","stream":false}),
            Some(CLIENT_KEY),
        )
    };

    let (status, headers, body) = send_full(request("gpt-direct-response-raw")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_slice(), DIRECT_RESPONSES_SHAPE.as_bytes());
    assert_eq!(
        headers
            .get("openai-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("direct-response-request")
    );
    assert!(headers.get("x-unsafe-secret").is_none());

    for (model, expected_request_id) in [
        (
            "gpt-direct-response-malformed",
            Some("direct-response-malformed"),
        ),
        ("gpt-direct-response-wrong-shape", None),
        ("gpt-direct-response-wrong-item", None),
        ("gpt-direct-response-wrong-usage", None),
        (
            "gpt-direct-response-oversized",
            Some("direct-response-oversized"),
        ),
    ] {
        let (status, headers, body) = send_full(request(model)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{model}");
        let error = json_body(&body);
        assert_eq!(error["error"]["type"], "server_error");
        assert_eq!(error["error"]["code"], "server_error");
        assert_eq!(error["error"]["param"], Value::Null);
        assert_eq!(
            headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            expected_request_id,
            "{model}"
        );
        assert!(headers.get("x-unsafe-secret").is_none(), "{model}");
    }

    let (status, headers, body) = send_full(request("gpt-direct-response-400")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("direct-response-400")
    );
    let error = json_body(&body);
    assert_eq!(error["error"]["message"], "direct response invalid");
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(error["error"]["code"], "direct_invalid");
    assert_eq!(error["error"]["fixture_extension"]["keep"], true);

    let (status, headers, body) = send_full(request("gpt-direct-response-500")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("direct-response-500")
    );
    assert_eq!(
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    assert_eq!(json_body(&body)["error"]["code"], "direct_unavailable");
    assert_upstream_metric("copilot_upstream_request_seconds", "responses", "ok");
    assert_upstream_metric(
        "copilot_upstream_request_seconds",
        "responses",
        "client_error",
    );
    assert_upstream_metric(
        "copilot_upstream_request_seconds",
        "responses",
        "server_error",
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn native_responses_forwards_statusless_codex_terminal_unchanged() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let (status, body) = send(post_json(
        "/v1/responses",
        codex_request("gpt-terminal-completed-no-status-usage", true),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let completed: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "response.completed")
        .collect();
    assert_eq!(completed.len(), 1, "{events:#?}");
    assert_eq!(completed[0]["response"]["id"], "resp_terminal_fixture");
    assert!(completed[0]["response"].get("status").is_none());
    assert_eq!(completed[0]["response"]["usage"]["input_tokens"], 11);
    let created = events
        .iter()
        .find(|event| event["type"] == "response.created")
        .expect("native created event");
    assert!(created["response"].get("model").is_none());
    assert!(
        !events.iter().any(|event| {
            matches!(
                event["type"].as_str(),
                Some("message_delta" | "message_stop")
            )
        }),
        "native Responses events were translated into Anthropic events"
    );

    let (status, body) = send(post_json(
        "/v1/responses",
        codex_request("gpt-contract-usage-wrong-input", true),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("native malformed-usage terminal remains forwarded");
    assert_eq!(completed["response"]["usage"]["input_tokens"], "5");
    assert!(
        !events.iter().any(|event| event["type"] == "error"),
        "translated usage validation leaked into native Responses"
    );

    let (status, body) = send(post_json(
        "/v1/responses",
        codex_request("gpt-scalar-function-added-missing", true),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    let malformed = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .expect("native malformed function item remains forwarded");
    assert_eq!(malformed["item"]["type"], "function_call");
    assert!(malformed["item"].get("call_id").is_none());
    assert!(
        !events.iter().any(|event| event["type"] == "message_stop"),
        "translated scalar validation leaked into native Responses"
    );
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn codex_0_144_1_responses_and_compaction_cross_public_axum_boundary() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let models_request = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(models_request).await;
    assert_eq!(status, StatusCode::OK);
    let models = json_body(&body);
    assert!(models["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == "responses-fixture/gpt-fixture"));
    assert!(models["data"].as_array().unwrap().iter().any(|model| {
        model["id"] == "coding-default" && model["mapped_to"] == "responses-fixture/gpt-fixture"
    }));
    let unknown_model_request = Request::builder()
        .method("GET")
        .uri("/v1/models/not-configured")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(unknown_model_request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&body)["error"]["code"], "model_not_found");

    let mut nonstream_body = codex_request("gpt-fixture", false);
    nonstream_body["model"] = json!("coding-default");
    let mut request = post_json("/v1/responses", nonstream_body, Some(CLIENT_KEY));
    request
        .headers_mut()
        .insert("user-agent", "codex_cli_rs/0.144.1".parse().unwrap());
    request
        .headers_mut()
        .insert("session-id", "session-fixture".parse().unwrap());
    request
        .headers_mut()
        .insert("x-client-request-id", "thread-fixture".parse().unwrap());
    let (status, body) = send(request).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(body["object"], "response");
    assert_eq!(body["output"][0]["type"], "reasoning");
    assert_eq!(body["output"][1]["call_id"], "call_a");
    assert_eq!(body["output"][2]["call_id"], "call_b");
    assert_eq!(body["usage"]["input_tokens_details"]["cached_tokens"], 4);
    assert_eq!(body["fixture_extension"]["preserved"], true);

    let streaming = post_json(
        "/v1/responses",
        codex_request("gpt-fixture", true),
        Some(CLIENT_KEY),
    );
    let (status, headers, body) = send_full(streaming).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let events = data_events(&body);
    let types: Vec<&str> = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert_eq!(types.first().copied(), Some("response.created"));
    assert_eq!(types.last().copied(), Some("response.completed"));
    assert_eq!(
        types
            .iter()
            .filter(|event_type| **event_type == "response.completed")
            .count(),
        1
    );
    assert_eq!(
        types
            .iter()
            .filter(|event_type| {
                matches!(
                    **event_type,
                    "response.failed" | "response.incomplete" | "error"
                )
            })
            .count(),
        0
    );
    let call_a_added = events
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.added" && event["item"]["call_id"] == "call_a"
        })
        .unwrap();
    let call_a_done = events
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.done" && event["item"]["call_id"] == "call_a"
        })
        .unwrap();
    assert_eq!(call_a_added["item"]["id"], call_a_done["item"]["id"]);
    assert_eq!(call_a_done["item"]["arguments"], "{\"path\":\"a\"}");

    let compact = post_json(
        "/v1/responses/compact",
        json!({
            "model":"responses-fixture/gpt-fixture",
            "instructions":"Keep decisions.",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"long history"}]},
                {"type":"function_call","call_id":"c1","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"done"}
            ],
            "tools":[],
            "parallel_tool_calls":true,
            "reasoning":{"effort":"high","summary":"auto"},
            "prompt_cache_key":"thread-fixture",
            "text":{"verbosity":"low"},
            "fixture_compact_extension":{"keep":true}
        }),
        Some(CLIENT_KEY),
    );
    let (status, body) = send(compact).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    let compacted_item = body["output"][0].clone();
    assert_eq!(compacted_item["type"], "compaction");
    assert_eq!(compacted_item["encrypted_content"], "enc_compacted_history");
    assert!(
        compacted_item.get("id").is_none(),
        "Codex-valid compaction output must remain id-less"
    );
    assert_eq!(body["fixture_extension"]["preserved"], true);

    let continuation = post_json(
        "/v1/responses",
        json!({
            "model":"responses-fixture/gpt-fixture",
            "instructions":"Continue after compaction.",
            "input":[
                compacted_item,
                {
                    "type":"message",
                    "role":"user",
                    "content":[{"type":"input_text","text":"next turn"}]
                }
            ],
            "stream":false,
            "fixture_compaction_continuation":true
        }),
        Some(CLIENT_KEY),
    );
    let (status, body) = send(continuation).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "id-less compacted history must be accepted on the next turn: {}",
        String::from_utf8_lossy(&body)
    );

    let captures = fixture.requests();
    let response_capture = captures
        .iter()
        .find(|capture| capture.path == "/v1/responses" && capture.body["stream"] == false)
        .expect("captured Responses request");
    assert_eq!(response_capture.body["model"], "gpt-fixture");
    assert_eq!(response_capture.body["instructions"], "You are Codex.");
    assert_eq!(response_capture.body["parallel_tool_calls"], true);
    assert_eq!(
        response_capture.body["previous_response_id"],
        "resp_previous"
    );
    assert_eq!(response_capture.body["conversation"], "conv_fixture");
    assert_eq!(
        response_capture.body["fixture_top_level_extension"]["keep"],
        true
    );
    assert_eq!(
        response_capture.headers["authorization"],
        format!("Bearer {UPSTREAM_KEY}")
    );
    assert_eq!(response_capture.headers["session-id"], "session-fixture");
    assert_eq!(
        response_capture.headers["x-client-request-id"],
        "thread-fixture"
    );
    assert_ne!(
        response_capture.headers["authorization"],
        format!("Bearer {CLIENT_KEY}")
    );
    let compact_capture = captures
        .iter()
        .find(|capture| capture.path == "/v1/responses/compact")
        .expect("captured compact request");
    assert_eq!(
        compact_capture.body["fixture_compact_extension"]["keep"],
        true
    );
    assert!(compact_capture.body.get("stream").is_none());
    let continuation_capture = captures
        .iter()
        .find(|capture| capture.body["fixture_compaction_continuation"] == true)
        .expect("captured post-compaction continuation");
    assert_eq!(continuation_capture.body["input"][0]["type"], "compaction");
    assert_eq!(
        continuation_capture.body["input"][0]["encrypted_content"],
        "enc_compacted_history"
    );
    assert!(continuation_capture.body["input"][0].get("id").is_none());
    assert_eq!(continuation_capture.body["input"][1]["type"], "message");
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn codex_0_144_1_optional_continuation_items_cross_provider_boundary() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    for (case, item) in codex_optional_response_item_cases() {
        let request = json!({
            "model":"responses-fixture/gpt-fixture",
            "input":[item.clone()],
            "stream":false,
            "fixture_optionality_case":case
        });
        let (status, body) = send(post_json("/v1/responses", request, Some(CLIENT_KEY))).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{case}: {}",
            String::from_utf8_lossy(&body)
        );

        let captures = fixture.requests();
        let captured = captures
            .iter()
            .rev()
            .find(|capture| capture.body["fixture_optionality_case"] == case)
            .unwrap_or_else(|| panic!("{case}: provider did not capture request"));
        assert_eq!(
            captured.body["input"][0],
            normalized_optional_item(item),
            "{case}: continuation item changed at the public boundary"
        );
    }
}

#[tokio::test]
#[serial_test::serial(client_compatibility)]
async fn protocol_native_failures_are_deterministic_at_public_boundary() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let (status, _, body) = send_full(post_json(
        "/v1/responses",
        codex_request("gpt-fixture", false),
        None,
    ))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = json_body(&body);
    assert!(error.get("type").is_none());
    assert_eq!(error["error"]["type"], "authentication_error");
    assert_eq!(error["error"]["code"], "invalid_api_key");

    let (status, body) = send(post_json(
        "/v1/messages",
        json!({"model":"m","max_tokens":1,"messages":[{"role":"user","content":"x"}]}),
        None,
    ))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = json_body(&body);
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "authentication_error");

    let malformed = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .body(Body::from("{not-json"))
        .unwrap();
    let (status, body) = send(malformed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error = json_body(&body);
    assert!(error.get("type").is_none());
    assert_eq!(error["error"]["type"], "invalid_request_error");

    let malformed_chat = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .body(Body::from("{not-json"))
        .unwrap();
    let (status, body) = send(malformed_chat).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error = json_body(&body);
    assert!(error.get("type").is_none());
    assert_eq!(error["error"]["type"], "invalid_request_error");

    let (status, body) = send(post_json(
        "/v1/responses",
        json!({"model":"responses-fixture/gpt-fixture","input":[]}),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json_body(&body)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("input"));

    let unknown = Request::builder()
        .method("POST")
        .uri("/v1/responses/not-a-route")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = json_body(&body);
    assert!(error.get("type").is_none());
    assert_eq!(error["error"]["code"], "not_found");

    let wrong_method = Request::builder()
        .method("GET")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(wrong_method).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let error = json_body(&body);
    assert!(error.get("type").is_none());
    assert_eq!(error["error"]["code"], "method_not_allowed");

    for (model, expected_status) in [
        ("gpt-rate-limit", StatusCode::TOO_MANY_REQUESTS),
        ("gpt-upstream-500", StatusCode::INTERNAL_SERVER_ERROR),
        ("gpt-forbidden", StatusCode::FORBIDDEN),
        ("gpt-overload", StatusCode::SERVICE_UNAVAILABLE),
        ("gpt-unknown", StatusCode::NOT_FOUND),
        ("gpt-malformed-json", StatusCode::BAD_GATEWAY),
        ("gpt-oversized-response", StatusCode::BAD_GATEWAY),
    ] {
        let (status, headers, body) = send_full(post_json(
            "/v1/responses",
            codex_request(model, false),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(
            status,
            expected_status,
            "{model}: {}",
            String::from_utf8_lossy(&body)
        );
        let error = json_body(&body);
        assert!(error.get("type").is_none(), "{model}: {error}");
        assert!(error["error"]["message"].is_string(), "{model}: {error}");
        if model == "gpt-rate-limit" {
            assert_eq!(headers["retry-after"], "5");
            assert_eq!(headers["x-request-id"], "responses-rate-1");
            assert_eq!(error["error"]["fixture_extension"], true);
        }
        if model == "gpt-overload" {
            assert_eq!(headers["retry-after"], "1");
            assert_eq!(error["error"]["code"], "server_overloaded");
        }
        if model == "gpt-unknown" {
            assert_eq!(error["error"]["code"], "model_not_found");
        }
    }

    for model in ["gpt-malformed-stream", "gpt-premature-eof"] {
        let (status, body) = send(post_json(
            "/v1/responses",
            codex_request(model, true),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let events = data_events(&body);
        let terminal: Vec<&Value> = events
            .iter()
            .filter(|event| {
                matches!(
                    event["type"].as_str(),
                    Some(
                        "response.completed" | "response.failed" | "response.incomplete" | "error"
                    )
                )
            })
            .collect();
        assert_eq!(
            terminal.len(),
            1,
            "{model}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_ne!(terminal[0]["type"], "response.completed");
    }

    let (status, body) = send(post_json(
        "/v1/responses",
        codex_request("gpt-incomplete", true),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = data_events(&body);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.incomplete")
            .count(),
        1
    );
    assert!(!events
        .iter()
        .any(|event| event["type"] == "response.completed"));

    for model in ["claude-malformed-stream", "claude-premature-eof"] {
        let (status, body) = send(post_json(
            "/v1/messages",
            json!({
                "model":format!("anthropic-fixture/{model}"),
                "max_tokens":8,
                "messages":[{"role":"user","content":"x"}],
                "stream":true
            }),
            Some(CLIENT_KEY),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let events = data_events(&body);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1,
            "{model}: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(!events.iter().any(|event| event["type"] == "message_stop"));
    }

    let (status, headers, body) = send_full(post_json(
        "/v1/messages",
        json!({
            "model":"anthropic-fixture/claude-rate-limit",
            "max_tokens":8,
            "messages":[{"role":"user","content":"x"}]
        }),
        Some(CLIENT_KEY),
    ))
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "7");
    let error = json_body(&body);
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "rate_limit_error");

    let oversized = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .body(Body::from(vec![
            b'x';
            copilot_api::libs::http::MAX_REQUEST_BODY_BYTES
                + 1
        ]))
        .unwrap();
    let (status, body) = send(oversized).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let error = json_body(&body);
    assert!(error.get("type").is_none());
    assert_eq!(error["error"]["code"], "request_too_large");
}

/// Opt-in installed-client canary. It is ignored in normal CI and uses only two
/// loopback listeners plus fake credentials. Run exactly with:
///
/// `cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored --nocapture`
#[tokio::test]
#[ignore = "requires Codex CLI 0.144.1 installed; loopback-only opt-in canary"]
#[serial_test::serial(client_compatibility)]
async fn installed_codex_cli_smoke() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let fixture = Fixture::start().await;
    configure(&fixture);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind public proxy canary");
    let addr = listener.local_addr().expect("public proxy address");
    let (shutdown, receiver) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, copilot_api::server::build_router())
            .with_graceful_shutdown(async {
                let _ = receiver.await;
            })
            .await
            .expect("serve public proxy canary");
    });

    let version = tokio::process::Command::new("codex")
        .arg("--version")
        .output()
        .await
        .expect("Codex CLI must be installed");
    assert!(
        String::from_utf8_lossy(&version.stdout).contains("0.144.1"),
        "this canary is pinned to Codex CLI 0.144.1"
    );

    let codex_home =
        std::env::temp_dir().join(format!("copilot-api-codex-smoke-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&codex_home).expect("create isolated CODEX_HOME");
    let provider = format!(
        "model_providers.local_proxy={{name=\"local_proxy\",base_url=\"http://{addr}/v1\",env_key=\"COPILOT_API_KEY\",wire_api=\"responses\"}}"
    );
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("codex")
            .arg("exec")
            .arg("--skip-git-repo-check")
            .arg("--sandbox")
            .arg("read-only")
            .arg("-c")
            .arg("model=\"responses-fixture/gpt-cli-smoke\"")
            .arg("-c")
            .arg("model_provider=\"local_proxy\"")
            .arg("-c")
            .arg(provider)
            .arg("Reply with exactly OK")
            .env("CODEX_HOME", &codex_home)
            .env("COPILOT_API_KEY", CLIENT_KEY)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .output(),
    )
    .await
    .expect("Codex local canary timed out")
    .expect("run Codex local canary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Codex local canary failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("OK") || stderr.contains("OK"),
        "Codex did not consume the fixture response\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(fixture
        .requests()
        .iter()
        .any(|request| request.path == "/v1/responses"));

    let _ = shutdown.send(());
    let _ = server.await;
    let _ = std::fs::remove_dir_all(codex_home);
}
