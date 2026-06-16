use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::libs::api_config::{copilot_base_url, copilot_headers};
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::client;
use crate::libs::state;

/// Mirrors services/copilot/create-embeddings.ts. The upstream response is
/// forwarded verbatim (TS does a runtime-no-op `as` cast), so we return the raw
/// JSON value rather than a reduced struct.
pub async fn create_embeddings(payload: &EmbeddingRequest) -> Result<Value, HttpError> {
    let st = state::snapshot();
    if st.copilot_token.as_deref().unwrap_or("").is_empty() {
        return Err(HttpError::internal("Copilot token not found"));
    }

    let base = copilot_base_url(&st);
    let response = client()
        .post(format!("{base}/embeddings"))
        .headers(copilot_headers(&st, None, false))
        .json(payload)
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to create embeddings: {e}")))?;

    if !response.status().is_success() {
        return Err(http_error_from_response("Failed to create embeddings", response).await);
    }

    response
        .json::<Value>()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to parse embeddings: {e}")))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub input: serde_json::Value,
    pub model: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
