//! `/responses` route handler — ports routes/responses/handler.ts `handleResponses`.

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::libs::approval::await_approval;
use crate::libs::config::{is_responses_api_web_search_enabled, resolve_mapped_model};
use crate::libs::error::AppError;
use crate::libs::provider_model::parse_provider_model_alias;
use crate::libs::rate_limit::check_rate_limit;
use crate::libs::state;
use crate::libs::subagent::SubagentMarker;
use crate::libs::token_usage::{create_copilot_token_usage_recorder, normalize_responses_usage};
use crate::libs::utils::{generate_request_id_from_payload, get_uuid};
use crate::services::copilot::create_responses::{
    create_responses, CreateResponsesReturn, InputField, ResponsesPayload, ResponsesRequestOptions,
};

use crate::routes::responses::stream_id_sync::{fix_stream_ids, StreamIdTracker};
use crate::routes::responses::utils::{
    apply_responses_api_context_management, compact_input_by_latest_compaction,
    get_responses_request_options, get_responses_transport_for_model,
    sanitize_oversized_input_images,
};

/// Mirrors routes/responses/handler.ts `handleResponses`.
pub async fn handle_responses(body: Value, headers: HeaderMap) -> Result<Response, AppError> {
    let mut payload: ResponsesPayload = serde_json::from_value(body)
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

    if let Some(alias) = parse_provider_model_alias(&payload.model) {
        payload.model = alias.model.clone();
        return crate::routes::provider::responses::handle_provider_responses_for_provider(
            payload,
            alias.provider,
            headers,
        )
        .await;
    }

    check_rate_limit().await?;

    let subagent_marker = get_codex_responses_subagent_marker(&headers);
    if let Some(marker) = subagent_marker.as_ref() {
        tracing::debug!("Detected Codex subagent headers: {marker:?}");
    }

    let incoming_session_id = get_incoming_responses_session_id(&headers);
    let session_id = incoming_session_id.as_deref().map(get_uuid);

    let request_id = responses_request_id(&payload, session_id.as_deref());
    tracing::debug!("Generated request ID: {request_id}");

    let fallback_session_id = session_id.unwrap_or_else(|| get_uuid(&request_id));
    tracing::debug!("Extracted session ID: {fallback_session_id}");

    let recorder = create_copilot_token_usage_recorder(
        "responses",
        payload.model.clone(),
        Some(fallback_session_id.clone()),
    );

    remove_unsupported_tools(&mut payload);

    if !is_responses_api_web_search_enabled() {
        remove_web_search_tool(&mut payload);
    }

    compact_input_by_latest_compaction(&mut payload);

    let selected_model = state::with_state(|s| {
        s.models.as_ref().and_then(|m| {
            m.data
                .iter()
                .find(|model| model.id == payload.model)
                .cloned()
        })
    });

    let responses_transport = get_responses_transport_for_model(selected_model.as_ref(), None);

    let Some(responses_transport) = responses_transport else {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "This model does not support the responses endpoint. Please choose a different model.",
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response());
    };

    let max_prompt_image_size = selected_model
        .as_ref()
        .and_then(|m| m.capabilities.limits.vision.as_ref())
        .and_then(|v| v.max_prompt_image_size);
    let sanitized_image_count =
        sanitize_oversized_input_images(&mut payload, max_prompt_image_size);
    if sanitized_image_count > 0 {
        tracing::warn!(
            "Omitted {sanitized_image_count} oversized input image(s) before forwarding to Copilot Responses"
        );
    }

    // Smaller than the client compaction threshold, use server-side compaction to
    // maintain cache hit rate.
    let max_prompt_tokens = selected_model
        .as_ref()
        .and_then(|m| m.capabilities.limits.max_prompt_tokens);
    apply_responses_api_context_management(&mut payload, max_prompt_tokens, 0.8);

    let (vision, inferred_initiator) = get_responses_request_options(&payload);
    let initiator = if subagent_marker.is_some() {
        "agent"
    } else {
        inferred_initiator
    };

    if state::with_state(|s| s.manual_approve) {
        await_approval().await?;
    }

    let is_stream = payload.stream.unwrap_or(false);

    let response = create_responses(
        &payload,
        ResponsesRequestOptions {
            vision,
            initiator,
            subagent_marker: subagent_marker.as_ref(),
            request_id: &request_id,
            session_id: Some(&fallback_session_id),
            compact_type: None,
            transport: responses_transport,
        },
    )
    .await?;

    match response {
        CreateResponsesReturn::Stream(upstream) => {
            let _ = is_stream;
            tracing::debug!("Forwarding native Responses stream");
            Ok(stream_responses_sse(upstream, recorder))
        }
        CreateResponsesReturn::Result(result) => {
            let usage_value = result
                .usage
                .as_ref()
                .and_then(|u| serde_json::to_value(u).ok());
            recorder.record(normalize_responses_usage(usage_value.as_ref()));
            Ok(Json(*result).into_response())
        }
    }
}

/// Mirrors the streaming arm: forward each upstream Responses SSE event after
/// running `fixStreamIds` over its data, while sniffing usage from the terminal
/// events, then record usage when the stream completes.
fn stream_responses_sse(
    upstream: crate::services::copilot::create_responses::ResponsesEventStream,
    recorder: crate::libs::token_usage::TokenUsageRecorder,
) -> Response {
    use crate::libs::token_usage::UsageTokens;

    let event_stream = upstream;

    let body = Body::from_stream(async_stream::stream! {
        let mut tracker = StreamIdTracker::new();
        let mut usage: UsageTokens = UsageTokens::default();
        futures_util::pin_mut!(event_stream);

        use futures_util::StreamExt;
        while let Some(item) = event_stream.next().await {
            let ev = match item {
                Ok(ev) => ev,
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };

            if let Some(captured) = sniff_responses_usage(&ev.data) {
                usage = captured;
            }

            let processed = fix_stream_ids(&ev.data, ev.event.as_deref(), &mut tracker);
            let frame = build_sse_frame(ev.id.as_deref(), ev.event.as_deref(), &processed);
            yield Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(frame));
        }

        recorder.record(usage);
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap()
}

/// Build an SSE frame matching hono's `streamSSE`/`writeSSE` field ordering
/// (`event`, then `id`, then one `data:` line per line of payload).
fn build_sse_frame(id: Option<&str>, event: Option<&str>, data: &str) -> String {
    let mut frame = String::new();
    if let Some(event) = event {
        frame.push_str("event: ");
        frame.push_str(event);
        frame.push('\n');
    }
    if let Some(id) = id {
        frame.push_str("id: ");
        frame.push_str(id);
        frame.push('\n');
    }
    for line in data.split('\n') {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    frame
}

/// Mirrors `parseResponsesStreamEvent` + the terminal-event usage capture:
/// returns normalized usage when `data` is a `response.completed`/`failed`/
/// `incomplete` event carrying a `response.usage`.
fn sniff_responses_usage(data: &str) -> Option<crate::libs::token_usage::UsageTokens> {
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let parsed: Value = serde_json::from_str(data).ok()?;
    let event_type = parsed.get("type").and_then(Value::as_str)?;
    if !matches!(
        event_type,
        "response.completed" | "response.failed" | "response.incomplete"
    ) {
        return None;
    }
    let usage = parsed.get("response").and_then(|r| r.get("usage"));
    Some(normalize_responses_usage(usage))
}

/// Mirrors `generateRequestIdFromPayload({ messages: payload.input }, sessionId)`,
/// honoring the string-input branch the array-only helper cannot represent.
fn responses_request_id(payload: &ResponsesPayload, session_id: Option<&str>) -> String {
    match payload.input.as_ref() {
        Some(InputField::Text(text)) if !text.is_empty() => {
            let mac = state::with_state(|s| s.mac_machine_id.clone().unwrap_or_default());
            get_uuid(&format!("{}{}{}", session_id.unwrap_or(""), mac, text))
        }
        Some(InputField::Items(items)) => {
            let values: Vec<Value> = items
                .iter()
                .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
                .collect();
            generate_request_id_from_payload(&values, session_id)
        }
        _ => generate_request_id_from_payload(&[], session_id),
    }
}

const COPILOT_UNSUPPORTED_TOOL_TYPES: &[&str] = &["image_generation"];

/// Mirrors `removeUnsupportedTools`: drop tools Copilot does not support.
pub fn remove_unsupported_tools(payload: &mut ResponsesPayload) {
    let Some(tools) = payload.tools.as_mut() else {
        return;
    };
    if tools.is_empty() {
        return;
    }

    let mut dropped: Vec<String> = Vec::new();
    tools.retain(|tool| {
        let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or("");
        if COPILOT_UNSUPPORTED_TOOL_TYPES.contains(&tool_type) {
            dropped.push(tool_type.to_string());
            return false;
        }
        true
    });
    if !dropped.is_empty() {
        tracing::debug!("Removed unsupported tools: {dropped:?}");
    }
}

/// Mirrors `removeWebSearchTool`.
fn remove_web_search_tool(payload: &mut ResponsesPayload) {
    let Some(tools) = payload.tools.as_mut() else {
        return;
    };
    if tools.is_empty() {
        return;
    }
    tools.retain(|tool| tool.get("type").and_then(Value::as_str) != Some("web_search"));
}

fn get_incoming_responses_session_id(headers: &HeaderMap) -> Option<String> {
    get_trimmed_header(headers, "session-id")
        .or_else(|| get_trimmed_header(headers, "x-session-id"))
}

const CODEX_SUBAGENT_HEADER_VALUES: &[&str] =
    &["collab_spawn", "compact", "memory_consolidation", "review"];

/// Mirrors `getCodexResponsesSubagentMarker`.
fn get_codex_responses_subagent_marker(headers: &HeaderMap) -> Option<SubagentMarker> {
    let agent_type = get_trimmed_header(headers, "x-openai-subagent")?;
    if !CODEX_SUBAGENT_HEADER_VALUES.contains(&agent_type.as_str()) {
        return None;
    }

    let thread_id = get_trimmed_header(headers, "thread-id");
    let root_session_id = get_incoming_responses_session_id(headers);
    let parent_thread_id = get_trimmed_header(headers, "x-codex-parent-thread-id");
    if thread_id.is_none() && root_session_id.is_none() && parent_thread_id.is_none() {
        return None;
    }

    let agent_id = thread_id
        .clone()
        .or_else(|| parent_thread_id.clone())
        .or_else(|| root_session_id.clone())
        .unwrap_or_else(|| agent_type.clone());

    let session_id = thread_id
        .or(root_session_id)
        .unwrap_or_else(|| agent_id.clone());

    Some(SubagentMarker {
        agent_id,
        agent_type,
        session_id,
    })
}

fn get_trimmed_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_with_tools(tools: Value) -> ResponsesPayload {
        let mut value = json!({ "model": "gpt-5" });
        value["tools"] = tools;
        serde_json::from_value(value).expect("payload")
    }

    #[test]
    fn remove_unsupported_tools_drops_image_generation() {
        let mut payload = payload_with_tools(json!([
            { "type": "function", "name": "f" },
            { "type": "image_generation" },
        ]));
        remove_unsupported_tools(&mut payload);
        let tools = payload.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
    }

    #[test]
    fn remove_web_search_tool_drops_web_search() {
        let mut payload = payload_with_tools(json!([
            { "type": "web_search" },
            { "type": "function", "name": "f" },
        ]));
        remove_web_search_tool(&mut payload);
        let tools = payload.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
    }

    #[test]
    fn subagent_marker_requires_known_agent_type() {
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", "unknown".parse().unwrap());
        headers.insert("thread-id", "t1".parse().unwrap());
        assert!(get_codex_responses_subagent_marker(&headers).is_none());
    }

    #[test]
    fn subagent_marker_requires_an_id_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", "review".parse().unwrap());
        assert!(get_codex_responses_subagent_marker(&headers).is_none());
    }

    #[test]
    fn subagent_marker_prefers_thread_id() {
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", "compact".parse().unwrap());
        headers.insert("thread-id", "thread-1".parse().unwrap());
        headers.insert("session-id", "sess-1".parse().unwrap());
        let marker = get_codex_responses_subagent_marker(&headers).expect("marker");
        assert_eq!(marker.agent_type, "compact");
        assert_eq!(marker.agent_id, "thread-1");
        assert_eq!(marker.session_id, "thread-1");
    }

    #[test]
    fn subagent_marker_falls_back_to_session_for_id() {
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", "collab_spawn".parse().unwrap());
        headers.insert("x-session-id", "  sess-9  ".parse().unwrap());
        let marker = get_codex_responses_subagent_marker(&headers).expect("marker");
        assert_eq!(marker.agent_id, "sess-9");
        assert_eq!(marker.session_id, "sess-9");
    }

    #[test]
    fn build_sse_frame_orders_event_id_data() {
        let frame = build_sse_frame(Some("e1"), Some("response.created"), "{\"a\":1}");
        assert_eq!(
            frame,
            "event: response.created\nid: e1\ndata: {\"a\":1}\n\n"
        );
    }

    #[test]
    fn build_sse_frame_splits_multiline_data() {
        let frame = build_sse_frame(None, None, "line1\nline2");
        assert_eq!(frame, "data: line1\ndata: line2\n\n");
    }

    #[test]
    fn sniff_usage_only_for_terminal_events() {
        let completed = r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#;
        assert!(sniff_responses_usage(completed).is_some());

        let delta = r#"{"type":"response.output_text.delta","delta":"hi"}"#;
        assert!(sniff_responses_usage(delta).is_none());

        assert!(sniff_responses_usage("[DONE]").is_none());
    }
}
