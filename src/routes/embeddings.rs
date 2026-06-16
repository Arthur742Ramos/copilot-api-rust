use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::libs::error::AppError;
use crate::libs::token_usage::{create_copilot_token_usage_recorder, UsageTokens};
use crate::services::copilot::create_embeddings::{create_embeddings, EmbeddingRequest};

/// POST /embeddings — mirrors routes/embeddings/route.ts.
pub async fn post_embeddings(body: Json<Value>) -> Response {
    match handle(body.0).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => AppError::into_response(error),
    }
}

async fn handle(body: Value) -> Result<Value, AppError> {
    let payload: EmbeddingRequest = serde_json::from_value(body)
        .map_err(|e| AppError::Other(anyhow::anyhow!("Invalid embeddings request: {e}")))?;

    let response = create_embeddings(&payload).await?;

    let prompt_tokens = response
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let recorder = create_copilot_token_usage_recorder("embeddings", payload.model.clone(), None);
    recorder.record(UsageTokens {
        input_tokens: Some(prompt_tokens),
        output_tokens: Some(0),
        ..Default::default()
    });

    Ok(response)
}
