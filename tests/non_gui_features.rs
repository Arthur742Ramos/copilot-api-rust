//! Focused conformance tests for the non-GUI feature-lead surface.

mod common;

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::{OriginalUri, RawQuery};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{json_body, send, send_full};
use copilot_api::libs::config::{
    set_cached_config_for_test, AppConfig, AuthConfig, ModelConfig, ProviderConfig,
};
use copilot_api::libs::state;
use serde_json::{json, Map, Value};

async fn alpha_fixture(
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if body.get("force_error") == Some(&Value::Bool(true)) {
        return (
            StatusCode::UNAUTHORIZED,
            [("x-request-id", "alpha-auth-failure")],
            Json(json!({
                "error": {
                    "type": "authentication_error",
                    "message": "fixture rejected credentials"
                }
            })),
        )
            .into_response();
    }
    Json(json!({
        "object": "alpha.search.results",
        "query": query,
        "upstream_path": uri.path(),
        "echo": body,
        "authenticated": headers.contains_key("authorization"),
        "authorization_replaced": headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some("Bearer inbound-client-token"),
        "accept": headers.get("accept").and_then(|value| value.to_str().ok()),
        "openai_beta": headers
            .get("openai-beta")
            .and_then(|value| value.to_str().ok()),
        "content_type": headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
    }))
    .into_response()
}

async fn chat_fixture(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if body.get("stream") == Some(&Value::Bool(true)) {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(
                "data: {\"id\":\"chat_fixture\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\n\
                 data: {\"id\":\"chat_fixture\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n\
                 data: [DONE]\n\n",
            ))
            .unwrap();
    }
    Json(json!({
        "id": "chat_fixture",
        "object": "chat.completion",
        "created": 1,
        "model": body["model"],
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        "future": body.get("future").cloned(),
        "saw_authorization": headers.contains_key("authorization"),
        "saw_x_api_key": headers.contains_key("x-api-key"),
    }))
    .into_response()
}

async fn responses_fixture(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if body.get("stream") == Some(&Value::Bool(true)) {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(
                "event: response.created\n\
                 data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_fixture\",\"model\":\"gpt-fixture\"}}\n\n\
                 event: response.completed\n\
                 data: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_fixture\",\"model\":\"gpt-fixture\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
            ))
            .unwrap();
    }
    Json(json!({
        "id": "resp_fixture",
        "object": "response",
        "created_at": 1,
        "model": body["model"],
        "status": "completed",
        "output": [],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
        "future": body.get("future").cloned(),
        "saw_authorization": headers.contains_key("authorization"),
        "saw_x_api_key": headers.contains_key("x-api-key"),
    }))
    .into_response()
}

async fn anthropic_fixture(headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    Json(json!({
        "id":"msg_mixed_fixture",
        "type":"message",
        "role":"assistant",
        "model":body["model"],
        "content":[{"type":"text","text":"native anthropic"}],
        "stop_reason":"end_turn",
        "stop_sequence":null,
        "usage":{"input_tokens":1,"output_tokens":1},
        "saw_authorization":headers.contains_key("authorization"),
        "saw_x_api_key":headers.contains_key("x-api-key"),
    }))
}

async fn compact_fixture(Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({
        "output": [],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
        "future_compact_field": true,
    }))
}

async fn start_fixture() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/v1/alpha/search", post(alpha_fixture))
        .route("/codex/alpha/search", post(alpha_fixture))
        .route("/v1/chat/completions", post(chat_fixture))
        .route("/v1/responses", post(responses_fixture))
        .route("/v1/messages", post(anthropic_fixture))
        .route("/v1/responses/compact", post(compact_fixture))
        .route(
            "/v1/models",
            get(|| async {
                Json(json!({"object":"list","data":[{"id":"fixture-model","object":"model"}]}))
            }),
        )
        .route(
            "/v1/images/generations",
            post(|| async { Json(json!({"created":1,"data":[{"b64_json":"ZmFrZQ=="}]})) }),
        )
        .route(
            "/v1/images/edits",
            post(|| async { Json(json!({"created":1,"data":[{"b64_json":"ZWRpdA=="}]})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), task)
}

fn provider(
    provider_type: &str,
    base_url: &str,
    capabilities: Option<Vec<&str>>,
) -> ProviderConfig {
    ProviderConfig {
        provider_type: Some(provider_type.to_string()),
        enabled: Some(true),
        base_url: Some(base_url.to_string()),
        api_key: Some("fixture-upstream-key".to_string()),
        auth_type: Some(if provider_type == "anthropic" {
            "x-api-key".to_string()
        } else {
            "authorization".to_string()
        }),
        models: None,
        capabilities: capabilities.map(|items| items.into_iter().map(str::to_string).collect()),
        adjust_input_tokens: None,
        extra: Map::new(),
    }
}

fn install_fixture_config(base_url: &str, client_keys: &[&str]) {
    let mixed_models = BTreeMap::from([
        (
            "mixed-chat".to_string(),
            ModelConfig {
                provider_type: Some("openai-compatible".to_string()),
                extra: Map::from_iter([(
                    "futureModelOption".to_string(),
                    json!({"preserved":true}),
                )]),
                ..Default::default()
            },
        ),
        (
            "mixed-responses".to_string(),
            ModelConfig {
                provider_type: Some("openai-responses".to_string()),
                ..Default::default()
            },
        ),
    ]);
    let native_model = BTreeMap::from([(
        "mixed-anthropic".to_string(),
        ModelConfig {
            provider_type: Some("anthropic".to_string()),
            ..Default::default()
        },
    )]);
    let providers = BTreeMap::from([
        (
            "responses-fixture".to_string(),
            provider("openai-responses", base_url, None),
        ),
        (
            "chat-fixture".to_string(),
            provider("openai-compatible", base_url, None),
        ),
        (
            "anthropic-fixture".to_string(),
            provider("anthropic", base_url, None),
        ),
        (
            "mixed-fixture".to_string(),
            ProviderConfig {
                models: Some(mixed_models),
                ..provider("anthropic", base_url, None)
            },
        ),
        (
            "responses-mixed-fixture".to_string(),
            ProviderConfig {
                models: Some(native_model),
                ..provider("openai-responses", base_url, None)
            },
        ),
        (
            "codex".to_string(),
            ProviderConfig {
                provider_type: Some("openai-responses".to_string()),
                enabled: Some(true),
                base_url: Some(base_url.to_string()),
                api_key: None,
                auth_type: Some("oauth2".to_string()),
                models: None,
                capabilities: None,
                adjust_input_tokens: None,
                extra: Map::new(),
            },
        ),
    ]);
    set_cached_config_for_test(AppConfig {
        auth: Some(AuthConfig {
            api_keys: Some(client_keys.iter().map(|key| json!(key)).collect()),
            admin_api_key: None,
        }),
        providers: Some(providers),
        use_responses_api_web_socket: Some(false),
        use_responses_api_context_management: Some(false),
        ..Default::default()
    });
    state::with_state_mut(|state| {
        state.codex_access_token = Some("fixture-codex-token".to_string());
        state.codex_refresh_token = Some("fixture-refresh-token".to_string());
        state.codex_account_id = Some("fixture-account".to_string());
        state.codex_expires_at = Some(chrono::Utc::now().timestamp_millis() + 3_600_000);
    });
}

fn post_json(path: &str, value: Value, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    builder.body(Body::from(value.to_string())).unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn alpha_search_public_provider_aliases_and_errors_are_conformant() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let (base_url, fixture) = start_fixture().await;
    install_fixture_config(&base_url, &[]);

    for path in [
        "/alpha/search?q=rust",
        "/v1/alpha/search?q=rust",
        "/codex/alpha/search?q=rust",
        "/codex/v1/alpha/search?q=rust",
        "/responses-fixture/alpha/search?q=rust",
        "/responses-fixture/v1/alpha/search?q=rust",
    ] {
        let mut request = post_json(
            path,
            json!({"query":"rust","future_option":{"rank":true}}),
            None,
        );
        request
            .headers_mut()
            .insert("accept", "application/vnd.alpha+json".parse().unwrap());
        request
            .headers_mut()
            .insert("openai-beta", "alpha-client-beta".parse().unwrap());
        request.headers_mut().insert(
            "authorization",
            "Bearer inbound-client-token".parse().unwrap(),
        );
        let (status, body) = send(request).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
        let body = json_body(&body);
        assert_eq!(body["query"], "q=rust");
        assert_eq!(body["echo"]["future_option"]["rank"], true);
        assert_eq!(body["authenticated"], true);
        assert_eq!(body["authorization_replaced"], true);
        assert_eq!(body["accept"], "application/vnd.alpha+json");
        assert_eq!(body["openai_beta"], "alpha-client-beta");
        assert_eq!(body["content_type"], "application/json");
        let expected_upstream = if path.starts_with("/responses-fixture/") {
            "/v1/alpha/search"
        } else {
            "/codex/alpha/search"
        };
        assert_eq!(body["upstream_path"], expected_upstream, "{path}");
    }

    let (status, body) = send(post_json(
        "/v1/alpha/search?mode=default",
        json!({"query":"default headers"}),
        None,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(body["accept"], "application/json");
    assert!(body["openai_beta"].is_null());

    for path in ["/alpha/search", "/responses-fixture/v1/alpha/search"] {
        let (status, body) = send(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from("{truncated"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert!(json_body(&body)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid JSON"));
    }

    let (status, headers, body) = send_full(post_json(
        "/responses-fixture/v1/alpha/search",
        json!({"force_error":true}),
        None,
    ))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["x-request-id"], "alpha-auth-failure");
    assert!(String::from_utf8_lossy(&body).contains("fixture rejected credentials"));

    install_fixture_config(&base_url, &["client-key"]);
    let (status, body) = send(post_json("/v1/alpha/search", json!({"query":"rust"}), None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let body = json_body(&body);
    assert!(body.get("type").is_none());
    assert_eq!(body["error"]["type"], "authentication_error");

    state::with_state_mut(|state| {
        state.codex_access_token = None;
        state.codex_refresh_token = None;
        state.codex_account_id = None;
        state.codex_expires_at = None;
    });
    fixture.abort();
    std::env::remove_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS");
}

#[tokio::test]
#[serial_test::serial]
async fn provider_route_breadth_supports_streaming_and_non_streaming_aliases() {
    std::env::set_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS", "1");
    let (base_url, fixture) = start_fixture().await;
    install_fixture_config(&base_url, &[]);

    for path in ["/chat-fixture/messages", "/chat-fixture/v1/messages"] {
        let (status, body) = send(post_json(
            path,
            json!({
                "model":"chat-model",
                "max_tokens":16,
                "messages":[{"role":"user","content":"hello"}]
            }),
            None,
        ))
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(json_body(&body)["role"], "assistant");
    }

    for path in [
        "/chat-fixture/messages/count_tokens",
        "/chat-fixture/v1/messages/count_tokens",
    ] {
        let (status, body) = send(post_json(
            path,
            json!({
                "model":"chat-model",
                "messages":[{"role":"user","content":"hello"}]
            }),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(json_body(&body)["input_tokens"].as_i64().is_some());
    }

    let (status, body) = send(post_json(
        "/mixed-fixture/v1/chat/completions",
        json!({
            "model":"mixed-chat",
            "messages":[{"role":"user","content":"hello"}]
        }),
        None,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(body["saw_authorization"], true);
    assert_eq!(body["saw_x_api_key"], false);

    let (status, body) = send(post_json(
        "/mixed-fixture/v1/responses",
        json!({"model":"mixed-responses","input":"hello"}),
        None,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(body["saw_authorization"], true);
    assert_eq!(body["saw_x_api_key"], false);

    for (path, model) in [
        ("/mixed-fixture/v1/messages", "mixed-chat"),
        ("/mixed-fixture/v1/messages", "mixed-responses"),
    ] {
        let (status, body) = send(post_json(
            path,
            json!({
                "model":model,
                "max_tokens":16,
                "messages":[{"role":"user","content":"hello"}]
            }),
            None,
        ))
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{model}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let (status, body) = send(post_json(
        "/responses-mixed-fixture/v1/messages",
        json!({
            "model":"mixed-anthropic",
            "max_tokens":16,
            "messages":[{"role":"user","content":"hello"}]
        }),
        None,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(body["saw_authorization"], false);
    assert_eq!(body["saw_x_api_key"], true);

    for (path, model) in [
        ("/mixed-fixture/v1/responses", "mixed-chat"),
        ("/mixed-fixture/v1/chat/completions", "mixed-responses"),
    ] {
        let (status, body) = send(post_json(
            path,
            json!({"model":model,"input":"hello","messages":[]}),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(
            json_body(&body)["error"]["code"],
            "unsupported_provider_capability"
        );
    }

    for path in [
        "/chat-fixture/chat/completions",
        "/chat-fixture/v1/chat/completions",
    ] {
        let (status, body) = send(post_json(
            path,
            json!({
                "model":"chat-model",
                "messages":[{"role":"user","content":"hello"}],
                "future":{"round_trip":true}
            }),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(json_body(&body)["future"]["round_trip"], true);

        let (status, body) = send(post_json(
            path,
            json!({
                "model":"chat-model",
                "messages":[{"role":"user","content":"hello"}],
                "stream":true
            }),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(String::from_utf8_lossy(&body).contains("[DONE]"));
    }

    for path in [
        "/responses-fixture/responses",
        "/responses-fixture/v1/responses",
    ] {
        let (status, body) = send(post_json(
            path,
            json!({
                "model":"gpt-fixture",
                "input":"hello",
                "future":{"round_trip":true}
            }),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(json_body(&body)["future"]["round_trip"], true);

        let (status, body) = send(post_json(
            path,
            json!({"model":"gpt-fixture","input":"hello","stream":true}),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let stream = String::from_utf8_lossy(&body);
        assert!(stream.contains("response.created"));
        assert!(stream.contains("response.completed"));
    }

    for path in [
        "/responses-fixture/responses/compact",
        "/responses-fixture/v1/responses/compact",
    ] {
        let (status, body) = send(post_json(
            path,
            json!({"model":"gpt-fixture","input":"hello","future_request":true}),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(json_body(&body)["future_compact_field"], true);
    }

    for path in ["/responses-fixture/models", "/responses-fixture/v1/models"] {
        let (status, body) = send(
            Request::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(json_body(&body)["data"][0]["id"], "fixture-model");
    }

    for path in [
        "/responses-fixture/images/generations",
        "/responses-fixture/v1/images/generations",
    ] {
        let (status, body) = send(post_json(
            path,
            json!({"model":"image-fixture","prompt":"robot","future_option":true}),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(json_body(&body)["data"][0]["b64_json"], "ZmFrZQ==");
    }

    for path in [
        "/responses-fixture/images/edits",
        "/responses-fixture/v1/images/edits",
    ] {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("content-type", "multipart/form-data; boundary=fixture")
            .body(Body::from(
                "--fixture\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit\r\n--fixture--\r\n",
            ))
            .unwrap();
        let (status, body) = send(request).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(json_body(&body)["data"][0]["b64_json"], "ZWRpdA==");
    }

    for path in [
        "/anthropic-fixture/responses",
        "/anthropic-fixture/v1/chat/completions",
        "/anthropic-fixture/alpha/search",
    ] {
        let (status, body) = send(post_json(
            path,
            json!({"model":"unsupported","input":"hello","messages":[]}),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert!(json_body(&body)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not support"));
    }

    for path in [
        "/missing-provider/responses",
        "/missing-provider/v1/chat/completions",
    ] {
        let (status, body) = send(post_json(
            path,
            json!({"model":"missing","input":"hello","messages":[]}),
            None,
        ))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(json_body(&body)["error"]["code"], "provider_not_found");
    }

    fixture.abort();
    std::env::remove_var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS");
}
