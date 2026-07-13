//! The three Anthropic `/v1/messages` request flows.
//!
//! Ported in full from `src/routes/messages/api-flows.ts`. Each handler takes an
//! already-preprocessed Anthropic payload plus the resolved [`FlowOptions`] and
//! drives one upstream transport:
//!
//! - [`handle_with_chat_completions`] — translate Anthropic -> OpenAI, apply the
//!   Copilot context cache, call the Copilot chat-completions API, translate the
//!   result (or each streaming chunk) back to Anthropic.
//! - [`handle_with_responses_api`] — translate Anthropic -> Responses payload,
//!   apply context-management/compaction, call the Copilot `/responses` API, and
//!   translate the result/stream back to Anthropic.
//! - [`handle_with_messages_api`] — preprocess for the native `/v1/messages`
//!   upstream and raw-forward the Anthropic response/stream (sniffing usage).
//!
//! The TS module carries a `ConsolaInstance` logger through the options; the Rust
//! port logs via `tracing` instead, so [`FlowOptions`] is logger-less.

use std::collections::HashSet;

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;

use serde_json::{json, Value};

use crate::libs::error::AppError;
use crate::libs::stream_metrics::{transport as stream_transport, StreamTimer};
use crate::libs::subagent::SubagentMarker;
use crate::libs::token_usage::{
    create_copilot_token_usage_recorder, merge_anthropic_usage, normalize_anthropic_usage,
    normalize_openai_usage, normalize_responses_usage, TokenUsageRecorder, UsageTokens,
};
use crate::libs::tool_search::resolve_bridge_tool_search_name;
use crate::libs::utils::parse_user_id_metadata;
use crate::routes::messages::anthropic_types::{
    AnthropicMessagesPayload, AnthropicStreamEventData, AnthropicStreamState,
};
use crate::routes::messages::non_stream_translation::{
    translate_to_anthropic, translate_to_openai,
};
use crate::routes::messages::responses_stream_translation::{
    build_error_event, terminate_responses_stream_with_error, translate_responses_stream_event,
    ResponsesStreamState,
};
use crate::routes::messages::responses_translation::{
    translate_anthropic_messages_to_responses_payload, translate_responses_result_to_anthropic,
};
use crate::routes::messages::stream_translation::{
    flush_pending_anthropic_stream_events, malformed_stream_error_events,
    translate_chunk_to_anthropic_events, translate_error_to_anthropic_error_event,
    transport_stream_error_events,
};
use crate::routes::responses::utils::{
    apply_responses_api_context_management, compact_input_by_latest_compaction,
    get_responses_request_options, get_responses_transport_for_model,
    DEFAULT_RESPONSES_COMPACT_THRESHOLD_RATIO,
};
use crate::services::copilot::create_chat_completions::{
    create_chat_completions, ChatCompletionsOptions, ChatCompletionsPayload, ChatCompletionsResult,
    Message,
};
use crate::services::copilot::create_messages::{
    create_messages, CreateMessagesOptions, CreateMessagesResult,
};
use crate::services::copilot::create_responses::{
    create_responses, CreateResponsesReturn, ResponsesBufferedContract, ResponsesRequestOptions,
    ResponsesTransport,
};
use crate::services::copilot::get_models::Model;

// ---------------------------------------------------------------------------
// Copilot context-cache constants
//
// Mirror the `const`s declared at the top of `api-flows.ts`.
// ---------------------------------------------------------------------------

/// `api-flows.ts`: `COPILOT_CONTEXT_CACHE_SYSTEM_MARKER_LIMIT = 2`
const COPILOT_CONTEXT_CACHE_SYSTEM_MARKER_LIMIT: usize = 2;

/// `api-flows.ts`: `COPILOT_CONTEXT_CACHE_NON_SYSTEM_MARKER_LIMIT = 2`
const COPILOT_CONTEXT_CACHE_NON_SYSTEM_MARKER_LIMIT: usize = 2;

/// `api-flows.ts`: `COPILOT_CONTEXT_CACHE_CONTROL = { type: "ephemeral" }`
fn copilot_context_cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

// ---------------------------------------------------------------------------
// Flow options
// ---------------------------------------------------------------------------

/// The resolved per-request options shared by all three flows.
///
/// Collapses the TS `FlowBaseOptions` / `ResponsesFlowOptions` /
/// `MessagesFlowOptions` hierarchy into one struct (the logger is dropped — see
/// the module docs). Fields not relevant to a given flow are simply unused by it.
#[derive(Debug, Default)]
pub struct FlowOptions {
    pub subagent_marker: Option<SubagentMarker>,
    pub request_id: String,
    pub session_id: Option<String>,
    pub compact_type: Option<i32>,
    pub selected_model: Option<Model>,
    pub anthropic_beta_header: Option<String>,
}

// ---------------------------------------------------------------------------
// SSE helpers
// ---------------------------------------------------------------------------

/// Render one translated Anthropic event directly into its final SSE bytes.
/// Serializing into the frame buffer avoids the previous intermediate JSON
/// `String` plus a second allocation/copy for every streamed token event.
fn emit_event(event: &AnthropicStreamEventData) -> Option<Bytes> {
    let event_name = event.event_name();
    let mut frame = Vec::with_capacity(event_name.len() + 128);
    frame.extend_from_slice(b"event: ");
    frame.extend_from_slice(event_name.as_bytes());
    frame.extend_from_slice(b"\ndata: ");
    serde_json::to_writer(&mut frame, event).ok()?;
    frame.extend_from_slice(b"\n\n");
    Some(Bytes::from(frame))
}

/// Count an upstream SSE chunk we could not JSON-parse, so a flaky upstream
/// emitting partial/garbage deltas is detectable on dashboards rather than
/// silently dropped. `flow` is a bounded label (chat_completions | responses).
/// A dropped chunk can also desync the translation state machine, so this is the
/// only signal that an SSE-reassembly bug or upstream corruption occurred. The
/// parse error is logged (as a structured field) to aid diagnosis, but the raw
/// chunk is never logged so user content can't leak.
pub(crate) fn record_stream_chunk_parse_failure(flow: &'static str, error: &serde_json::Error) {
    metrics::counter!("proxy_stream_chunk_parse_failures_total", "flow" => flow).increment(1);
    tracing::warn!(error = %error, "unparseable upstream SSE chunk terminated {flow} stream");
}

/// Build a `text/event-stream` response over a byte stream, with the same header
/// set the chat handler uses for streaming.
fn sse_response<S>(stream: S) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let body = Body::from_stream(stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap()
}

/// Mirrors `getMetadataSessionId`: the session id encoded in the request
/// `metadata.user_id`, if any.
fn metadata_session_id(payload: &AnthropicMessagesPayload) -> Option<String> {
    payload
        .metadata
        .as_ref()
        .and_then(|m| parse_user_id_metadata(m.user_id.as_deref()).session_id)
}

/// Mirrors `createCopilotUsageRecorder`: a Copilot recorder whose `session_id` is
/// derived from the request metadata and whose `fallback_session_id` is the flow
/// session id.
fn build_recorder(
    endpoint: &'static str,
    model: String,
    fallback_session_id: Option<String>,
    payload: &AnthropicMessagesPayload,
) -> TokenUsageRecorder {
    let mut recorder = create_copilot_token_usage_recorder(endpoint, model, fallback_session_id);
    recorder.session_id = metadata_session_id(payload);
    recorder
}

// ---------------------------------------------------------------------------
// Flow 1: chat completions
// ---------------------------------------------------------------------------

/// Mirrors `handleWithChatCompletions`.
pub async fn handle_with_chat_completions(
    payload: &AnthropicMessagesPayload,
    opts: FlowOptions,
) -> Result<Response, AppError> {
    let mut openai_payload = translate_to_openai(payload)?;
    prepare_copilot_chat_completions_payload(&mut openai_payload);

    // Capture the request context while the task-local is in scope (the stream
    // body is polled later, outside it) and record the dispatch flow for the
    // `request.completed` summary line.
    let req_ctx = crate::libs::request_context::request_context_store();
    if let Some(ctx) = &req_ctx {
        ctx.set_flow(
            &openai_payload.model,
            "chat_completions",
            stream_transport::TRANSLATED,
        );
    }

    let recorder = build_recorder(
        "chat_completions",
        openai_payload.model.clone(),
        opts.session_id.clone(),
        payload,
    );

    let result = create_chat_completions(
        &openai_payload,
        ChatCompletionsOptions {
            subagent_marker: opts.subagent_marker,
            request_id: opts.request_id,
            session_id: opts.session_id,
            compact_type: opts.compact_type,
        },
    )
    .await?;

    match result {
        ChatCompletionsResult::NonStreaming { response, headers } => {
            let anthropic_response = translate_to_anthropic(&response).map_err(|mut error| {
                error.headers = headers;
                AppError::Http(error)
            })?;
            recorder.record(normalize_openai_usage(response.get("usage")));
            Ok(Json(anthropic_response).into_response())
        }
        ChatCompletionsResult::Streaming(upstream) => Ok(stream_chat_completions_response(
            upstream, recorder, req_ctx,
        )),
    }
}

fn stream_chat_completions_response(
    upstream: reqwest::Response,
    recorder: TokenUsageRecorder,
    req_ctx: Option<crate::libs::request_context::RequestContext>,
) -> Response {
    let stream = async_stream::stream! {
        let mut timer = StreamTimer::new("chat_completions", stream_transport::TRANSLATED)
            .with_request_context(req_ctx);
        let mut state = AnthropicStreamState::default();
        let mut usage = UsageTokens::default();

        let heartbeat = crate::libs::sse::sse_heartbeat_interval();
        let sse = crate::libs::sse::events(upstream);
        futures_util::pin_mut!(sse);
        loop {
            let item = match heartbeat {
                Some(interval) => match tokio::time::timeout(interval, sse.next()).await {
                    Ok(next) => next,
                    // Upstream silent but still alive (its own read_timeout
                    // still bounds a truly wedged connection): emit a ping
                    // so sub-120s intermediaries keep the stream open. A
                    // ping is not content, so it must not touch the
                    // timer/TTFT accounting below.
                    Err(_) => {
                        yield Ok(Bytes::from_static(
                            crate::libs::sse::ANTHROPIC_PING_FRAME,
                        ));
                        continue;
                    }
                },
                None => sse.next().await,
            };
            let Some(item) = item else { break };
            let raw_event = match item {
                Ok(ev) => ev,
                Err(e) => {
                    tracing::warn!("chat-completions stream error: {e}; sending terminal error event");
                    timer.mark_error();
                    for event in transport_stream_error_events(&mut state, Some(&e)) {
                        if let Some(frame) = emit_event(&event) {
                            yield Ok(frame);
                        }
                    }
                    // Record whatever usage was sniffed before the error so
                    // partial-stream accounting isn't silently dropped.
                    recorder.record(usage);
                    return;
                }
            };

            if raw_event.data == "[DONE]" {
                break;
            }
            if raw_event.data.is_empty() {
                continue;
            }

            let chunk: Value = match serde_json::from_str(&raw_event.data) {
                Ok(c) => c,
                Err(e) => {
                    record_stream_chunk_parse_failure("chat_completions", &e);
                    timer.mark_error();
                    for event in malformed_stream_error_events(&mut state) {
                        if let Some(frame) = emit_event(&event) {
                            yield Ok(frame);
                        }
                    }
                    recorder.record(usage);
                    return;
                }
            };
            let was_terminal = state.terminal_event_emitted;
            let translated = translate_chunk_to_anthropic_events(&chunk, &mut state);
            let terminal_error = translated
                .iter()
                .any(|event| matches!(event, AnthropicStreamEventData::Error { .. }));
            // The translator validates every usage field this flow consumes.
            // Account only a chunk it accepted, and never let trailing records
            // mutate usage after success or failure became terminal.
            if !was_terminal && !terminal_error {
                if let Some(u) = chunk.get("usage").filter(|usage| !usage.is_null()) {
                    usage = normalize_openai_usage(Some(u));
                }
            }
            for event in translated {
                if let Some(frame) = emit_event(&event) {
                    if !matches!(&event, AnthropicStreamEventData::Error { .. }) {
                        timer.on_content_frame();
                    }
                    yield Ok(frame);
                }
            }
            if terminal_error {
                tracing::warn!(
                    "chat-completions stream reported a terminal upstream error"
                );
                timer.mark_error();
                recorder.record(usage);
                return;
            }
        }

        // A message was started but we never emitted `message_stop`, and
        // no deferred usage delta is pending — i.e. the upstream stream
        // ended (whether via an early `[DONE]`, a dropped finishing
        // chunk, or a silent close) before a proper terminal event. Mark
        // the timer as an error so a truncated/incomplete stream is
        // distinguishable from a clean completion on the latency/outcome
        // dashboards (mirrors the responses flow). The
        // `pending_message_delta` guard avoids flagging the legitimate
        // deferred-usage path where a finish_reason was received but its
        // usage arrived on a later/absent chunk.
        let truncated =
            !state.terminal_event_emitted && state.pending_message_delta.is_none();
        if truncated {
            tracing::warn!(
                "chat-completions stream ended without a finish reason (truncated or empty)"
            );
            timer.mark_error();
        }

        for event in flush_pending_anthropic_stream_events(&mut state) {
            if let Some(frame) = emit_event(&event) {
                timer.on_content_frame();
                yield Ok(frame);
            }
        }

        if state.message_stop_emitted {
            timer.mark_finished();
        }
        recorder.record(usage);
    };
    sse_response(stream)
}

// ---------------------------------------------------------------------------
// Flow 2: responses API
// ---------------------------------------------------------------------------

/// Mirrors `handleWithResponsesApi`.
pub async fn handle_with_responses_api(
    payload: &AnthropicMessagesPayload,
    opts: FlowOptions,
) -> Result<Response, AppError> {
    let subagent_agent_id = opts.subagent_marker.as_ref().map(|m| m.agent_id.as_str());
    let mut responses_payload =
        translate_anthropic_messages_to_responses_payload(payload, subagent_agent_id)?;
    let response_model = responses_payload.model.clone();

    // Capture context in-scope for the deferred stream body + summary line.
    let req_ctx = crate::libs::request_context::request_context_store();
    if let Some(ctx) = &req_ctx {
        ctx.set_flow(
            &responses_payload.model,
            "responses",
            stream_transport::TRANSLATED,
        );
    }

    let recorder = build_recorder(
        "responses",
        responses_payload.model.clone(),
        opts.session_id.clone(),
        payload,
    );

    let max_prompt_tokens = opts
        .selected_model
        .as_ref()
        .and_then(|m| m.capabilities.limits.max_prompt_tokens);
    apply_responses_api_context_management(
        &mut responses_payload,
        max_prompt_tokens,
        DEFAULT_RESPONSES_COMPACT_THRESHOLD_RATIO,
    );

    compact_input_by_latest_compaction(&mut responses_payload);

    let (vision, initiator) = get_responses_request_options(&responses_payload);
    let transport =
        get_responses_transport_for_model(opts.selected_model.as_ref(), opts.compact_type)
            .unwrap_or(ResponsesTransport::Http);

    // resolveBridgeToolSearchName(anthropicPayload.tools)
    let tool_values: Vec<Value> = payload
        .tools
        .as_ref()
        .map(|ts| {
            ts.iter()
                .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
                .collect()
        })
        .unwrap_or_default();
    let tool_slice: Option<&[Value]> = if tool_values.is_empty() {
        None
    } else {
        Some(tool_values.as_slice())
    };
    let tool_search_name = resolve_bridge_tool_search_name(tool_slice);

    let result = create_responses(
        responses_payload,
        ResponsesRequestOptions {
            vision,
            initiator,
            subagent_marker: opts.subagent_marker.as_ref(),
            request_id: &opts.request_id,
            session_id: opts.session_id.as_deref(),
            compact_type: opts.compact_type,
            transport,
            buffered_contract: ResponsesBufferedContract::Regular,
        },
    )
    .await?;

    match result {
        CreateResponsesReturn::Stream(upstream) => {
            let stream = async_stream::stream! {
                let mut timer = StreamTimer::new("responses", stream_transport::TRANSLATED)
                    .with_request_context(req_ctx);
                let mut state = ResponsesStreamState::new_with_model(
                    Some(tool_search_name),
                    Some(response_model),
                );
                let mut usage = UsageTokens::default();

                let heartbeat = crate::libs::sse::sse_heartbeat_interval();
                let sse = upstream;
                futures_util::pin_mut!(sse);
                loop {
                    let item = match heartbeat {
                        Some(interval) => match tokio::time::timeout(interval, sse.next()).await {
                            Ok(next) => next,
                            // Idle-but-alive upstream: keep downstream warm with a
                            // ping. Not content — leaves timer/TTFT untouched.
                            Err(_) => {
                                yield Ok(Bytes::from_static(
                                    crate::libs::sse::ANTHROPIC_PING_FRAME,
                                ));
                                continue;
                            }
                        },
                        None => sse.next().await,
                    };
                    let Some(item) = item else { break };
                    let chunk = match item {
                        Ok(ev) => ev,
                        Err(e) => {
                            tracing::warn!("responses stream error: {e}; sending terminal error event");
                            timer.mark_error();
                            let error_event = translate_error_to_anthropic_error_event(Some(&e));
                            for event in terminate_responses_stream_with_error(&mut state, error_event) {
                                if let Some(frame) = emit_event(&event) {
                                    yield Ok(frame);
                                }
                            }
                            recorder.record(usage);
                            return;
                        }
                    };

                    if chunk.event.as_deref() == Some("ping") {
                        // Pings are keep-alives, not content — don't count them
                        // toward TTFT (they'd systematically under-report it).
                        yield Ok(Bytes::from_static(crate::libs::sse::ANTHROPIC_PING_FRAME));
                        continue;
                    }
                    if chunk.data.is_empty() {
                        continue;
                    }
                    if chunk.data == "[DONE]" {
                        break;
                    }

                    let response_event: Value = match serde_json::from_str(&chunk.data) {
                        Ok(v) => v,
                        Err(e) => {
                            record_stream_chunk_parse_failure("responses", &e);
                            timer.mark_error();
                            let error_event =
                                build_error_event("The upstream Responses stream returned a malformed event.");
                            for event in
                                terminate_responses_stream_with_error(&mut state, error_event)
                            {
                                if let Some(frame) = emit_event(&event) {
                                    yield Ok(frame);
                                }
                            }
                            recorder.record(usage);
                            return;
                        }
                    };
                    match response_event.get("type").and_then(Value::as_str) {
                        Some("response.completed") | Some("response.failed")
                        | Some("response.incomplete") => {
                            usage = normalize_responses_usage(
                                response_event.get("response").and_then(|r| r.get("usage")),
                            );
                        }
                        _ => {}
                    }

                    for event in translate_responses_stream_event(&response_event, &mut state) {
                        if let Some(frame) = emit_event(&event) {
                            timer.on_content_frame();
                            yield Ok(frame);
                        }
                    }

                    if state.message_completed {
                        timer.mark_finished();
                        break;
                    }
                }

                if !state.message_completed {
                    tracing::warn!(
                        "Responses stream ended without completion; sending error event"
                    );
                    timer.mark_error();
                    let error_event = build_error_event(
                        "The upstream Responses stream ended without a completion event.",
                    );
                    for event in
                        terminate_responses_stream_with_error(&mut state, error_event)
                    {
                        if let Some(frame) = emit_event(&event) {
                            yield Ok(frame);
                        }
                    }
                }

                recorder.record(usage);
            };
            Ok(sse_response(stream))
        }
        CreateResponsesReturn::Result(response) => {
            let anthropic_response =
                translate_responses_result_to_anthropic(&response.parsed, Some(&tool_search_name))?;
            let usage_value = response
                .parsed
                .usage
                .as_ref()
                .map(|u| serde_json::to_value(u).unwrap_or(Value::Null));
            recorder.record(normalize_responses_usage(usage_value.as_ref()));
            Ok(Json(anthropic_response).into_response())
        }
        CreateResponsesReturn::CompactResult(_) => Err(crate::libs::error::HttpError::internal(
            "Messages Responses flow unexpectedly returned a compact result",
        )
        .into()),
    }
}

// ---------------------------------------------------------------------------
// Flow 3: native messages API
// ---------------------------------------------------------------------------

/// Mirrors `handleWithMessagesApi`.
pub async fn handle_with_messages_api(
    payload: &AnthropicMessagesPayload,
    opts: FlowOptions,
) -> Result<Response, AppError> {
    // Capture context in-scope for the deferred stream body + summary line.
    let req_ctx = crate::libs::request_context::request_context_store();
    if let Some(ctx) = &req_ctx {
        ctx.set_flow(&payload.model, "messages", stream_transport::TRANSLATED);
    }

    let recorder = build_recorder(
        "messages",
        payload.model.clone(),
        opts.session_id.clone(),
        payload,
    );

    let result = create_messages(
        payload,
        opts.anthropic_beta_header.as_deref(),
        CreateMessagesOptions {
            subagent_marker: opts.subagent_marker.as_ref(),
            request_id: &opts.request_id,
            session_id: opts.session_id.as_deref(),
            compact_type: opts.compact_type,
        },
    )
    .await?;

    match result {
        CreateMessagesResult::NonStreaming(response) => {
            let usage_value = serde_json::to_value(&response.usage).ok();
            recorder.record(normalize_anthropic_usage(usage_value.as_ref()));
            Ok(Json(response).into_response())
        }
        CreateMessagesResult::Streaming(upstream) => {
            let stream = async_stream::stream! {
                let mut timer = StreamTimer::new("messages", stream_transport::TRANSLATED)
                    .with_request_context(req_ctx);
                let mut usage = UsageTokens::default();
                // Native Anthropic streams must explicitly terminate with either
                // message_stop or error. Never turn a truncated stream into a
                // synthetic success.
                let mut terminal_event_seen = false;

                let heartbeat = crate::libs::sse::sse_heartbeat_interval();
                let sse = crate::libs::sse::events(upstream);
                futures_util::pin_mut!(sse);
                loop {
                    let item = match heartbeat {
                        Some(interval) => match tokio::time::timeout(interval, sse.next()).await {
                            Ok(next) => next,
                            // Idle-but-alive upstream: keep downstream warm with a
                            // ping. Not content — leaves timer/TTFT untouched.
                            Err(_) => {
                                yield Ok(Bytes::from_static(
                                    crate::libs::sse::ANTHROPIC_PING_FRAME,
                                ));
                                continue;
                            }
                        },
                        None => sse.next().await,
                    };
                    let Some(item) = item else { break };
                    let event = match item {
                        Ok(ev) => ev,
                        Err(e) => {
                            tracing::warn!("messages stream error: {e}; sending terminal error event");
                            timer.mark_error();
                            if let Some(frame) =
                                emit_event(&translate_error_to_anthropic_error_event(Some(&e)))
                            {
                                yield Ok(frame);
                            }
                            // Record sniffed usage (message_start input tokens are
                            // captured early here) before bailing on the error.
                            recorder.record(usage);
                            return;
                        }
                    };

                    let event_name = event.event.clone();
                    let data = event.data;
                    if data == "[DONE]" {
                        break;
                    }
                    if data.is_empty() {
                        continue;
                    }

                    if native_message_event_needs_parsing(event_name.as_deref(), &data) {
                        let parsed = match serde_json::from_str::<Value>(&data) {
                            Ok(parsed) => parsed,
                            Err(e) => {
                                record_stream_chunk_parse_failure("messages", &e);
                                timer.mark_error();
                                if let Some(frame) = emit_event(&build_error_event(
                                    "The upstream Messages stream returned a malformed event.",
                                )) {
                                    yield Ok(frame);
                                }
                                recorder.record(usage);
                                return;
                            }
                        };
                        match parsed.get("type").and_then(Value::as_str) {
                            Some("message_start") => {
                                let next = normalize_anthropic_usage(
                                    parsed.get("message").and_then(|m| m.get("usage")),
                                );
                                usage = merge_anthropic_usage(usage, next);
                            }
                            Some("message_delta") => {
                                let next = normalize_anthropic_usage(parsed.get("usage"));
                                usage = merge_anthropic_usage(usage, next);
                            }
                            Some("message_stop" | "error") => {
                                terminal_event_seen = true;
                            }
                            _ => {}
                        }
                    }

                    timer.on_content_frame();
                    yield Ok(native_message_frame(event_name.as_deref(), &data));
                    if terminal_event_seen {
                        break;
                    }
                }

                if !terminal_event_seen {
                    tracing::warn!(
                        "messages stream ended without message_stop/error; sending terminal error"
                    );
                    timer.mark_error();
                    if let Some(frame) = emit_event(&build_error_event(
                        "The upstream Messages stream ended before a terminal event.",
                    )) {
                        yield Ok(frame);
                    }
                } else {
                    timer.mark_finished();
                }

                recorder.record(usage);
            };
            Ok(sse_response(stream))
        }
    }
}

fn native_message_event_needs_parsing(event_name: Option<&str>, data: &str) -> bool {
    match event_name {
        Some("message_start" | "message_delta" | "message_stop" | "error") => true,
        Some(_) => false,
        None => {
            let mut remaining = data;
            while let Some(index) = remaining.find("\"type\"") {
                let after_key = &remaining[index + "\"type\"".len()..];
                if let Some(value) = after_key.trim_start().strip_prefix(':') {
                    let value = value.trim_start();
                    if [
                        "\"message_start\"",
                        "\"message_delta\"",
                        "\"message_stop\"",
                        "\"error\"",
                    ]
                    .iter()
                    .any(|marker| value.starts_with(marker))
                    {
                        return true;
                    }
                }
                remaining = after_key;
            }
            false
        }
    }
}

fn native_message_frame(event_name: Option<&str>, data: &str) -> Bytes {
    let event_len = event_name.map_or(0, str::len);
    let mut frame = Vec::with_capacity(event_len + data.len() + 16);
    if let Some(event_name) = event_name {
        frame.extend_from_slice(b"event: ");
        frame.extend_from_slice(event_name.as_bytes());
        frame.push(b'\n');
    }
    frame.extend_from_slice(b"data: ");
    frame.extend_from_slice(data.as_bytes());
    frame.extend_from_slice(b"\n\n");
    Bytes::from(frame)
}

// ---------------------------------------------------------------------------
// Copilot context cache (chat completions)
// ---------------------------------------------------------------------------

/// Mirrors `prepareCopilotChatCompletionsPayload`.
pub fn prepare_copilot_chat_completions_payload(payload: &mut ChatCompletionsPayload) {
    apply_copilot_context_cache(payload);
    request_streaming_usage(payload);
}

/// For streaming requests, ask the OpenAI-compatible upstream to emit a terminal
/// usage chunk via `stream_options.include_usage = true`. Without it the stream
/// carries `usage: null` and the flow records zero tokens (mis-counting the
/// per-API-key daily budget). Mirrors the provider chat-completions path.
fn request_streaming_usage(payload: &mut ChatCompletionsPayload) {
    if payload.stream != Some(true) {
        return;
    }
    let mut stream_options = payload
        .extra
        .get("stream_options")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    stream_options.insert("include_usage".to_string(), serde_json::Value::Bool(true));
    payload.extra.insert(
        "stream_options".to_string(),
        serde_json::Value::Object(stream_options),
    );
}

/// Mirrors `applyCopilotContextCache`: stamp the ephemeral `copilot_cache_control`
/// marker onto the selected system + trailing non-system messages.
fn apply_copilot_context_cache(payload: &mut ChatCompletionsPayload) {
    let indexes = select_copilot_context_cache_message_indexes(&payload.messages);
    for index in indexes {
        if let Some(message) = payload.messages.get_mut(index) {
            message.extra.insert(
                "copilot_cache_control".to_string(),
                copilot_context_cache_control(),
            );
        }
    }
}

/// Mirrors `selectCopilotContextCacheMessageIndexes`: the first N eligible system
/// messages plus the last N eligible non-system messages, deduped and sorted.
fn select_copilot_context_cache_message_indexes(messages: &[Message]) -> Vec<usize> {
    let system_indexes = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "system" && is_copilot_context_cache_eligible(m))
        .map(|(i, _)| i)
        .take(COPILOT_CONTEXT_CACHE_SYSTEM_MARKER_LIMIT);

    let reverse_non_system_indexes = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != "system" && is_copilot_context_cache_eligible(m))
        .map(|(i, _)| i)
        .rev()
        .take(COPILOT_CONTEXT_CACHE_NON_SYSTEM_MARKER_LIMIT);

    let mut seen: HashSet<usize> = HashSet::new();
    let mut combined: Vec<usize> = Vec::new();
    for index in system_indexes.chain(reverse_non_system_indexes) {
        if seen.insert(index) {
            combined.push(index);
        }
    }
    combined.sort_unstable();
    combined
}

/// Mirrors `isCopilotContextCacheEligible`: a non-empty string content or a
/// non-empty array content.
fn is_copilot_context_cache_eligible(message: &Message) -> bool {
    match &message.content {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn msg(role: &str, content: Value) -> Message {
        Message {
            role: role.to_string(),
            content: Some(content),
            extra: serde_json::Map::new(),
        }
    }

    fn payload(messages: Vec<Message>) -> ChatCompletionsPayload {
        ChatCompletionsPayload {
            messages,
            model: "gpt-4o".to_string(),
            max_tokens: None,
            stream: None,
            extra: serde_json::Map::new(),
        }
    }

    async fn upstream_sse_response(body: &str) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind SSE test server");
        let addr = listener.local_addr().expect("SSE test server address");
        let body = body.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept SSE request");
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.expect("read SSE request");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write SSE response");
        });

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/stream"))
            .send()
            .await
            .expect("receive SSE response");
        server.await.expect("SSE test server task");
        response
    }

    fn has_cache_marker(message: &Message) -> bool {
        message
            .extra
            .get("copilot_cache_control")
            .map(|v| v == &json!({ "type": "ephemeral" }))
            .unwrap_or(false)
    }

    #[test]
    fn eligibility_rules() {
        assert!(is_copilot_context_cache_eligible(&msg("user", json!("hi"))));
        assert!(!is_copilot_context_cache_eligible(&msg("user", json!(""))));
        assert!(is_copilot_context_cache_eligible(&msg(
            "user",
            json!([{ "type": "text", "text": "x" }])
        )));
        assert!(!is_copilot_context_cache_eligible(&msg("user", json!([]))));
        assert!(!is_copilot_context_cache_eligible(&Message {
            role: "user".to_string(),
            content: None,
            extra: serde_json::Map::new(),
        }));
    }

    #[test]
    fn selects_first_systems_and_last_non_systems() {
        // 3 systems (only first 2 selected) and 3 user messages (only last 2).
        let messages = vec![
            msg("system", json!("s0")),
            msg("system", json!("s1")),
            msg("system", json!("s2")),
            msg("user", json!("u3")),
            msg("assistant", json!("a4")),
            msg("user", json!("u5")),
        ];
        let indexes = select_copilot_context_cache_message_indexes(&messages);
        assert_eq!(indexes, vec![0, 1, 4, 5]);
    }

    #[test]
    fn skips_ineligible_messages() {
        let messages = vec![
            msg("system", json!("")),   // ineligible
            msg("system", json!("s1")), // eligible (1st system)
            msg("user", json!([])),     // ineligible
            msg("user", json!("u3")),   // eligible (last non-system)
        ];
        let indexes = select_copilot_context_cache_message_indexes(&messages);
        assert_eq!(indexes, vec![1, 3]);
    }

    #[test]
    fn apply_stamps_only_selected_indexes() {
        let mut p = payload(vec![
            msg("system", json!("s0")),
            msg("user", json!("u1")),
            msg("assistant", json!("a2")),
            msg("user", json!("u3")),
        ]);
        prepare_copilot_chat_completions_payload(&mut p);
        // system 0, last two non-system (2 and 3) get the marker; user 1 does not.
        assert!(has_cache_marker(&p.messages[0]));
        assert!(!has_cache_marker(&p.messages[1]));
        assert!(has_cache_marker(&p.messages[2]));
        assert!(has_cache_marker(&p.messages[3]));
    }

    #[test]
    fn emit_event_frame_format() {
        let frame = emit_event(&AnthropicStreamEventData::MessageStop).unwrap();
        assert_eq!(
            frame.as_ref(),
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }

    #[test]
    fn terminal_error_frame_is_well_formed() {
        // The frame yielded when a streaming flow hits an upstream error must be a
        // complete Anthropic `error` SSE event so Claude Code can retry instead of
        // hanging on a truncated body.
        let frame = emit_event(&translate_error_to_anthropic_error_event(None)).unwrap();
        let frame = std::str::from_utf8(&frame).unwrap();
        assert!(frame.starts_with("event: error\ndata: "));
        assert!(frame.ends_with("\n\n"));

        let data = frame
            .strip_prefix("event: error\ndata: ")
            .and_then(|s| s.strip_suffix("\n\n"))
            .unwrap();
        let parsed: Value = serde_json::from_str(data).unwrap();
        assert_eq!(
            parsed,
            json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": "An unexpected error occurred during streaming."
                }
            })
        );
    }

    #[tokio::test]
    async fn public_translated_driver_stops_after_malformed_choices() {
        let upstream = upstream_sse_response(concat!(
            "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"usage\":{}}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late success\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
            "data: {\"error\":{\"type\":\"server_error\",\"message\":\"late error\"}}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let recorder = create_copilot_token_usage_recorder("chat_completions", "m", None);
        let response = stream_chat_completions_response(upstream, recorder, None);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect translated public stream")
            .to_bytes();
        let body = std::str::from_utf8(&body).expect("translated stream is UTF-8");

        assert_eq!(body.matches("event: error\n").count(), 1);
        assert!(body.contains("The upstream model stream returned a malformed event."));
        assert!(body.contains(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}"
        ));
        assert!(!body.contains("event: message_delta"));
        assert!(!body.contains("event: message_stop"));
        assert!(!body.contains("late success"));
        assert!(!body.contains("late error"));
        assert!(
            body.ends_with(
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"The upstream model stream returned a malformed event.\"}}\n\n"
            ),
            "terminal malformed error must be the final public frame: {body}"
        );
    }

    #[tokio::test]
    async fn public_translated_driver_stops_after_malformed_nested_fields() {
        let cases = [
            (
                "delta/reasoning",
                concat!(
                    "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_text\":\"partial thought\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":42},\"finish_reason\":null}]}\n\n",
                ),
            ),
            (
                "tool/function",
                concat!(
                    "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"deferred\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":{}}}]},\"finish_reason\":null}]}\n\n",
                ),
            ),
            (
                "usage/details",
                concat!(
                    "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens_details\":{\"cached_tokens\":0.5}}}\n\n",
                ),
            ),
        ];

        for (class, prefix) in cases {
            let stream = format!(
                "{}{}",
                prefix,
                concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late success\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: {\"error\":{\"type\":\"server_error\",\"message\":\"late error\"}}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: [DONE]\n\n",
                )
            );
            let upstream = upstream_sse_response(&stream).await;
            let recorder = create_copilot_token_usage_recorder("chat_completions", "m", None);
            let response = stream_chat_completions_response(upstream, recorder, None);
            let body = response
                .into_body()
                .collect()
                .await
                .expect("collect translated public stream")
                .to_bytes();
            let body = std::str::from_utf8(&body).expect("translated stream is UTF-8");

            assert_eq!(body.matches("event: error\n").count(), 1, "{class}: {body}");
            assert!(
                body.contains("The upstream model stream returned a malformed event."),
                "{class}: {body}"
            );
            assert!(
                body.contains("event: content_block_stop\n"),
                "{class}: an open block or pending finish must close before error: {body}"
            );
            assert!(!body.contains("event: message_delta"), "{class}: {body}");
            assert!(!body.contains("event: message_stop"), "{class}: {body}");
            assert!(!body.contains("late success"), "{class}: {body}");
            assert!(!body.contains("late error"), "{class}: {body}");
            assert!(!body.contains("deferred"), "{class}: {body}");
            assert!(
                body.ends_with(
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"The upstream model stream returned a malformed event.\"}}\n\n"
                ),
                "{class}: terminal malformed error must be the final public frame: {body}"
            );
        }
    }

    #[test]
    fn native_message_frame_preserves_existing_wire_format() {
        assert_eq!(
            native_message_frame(Some("content_block_delta"), r#"{"type":"delta"}"#).as_ref(),
            b"event: content_block_delta\ndata: {\"type\":\"delta\"}\n\n"
        );
        assert_eq!(
            native_message_frame(None, "[DONE]").as_ref(),
            b"data: [DONE]\n\n"
        );
    }

    #[test]
    fn native_message_parse_fast_path_skips_token_frames() {
        for event_name in ["message_start", "message_delta", "message_stop", "error"] {
            assert!(native_message_event_needs_parsing(
                Some(event_name),
                "not-json"
            ));
        }

        assert!(!native_message_event_needs_parsing(
            Some("content_block_delta"),
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"message_stop"}}"#
        ));
        assert!(!native_message_event_needs_parsing(
            Some("ping"),
            r#"{"type":"ping"}"#
        ));

        assert!(native_message_event_needs_parsing(
            None,
            r#"{ "type" : "message_start", "message": {} }"#
        ));
        assert!(native_message_event_needs_parsing(
            None,
            r#"{"type":"error","error":{"type":"api_error"}}"#
        ));
        assert!(!native_message_event_needs_parsing(
            None,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"token"}}"#
        ));
    }
}
