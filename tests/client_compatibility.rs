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
                "response":{"id":"resp_scalar_fixture","output":output}
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

    if body["stream"] == true {
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
        "gpt-scalar-function-valid",
        "gpt-scalar-function-whitespace-namespace-valid",
        "gpt-scalar-tool-search-valid",
        "gpt-scalar-tool-search-output-valid",
        "gpt-scalar-message-valid",
        "gpt-scalar-reasoning-valid",
        "gpt-scalar-compaction-valid",
        "gpt-scalar-function-added-missing",
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
        "gpt-scalar-tool-search-missing-execution",
        "gpt-scalar-tool-search-wrong",
        "gpt-scalar-tool-search-wrong-execution",
        "gpt-scalar-tool-search-wrong-status",
        "gpt-scalar-tool-search-missing-arguments",
        "gpt-scalar-tool-search-output-malformed",
        "gpt-scalar-tool-search-output-wrong-execution",
        "gpt-scalar-tool-search-output-wrong-tools",
        "gpt-scalar-message-missing",
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
        "gpt-contract-created-model-less",
        "gpt-contract-created-with-model",
        "gpt-contract-created-upstream-model",
        "gpt-contract-created-empty-id",
        "gpt-contract-created-wrong-id",
        "gpt-contract-created-missing-id",
        "gpt-contract-created-empty-model",
        "gpt-contract-created-wrong-model",
        "gpt-contract-created-status-mismatch",
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
        "gpt-terminal-completed-mismatched-status",
        "gpt-terminal-incomplete-no-status-usage",
        "gpt-terminal-incomplete-no-status-no-usage",
        "gpt-terminal-incomplete-matching-status",
        "gpt-terminal-incomplete-mismatched-status",
        "gpt-terminal-completed-pending-item",
        "gpt-terminal-incomplete-pending-item",
        "gpt-terminal-completed-repeated-later",
        "gpt-terminal-incomplete-repeated-later",
        "gpt-terminal-failed-later",
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
        "gpt-terminal-incomplete-mismatched-status",
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
        "gpt-scalar-function-whitespace-namespace-valid",
        "gpt-scalar-tool-search-valid",
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
        "gpt-scalar-tool-search-missing-execution",
        "gpt-scalar-tool-search-wrong",
        "gpt-scalar-tool-search-wrong-execution",
        "gpt-scalar-tool-search-wrong-status",
        "gpt-scalar-tool-search-missing-arguments",
        "gpt-scalar-tool-search-output-malformed",
        "gpt-scalar-tool-search-output-wrong-execution",
        "gpt-scalar-tool-search-output-wrong-tools",
        "gpt-scalar-message-missing",
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
