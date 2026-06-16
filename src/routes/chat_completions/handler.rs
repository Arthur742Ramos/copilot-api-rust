use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::libs::approval::await_approval;
use crate::libs::config::resolve_mapped_model;
use crate::libs::error::AppError;
use crate::libs::provider_model::parse_provider_model_alias;
use crate::libs::rate_limit::check_rate_limit;
use crate::libs::state;
use crate::libs::token_usage::{create_copilot_token_usage_recorder, normalize_openai_usage};
use crate::libs::utils::{generate_request_id_from_payload, get_uuid};
use crate::services::copilot::create_chat_completions::{
    create_chat_completions, ChatCompletionsOptions, ChatCompletionsPayload, ChatCompletionsResult,
};

/// Mirrors routes/chat-completions/handler.ts `handleCompletion`.
pub async fn handle_completion(body: Value, headers: HeaderMap) -> Result<Response, AppError> {
    let mut payload: ChatCompletionsPayload = serde_json::from_value(body)
        .map_err(|e| AppError::Other(anyhow::anyhow!("Invalid request payload: {e}")))?;

    let requested_model = payload.model.clone();
    payload.model = resolve_mapped_model(&payload.model);
    if payload.model != requested_model {
        tracing::debug!(
            "Resolved model mapping: {requested_model} -> {}",
            payload.model
        );
    }

    if let Some(alias) = parse_provider_model_alias(&payload.model) {
        payload.model = alias.model.clone();
        return crate::routes::provider::chat_completions::handle_provider_chat_completions(
            payload,
            alias.provider,
            headers,
        )
        .await;
    }

    check_rate_limit().await?;

    // Find the selected model from the cache.
    let selected_model = state::with_state(|s| {
        s.models.as_ref().and_then(|m| {
            m.data
                .iter()
                .find(|model| model.id == payload.model)
                .cloned()
        })
    });

    if selected_model.as_ref().map(|m| m.id.as_str()) == Some("gpt-5.4") {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "Please use `/v1/responses` or `/v1/messages` API",
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response());
    }

    if state::with_state(|s| s.manual_approve) {
        await_approval().await?;
    }

    if payload.max_tokens.is_none() {
        payload.max_tokens = selected_model
            .as_ref()
            .and_then(|m| m.capabilities.limits.max_output_tokens);
    }

    let request_id = generate_request_id_from_payload(&messages_as_values(&payload), None);
    tracing::debug!("Generated request ID: {request_id}");

    let session_id = get_uuid(&request_id);
    tracing::debug!("Extracted session ID: {session_id}");

    let recorder = create_copilot_token_usage_recorder(
        "chat_completions",
        payload.model.clone(),
        Some(session_id.clone()),
    );

    let is_stream = payload.stream.unwrap_or(false);
    let result = create_chat_completions(
        &payload,
        ChatCompletionsOptions {
            subagent_marker: None,
            request_id,
            session_id: Some(session_id),
            compact_type: None,
        },
    )
    .await?;

    match result {
        ChatCompletionsResult::NonStreaming(response) => {
            recorder.record(normalize_openai_usage(response.get("usage")));
            Ok(Json(response).into_response())
        }
        ChatCompletionsResult::Streaming(upstream) => {
            let _ = is_stream;
            Ok(stream_sse(upstream, recorder).into_response())
        }
    }
}

fn messages_as_values(payload: &ChatCompletionsPayload) -> Vec<Value> {
    payload
        .messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
        .collect()
}

/// Forward the upstream SSE byte stream to the client unchanged, while sniffing
/// `usage` out of each `data:` line to feed the token-usage recorder at the end.
fn stream_sse(
    upstream: reqwest::Response,
    recorder: crate::libs::token_usage::TokenUsageRecorder,
) -> Response {
    use crate::libs::token_usage::UsageTokens;
    use std::sync::{Arc, Mutex};

    let usage_acc: Arc<Mutex<UsageTokens>> = Arc::new(Mutex::new(UsageTokens::default()));
    let usage_for_stream = usage_acc.clone();
    let recorder = Arc::new(recorder);

    let byte_stream = upstream.bytes_stream();
    let mapped = byte_stream.map(move |chunk| {
        if let Ok(bytes) = &chunk {
            if let Some(usage) = sniff_usage(bytes) {
                *usage_for_stream.lock().unwrap() = usage;
            }
        }
        chunk.map_err(std::io::Error::other)
    });

    // Record usage when the stream completes (the recorder is held by the
    // stream's closure; cloning the Arc lets us record on the final chunk).
    let recorder_final = recorder.clone();
    let usage_final = usage_acc.clone();
    let finalizing = mapped.chain(futures_util::stream::once(async move {
        let usage = std::mem::take(&mut *usage_final.lock().unwrap());
        recorder_final.record(usage);
        Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::new())
    }));

    let body = Body::from_stream(finalizing);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap()
}

/// Parse `data: {json}` lines out of an SSE chunk and return normalized usage
/// if a chunk carries a `usage` object.
fn sniff_usage(bytes: &[u8]) -> Option<crate::libs::token_usage::UsageTokens> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut found = None;
    for line in text.lines() {
        let data = line.strip_prefix("data:")?.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            if value.get("usage").map(|u| !u.is_null()).unwrap_or(false) {
                found = Some(normalize_openai_usage(value.get("usage")));
            }
        }
    }
    found
}
