use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::libs::api_config::{
    copilot_base_url, copilot_headers, prepare_for_compact, prepare_interaction_headers, set_header,
};
use crate::libs::copilot_rate_limit::log_copilot_rate_limits;
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::client;
use crate::libs::state;
use crate::libs::subagent::SubagentMarker;

/// Options for `create_chat_completions`, mirroring the TS options object.
pub struct ChatCompletionsOptions {
    pub subagent_marker: Option<SubagentMarker>,
    pub request_id: String,
    pub session_id: Option<String>,
    pub compact_type: Option<i32>,
}

/// The result of a chat-completions call: either a fully-buffered JSON response
/// (non-streaming) or a streaming reqwest response whose SSE body is forwarded.
pub enum ChatCompletionsResult {
    NonStreaming(Value),
    Streaming(reqwest::Response),
}

/// Mirrors `createChatCompletions` in services/copilot/create-chat-completions.ts.
pub async fn create_chat_completions(
    payload: &ChatCompletionsPayload,
    options: ChatCompletionsOptions,
) -> Result<ChatCompletionsResult, HttpError> {
    let st = state::snapshot();
    if st.copilot_token.as_deref().unwrap_or("").is_empty() {
        return Err(HttpError::internal("Copilot token not found"));
    }

    let enable_vision = payload.messages.iter().any(|m| match &m.content {
        Some(Value::Array(parts)) => parts
            .iter()
            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("image_url")),
        _ => false,
    });

    // x-initiator: agent when the LAST message is from assistant/tool.
    let is_agent_call = payload
        .messages
        .last()
        .map(|m| m.role == "assistant" || m.role == "tool")
        .unwrap_or(false);

    let mut headers: HeaderMap = copilot_headers(&st, Some(&options.request_id), enable_vision);
    set_header(
        &mut headers,
        "x-initiator",
        if is_agent_call { "agent" } else { "user" },
    );

    prepare_interaction_headers(
        options.session_id.as_deref(),
        options.subagent_marker.is_some(),
        &mut headers,
    );

    prepare_for_compact(&mut headers, options.compact_type);

    tracing::info!("<-- model: {}", payload.model);

    let base = copilot_base_url(&st);
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    let upstream_start = std::time::Instant::now();
    let response = client()
        .post(format!("{base}/chat/completions"))
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|e| {
            crate::libs::metrics::record_upstream_request(
                "chat",
                crate::libs::metrics::UpstreamStatus::TransportError,
                upstream_start.elapsed().as_secs_f64(),
            );
            HttpError::internal(format!("Failed to create chat completions: {e}"))
        })?;
    crate::libs::metrics::record_upstream_request(
        "chat",
        crate::libs::metrics::UpstreamStatus::from_code(response.status().as_u16()),
        upstream_start.elapsed().as_secs_f64(),
    );

    {
        // Convert reqwest headers to axum HeaderMap for the rate-limit logger.
        let axum_headers = reqwest_headers_to_axum(response.headers());
        log_copilot_rate_limits(&axum_headers);
    }

    if !response.status().is_success() {
        tracing::error!("Failed to create chat completions");
        return Err(http_error_from_response("Failed to create chat completions", response).await);
    }

    if payload.stream.unwrap_or(false) {
        Ok(ChatCompletionsResult::Streaming(response))
    } else {
        let json = crate::libs::http::read_json_capped::<Value>(response)
            .await
            .map_err(|e| HttpError::internal(format!("Failed to parse chat completions: {e}")))?;
        Ok(ChatCompletionsResult::NonStreaming(json))
    }
}

pub fn reqwest_headers_to_axum(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::from_bytes(name.as_str().as_bytes()),
            axum::http::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(n, v);
        }
    }
    out
}

// --- Payload types ---------------------------------------------------------
//
// The wire payload is largely passthrough; we keep the fields the handler needs
// (model, messages, max_tokens, stream) strongly typed and capture everything
// else in `extra` so the body round-trips unchanged to the upstream API.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionsPayload {
    pub messages: Vec<Message>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
