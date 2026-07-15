//! Native OpenAI-compatible Images API proxy.
//!
//! HTTP callers are forwarded to the provider's Images endpoint without losing
//! unknown JSON fields or multipart bytes. The separate MCP image tool continues
//! to use the Responses `image_generation` tool because it needs to materialize
//! image bytes into a local file.

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Path};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::Value;

use crate::libs::config::get_image_model;
use crate::libs::error::{AppError, HttpError};
use crate::libs::provider_resolver::resolve_provider_config;
use crate::libs::token_usage::{create_provider_token_usage_recorder, normalize_responses_usage};
use crate::services::codex::images::forward_codex_images;
use crate::services::providers::provider_proxy::{
    forward_provider_images, provider_proxy_response_parts, ImagesOperation,
};

pub async fn post_images_generations(
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    render(handle(None, ImagesOperation::Generations, uri, headers, body).await)
}

pub async fn post_images_edits(
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    render(handle(None, ImagesOperation::Edits, uri, headers, body).await)
}

pub async fn post_provider_images_generations(
    Path(provider): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    render(
        handle(
            Some(provider),
            ImagesOperation::Generations,
            uri,
            headers,
            body,
        )
        .await,
    )
}

pub async fn post_provider_images_edits(
    Path(provider): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    render(handle(Some(provider), ImagesOperation::Edits, uri, headers, body).await)
}

fn render(result: Result<Response, AppError>) -> Response {
    match result {
        Ok(response) => response,
        Err(error) => error.into_openai_response(),
    }
}

async fn handle(
    provider: Option<String>,
    operation: ImagesOperation,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    crate::libs::admission::check_shared_admission()
        .await
        .map_err(AppError::Http)?;
    crate::libs::premium_interactions::check_premium_interactions()?;

    let provider_name = provider.as_deref().unwrap_or("codex").trim().to_string();
    let provider_config = resolve_provider_config(&provider_name)
        .await
        .ok_or_else(|| provider_not_found(&provider_name))?;

    let query = uri.query();
    let (upstream, model) = if provider_config.name == "codex" {
        let (body, model) = prepare_codex_body(body, operation)?;
        let upstream = forward_codex_images(
            body,
            &headers,
            &provider_config.base_url,
            &provider_config.api_key,
            operation,
            query,
        )
        .await?;
        (upstream, model)
    } else {
        let model = request_model(&body).unwrap_or_else(|| "images".to_string());
        let upstream =
            forward_provider_images(&provider_config, body, &headers, operation, query).await?;
        (upstream, model)
    };

    Ok(proxy_images_response(upstream, provider_config.name, model))
}

fn provider_not_found(provider: &str) -> AppError {
    AppError::Http(HttpError::new(
        format!("Provider '{provider}' not found or disabled"),
        StatusCode::NOT_FOUND,
        HeaderMap::new(),
        String::new(),
    ))
}

#[allow(clippy::result_large_err)]
fn prepare_codex_body(
    body: Bytes,
    operation: ImagesOperation,
) -> Result<(Bytes, String), AppError> {
    if operation == ImagesOperation::Edits {
        return Ok((body, get_image_model()));
    }

    let mut value: Value = serde_json::from_slice(&body)
        .map_err(|error| AppError::BadRequest(format!("Invalid JSON: {error}")))?;
    let object = value.as_object_mut().ok_or_else(|| {
        AppError::BadRequest("Image generation request must be a JSON object".to_string())
    })?;
    object
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(
                "prompt: field required and must be a non-empty string".to_string(),
            )
        })?;

    let default_model = get_image_model();
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(&default_model)
        .to_string();
    if object
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(|model| model.trim().is_empty())
    {
        object.insert("model".to_string(), Value::String(default_model));
    }

    let bytes = serde_json::to_vec(&value).map_err(|error| {
        AppError::Other(anyhow::anyhow!(
            "Failed to serialize image generation request: {error}"
        ))
    })?;
    Ok((Bytes::from(bytes), model))
}

fn request_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
}

fn proxy_images_response(upstream: reqwest::Response, provider: String, model: String) -> Response {
    let (status, headers) = provider_proxy_response_parts(&upstream);
    let should_record = status.is_success();
    let recorder = create_provider_token_usage_recorder("images", model, provider.clone(), None);
    let mut stream = upstream.bytes_stream();

    let body = Body::from_stream(async_stream::stream! {
        let mut captured = Some(Vec::new());
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    if let Some(buffer) = captured.as_mut() {
                        if buffer.len() + chunk.len()
                            <= crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
                        {
                            buffer.extend_from_slice(&chunk);
                        } else {
                            captured = None;
                            tracing::warn!(
                                provider,
                                "Image response exceeded the usage-inspection cap; forwarding without local usage recording"
                            );
                        }
                    }
                    yield Ok::<Bytes, reqwest::Error>(chunk);
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }

        if should_record {
            if let Some(buffer) = captured {
                match serde_json::from_slice::<Value>(&buffer) {
                    Ok(value) => recorder.record(normalize_responses_usage(value.get("usage"))),
                    Err(error) => tracing::debug!(
                        provider,
                        %error,
                        "Image response was not JSON; usage was not recorded"
                    ),
                }
            }
        }
    });

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn generation_defaults_model_without_dropping_unknown_fields() {
        let (body, model) = prepare_codex_body(
            Bytes::from(
                json!({
                    "prompt": "A robot watering a plant",
                    "quality": "high",
                    "future_option": {"enabled": true}
                })
                .to_string(),
            ),
            ImagesOperation::Generations,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(model, get_image_model());
        assert_eq!(value["model"], get_image_model());
        assert_eq!(value["future_option"]["enabled"], true);
    }

    #[test]
    fn generation_preserves_explicit_model() {
        let (body, model) = prepare_codex_body(
            Bytes::from(r#"{"model":"gpt-image-custom","prompt":"A cat"}"#),
            ImagesOperation::Generations,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(model, "gpt-image-custom");
        assert_eq!(value["model"], "gpt-image-custom");
    }

    #[test]
    fn generation_rejects_missing_prompt_but_edits_remain_opaque() {
        assert!(prepare_codex_body(
            Bytes::from_static(br#"{"model":"gpt-image-2"}"#),
            ImagesOperation::Generations
        )
        .is_err());

        let multipart = Bytes::from_static(b"--boundary\r\nbinary\x00bytes\r\n--boundary--");
        let (forwarded, _) = prepare_codex_body(multipart.clone(), ImagesOperation::Edits).unwrap();
        assert_eq!(forwarded, multipart);
    }
}
