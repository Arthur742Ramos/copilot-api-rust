//! `/responses` route handler — ports routes/responses/handler.ts `handleResponses`.

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

use crate::libs::approval::await_approval;
use crate::libs::config::{is_responses_api_web_search_enabled, resolve_mapped_model};
use crate::libs::error::{openai_error_response, AppError};
use crate::libs::provider_model::parse_provider_model_alias;
use crate::libs::state;
use crate::libs::subagent::SubagentMarker;
use crate::libs::token_usage::{create_copilot_token_usage_recorder, normalize_responses_usage};
use crate::libs::utils::{generate_request_id_from_payload, get_uuid};
use crate::services::copilot::create_responses::{
    create_responses, CreateResponsesReturn, InputField, MessageContent, ResponseInputItem,
    ResponsesBufferedContract, ResponsesPayload, ResponsesRequestOptions,
};

use crate::routes::files::materialize_responses_file_references;
use crate::routes::responses::stream_guard::{ResponsesStreamGuard, ResponsesTerminal};
use crate::routes::responses::stream_id_sync::StreamIdTracker;
use crate::routes::responses::utils::{
    apply_responses_api_context_management, compact_input_by_latest_compaction,
    get_responses_request_options, get_responses_transport_for_model,
    sanitize_oversized_input_images,
};

/// Mirrors routes/responses/handler.ts `handleResponses`.
pub async fn handle_responses(body: Value, headers: HeaderMap) -> Result<Response, AppError> {
    let mut payload: ResponsesPayload = serde_json::from_value(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid request payload: {e}")))?;

    validate_responses_payload(&payload)?;
    compact_input_by_latest_compaction(&mut payload);
    materialize_responses_file_references(&mut payload).await?;

    let requested_model = payload.model.clone();
    payload.model = resolve_mapped_model(&payload.model);
    if payload.model != requested_model {
        tracing::debug!(
            "Resolved model mapping: {requested_model} -> {}",
            payload.model
        );
    }

    // Provider aliases return early, so shared admission belongs before the
    // dispatch split rather than only in the Copilot arm.
    crate::libs::admission::check_shared_admission()
        .await
        .map_err(AppError::Http)?;

    if let Some(alias) = parse_provider_model_alias(&payload.model) {
        payload.model = alias.model.clone();
        return crate::routes::provider::responses::handle_provider_responses_for_provider(
            payload,
            alias.provider,
            headers,
        )
        .await;
    }

    crate::libs::premium_interactions::check_premium_interactions()?;

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
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("model_not_supported"),
            "This model does not support the responses endpoint. Please choose a different model.",
        ));
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

    let response_model = payload.model.clone();

    let response = create_responses(
        payload,
        ResponsesRequestOptions {
            vision,
            initiator,
            subagent_marker: subagent_marker.as_ref(),
            request_id: &request_id,
            session_id: Some(&fallback_session_id),
            compact_type: None,
            transport: responses_transport,
            buffered_contract: ResponsesBufferedContract::Regular,
        },
    )
    .await?;

    match response {
        CreateResponsesReturn::Stream(upstream) => {
            tracing::debug!("Forwarding native Responses stream");
            Ok(stream_responses_sse(upstream, recorder))
        }
        CreateResponsesReturn::Result(result) => {
            let result = *result;
            // Native non-streaming responses never pass through a StreamTimer, so
            // record the flow/model/transport headline here so the trace
            // middleware's `has_flow` guard emits the single `request.completed`
            // line (streaming responses are covered by the StreamTimer drop).
            if let Some(ctx) = crate::libs::request_context::request_context_store() {
                ctx.set_flow_transport_model_non_streaming(
                    &response_model,
                    "responses",
                    crate::libs::stream_metrics::transport::NATIVE,
                );
            }
            let usage_value = result
                .parsed
                .usage
                .as_ref()
                .and_then(|u| serde_json::to_value(u).ok());
            recorder.record(normalize_responses_usage(usage_value.as_ref()));
            let mut response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(result.raw))
                .expect("static native Responses response");
            for (name, value) in &result.headers {
                response.headers_mut().insert(name, value.clone());
            }
            Ok(response)
        }
        CreateResponsesReturn::CompactResult(_) => Err(crate::libs::error::HttpError::internal(
            "Responses endpoint unexpectedly returned a compact result",
        )
        .into()),
    }
}

#[allow(clippy::result_large_err)]
pub(crate) fn validate_responses_payload(payload: &ResponsesPayload) -> Result<(), AppError> {
    if payload.model.trim().is_empty() {
        return Err(AppError::BadRequest(
            "model: field required and must be a non-empty string".to_string(),
        ));
    }
    match payload.input.as_ref() {
        Some(InputField::Text(text)) if !text.trim().is_empty() => {}
        Some(InputField::Items(items)) if !items.is_empty() => {}
        _ => {
            return Err(AppError::BadRequest(
                "input: field required and must not be empty".to_string(),
            ))
        }
    }
    if matches!(payload.max_output_tokens, Some(value) if value <= 0) {
        return Err(AppError::BadRequest(
            "max_output_tokens: must be a positive integer".to_string(),
        ));
    }
    Ok(())
}

/// Mirrors the streaming arm: forward each upstream Responses SSE event after
/// running `fixStreamIds` over its data, while sniffing usage from the terminal
/// events, then record usage when the stream completes.
fn stream_responses_sse(
    upstream: crate::services::copilot::create_responses::ResponsesEventStream,
    recorder: crate::libs::token_usage::TokenUsageRecorder,
) -> Response {
    use crate::libs::stream_metrics::{transport, StreamTimer};
    use crate::libs::token_usage::UsageTokens;

    let event_stream = upstream;

    // Capture the request context while the task-local is still in scope; the
    // stream body below is polled later (outside the scope), so it must be
    // moved in for the timer's `request.completed` emission.
    let req_ctx = crate::libs::request_context::request_context_store();

    let body = Body::from_stream(async_stream::stream! {
        // Cover the native /v1/responses stream with the same proxy_stream_*
        // dashboards as the messages flows (transport=native). The timer drops
        // at end-of-stream (or client disconnect), recording stream-complete.
        let mut timer = StreamTimer::new("responses", transport::NATIVE)
            .with_request_context(req_ctx);
        let mut tracker = StreamIdTracker::new();
        let mut guard = ResponsesStreamGuard::new();
        let mut usage: UsageTokens = UsageTokens::default();
        futures_util::pin_mut!(event_stream);

        use futures_util::StreamExt;
        let heartbeat = crate::libs::sse::sse_heartbeat_interval();
        loop {
            let item = match heartbeat {
                Some(interval) => match tokio::time::timeout(interval, event_stream.next()).await {
                    Ok(next) => next,
                    // Idle-but-alive upstream (its read_timeout still bounds a
                    // truly wedged connection): emit a comment keep-alive so
                    // sub-120s intermediaries don't drop the stream. A comment is
                    // not content, so it must not touch the timer/TTFT accounting.
                    Err(_) => {
                        yield Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from_static(
                            crate::libs::sse::SSE_COMMENT_PING,
                        ));
                        continue;
                    }
                },
                None => event_stream.next().await,
            };
            let Some(item) = item else { break };
            let ev = match item {
                Ok(ev) => ev,
                Err(error) => {
                    tracing::warn!(error = %error, "Responses upstream stream transport failed");
                    timer.mark_error();
                    if let Some(frame) = guard.fail(
                        "upstream_transport_error",
                        "The upstream Responses stream was interrupted.",
                    ) {
                        yield Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(frame));
                    }
                    recorder.record(usage);
                    return;
                }
            };

            let processed = match guard.process(&ev, &mut tracker) {
                Ok(Some(processed)) => processed,
                Ok(None) => continue,
                Err(reason) => {
                    tracing::warn!(reason, "Rejected malformed Responses stream event");
                    timer.mark_error();
                    if let Some(frame) = guard.fail(
                        "invalid_stream",
                        "The upstream Responses stream returned malformed data.",
                    ) {
                        yield Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(frame));
                    }
                    recorder.record(usage);
                    return;
                }
            };

            if let Some(captured) = responses_event_usage(&processed.value) {
                usage = captured;
            }

            let event_type = processed.value.get("type").and_then(Value::as_str);
            if !matches!(
                event_type,
                Some("ping" | "codex.rate_limits" | "response.metadata")
            ) {
                timer.on_content_frame();
            }
            let terminal = processed.terminal;
            yield Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(processed.frame));

            if let Some(terminal) = terminal {
                match terminal {
                    ResponsesTerminal::Completed => timer.mark_finished(),
                    ResponsesTerminal::Failed => timer.mark_error(),
                }
                recorder.record(usage);
                return;
            }
        }

        timer.mark_error();
        if let Some(frame) = guard.fail(
            "upstream_eof",
            "The upstream Responses stream ended before a terminal event.",
        ) {
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

/// Mirrors `parseResponsesStreamEvent` + the terminal-event usage capture:
/// returns normalized usage when `data` is a `response.completed`/`failed`/
/// `incomplete` event carrying a `response.usage`.
#[cfg(test)]
fn sniff_responses_usage(data: &str) -> Option<crate::libs::token_usage::UsageTokens> {
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    // Cheap substring pre-filter so the (overwhelmingly common) delta/reasoning
    // events skip the full JSON parse — usage only ever rides on the three
    // terminal event types. fix_stream_ids parses the data again downstream, so
    // for non-terminal events this avoids a redundant second deserialization.
    if !(data.contains("response.completed")
        || data.contains("response.failed")
        || data.contains("response.incomplete"))
    {
        return None;
    }
    let parsed: Value = serde_json::from_str(data).ok()?;
    responses_event_usage(&parsed)
}

fn responses_event_usage(parsed: &Value) -> Option<crate::libs::token_usage::UsageTokens> {
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
pub(crate) fn responses_request_id(payload: &ResponsesPayload, session_id: Option<&str>) -> String {
    match payload.input.as_ref() {
        Some(InputField::Text(text)) if !text.is_empty() => {
            let mac = state::with_state(|s| s.mac_machine_id.clone().unwrap_or_default());
            get_uuid(&format!("{}{}{}", session_id.unwrap_or(""), mac, text))
        }
        Some(InputField::Items(items)) => {
            let content = responses_last_user_content(items);
            crate::libs::utils::generate_request_id_from_user_content(
                content.as_deref(),
                session_id,
            )
        }
        _ => generate_request_id_from_payload(&[], session_id),
    }
}

/// Typed equivalent of `find_last_user_content` that walks backward and only
/// serializes the chosen user's content blocks. The previous implementation
/// materialized every Responses input item as JSON merely to inspect the last
/// user message, temporarily duplicating a potentially 1M-token conversation.
fn responses_last_user_content(items: &[ResponseInputItem]) -> Option<String> {
    for item in items.iter().rev() {
        match item {
            ResponseInputItem::Message(message) if message.role == "user" => {
                let Some(content) = message.content.as_ref() else {
                    continue;
                };
                match content {
                    MessageContent::Text(text) if !text.is_empty() => return Some(text.clone()),
                    MessageContent::Text(_) => continue,
                    MessageContent::Blocks(blocks) => {
                        let filtered: Vec<Value> = blocks
                            .iter()
                            .filter_map(|block| {
                                let mut value = serde_json::to_value(block).ok()?;
                                if value.get("type").and_then(Value::as_str) == Some("tool_result")
                                {
                                    return None;
                                }
                                if let Some(object) = value.as_object_mut() {
                                    object.insert("cache_control".to_string(), Value::Null);
                                }
                                Some(value)
                            })
                            .collect();
                        if !filtered.is_empty() {
                            return serde_json::to_string(&filtered).ok();
                        }
                    }
                }
            }
            // Preserve unknown message-like input shapes by running the generic
            // probe over this one item, without retaining a full input clone.
            ResponseInputItem::Other(value) => {
                if let Some(content) =
                    crate::libs::utils::find_last_user_content(std::slice::from_ref(value))
                {
                    return Some(content);
                }
            }
            _ => {}
        }
    }
    None
}

const COPILOT_UNSUPPORTED_TOOL_TYPES: &[&str] = &["image_generation"];
const COPILOT_UNSUPPORTED_TOOL_NAMESPACES: &[&str] = &["image_gen"];

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
        let namespace = tool.get("name").and_then(Value::as_str);
        let unsupported_namespace = tool_type == "namespace"
            && namespace.is_some_and(|name| COPILOT_UNSUPPORTED_TOOL_NAMESPACES.contains(&name));
        if COPILOT_UNSUPPORTED_TOOL_TYPES.contains(&tool_type) || unsupported_namespace {
            dropped.push(match namespace.filter(|_| unsupported_namespace) {
                Some(name) => format!("{tool_type}:{name}"),
                None => tool_type.to_string(),
            });
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

pub(crate) fn get_incoming_responses_session_id(headers: &HeaderMap) -> Option<String> {
    get_trimmed_header(headers, "session-id")
        .or_else(|| get_trimmed_header(headers, "x-session-id"))
}

const CODEX_SUBAGENT_HEADER_VALUES: &[&str] =
    &["collab_spawn", "compact", "memory_consolidation", "review"];

/// Mirrors `getCodexResponsesSubagentMarker`.
pub(crate) fn get_codex_responses_subagent_marker(headers: &HeaderMap) -> Option<SubagentMarker> {
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
    use serde_json::json;

    /// Env-mutating heartbeat tests share the `COPILOT_API_SSE_HEARTBEAT_SECS`
    /// process-global, so they run serially (via `serial_test`) to avoid races.
    #[test]
    #[serial_test::serial(sse_heartbeat_env)]
    fn heartbeat_interval_env_override() {
        std::env::set_var("COPILOT_API_SSE_HEARTBEAT_SECS", "0");
        assert!(crate::libs::sse::sse_heartbeat_interval().is_none());
        std::env::set_var("COPILOT_API_SSE_HEARTBEAT_SECS", "7");
        assert_eq!(
            crate::libs::sse::sse_heartbeat_interval(),
            Some(std::time::Duration::from_secs(7))
        );
        std::env::remove_var("COPILOT_API_SSE_HEARTBEAT_SECS");
        assert_eq!(
            crate::libs::sse::sse_heartbeat_interval(),
            Some(std::time::Duration::from_secs(
                crate::libs::sse::DEFAULT_SSE_HEARTBEAT_SECS
            ))
        );
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial(sse_heartbeat_env)]
    async fn idle_responses_stream_emits_comment_heartbeats() {
        std::env::set_var("COPILOT_API_SSE_HEARTBEAT_SECS", "10");

        // Upstream stays silent for 25 virtual seconds, then emits one real event
        // and ends. With a 10s heartbeat that idle gap must produce comment pings
        // BEFORE the real frame, and the pings must not appear after the stream
        // ends (a None read resolves immediately, breaking the loop).
        let upstream: crate::services::copilot::create_responses::ResponsesEventStream = Box::pin(
            async_stream::stream! {
                tokio::time::sleep(std::time::Duration::from_secs(25)).await;
                yield Ok(crate::libs::sse::SseEvent {
                    id: None,
                    event: Some("response.created".to_string()),
                    data: "{\"type\":\"response.created\",\"response\":{\"id\":\"resp_heartbeat\",\"model\":\"gpt-5\"}}".to_string(),
                });
            },
        );

        let recorder = crate::libs::token_usage::create_copilot_token_usage_recorder(
            "responses",
            "m".to_string(),
            None,
        );
        let response = stream_responses_sse(upstream, recorder);
        std::env::remove_var("COPILOT_API_SSE_HEARTBEAT_SECS");

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // The 25s idle gap (heartbeat = 10s) must surface at least one comment
        // ping, emitted BEFORE the real frame. The exact count depends on the
        // paused-clock auto-advance granularity, so we assert presence + ordering
        // rather than a brittle exact tally.
        let ping_count = text.matches(":\n\n").count();
        assert!(ping_count >= 1, "expected >=1 heartbeat ping, got {text:?}");
        let first_data = text.find("data: ").expect("real data frame present");
        let first_ping = text.find(":\n\n").expect("ping present");
        assert!(
            first_ping < first_data,
            "heartbeat must precede the first real frame: {text:?}"
        );
        assert!(text.contains("response.created"));
    }

    fn payload_with_tools(tools: Value) -> ResponsesPayload {
        let mut value = json!({ "model": "gpt-5" });
        value["tools"] = tools;
        serde_json::from_value(value).expect("payload")
    }

    #[test]
    fn typed_request_id_probe_matches_generic_user_content_semantics() {
        let payload: ResponsesPayload = serde_json::from_value(json!({
            "model": "gpt-5",
            "input": [
                { "type": "message", "role": "user", "content": "older" },
                { "type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}" },
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "t1", "content": "skip" },
                        { "type": "input_text", "text": "newest", "cache_control": { "type": "ephemeral" } }
                    ]
                }
            ]
        }))
        .expect("responses payload");
        let InputField::Items(items) = payload.input.expect("items") else {
            panic!("expected input items");
        };

        let generic_values: Vec<Value> = items
            .iter()
            .map(|item| serde_json::to_value(item).expect("serialize item"))
            .collect();
        let generic = crate::libs::utils::find_last_user_content(&generic_values);
        let typed = responses_last_user_content(&items);
        assert_eq!(typed, generic);
        assert!(typed
            .as_deref()
            .is_some_and(|value| value.contains("newest")));
        assert!(typed
            .as_deref()
            .is_some_and(|value| !value.contains("skip")));
    }

    #[test]
    fn remove_unsupported_tools_drops_image_generation() {
        let mut payload = payload_with_tools(json!([
            { "type": "function", "name": "f" },
            { "type": "image_generation" },
            { "type": "namespace", "name": "image_gen" },
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
        let frame = crate::routes::responses::stream_guard::build_sse_frame(
            Some("e1"),
            Some("response.created"),
            "{\"a\":1}",
        );
        assert_eq!(
            frame,
            "event: response.created\nid: e1\ndata: {\"a\":1}\n\n"
        );
    }

    #[test]
    fn build_sse_frame_splits_multiline_data() {
        let frame =
            crate::routes::responses::stream_guard::build_sse_frame(None, None, "line1\nline2");
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
