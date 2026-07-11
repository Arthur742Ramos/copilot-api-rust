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
use crate::libs::state;
use crate::libs::token_usage::{create_copilot_token_usage_recorder, normalize_openai_usage};
use crate::libs::utils::{generate_request_id_from_payload, get_uuid};
use crate::services::copilot::create_chat_completions::{
    create_chat_completions, ChatCompletionsOptions, ChatCompletionsPayload, ChatCompletionsResult,
};

/// Mirrors routes/chat-completions/handler.ts `handleCompletion`.
pub async fn handle_completion(body: Value, headers: HeaderMap) -> Result<Response, AppError> {
    let mut payload: ChatCompletionsPayload = serde_json::from_value(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid request payload: {e}")))?;

    if payload.model.trim().is_empty() {
        return Err(AppError::BadRequest(
            "model: field required and must be a non-empty string".to_string(),
        ));
    }

    let requested_model = payload.model.clone();
    payload.model = resolve_mapped_model(&payload.model);
    if payload.model != requested_model {
        tracing::debug!(
            "Resolved model mapping: {requested_model} -> {}",
            payload.model
        );
    }

    // Provider aliases return early, so apply policies shared by every
    // billable upstream before resolving the concrete transport.
    crate::libs::admission::check_shared_admission()
        .await
        .map_err(AppError::Http)?;

    if let Some(alias) = parse_provider_model_alias(&payload.model) {
        payload.model = alias.model.clone();
        return crate::routes::provider::chat_completions::handle_provider_chat_completions(
            payload,
            alias.provider,
            headers,
        )
        .await;
    }

    crate::libs::premium_interactions::check_premium_interactions()?;

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
            // Native non-streaming responses never pass through a StreamTimer, so
            // record the flow/model/transport headline here. This lets the
            // trace middleware's `has_flow` guard emit the single
            // `request.completed` line for this request (streaming responses are
            // covered by the StreamTimer drop instead — never both).
            if let Some(ctx) = crate::libs::request_context::request_context_store() {
                ctx.set_flow_transport_model_non_streaming(
                    &payload.model,
                    "chat_completions",
                    crate::libs::stream_metrics::transport::NATIVE,
                );
            }
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
    use crate::libs::stream_metrics::{transport, StreamTimer};
    use crate::libs::token_usage::UsageTokens;
    use std::sync::{Arc, Mutex};

    let usage_acc: Arc<Mutex<UsageTokens>> = Arc::new(Mutex::new(UsageTokens::default()));
    let usage_for_stream = usage_acc.clone();
    let recorder = Arc::new(recorder);

    // Time the native (raw byte-forwarded) stream so /v1/chat/completions is
    // covered by the same proxy_stream_* dashboards as the messages flows. TTFT
    // here is a coarser first-non-empty-chunk approximation (transport=native).
    // The timer is held by the stream closure and drops (recording
    // stream-complete) when the stream ends or the client disconnects.
    let timer_for_stream = Arc::new(Mutex::new(
        StreamTimer::new("chat_completions", transport::NATIVE)
            .with_request_context(crate::libs::request_context::request_context_store()),
    ));
    // Clone for the finalizing `once` future so `mark_finished` can be called
    // when the byte stream is fully exhausted (vs client disconnect, which drops
    // the stream and this clone simultaneously without the `once` ever running).
    let timer_for_finalizing = timer_for_stream.clone();

    let byte_stream = upstream.bytes_stream();
    let mapped = byte_stream.map(move |chunk| {
        match &chunk {
            Ok(bytes) => {
                if chunk_has_content(bytes) {
                    timer_for_stream
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .on_content_frame();
                }
                if let Some(usage) = sniff_usage(bytes) {
                    *usage_for_stream
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = usage;
                }
            }
            Err(_) => {
                // Upstream read failure: record the stream as errored so
                // proxy_stream_complete_seconds is labelled outcome="error".
                timer_for_stream
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .mark_error();
            }
        }
        chunk.map_err(std::io::Error::other)
    });

    // Record usage when the stream completes. The StreamTimer is captured by the
    // `map` closure above (via `timer_for_stream`), so it drops — recording
    // stream-complete — when the whole stream is dropped/exhausted, not in this
    // terminal future. This `once` only flushes the accumulated usage.
    let recorder_final = recorder.clone();
    let usage_final = usage_acc.clone();
    let finalizing = mapped.chain(futures_util::stream::once(async move {
        // Stream was fully exhausted (client received all bytes) — mark as
        // finished so the Drop outcome is "ok" rather than "cancelled".
        timer_for_finalizing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .mark_finished();
        let usage = std::mem::take(
            &mut *usage_final
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
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

/// Whether an SSE chunk carries a real content `data:` line (not empty / `[DONE]`).
/// Used to start time-to-first-token on the first genuine content frame.
fn chunk_has_content(bytes: &[u8]) -> bool {
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return false,
    };
    text.lines().any(|line| {
        let data = line.strip_prefix("data:").map(str::trim).unwrap_or("");
        !data.is_empty() && data != "[DONE]"
    })
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
