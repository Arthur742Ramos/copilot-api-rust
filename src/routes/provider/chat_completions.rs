//! Port of routes/provider/chat-completions/handler.ts
//! `handleProviderChatCompletionsForProvider`.
//!
//! Forwards an OpenAI-style `/v1/chat/completions` request to a configured
//! `openai-compatible` upstream provider, applying the provider/model defaults,
//! then either proxies the JSON response back unchanged or raw-forwards the SSE
//! stream (sniffing `usage` for the token recorder).

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::libs::config::ModelConfig;
use crate::libs::error::{http_error_from_response, AppError};
use crate::libs::provider_resolver::resolve_provider_config;
use crate::libs::token_usage::{
    create_provider_token_usage_recorder, normalize_openai_usage, TokenUsageRecorder, UsageTokens,
};
use crate::services::copilot::create_chat_completions::ChatCompletionsPayload;
use crate::services::providers::provider_proxy::forward_provider_chat_completions;

/// Mirrors `handleProviderChatCompletionsForProvider`.
pub async fn handle_provider_chat_completions(
    mut payload: ChatCompletionsPayload,
    provider: String,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let provider_config = resolve_provider_config(&provider).await;
    let provider_config = match provider_config {
        Some(cfg) if cfg.provider_type == "openai-compatible" => cfg,
        _ => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!(
                            "Provider '{provider}' does not support the /v1/chat/completions endpoint"
                        ),
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response());
        }
    };

    let model_config = provider_config
        .models
        .as_ref()
        .and_then(|m| m.get(&payload.model))
        .cloned();

    apply_provider_model_defaults(&mut payload, model_config.as_ref());
    apply_missing_extra_body(&mut payload, model_config.as_ref());
    apply_provider_stream_options(&mut payload);

    let upstream_response =
        forward_provider_chat_completions(&provider_config, &payload, &headers).await?;

    if !upstream_response.status().is_success() {
        tracing::error!(
            "Failed to create provider chat completions: provider={provider} status={}",
            upstream_response.status()
        );
        return Err(http_error_from_response(
            format!("Failed to create {provider} chat completions"),
            upstream_response,
        )
        .await
        .into());
    }

    let recorder = create_provider_token_usage_recorder(
        "chat_completions",
        payload.model.clone(),
        provider,
        None,
    );

    let content_type = upstream_response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let is_streaming_response =
        payload.stream.unwrap_or(false) && content_type.contains("text/event-stream");

    if is_streaming_response {
        return Ok(stream_provider_chat_completions(
            upstream_response,
            recorder,
        ));
    }

    // Non-streaming: clone-equivalent — buffer the JSON, record usage, then
    // forward it back to the client with the proxy header policy.
    let status = upstream_response.status();
    let resp_headers = upstream_response.headers().clone();
    let bytes = upstream_response.bytes().await.map_err(|e| {
        AppError::Other(anyhow::anyhow!(
            "Failed to read provider response body: {e}"
        ))
    })?;

    if let Ok(body_value) = serde_json::from_slice::<Value>(&bytes) {
        recorder.record(normalize_openai_usage(body_value.get("usage")));
    }

    Ok(build_proxy_response_from_parts(
        status,
        &resp_headers,
        bytes,
    ))
}

/// Mirrors `applyProviderModelDefaults`.
fn apply_provider_model_defaults(
    payload: &mut ChatCompletionsPayload,
    model_config: Option<&ModelConfig>,
) {
    set_default_number(
        &mut payload.extra,
        "temperature",
        model_config.and_then(|m| m.temperature),
    );
    set_default_number(
        &mut payload.extra,
        "top_p",
        model_config.and_then(|m| m.top_p),
    );
    set_default_number(
        &mut payload.extra,
        "top_k",
        model_config.and_then(|m| m.top_k),
    );
}

/// `payload[key] ??= value` for an `f64` config default.
fn set_default_number(extra: &mut serde_json::Map<String, Value>, key: &str, value: Option<f64>) {
    if extra.get(key).map(|v| !v.is_null()).unwrap_or(false) {
        return;
    }
    if let Some(value) = value {
        extra.insert(key.to_string(), json!(value));
    }
}

/// Mirrors `applyMissingExtraBody`: copy each `extraBody` key not already
/// present on the payload.
fn apply_missing_extra_body(
    payload: &mut ChatCompletionsPayload,
    model_config: Option<&ModelConfig>,
) {
    let Some(extra_body) = model_config.and_then(|m| m.extra_body.as_ref()) else {
        return;
    };
    for (key, value) in extra_body {
        if payload_has_own(payload, key) {
            continue;
        }
        payload.extra.insert(key.clone(), value.clone());
    }
}

/// Mirrors `Object.hasOwn(payload, key)` for the typed payload (model / messages
/// / max_tokens / stream are real fields, everything else lives in `extra`).
fn payload_has_own(payload: &ChatCompletionsPayload, key: &str) -> bool {
    matches!(key, "model" | "messages" | "max_tokens" | "stream") || payload.extra.contains_key(key)
}

/// Mirrors `applyProviderStreamOptions`.
fn apply_provider_stream_options(payload: &mut ChatCompletionsPayload) {
    if !payload.stream.unwrap_or(false) {
        return;
    }
    let mut stream_options = payload
        .extra
        .get("stream_options")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    stream_options.insert("include_usage".to_string(), Value::Bool(true));
    payload
        .extra
        .insert("stream_options".to_string(), Value::Object(stream_options));
}

/// Mirrors `streamProviderChatCompletions`: raw-forward each upstream SSE event
/// while sniffing the OpenAI `usage` object, then record at stream end.
fn stream_provider_chat_completions(
    upstream: reqwest::Response,
    recorder: TokenUsageRecorder,
) -> Response {
    let event_stream = crate::libs::sse::events(upstream);

    let body = Body::from_stream(async_stream::stream! {
        let mut usage = UsageTokens::default();
        futures_util::pin_mut!(event_stream);

        while let Some(item) = event_stream.next().await {
            let chunk = match item {
                Ok(ev) => ev,
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };

            if !chunk.data.is_empty() && chunk.data != "[DONE]" {
                if let Ok(parsed) = serde_json::from_str::<Value>(&chunk.data) {
                    if let Some(u) = parsed.get("usage") {
                        if !u.is_null() {
                            usage = normalize_openai_usage(Some(u));
                        }
                    }
                }
            }

            let frame = match chunk.event.as_deref() {
                Some(name) => format!("event: {name}\ndata: {}\n\n", chunk.data),
                None => format!("data: {}\n\n", chunk.data),
            };
            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
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

/// Build a pass-through proxy response from buffered parts, mirroring
/// `createProviderProxyResponse`'s header policy (strip hop-by-hop/encoding
/// headers) over an already-read body.
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
            // append, not insert: preserve repeated headers like set-cookie.
            headers.append(n, v);
        }
    }

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
