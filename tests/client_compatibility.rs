//! Credential-free black-box compatibility coverage for Claude Code 2.1.207 and
//! Codex CLI 0.144.1.
//!
//! Requests enter through the production Axum router. Provider traffic is sent to
//! a deterministic loopback Axum fixture on an ephemeral port; no provider
//! credentials, external network, paid calls, or port 4141 are used.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;

const CLIENT_KEY: &str = "fixture-client-key";
const UPSTREAM_KEY: &str = "fixture-upstream-key";

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
        let state = FixtureState::default();
        let app = Router::new()
            .route("/v1/messages", post(fixture_handler))
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
        "/v1/responses" => responses_fixture(&body),
        "/v1/responses/compact" => Json(json!({
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
        other => panic!("unexpected fixture path {other}"),
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
            "encrypted_content":"encrypted-absent"
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
        _ => return None,
    })
}

fn responses_fixture(body: &Value) -> Response {
    let model = body["model"].as_str().unwrap_or("gpt-fixture");
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
                    "item":reasoning
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
                        "output":[],
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
                            "output":[],
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
                    "response.output_text.delta",
                    json!({
                        "type":"response.output_text.delta",
                        "sequence_number":1,
                        "output_index":0,
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
                    "sequence_number":2,
                    "response":{
                        "id":"resp_fixture",
                        "status":"incomplete",
                        "incomplete_details":{"reason":"max_output_tokens"},
                        "output":[],
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
                        "output":[],
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
                        "id":"different_a",
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
                        "id":"different_b",
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
    ]
    .into_iter()
    .map(|model| (model.to_string(), ModelConfig::default()))
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
