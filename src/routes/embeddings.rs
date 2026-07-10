//! `/v1/embeddings` endpoint: forwards embedding requests to the Copilot API.

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::libs::error::AppError;
use crate::libs::token_usage::{create_copilot_token_usage_recorder, UsageTokens};
use crate::routes::parse_json_body;
use crate::services::copilot::create_embeddings::{create_embeddings, EmbeddingRequest};

/// POST /embeddings — mirrors routes/embeddings/route.ts.
pub async fn post_embeddings(body: Bytes) -> Response {
    let payload = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    match handle(payload).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => AppError::into_response(error),
    }
}

async fn handle(body: Value) -> Result<Value, AppError> {
    let mut payload: EmbeddingRequest = serde_json::from_value(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid embeddings request: {e}")))?;

    // The OpenAI embeddings contract accepts a bare string OR an array for
    // `input`, but Copilot's upstream rejects a bare string. Normalize a string
    // into a single-element array so standard OpenAI clients work unchanged;
    // arrays (of strings or token-id arrays) pass through untouched.
    normalize_embedding_input(&mut payload.input);

    crate::libs::admission::check_shared_admission().await?;

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

/// Wrap a bare-string `input` into a single-element array. Leaves arrays and any
/// other shape untouched.
fn normalize_embedding_input(input: &mut Value) {
    if input.is_string() {
        *input = Value::Array(vec![input.take()]);
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_embedding_input;
    use serde_json::json;

    #[test]
    fn string_input_is_wrapped_in_array() {
        let mut input = json!("hello world");
        normalize_embedding_input(&mut input);
        assert_eq!(input, json!(["hello world"]));
    }

    #[test]
    fn array_input_is_unchanged() {
        let mut input = json!(["a", "b"]);
        normalize_embedding_input(&mut input);
        assert_eq!(input, json!(["a", "b"]));
        // Token-id arrays (arrays of arrays) also pass through.
        let mut tokens = json!([[1, 2, 3]]);
        normalize_embedding_input(&mut tokens);
        assert_eq!(tokens, json!([[1, 2, 3]]));
    }
}
