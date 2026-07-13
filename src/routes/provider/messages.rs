//! Port of routes/provider/messages/handler.ts
//! `handleProviderMessagesForProvider`.
//!
//! Routes an Anthropic `/v1/messages` request to a configured provider. Three
//! provider types are handled:
//!
//! - `anthropic` — forward the Anthropic payload unchanged, optionally adjusting
//!   reported input tokens, raw-forwarding the stream / JSON.
//! - `openai-compatible` — translate Anthropic <-> OpenAI chat-completions.
//! - `openai-responses` — translate Anthropic <-> Responses API (codex transport
//!   or generic `/v1/responses`), including the web-search-only sub-flow.

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::libs::config::{provider_uses_responses_api, ModelConfig, ResolvedProviderConfig};
use crate::libs::error::{anthropic_error_response, http_error_from_response, AppError};
use crate::libs::provider_resolver::resolve_provider_config;
use crate::libs::token_usage::{
    create_provider_token_usage_recorder, merge_anthropic_usage, normalize_anthropic_usage,
    normalize_openai_usage, normalize_responses_usage, TokenUsageRecorder, UsageTokens,
};
use crate::libs::tool_search::resolve_bridge_tool_search_name;
use crate::libs::utils::parse_user_id_metadata;
use crate::routes::messages::anthropic_types::{
    AnthropicMessagesPayload, AnthropicStreamEventData, AnthropicStreamState,
};
use crate::routes::messages::non_stream_translation::{
    translate_to_anthropic, translate_to_openai_with_options, TranslateToOpenAiOptions,
};
use crate::routes::messages::preprocess::normalize_system_messages;
use crate::routes::messages::request_validation::validate_messages_request_shape;
use crate::routes::messages::responses_stream_translation::{
    build_error_event, terminate_responses_stream_with_error, translate_responses_stream_event,
    ResponsesStreamState,
};
use crate::routes::messages::responses_translation::{
    translate_anthropic_messages_to_responses_payload, translate_responses_result_to_anthropic,
    validate_raw_responses_usage, validate_responses_request_controls,
};
use crate::routes::messages::stream_translation::{
    flush_pending_anthropic_stream_events, malformed_stream_error_events,
    translate_chunk_to_anthropic_events, translate_error_to_anthropic_error_event,
    transport_stream_error_events,
};
use crate::routes::messages::web_search::fulfill::{
    build_synthetic_stream_events, collect_web_search_responses_stream_result_with_usage_observer,
    has_web_search_server_tool, is_web_search_only_request, prepare_web_search_responses_payload,
    reconstruct_web_search_response, strip_web_search_server_tool,
    validate_reconstructed_payload_budget, validate_web_search_result,
};
use crate::routes::responses::utils::{
    apply_responses_api_context_management, compact_input_by_latest_compaction,
    DEFAULT_RESPONSES_COMPACT_THRESHOLD_RATIO,
};
use crate::services::codex::create_responses::forward_codex_responses;
use crate::services::codex::get_models::get_codex_models;
use crate::services::copilot::create_chat_completions::ChatCompletionsPayload;
use crate::services::copilot::create_responses::ResponsesResult;
use crate::services::providers::provider_proxy::{
    forward_provider_chat_completions, forward_provider_messages, forward_provider_responses,
};

const OPENAI_COMPATIBLE_CONTEXT_CACHE_MARKER_LIMIT: usize = 4;
const OPENAI_COMPATIBLE_CONTEXT_CACHE_ROLES: [&str; 4] = ["system", "user", "assistant", "tool"];

/// Thin axum entrypoint: extracts the `:provider` path param and Anthropic body,
/// then delegates to [`handle_provider_messages_for_provider`].
pub async fn post_provider_messages(
    axum::extract::Path(provider): axum::extract::Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let value = match crate::routes::parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    if let Err(error) = validate_messages_request_shape(&value) {
        return error.into_response();
    }
    let payload: AnthropicMessagesPayload = match serde_json::from_value(value) {
        Ok(p) => p,
        Err(e) => {
            return AppError::BadRequest(format!("Invalid request payload: {e}")).into_response()
        }
    };
    if provider_uses_responses_api(&provider) {
        if let Err(error) = validate_responses_request_controls(&payload, provider == "codex") {
            return error.into_response();
        }
    }
    if payload.model.trim().is_empty() {
        return AppError::BadRequest(
            "model: field required and must be a non-empty string".to_string(),
        )
        .into_response();
    }
    if payload.messages.is_empty() {
        return AppError::BadRequest("messages: must contain at least one message".to_string())
            .into_response();
    }
    if !matches!(payload.max_tokens, Some(value) if value > 0) {
        return AppError::BadRequest(
            "max_tokens: field required and must be a positive integer".to_string(),
        )
        .into_response();
    }
    // Internal provider dispatches (model aliases / web search) already pass
    // through the public route's shared gate. This direct provider endpoint does
    // not, so gate it here before resolving or contacting the provider.
    match crate::libs::admission::check_shared_admission().await {
        Ok(()) => {}
        Err(error) => return AppError::Http(error).into_response(),
    }
    match handle_provider_messages_for_provider(payload, provider, headers).await {
        Ok(r) => r,
        Err(e) => AppError::into_response(e),
    }
}

/// Mirrors `handleProviderMessagesForProvider`.
pub async fn handle_provider_messages_for_provider(
    mut payload: AnthropicMessagesPayload,
    provider: String,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let Some(provider_config) = resolve_provider_config(&provider).await else {
        return Ok(anthropic_error_response(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            format!("Provider '{provider}' not found or disabled"),
        ));
    };
    if provider_config.provider_type == "openai-responses" {
        validate_responses_request_controls(&payload, provider == "codex")?;
    }

    let model_config = provider_config
        .models
        .as_ref()
        .and_then(|m| m.get(&payload.model))
        .cloned();

    // normalizeSystemMessages operates on a Value in the Rust port.
    let mut payload_value = serde_json::to_value(&payload)?;
    normalize_system_messages(&mut payload_value);
    payload = serde_json::from_value(payload_value)?;
    if payload.messages.is_empty() {
        return Err(AppError::BadRequest(
            "messages: must contain at least one user or assistant message".to_string(),
        ));
    }

    apply_model_defaults(&mut payload, model_config.as_ref());

    match provider_config.provider_type.as_str() {
        "openai-responses" => {
            if has_web_search_server_tool(&payload) {
                if is_web_search_only_request(&payload) {
                    return handle_openai_responses_provider_web_search_messages(
                        payload,
                        &provider,
                        &provider_config,
                        &headers,
                    )
                    .await;
                }
                strip_web_search_server_tool(&mut payload);
            }

            handle_openai_responses_provider_messages(
                payload,
                &provider,
                &provider_config,
                &headers,
            )
            .await
        }
        "openai-compatible" => {
            // stripWebSearchServerTool — no-op pass-through here (web-search
            // server tools are not represented in the typed payload yet).
            handle_openai_compatible_provider_messages(
                payload,
                &provider,
                &provider_config,
                model_config.as_ref(),
                &headers,
            )
            .await
        }
        _ => {
            // anthropic passthrough
            apply_missing_extra_body(&mut payload, model_config.as_ref());

            let upstream_response =
                forward_provider_messages(&provider_config, &payload, &headers).await?;

            if !upstream_response.status().is_success() {
                tracing::error!("Failed to create responses (provider messages): {provider}");
                return Err(http_error_from_response(
                    "Failed to create responses",
                    upstream_response,
                )
                .await
                .into());
            }

            let content_type = response_content_type(&upstream_response);
            let is_streaming =
                payload.stream.unwrap_or(false) && content_type.contains("text/event-stream");

            if is_streaming {
                Ok(stream_provider_messages(
                    upstream_response,
                    &payload,
                    &provider,
                    &provider_config,
                ))
            } else {
                respond_provider_messages_json(
                    upstream_response,
                    &payload,
                    &provider,
                    &provider_config,
                )
                .await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// openai-responses branch
// ---------------------------------------------------------------------------

/// Mirrors `handleOpenAIResponsesProviderMessages`: translate the Anthropic
/// payload to a Responses payload, run it through the codex transport (codex
/// provider) or the generic `/v1/responses` endpoint, then translate the result
/// back to Anthropic (streaming or JSON).
async fn handle_openai_responses_provider_messages(
    payload: AnthropicMessagesPayload,
    provider: &str,
    provider_config: &ResolvedProviderConfig,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let max_prompt_tokens = codex_max_prompt_tokens(provider_config, &payload.model);

    let mut responses_payload = translate_anthropic_messages_to_responses_payload(&payload, None)?;

    apply_responses_api_context_management(
        &mut responses_payload,
        max_prompt_tokens,
        DEFAULT_RESPONSES_COMPACT_THRESHOLD_RATIO,
    );
    compact_input_by_latest_compaction(&mut responses_payload);

    let is_stream = responses_payload.stream.unwrap_or(false);
    let is_codex = provider_config.name == "codex";

    if is_codex {
        let upstream_response =
            forward_codex_responses(responses_payload, headers, &provider_config.base_url).await?;

        // forward_codex_responses relays non-401 errors verbatim, so guard the
        // status here too (mirrors the generic branch below) to avoid wrapping a
        // 4xx/5xx error body in an HTTP 200.
        if !upstream_response.status().is_success() {
            tracing::error!("Failed to create provider responses: {provider}");
            return Err(http_error_from_response(
                "Failed to create provider responses",
                upstream_response,
            )
            .await
            .into());
        }

        if is_stream {
            return Ok(stream_responses_provider_messages(
                upstream_response,
                &payload,
                provider,
                is_codex,
            ));
        }

        let body = read_responses_result(upstream_response).await?;
        return respond_responses_provider_messages_json(&body, &payload, provider);
    }

    let upstream_response =
        forward_provider_responses(provider_config, &responses_payload, headers).await?;

    if !upstream_response.status().is_success() {
        tracing::error!("Failed to create provider responses: {provider}");
        return Err(http_error_from_response(
            "Failed to create provider responses",
            upstream_response,
        )
        .await
        .into());
    }

    if is_stream {
        return Ok(stream_responses_provider_messages(
            upstream_response,
            &payload,
            provider,
            is_codex,
        ));
    }

    let body = read_responses_result(upstream_response).await?;
    respond_responses_provider_messages_json(&body, &payload, provider)
}

/// Mirrors `handleOpenAIResponsesProviderWebSearchMessages`: the web-search-only
/// sub-flow. Switches the request to the Responses `web_search` tool, collects
/// the streamed result into a single [`ResponsesResult`], reconstructs the
/// native Anthropic `server_tool_use` + `web_search_tool_result` blocks, and
/// replays them (streaming) or returns them as JSON.
async fn handle_openai_responses_provider_web_search_messages(
    payload: AnthropicMessagesPayload,
    provider: &str,
    provider_config: &ResolvedProviderConfig,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let max_prompt_tokens = codex_max_prompt_tokens(provider_config, &payload.model);

    // `prepare_web_search_responses_payload` keeps the original model (model =
    // None), drops the Anthropic server tool, and sets stream = true.
    let mut responses_payload = prepare_web_search_responses_payload(&payload, None, None)?;
    responses_payload.stream = Some(true);

    apply_responses_api_context_management(
        &mut responses_payload,
        max_prompt_tokens,
        DEFAULT_RESPONSES_COMPACT_THRESHOLD_RATIO,
    );
    compact_input_by_latest_compaction(&mut responses_payload);
    let requested_response_model = responses_payload.model.clone();

    let is_codex = provider_config.name == "codex";
    let error_prefix = format!("{provider} web search responses stream");
    let recorder = create_provider_messages_usage_recorder(&payload, provider);

    let body: ResponsesResult = if is_codex {
        let upstream_response =
            forward_codex_responses(responses_payload, headers, &provider_config.base_url).await?;
        if !upstream_response.status().is_success() {
            tracing::error!("Failed to create provider web search responses: {provider}");
            return Err(http_error_from_response(
                "Failed to create provider web search responses",
                upstream_response,
            )
            .await
            .into());
        }
        let stream = Box::pin(crate::libs::sse::events(upstream_response));
        let mut observed_usage = None;
        let collected = collect_web_search_responses_stream_result_with_usage_observer(
            stream,
            &error_prefix,
            Some(&requested_response_model),
            |terminal| {
                observed_usage = Some(normalize_responses_usage(terminal.get("usage")));
            },
        )
        .await;
        if let Some(usage) = observed_usage {
            recorder.record(usage);
        }
        collected?
    } else {
        let upstream_response =
            forward_provider_responses(provider_config, &responses_payload, headers).await?;

        if !upstream_response.status().is_success() {
            tracing::error!("Failed to create provider web search responses: {provider}");
            return Err(http_error_from_response(
                "Failed to create provider web search responses",
                upstream_response,
            )
            .await
            .into());
        }

        let content_type = response_content_type(&upstream_response);
        if content_type.contains("text/event-stream") {
            let stream = Box::pin(crate::libs::sse::events(upstream_response));
            let mut observed_usage = None;
            let collected = collect_web_search_responses_stream_result_with_usage_observer(
                stream,
                &error_prefix,
                Some(&requested_response_model),
                |terminal| {
                    observed_usage = Some(normalize_responses_usage(terminal.get("usage")));
                },
            )
            .await;
            if let Some(usage) = observed_usage {
                recorder.record(usage);
            }
            collected?
        } else {
            let result = read_responses_result(upstream_response).await?;
            let raw = serde_json::to_value(&result).unwrap_or(Value::Null);
            if validate_raw_responses_usage(&raw).is_ok() {
                recorder.record(normalize_responses_usage(raw.get("usage")));
            }
            result
        }
    };

    respond_web_search_provider_messages_json(&body, &payload, provider)
}

/// `codex` provider only: the configured model's `max_prompt_tokens` (used as the
/// context-management limit). Mirrors the TS `selectedModel?.capabilities.limits.max_prompt_tokens`.
fn codex_max_prompt_tokens(provider_config: &ResolvedProviderConfig, model: &str) -> Option<i64> {
    if provider_config.name != "codex" {
        return None;
    }
    get_codex_models()
        .data
        .iter()
        .find(|m| m.id == model)
        .and_then(|m| m.capabilities.limits.max_prompt_tokens)
}

/// Mirrors `streamResponsesProviderMessages`: drive the upstream Responses SSE
/// stream through the Anthropic stream translator. Emits a synthetic error event
/// if the stream ends without a completion event.
fn stream_responses_provider_messages(
    upstream: reqwest::Response,
    payload: &AnthropicMessagesPayload,
    provider: &str,
    is_codex: bool,
) -> Response {
    let recorder = create_provider_messages_usage_recorder(payload, provider);
    let tool_search_name =
        resolve_bridge_tool_search_name(anthropic_tools_as_slice(payload).as_deref());
    let response_model = payload.model.clone();
    let provider_label = provider.to_string();
    let event_stream = crate::libs::sse::events(upstream);

    let body = Body::from_stream(async_stream::stream! {
        use crate::libs::stream_metrics::{transport, StreamTimer};
        let mut timer = StreamTimer::new("provider_messages", transport::NATIVE)
            .with_request_context(crate::libs::request_context::request_context_store());
        let mut usage = UsageTokens::default();
        let mut state = ResponsesStreamState::new_with_model(
            Some(tool_search_name),
            Some(response_model),
        );
        futures_util::pin_mut!(event_stream);

        while let Some(item) = event_stream.next().await {
            let chunk = match item {
                Ok(ev) => ev,
                Err(err) => {
                    timer.mark_error();
                    let error_event = translate_error_to_anthropic_error_event(Some(&err));
                    for event in
                        terminate_responses_stream_with_error(&mut state, error_event)
                    {
                        if let Some(frame) = emit_event(&event) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                        }
                    }
                    recorder.record(usage);
                    return;
                }
            };

            if chunk.event.as_deref() == Some("ping") {
                if !state.translation_failed {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from_static(
                        b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
                    ));
                }
                continue;
            }

            if chunk.data.is_empty() || chunk.data == "[DONE]" {
                if chunk.data == "[DONE]" {
                    break;
                }
                continue;
            }

            let parsed: Value = match serde_json::from_str(&chunk.data) {
                Ok(v) => v,
                Err(error) => {
                    crate::routes::messages::api_flows::record_stream_chunk_parse_failure(
                        "provider_responses",
                        &error,
                    );
                    timer.mark_error();
                    let error_event = build_error_event(
                        "The upstream Responses stream returned a malformed event.",
                    );
                    for event in
                        terminate_responses_stream_with_error(&mut state, error_event)
                    {
                        if let Some(frame) = emit_event(&event) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                        }
                    }
                    recorder.record(usage);
                    return;
                }
            };

            // Codex: log `codex.rate_limits` events (mirrors
            // parseResponsesProviderStreamChunk).
            if is_codex {
                crate::libs::codex_rate_limit::log_codex_rate_limits_event(&parsed);
            }

            let observed_terminal = matches!(
                parsed.get("type").and_then(Value::as_str),
                Some("response.completed") | Some("response.failed")
                    | Some("response.incomplete")
            );
            if observed_terminal {
                if let Some(response) = parsed.get("response") {
                    if validate_raw_responses_usage(response).is_ok() {
                        usage = normalize_responses_usage(response.get("usage"));
                    }
                }
            }

            for event in translate_responses_stream_event(&parsed, &mut state) {
                if let Some(frame) = emit_event(&event) {
                    if !matches!(&event, AnthropicStreamEventData::Error { .. }) {
                        timer.on_content_frame();
                    }
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                }
            }
            if state.message_completed {
                if state.translation_failed {
                    timer.mark_error();
                    if observed_terminal {
                        break;
                    }
                } else {
                    timer.mark_finished();
                    break;
                }
            }
        }

        if !state.message_completed {
            timer.mark_error();
            let error_event = build_error_event(&format!(
                "{provider_label} stream ended without a completion event"
            ));
            for event in terminate_responses_stream_with_error(&mut state, error_event) {
                if let Some(frame) = emit_event(&event) {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                }
            }
        } else if state.translation_failed {
            timer.mark_error();
        } else {
            timer.mark_finished();
        }

        recorder.record(usage);
    });

    sse_response(body)
}

/// Mirrors `respondResponsesProviderMessagesJson`.
#[allow(clippy::result_large_err)]
fn respond_responses_provider_messages_json(
    body: &ResponsesResult,
    payload: &AnthropicMessagesPayload,
    provider: &str,
) -> Result<Response, AppError> {
    let recorder = create_provider_messages_usage_recorder(payload, provider);
    let raw = serde_json::to_value(body).unwrap_or(Value::Null);
    if validate_raw_responses_usage(&raw).is_ok() {
        recorder.record(normalize_responses_usage(raw.get("usage")));
    }

    let tool_search_name =
        resolve_bridge_tool_search_name(anthropic_tools_as_slice(payload).as_deref());
    let anthropic_response =
        translate_responses_result_to_anthropic(body, Some(&tool_search_name))?;
    Ok(Json(anthropic_response).into_response())
}

/// Mirrors `respondWebSearchProviderMessagesJson`: record usage, reconstruct the
/// native Anthropic web-search response, then JSON or synthetic-SSE replay it.
#[allow(clippy::result_large_err)]
fn respond_web_search_provider_messages_json(
    body: &ResponsesResult,
    payload: &AnthropicMessagesPayload,
    provider: &str,
) -> Result<Response, AppError> {
    validate_web_search_result(body)?;

    let request_id = if body.id.is_empty() {
        format!("{provider}:{}", payload.model)
    } else {
        body.id.clone()
    };
    let (_extract, response) = reconstruct_web_search_response(payload, body, &request_id);
    validate_reconstructed_payload_budget(&response)?;

    if !payload.stream.unwrap_or(false) {
        return Ok(Json(response.to_json()).into_response());
    }

    let events = build_synthetic_stream_events(&response);
    let body_stream = async_stream::stream! {
        for event in events {
            let event_type = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message")
                .to_string();
            let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            let frame = format!("event: {event_type}\ndata: {data}\n\n");
            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
        }
    };
    Ok(sse_response(Body::from_stream(body_stream)))
}

/// Anthropic typed tools serialized to `Value`s for `resolve_bridge_tool_search_name`.
fn anthropic_tools_as_slice(payload: &AnthropicMessagesPayload) -> Option<Vec<Value>> {
    payload.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
            .collect()
    })
}

/// Read a Responses-API body into a typed [`ResponsesResult`].
#[allow(clippy::result_large_err)]
async fn read_responses_result(response: reqwest::Response) -> Result<ResponsesResult, AppError> {
    crate::libs::http::read_json_capped(response)
        .await
        .map_err(|e| {
            AppError::Other(anyhow::anyhow!(
                "Failed to read or parse provider responses body: {e}"
            ))
        })
}

/// Mirrors `applyModelDefaults` for the Anthropic payload (typed top_k is i64;
/// config top_k is f64 — round to match the JS number).
fn apply_model_defaults(
    payload: &mut AnthropicMessagesPayload,
    model_config: Option<&ModelConfig>,
) {
    if payload.temperature.is_none() {
        payload.temperature = model_config.and_then(|m| m.temperature);
    }
    if payload.top_p.is_none() {
        payload.top_p = model_config.and_then(|m| m.top_p);
    }
    if payload.top_k.is_none() {
        payload.top_k = model_config.and_then(|m| m.top_k).map(|k| k as i64);
    }
}

/// Mirrors `applyMissingExtraBody` for the Anthropic payload: copy each
/// `extraBody` key not already present (typed fields plus the `extra` bag).
fn apply_missing_extra_body(
    payload: &mut AnthropicMessagesPayload,
    model_config: Option<&ModelConfig>,
) {
    let Some(extra_body) = model_config.and_then(|m| m.extra_body.as_ref()) else {
        return;
    };
    for (key, value) in extra_body {
        if anthropic_payload_has_own(payload, key) {
            continue;
        }
        payload.extra.insert(key.clone(), value.clone());
    }
}

const ANTHROPIC_KNOWN_KEYS: [&str; 16] = [
    "model",
    "messages",
    "cache_control",
    "system",
    "stop_sequences",
    "stream",
    "top_p",
    "top_k",
    "tools",
    "tool_choice",
    "max_tokens",
    "thinking",
    "service_tier",
    "output_config",
    "metadata",
    "temperature",
];

fn anthropic_payload_has_own(payload: &AnthropicMessagesPayload, key: &str) -> bool {
    ANTHROPIC_KNOWN_KEYS.contains(&key) || payload.extra.contains_key(key)
}

// ---------------------------------------------------------------------------
// openai-compatible branch
// ---------------------------------------------------------------------------

async fn handle_openai_compatible_provider_messages(
    payload: AnthropicMessagesPayload,
    provider: &str,
    provider_config: &ResolvedProviderConfig,
    model_config: Option<&ModelConfig>,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let openai_payload = create_openai_compatible_payload(&payload, model_config)?;

    let upstream_response =
        forward_provider_chat_completions(provider_config, &openai_payload, headers).await?;

    if !upstream_response.status().is_success() {
        tracing::error!("Failed to create openai-compatible responses: {provider}");
        return Err(http_error_from_response(
            "Failed to create openai-compatible responses",
            upstream_response,
        )
        .await
        .into());
    }

    let content_type = response_content_type(&upstream_response);
    let is_streaming =
        openai_payload.stream.unwrap_or(false) && content_type.contains("text/event-stream");

    if is_streaming {
        Ok(stream_openai_compatible_provider_messages(
            upstream_response,
            &payload,
            provider,
        ))
    } else {
        respond_openai_compatible_provider_messages_json(upstream_response, &payload, provider)
            .await
    }
}

/// Mirrors `createOpenAICompatiblePayload`.
#[allow(clippy::result_large_err)]
fn create_openai_compatible_payload(
    payload: &AnthropicMessagesPayload,
    model_config: Option<&ModelConfig>,
) -> Result<ChatCompletionsPayload, AppError> {
    // Thread the PDF / tool-content support flags from the provider model config
    // (TS passes `{ supportPdf, toolContentSupportType }` here).
    let translation_options = TranslateToOpenAiOptions {
        support_pdf: model_config.and_then(|m| m.support_pdf).unwrap_or(false),
        tool_content_support_type: model_config
            .and_then(|m| m.tool_content_support_type.clone())
            .unwrap_or_default(),
    };
    let mut openai_payload = translate_to_openai_with_options(payload, &translation_options)?;

    apply_openai_compatible_thinking_budget(&mut openai_payload, payload);

    if let Some(top_k) = payload.top_k {
        openai_payload
            .extra
            .insert("top_k".to_string(), json!(top_k));
    }

    if openai_payload.stream.unwrap_or(false) {
        openai_payload.extra.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    normalize_openai_compatible_reasoning_content(&mut openai_payload);

    apply_openai_compatible_request_overrides(&mut openai_payload, model_config, payload);

    apply_missing_extra_body_chat(&mut openai_payload, model_config);

    apply_openai_compatible_extra_body_thinking_budget(&mut openai_payload, model_config);

    if !chat_payload_has_own(&openai_payload, "parallel_tool_calls") {
        openai_payload
            .extra
            .insert("parallel_tool_calls".to_string(), Value::Bool(true));
    }

    if model_config.and_then(|m| m.context_cache) != Some(false) {
        apply_openai_compatible_context_cache(&mut openai_payload);
    }

    Ok(openai_payload)
}

/// Mirrors `applyOpenAICompatibleThinkingBudget`.
fn apply_openai_compatible_thinking_budget(
    openai_payload: &mut ChatCompletionsPayload,
    source: &AnthropicMessagesPayload,
) {
    if let Some(budget) = request_thinking_budget(source) {
        openai_payload
            .extra
            .insert("thinking_budget".to_string(), json!(budget));
        return;
    }
    // `if (payload.thinking_budget === undefined) delete payload.thinking_budget`
    // — a no-op when the key is already absent; remove a null/undefined entry.
    if openai_payload
        .extra
        .get("thinking_budget")
        .map(Value::is_null)
        .unwrap_or(false)
    {
        openai_payload.extra.remove("thinking_budget");
    }
}

fn request_thinking_budget(payload: &AnthropicMessagesPayload) -> Option<i64> {
    let budget = payload.thinking.as_ref().and_then(|t| t.budget_tokens)?;
    Some(budget)
}

/// Mirrors `applyOpenAICompatibleExtraBodyThinkingBudget`.
fn apply_openai_compatible_extra_body_thinking_budget(
    openai_payload: &mut ChatCompletionsPayload,
    model_config: Option<&ModelConfig>,
) {
    let Some(extra_body) = model_config.and_then(|m| m.extra_body.as_ref()) else {
        return;
    };
    if let Some(value) = extra_body.get("thinking_budget") {
        openai_payload
            .extra
            .insert("thinking_budget".to_string(), value.clone());
    }
}

/// Mirrors `normalizeOpenAICompatibleReasoningContent`: for assistant messages,
/// promote `reasoning_text` to `reasoning_content` and drop the opaque fields.
fn normalize_openai_compatible_reasoning_content(payload: &mut ChatCompletionsPayload) {
    for message in payload.messages.iter_mut() {
        if message.role != "assistant" {
            continue;
        }
        let has_reasoning_content = message
            .extra
            .get("reasoning_content")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let reasoning_text = message.extra.get("reasoning_text").cloned();
        if !has_reasoning_content {
            if let Some(text) = reasoning_text {
                if !text.is_null() {
                    message.extra.insert("reasoning_content".to_string(), text);
                }
            }
        }
        message.extra.remove("reasoning_text");
        message.extra.remove("reasoning_opaque");
    }
}

/// Mirrors `applyOpenAICompatibleRequestOverrides`: for each `extraBody` key that
/// is also present on the source Anthropic payload, copy the source value.
fn apply_openai_compatible_request_overrides(
    openai_payload: &mut ChatCompletionsPayload,
    model_config: Option<&ModelConfig>,
    source: &AnthropicMessagesPayload,
) {
    let Some(extra_body) = model_config.and_then(|m| m.extra_body.as_ref()) else {
        return;
    };
    let source_value = serde_json::to_value(source).unwrap_or(Value::Null);
    let source_obj = source_value.as_object();
    for key in extra_body.keys() {
        if let Some(value) = source_obj.and_then(|o| o.get(key)) {
            openai_payload.extra.insert(key.clone(), value.clone());
        }
    }
}

/// Mirrors `applyMissingExtraBody` for the chat-completions payload.
fn apply_missing_extra_body_chat(
    openai_payload: &mut ChatCompletionsPayload,
    model_config: Option<&ModelConfig>,
) {
    let Some(extra_body) = model_config.and_then(|m| m.extra_body.as_ref()) else {
        return;
    };
    for (key, value) in extra_body {
        if chat_payload_has_own(openai_payload, key) {
            continue;
        }
        openai_payload.extra.insert(key.clone(), value.clone());
    }
}

fn chat_payload_has_own(payload: &ChatCompletionsPayload, key: &str) -> bool {
    matches!(key, "model" | "messages" | "max_tokens" | "stream") || payload.extra.contains_key(key)
}

// ---------------------------------------------------------------------------
// openai-compatible context cache
// ---------------------------------------------------------------------------

/// Mirrors `applyOpenAICompatibleContextCache`.
fn apply_openai_compatible_context_cache(payload: &mut ChatCompletionsPayload) {
    let indexes = select_context_cache_message_indexes(&payload.messages);
    for index in indexes {
        apply_context_cache_control(&mut payload.messages[index]);
    }
}

/// Mirrors `selectContextCacheMessageIndexes`.
fn select_context_cache_message_indexes(
    messages: &[crate::services::copilot::create_chat_completions::Message],
) -> Vec<usize> {
    let cacheable: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_context_cache_marker_eligible(m))
        .map(|(i, _)| i)
        .collect();

    let system_indexes: Vec<usize> = cacheable
        .iter()
        .copied()
        .filter(|&i| messages[i].role == "system")
        .take(2)
        .collect();

    let non_system: Vec<usize> = cacheable
        .iter()
        .copied()
        .filter(|&i| messages[i].role != "system")
        .collect();
    let non_system_tail: Vec<usize> = non_system
        .iter()
        .copied()
        .skip(non_system.len().saturating_sub(2))
        .collect();

    let mut combined: Vec<usize> = system_indexes;
    combined.extend(non_system_tail);
    let unique = unique_indexes(combined);
    let mut sorted = unique;
    sorted.sort_unstable();
    sorted
}

fn unique_indexes(indexes: Vec<usize>) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for i in indexes {
        if seen.insert(i) {
            out.push(i);
        }
        if out.len() >= OPENAI_COMPATIBLE_CONTEXT_CACHE_MARKER_LIMIT {
            break;
        }
    }
    out
}

fn is_context_cache_marker_eligible(
    message: &crate::services::copilot::create_chat_completions::Message,
) -> bool {
    if !OPENAI_COMPATIBLE_CONTEXT_CACHE_ROLES.contains(&message.role.as_str()) {
        return false;
    }
    match message.content.as_ref() {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(arr)) => !arr.is_empty(),
        _ => false,
    }
}

/// Mirrors `applyContextCacheControl` + `setContextCacheControl`.
fn apply_context_cache_control(
    message: &mut crate::services::copilot::create_chat_completions::Message,
) {
    let cache_control = json!({ "type": "ephemeral" });
    match message.content.take() {
        Some(Value::String(text)) => {
            message.content = Some(json!([
                {
                    "type": "text",
                    "text": text,
                    "cache_control": cache_control,
                }
            ]));
        }
        Some(Value::Array(mut parts)) => {
            if let Some(last) = parts.last_mut() {
                if let Some(obj) = last.as_object_mut() {
                    obj.entry("cache_control".to_string())
                        .or_insert(cache_control);
                }
            }
            message.content = Some(Value::Array(parts));
        }
        other => {
            message.content = other;
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming: anthropic passthrough
// ---------------------------------------------------------------------------

/// Mirrors `streamProviderMessages`: raw-forward each Anthropic SSE event,
/// adjusting input tokens on `message_start` / `message_delta` and recording
/// usage at stream end.
fn stream_provider_messages(
    upstream: reqwest::Response,
    payload: &AnthropicMessagesPayload,
    provider: &str,
    provider_config: &ResolvedProviderConfig,
) -> Response {
    let recorder = create_provider_messages_usage_recorder(payload, provider);
    let adjust = provider_config.adjust_input_tokens.unwrap_or(false);
    let event_stream = crate::libs::sse::events(upstream);

    let body = Body::from_stream(async_stream::stream! {
        use crate::libs::stream_metrics::{transport, StreamTimer};
        let mut timer = StreamTimer::new("provider_messages", transport::NATIVE)
            .with_request_context(crate::libs::request_context::request_context_store());
        let mut usage = UsageTokens::default();
        let mut terminal_event_seen = false;
        futures_util::pin_mut!(event_stream);

        while let Some(item) = event_stream.next().await {
            let chunk = match item {
                Ok(ev) => ev,
                Err(err) => {
                    timer.mark_error();
                    if let Some(frame) =
                        emit_event(&translate_error_to_anthropic_error_event(Some(&err)))
                    {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                    }
                    recorder.record(usage);
                    return;
                }
            };

            let event_name = chunk.event.clone();
            if event_name.as_deref() == Some("ping") {
                yield Ok::<Bytes, std::io::Error>(Bytes::from_static(
                    b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
                ));
                continue;
            }

            if chunk.data.is_empty() {
                continue;
            }
            if chunk.data == "[DONE]" {
                break;
            }

            let parsed = match parse_provider_stream_event(&chunk.data, adjust) {
                Some(parsed) => parsed,
                None => {
                    timer.mark_error();
                    if let Some(frame) = emit_event(&build_error_event(
                        "The upstream provider Messages stream returned a malformed event.",
                    )) {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                    }
                    recorder.record(usage);
                    return;
                }
            };
            usage = merge_anthropic_usage(usage, parsed.usage);
            let data = parsed.data;
            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                terminal_event_seen = matches!(
                    value.get("type").and_then(Value::as_str),
                    Some("message_stop" | "error")
                );
            }

            let frame = match event_name {
                Some(name) => format!("event: {name}\ndata: {data}\n\n"),
                None => format!("data: {data}\n\n"),
            };
            timer.on_content_frame();
            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
            if terminal_event_seen {
                break;
            }
        }

        if terminal_event_seen {
            timer.mark_finished();
        } else {
            timer.mark_error();
            if let Some(frame) = emit_event(&build_error_event(
                "The upstream provider Messages stream ended before a terminal event.",
            )) {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
            }
        }
        recorder.record(usage);
    });

    sse_response(body)
}

struct ParsedProviderStreamEvent {
    data: String,
    usage: UsageTokens,
}

/// Mirrors `parseProviderStreamEvent`: re-serialize after adjusting input tokens
/// on message_start / message_delta, returning normalized usage.
fn parse_provider_stream_event(data: &str, adjust: bool) -> Option<ParsedProviderStreamEvent> {
    let mut parsed: Value = serde_json::from_str(data).ok()?;
    match parsed.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            if let Some(usage) = parsed.pointer_mut("/message/usage") {
                adjust_input_tokens(adjust, usage);
            }
            let usage = normalize_anthropic_usage(parsed.pointer("/message/usage"));
            Some(ParsedProviderStreamEvent {
                data: serde_json::to_string(&parsed).ok()?,
                usage,
            })
        }
        Some("message_delta") => {
            if let Some(usage) = parsed.get_mut("usage") {
                adjust_input_tokens(adjust, usage);
            }
            let usage = normalize_anthropic_usage(parsed.get("usage"));
            Some(ParsedProviderStreamEvent {
                data: serde_json::to_string(&parsed).ok()?,
                usage,
            })
        }
        _ => Some(ParsedProviderStreamEvent {
            data: serde_json::to_string(&parsed).ok()?,
            usage: UsageTokens::default(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Streaming: openai-compatible translation
// ---------------------------------------------------------------------------

/// Mirrors `streamOpenAICompatibleProviderMessages`: translate each OpenAI chunk
/// into Anthropic stream events.
fn stream_openai_compatible_provider_messages(
    upstream: reqwest::Response,
    payload: &AnthropicMessagesPayload,
    provider: &str,
) -> Response {
    let recorder = create_provider_messages_usage_recorder(payload, provider);
    let event_stream = crate::libs::sse::events(upstream);

    let body = Body::from_stream(async_stream::stream! {
        use crate::libs::stream_metrics::{transport, StreamTimer};
        let mut timer = StreamTimer::new("provider_messages", transport::NATIVE)
            .with_request_context(crate::libs::request_context::request_context_store());
        let mut usage = UsageTokens::default();
        let mut state = AnthropicStreamState::default();
        futures_util::pin_mut!(event_stream);

        while let Some(item) = event_stream.next().await {
            let chunk = match item {
                Ok(ev) => ev,
                Err(err) => {
                    timer.mark_error();
                    for event in transport_stream_error_events(&mut state, Some(&err)) {
                        if let Some(frame) = emit_event(&event) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                        }
                    }
                    recorder.record(usage);
                    return;
                }
            };

            if chunk.event.as_deref() == Some("ping") {
                yield Ok::<Bytes, std::io::Error>(Bytes::from_static(
                    b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
                ));
                continue;
            }

            if chunk.data.is_empty() || chunk.data == "[DONE]" {
                if chunk.data == "[DONE]" {
                    break;
                }
                continue;
            }

            let parsed: Value = match serde_json::from_str(&chunk.data) {
                Ok(v) => v,
                Err(error) => {
                    crate::routes::messages::api_flows::record_stream_chunk_parse_failure(
                        "provider_chat_completions",
                        &error,
                    );
                    timer.mark_error();
                    for event in malformed_stream_error_events(&mut state) {
                        if let Some(frame) = emit_event(&event) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                        }
                    }
                    recorder.record(usage);
                    return;
                }
            };

            let was_terminal = state.terminal_event_emitted;
            let translated = translate_chunk_to_anthropic_events(&parsed, &mut state);
            let terminal_error = translated
                .iter()
                .any(|event| matches!(event, AnthropicStreamEventData::Error { .. }));
            // The translator validates every usage field this flow consumes.
            // Account only a chunk it accepted, and never let trailing records
            // mutate usage after success or failure became terminal.
            if !was_terminal && !terminal_error {
                if let Some(u) = parsed.get("usage").filter(|usage| !usage.is_null()) {
                    usage = normalize_openai_usage(Some(u));
                }
            }
            for event in translated {
                if let Some(frame) = emit_event(&event) {
                    if !matches!(&event, AnthropicStreamEventData::Error { .. }) {
                        timer.on_content_frame();
                    }
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                }
            }
            if terminal_error {
                tracing::warn!(
                    "provider chat-completions stream reported a terminal upstream error"
                );
                timer.mark_error();
                recorder.record(usage);
                return;
            }
        }

        let incomplete = !state.terminal_event_emitted && state.pending_message_delta.is_none();
        if incomplete {
            timer.mark_error();
        }
        for event in flush_pending_anthropic_stream_events(&mut state) {
            if let Some(frame) = emit_event(&event) {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
            }
        }

        if state.message_stop_emitted {
            timer.mark_finished();
        }
        recorder.record(usage);
    });

    sse_response(body)
}

// ---------------------------------------------------------------------------
// Non-streaming responses
// ---------------------------------------------------------------------------

/// Mirrors `respondProviderMessagesJson` (anthropic passthrough).
async fn respond_provider_messages_json(
    upstream: reqwest::Response,
    payload: &AnthropicMessagesPayload,
    provider: &str,
    provider_config: &ResolvedProviderConfig,
) -> Result<Response, AppError> {
    let mut body: Value = read_json(upstream).await?;
    let recorder = create_provider_messages_usage_recorder(payload, provider);

    let adjust = provider_config.adjust_input_tokens.unwrap_or(false);
    if let Some(usage) = body.get_mut("usage") {
        adjust_input_tokens(adjust, usage);
    }
    recorder.record(normalize_anthropic_usage(body.get("usage")));

    Ok(Json(body).into_response())
}

/// Mirrors `respondOpenAICompatibleProviderMessagesJson`.
async fn respond_openai_compatible_provider_messages_json(
    upstream: reqwest::Response,
    payload: &AnthropicMessagesPayload,
    provider: &str,
) -> Result<Response, AppError> {
    let (body, headers) = read_chat_json(upstream).await?;
    let recorder = create_provider_messages_usage_recorder(payload, provider);
    let anthropic_response = translate_to_anthropic(&body).map_err(|mut error| {
        error.headers = headers;
        AppError::Http(error)
    })?;
    recorder.record(normalize_openai_usage(body.get("usage")));
    Ok(Json(anthropic_response).into_response())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Mirrors `createProviderMessagesUsageRecorder`.
fn create_provider_messages_usage_recorder(
    payload: &AnthropicMessagesPayload,
    provider: &str,
) -> TokenUsageRecorder {
    let session_id = payload
        .metadata
        .as_ref()
        .and_then(|m| parse_user_id_metadata(m.user_id.as_deref()).session_id);
    let mut recorder = create_provider_token_usage_recorder(
        "provider_messages",
        payload.model.clone(),
        provider,
        None,
    );
    recorder.session_id = session_id;
    recorder
}

/// Mirrors `adjustInputTokens`: subtract cache read/creation tokens from the
/// reported input tokens (clamped at 0) when the provider opts in.
fn adjust_input_tokens(adjust: bool, usage: &mut Value) {
    if !adjust {
        return;
    }
    let Some(obj) = usage.as_object_mut() else {
        return;
    };
    let input = obj.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
    let cache_read = obj
        .get("cache_read_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_creation = obj
        .get("cache_creation_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let adjusted = (input - cache_read - cache_creation).max(0);
    obj.insert("input_tokens".to_string(), json!(adjusted));
}

fn response_content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn read_json(response: reqwest::Response) -> Result<Value, AppError> {
    crate::libs::http::read_json_capped(response)
        .await
        .map_err(|e| {
            AppError::Other(anyhow::anyhow!(
                "Failed to read or parse provider response body: {e}"
            ))
        })
}

async fn read_chat_json(response: reqwest::Response) -> Result<(Value, HeaderMap), AppError> {
    let headers = crate::libs::error::upstream_response_headers(&response);
    let value = crate::libs::http::read_json_capped(response)
        .await
        .map_err(|error| {
            tracing::warn!(?error, "invalid provider Chat Completions response body");
            let mut error = crate::libs::error::HttpError::bad_gateway(
                "The upstream Chat Completions response body was malformed.",
            );
            error.headers = headers.clone();
            AppError::Http(error)
        })?;
    Ok((value, headers))
}

/// Render one translated Anthropic event as an SSE frame (`event: {type}\ndata:
/// {json}\n\n`), matching the `api_flows.rs` `emit_event` idiom.
fn emit_event(event: &AnthropicStreamEventData) -> Option<String> {
    let value = serde_json::to_value(event).ok()?;
    let event_name = value.get("type").and_then(Value::as_str)?.to_string();
    let data = serde_json::to_string(&value).ok()?;
    Some(format!("event: {event_name}\ndata: {data}\n\n"))
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::copilot::create_chat_completions::Message;
    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn msg(role: &str, content: Value) -> Message {
        Message {
            role: role.to_string(),
            content: Some(content),
            extra: serde_json::Map::new(),
        }
    }

    async fn upstream_sse_response(body: &str) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind provider SSE test server");
        let addr = listener
            .local_addr()
            .expect("provider SSE test server address");
        let body = body.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept provider SSE request");
            let mut request = [0u8; 1024];
            let _ = socket
                .read(&mut request)
                .await
                .expect("read provider SSE request");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write provider SSE response");
        });

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/stream"))
            .send()
            .await
            .expect("receive provider SSE response");
        server.await.expect("provider SSE test server task");
        response
    }

    #[test]
    fn adjust_input_tokens_clamps_and_subtracts_cache() {
        let mut usage = json!({
            "input_tokens": 100,
            "cache_read_input_tokens": 30,
            "cache_creation_input_tokens": 20,
        });
        adjust_input_tokens(true, &mut usage);
        assert_eq!(usage["input_tokens"], 50);

        // clamps at zero
        let mut usage2 = json!({ "input_tokens": 10, "cache_read_input_tokens": 40 });
        adjust_input_tokens(true, &mut usage2);
        assert_eq!(usage2["input_tokens"], 0);
    }

    #[test]
    fn adjust_input_tokens_noop_when_disabled() {
        let mut usage = json!({ "input_tokens": 100, "cache_read_input_tokens": 30 });
        adjust_input_tokens(false, &mut usage);
        assert_eq!(usage["input_tokens"], 100);
    }

    #[test]
    fn context_cache_selects_two_system_and_trailing_two_nonsystem() {
        let messages = vec![
            msg("system", json!("a")),
            msg("system", json!("b")),
            msg("system", json!("c")),
            msg("user", json!("u1")),
            msg("assistant", json!("a1")),
            msg("user", json!("u2")),
        ];
        let indexes = select_context_cache_message_indexes(&messages);
        // first two system (0,1) + last two non-system (4,5), sorted & unique, capped at 4.
        assert_eq!(indexes, vec![0, 1, 4, 5]);
    }

    #[test]
    fn context_cache_skips_empty_content() {
        let messages = vec![
            msg("system", json!("")),
            msg("user", json!([])),
            msg("user", json!("hello")),
        ];
        let indexes = select_context_cache_message_indexes(&messages);
        // empty string/array are ineligible; only index 2 remains.
        assert_eq!(indexes, vec![2]);
    }

    #[test]
    fn apply_context_cache_control_wraps_string() {
        let mut message = msg("system", json!("hi"));
        apply_context_cache_control(&mut message);
        let content = message.content.unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hi");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn apply_context_cache_control_marks_last_array_part() {
        let mut message = msg(
            "user",
            json!([{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }]),
        );
        apply_context_cache_control(&mut message);
        let content = message.content.unwrap();
        assert!(content[0].get("cache_control").is_none());
        assert_eq!(content[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn parse_provider_stream_event_adjusts_message_start() {
        let data = r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":40}}}"#;
        let parsed = parse_provider_stream_event(data, true).unwrap();
        let value: Value = serde_json::from_str(&parsed.data).unwrap();
        assert_eq!(value["message"]["usage"]["input_tokens"], 60);
        assert_eq!(parsed.usage.input_tokens, Some(60));
    }

    #[tokio::test]
    async fn provider_translated_driver_stops_after_malformed_choices() {
        let upstream = upstream_sse_response(concat!(
            "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"deferred\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":\"not-an-array\"}\n\n",
            "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late success\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: {\"error\":{\"type\":\"server_error\",\"message\":\"late error\"}}\n\n",
            "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let payload = AnthropicMessagesPayload {
            model: "m".to_string(),
            ..Default::default()
        };
        let response =
            stream_openai_compatible_provider_messages(upstream, &payload, "test-provider");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect translated provider stream")
            .to_bytes();
        let body = std::str::from_utf8(&body).expect("translated provider stream is UTF-8");

        assert_eq!(body.matches("event: error\n").count(), 1);
        assert!(body.contains("The upstream model stream returned a malformed event."));
        assert!(body.contains("\"type\":\"tool_use\",\"id\":\"call_1\""));
        assert!(body.contains(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}"
        ));
        assert!(!body.contains("deferred"));
        assert!(!body.contains("event: message_delta"));
        assert!(!body.contains("event: message_stop"));
        assert!(!body.contains("late success"));
        assert!(!body.contains("late error"));
        assert!(
            body.ends_with(
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"The upstream model stream returned a malformed event.\"}}\n\n"
            ),
            "terminal malformed error must be the final provider frame: {body}"
        );
    }

    #[tokio::test]
    async fn provider_translated_driver_stops_after_malformed_nested_fields() {
        let cases = [
            (
                "delta/reasoning",
                concat!(
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_text\":\"partial thought\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_opaque\":[]},\"finish_reason\":null}]}\n\n",
                ),
            ),
            (
                "tool/function",
                concat!(
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"deferred\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":\"bad\",\"function\":{\"arguments\":\"late fragment\"}}]},\"finish_reason\":null}]}\n\n",
                ),
            ),
            (
                "usage/details",
                concat!(
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":\"bad\",\"completion_tokens\":[]}}\n\n",
                ),
            ),
        ];

        for (class, prefix) in cases {
            let stream = format!(
                "{}{}",
                prefix,
                concat!(
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late success\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: {\"error\":{\"type\":\"server_error\",\"message\":\"late error\"}}\n\n",
                    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: [DONE]\n\n",
                )
            );
            let upstream = upstream_sse_response(&stream).await;
            let payload = AnthropicMessagesPayload {
                model: "m".to_string(),
                ..Default::default()
            };
            let response =
                stream_openai_compatible_provider_messages(upstream, &payload, "test-provider");
            let body = response
                .into_body()
                .collect()
                .await
                .expect("collect translated provider stream")
                .to_bytes();
            let body = std::str::from_utf8(&body).expect("translated provider stream is UTF-8");

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
                "{class}: terminal malformed error must be the final provider frame: {body}"
            );
        }
    }
}
