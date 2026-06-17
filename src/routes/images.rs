//! `/v1/images/generations` endpoint.
//!
//! OpenAI-compatible image generation. GitHub Copilot has no image backend, so
//! this forwards to the Codex Responses transport's native `image_generation`
//! tool using the stored Codex ("Sign in with ChatGPT") OAuth credentials. The
//! request/response shapes mirror the OpenAI Images API so standard OpenAI SDK
//! clients (`images.generate(model=..., prompt=...)`) work unchanged.

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::libs::error::AppError;
use crate::libs::provider_resolver::resolve_provider_config;
use crate::routes::parse_json_body;
use crate::services::codex::create_image::{create_codex_image, ImageGenerationRequest};

/// POST /images/generations — generate image(s) via the Codex transport.
pub async fn post_images(headers: axum::http::HeaderMap, body: Bytes) -> Response {
    let payload = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    match handle(payload, headers).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => AppError::into_response(error),
    }
}

async fn handle(body: Value, headers: axum::http::HeaderMap) -> Result<Value, AppError> {
    let req = ImageGenerationRequest::from_value(&body)?;

    // Ensure the Codex OAuth token is loaded/refreshed into global state before
    // forwarding (no-op if the refresh loop already populated it). A missing
    // Codex login resolves to `None` -> a clear 400 rather than an opaque 401.
    if resolve_provider_config("codex").await.is_none() {
        return Err(AppError::BadRequest(
            "Image generation requires Codex (Sign in with ChatGPT) credentials. \
             Run `copilot-api auth --provider codex` first."
                .to_string(),
        ));
    }

    let images = create_codex_image(&req, &headers).await?;

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let data: Vec<Value> = images
        .into_iter()
        .map(|b64| json!({ "b64_json": b64 }))
        .collect();

    Ok(json!({
        "created": created,
        "data": data,
    }))
}
