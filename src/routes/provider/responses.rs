//! Port of routes/provider/responses/handler.ts
//! `handleProviderResponsesForProvider`.
//!
//! Forwards an OpenAI Responses-API request to a configured `openai-responses`
//! provider. The `codex` provider goes through the Codex transport (with the
//! hardcoded model catalog for the context-management limit); every other
//! provider forwards to `{baseUrl}/v1/responses`. Streaming responses are peeked
//! for a leading `error` event (surfaced as a JSON error) and Codex events are
//! re-serialized/normalized as they pass through.

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::libs::error::{http_error_from_response, AppError};
use crate::libs::provider_resolver::resolve_provider_config;
use crate::libs::request_context::request_context_store;
use crate::libs::token_usage::{
    create_provider_token_usage_recorder, normalize_responses_usage, TokenUsageRecorder,
    UsageTokens,
};
use crate::routes::responses::utils::{
    apply_responses_api_context_management, compact_input_by_latest_compaction,
};
use crate::routes::responses::{
    stream_guard::{ResponsesStreamGuard, ResponsesTerminal},
    stream_id_sync::StreamIdTracker,
};
use crate::services::codex::create_responses::forward_codex_responses;
use crate::services::codex::get_models::get_codex_models;
use crate::services::copilot::create_responses::ResponsesPayload;
use crate::services::providers::provider_proxy::forward_provider_responses;

/// Mirrors `handleProviderResponsesForProvider`.
pub async fn handle_provider_responses_for_provider(
    mut payload: ResponsesPayload,
    provider: String,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let provider_config = resolve_provider_config(&provider).await;
    let provider_config = match provider_config {
        Some(cfg) if cfg.provider_type == "openai-responses" => cfg,
        _ => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!(
                            "Provider '{provider}' does not support the /v1/responses endpoint"
                        ),
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response());
        }
    };

    let max_prompt_tokens = if provider_config.name == "codex" {
        get_codex_models()
            .data
            .iter()
            .find(|m| m.id == payload.model)
            .and_then(|m| m.capabilities.limits.max_prompt_tokens)
            .unwrap_or(0)
    } else {
        0
    };

    // Smaller than the client compaction threshold; use server-side compaction
    // to maintain cache hit rate.
    apply_responses_api_context_management(&mut payload, Some(max_prompt_tokens), 0.8);
    compact_input_by_latest_compaction(&mut payload);

    let is_stream = payload.stream.unwrap_or(false);
    let recorder = create_provider_responses_usage_recorder(&payload, &provider);

    if provider_config.name == "codex" {
        let upstream_response =
            forward_codex_responses(payload, &headers, &provider_config.base_url).await?;

        // forward_codex_responses only special-cases 401 (refresh + retry) and
        // otherwise hands back the live response verbatim, so a 4xx/5xx error
        // body would otherwise be relayed to the client as HTTP 200. Mirror the
        // generic branch and surface upstream failures as real errors.
        if !upstream_response.status().is_success() {
            return Err(http_error_from_response(
                format!("Failed to create {provider} responses"),
                upstream_response,
            )
            .await
            .into());
        }

        // forward_codex_responses returns the live reqwest::Response; the TS
        // `isResponsesStream` check maps to "the request asked to stream".
        if is_stream {
            return stream_provider_responses(upstream_response, &provider, recorder, true).await;
        }

        let status = upstream_response.status();
        let resp_headers = upstream_response.headers().clone();
        let response_body = read_responses_result(upstream_response).await?;
        recorder.record(normalize_responses_usage(
            usage_value(&response_body.value).as_ref(),
        ));
        return Ok(build_proxy_response_from_parts(
            status,
            &resp_headers,
            response_body.bytes,
        ));
    }

    let upstream_response =
        forward_provider_responses(&provider_config, &payload, &headers).await?;

    if !upstream_response.status().is_success() {
        return Err(http_error_from_response(
            format!("Failed to create {provider} responses"),
            upstream_response,
        )
        .await
        .into());
    }

    if is_stream {
        return stream_provider_responses(upstream_response, &provider, recorder, false).await;
    }

    // Non-streaming: buffer, record usage, forward unchanged.
    let status = upstream_response.status();
    let resp_headers = upstream_response.headers().clone();
    let bytes = crate::libs::http::read_bytes_capped(upstream_response)
        .await
        .map_err(|error| {
            AppError::Http(crate::libs::error::HttpError::new(
                if error.contains("too large") {
                    "Upstream Responses body exceeded the maximum allowed size."
                } else {
                    "The upstream Responses body could not be read."
                },
                StatusCode::BAD_GATEWAY,
                HeaderMap::new(),
                String::new(),
            ))
        })?;

    let body_value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        AppError::Http(crate::libs::error::HttpError::new(
            "The upstream Responses body was not valid JSON.",
            StatusCode::BAD_GATEWAY,
            HeaderMap::new(),
            String::new(),
        ))
    })?;
    recorder.record(normalize_responses_usage(body_value.get("usage")));

    Ok(build_proxy_response_from_parts(
        status,
        &resp_headers,
        bytes,
    ))
}

/// Mirrors `createProviderResponsesUsageRecorder`: session id derived from the
/// request-context session affinity.
fn create_provider_responses_usage_recorder(
    payload: &ResponsesPayload,
    provider: &str,
) -> TokenUsageRecorder {
    let session_affinity = request_context_store()
        .and_then(|s| s.session_affinity)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut recorder =
        create_provider_token_usage_recorder("responses", payload.model.clone(), provider, None);
    recorder.session_id = Some(session_affinity.unwrap_or_default());
    recorder
}

/// Mirrors `streamProviderResponses`: peek the first event for a leading `error`
/// (surfaced as a JSON error response), then SSE-forward the remainder, applying
/// the Codex normalize step and sniffing usage from terminal events.
async fn stream_provider_responses(
    upstream: reqwest::Response,
    provider: &str,
    recorder: TokenUsageRecorder,
    normalize_codex: bool,
) -> Result<Response, AppError> {
    let mut event_stream = Box::pin(crate::libs::sse::events(upstream));

    // Peek the first non-empty chunk to surface a leading `error` event as a
    // JSON error instead of an SSE stream.
    let first = match event_stream.next().await {
        Some(Ok(ev)) => Some(ev),
        Some(Err(err)) => {
            return Err(AppError::Other(anyhow::anyhow!(
                "Provider responses stream error: {err}"
            )))
        }
        None => None,
    };

    let Some(first_chunk) = first else {
        return Err(crate::libs::error::HttpError::new(
            format!("Empty stream from {provider} responses"),
            StatusCode::BAD_GATEWAY,
            HeaderMap::new(),
            String::new(),
        )
        .into());
    };

    if !first_chunk.data.is_empty() && first_chunk.data != "[DONE]" {
        if let Ok(parsed) = serde_json::from_str::<Value>(&first_chunk.data) {
            if parsed.get("type").and_then(Value::as_str) == Some("error") {
                let status_code = parsed
                    .get("status_code")
                    .and_then(Value::as_i64)
                    .and_then(|c| u16::try_from(c).ok())
                    .and_then(|c| StatusCode::from_u16(c).ok())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

                let mut error_obj = serde_json::Map::new();
                if let Some(message) = parsed.get("message") {
                    error_obj.insert("message".to_string(), message.clone());
                }
                if let Some(Value::Object(inner)) = parsed.get("error") {
                    for (k, v) in inner {
                        error_obj.insert(k.clone(), v.clone());
                    }
                }
                return Ok((status_code, Json(json!({ "error": error_obj }))).into_response());
            }
        }
    }

    let provider_label = provider.to_string();
    let body = Body::from_stream(async_stream::stream! {
        let mut usage = UsageTokens::default();
        let mut guard = ResponsesStreamGuard::new();
        let mut ids = StreamIdTracker::new();

        let combined = futures_util::stream::once(async move {
            Ok::<_, std::io::Error>(first_chunk)
        })
        .chain(event_stream);
        futures_util::pin_mut!(combined);
        while let Some(item) = combined.next().await {
            let chunk = match item {
                Ok(ev) => ev,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        provider = %provider_label,
                        "Provider Responses stream failed"
                    );
                    if let Some(frame) = guard.fail(
                        "upstream_transport_error",
                        "The upstream Responses stream was interrupted.",
                    ) {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                    }
                    recorder.record(usage);
                    return;
                }
            };

            let processed = match guard.process(&chunk, &mut ids) {
                Ok(Some(processed)) => processed,
                Ok(None) => continue,
                Err(reason) => {
                    tracing::warn!(
                        reason,
                        provider = %provider_label,
                        "Rejected malformed provider Responses stream event"
                    );
                    if let Some(frame) = guard.fail(
                        "invalid_stream",
                        "The upstream Responses stream returned malformed data.",
                    ) {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
                    }
                    recorder.record(usage);
                    return;
                }
            };

            if normalize_codex {
                crate::libs::codex_rate_limit::log_codex_rate_limits_event(&processed.value);
            }
            if let Some(next) = responses_stream_event_usage(&processed.value) {
                usage = next;
            }

            let terminal = processed.terminal;
            yield Ok::<Bytes, std::io::Error>(Bytes::from(processed.frame));
            if let Some(terminal) = terminal {
                if terminal == ResponsesTerminal::Failed {
                    tracing::warn!(
                        provider = %provider_label,
                        "Provider Responses stream ended with failure"
                    );
                }
                recorder.record(usage);
                return;
            }
        }

        if let Some(frame) = guard.fail(
            "upstream_eof",
            "The upstream Responses stream ended before a terminal event.",
        ) {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
        }
        recorder.record(usage);
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap())
}

/// Mirrors `getResponsesStreamEventUsage`: usage from the terminal events.
fn responses_stream_event_usage(event: &Value) -> Option<UsageTokens> {
    match event.get("type").and_then(Value::as_str) {
        Some("response.completed") | Some("response.failed") | Some("response.incomplete") => Some(
            normalize_responses_usage(event.get("response").and_then(|r| r.get("usage"))),
        ),
        _ => None,
    }
}

struct BufferedResponsesBody {
    bytes: Bytes,
    value: Value,
}

/// Read a non-streaming Codex responses body without reserializing its JSON.
async fn read_responses_result(
    response: reqwest::Response,
) -> Result<BufferedResponsesBody, AppError> {
    let bytes = crate::libs::http::read_bytes_capped(response)
        .await
        .map_err(|error| {
            AppError::Http(crate::libs::error::HttpError::new(
                if error.contains("too large") {
                    "Upstream Responses body exceeded the maximum allowed size."
                } else {
                    "The upstream Responses body could not be read."
                },
                StatusCode::BAD_GATEWAY,
                HeaderMap::new(),
                String::new(),
            ))
        })?;
    let value = serde_json::from_slice(&bytes).map_err(|_| {
        AppError::Http(crate::libs::error::HttpError::new(
            "The upstream Responses body was not valid JSON.",
            StatusCode::BAD_GATEWAY,
            HeaderMap::new(),
            String::new(),
        ))
    })?;
    Ok(BufferedResponsesBody { bytes, value })
}

fn usage_value(body: &Value) -> Option<Value> {
    body.get("usage").cloned()
}

/// Build a pass-through proxy response from buffered parts (mirrors
/// `createProviderProxyResponse`'s header policy over an already-read body).
fn build_proxy_response_from_parts(
    status: reqwest::StatusCode,
    upstream_headers: &reqwest::header::HeaderMap,
    body: Bytes,
) -> Response {
    use axum::http::{HeaderName, HeaderValue};

    const STRIPPED: [&str; 10] = [
        "connection",
        "content-encoding",
        "content-length",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];

    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut headers = HeaderMap::new();
    for (name, value) in upstream_headers.iter() {
        if STRIPPED.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            // append, not insert: insert collapses repeated headers (e.g.
            // multiple set-cookie) to the last value. Matches the streaming
            // proxy path in provider_proxy.rs.
            headers.append(n, v);
        }
    }

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
