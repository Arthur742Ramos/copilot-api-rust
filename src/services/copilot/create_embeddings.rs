use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::libs::api_config::{copilot_base_url, copilot_headers};
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::{client, read_json_capped, retry_endpoint, send_with_retry, RetryPolicy};
use crate::libs::metrics::{record_upstream_request, UpstreamStatus};
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
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    let upstream_start = std::time::Instant::now();
    let request = client()
        .post(format!("{base}/embeddings"))
        .headers(copilot_headers(&st, None, false))
        .body(body);
    let response = send_with_retry(request, retry_endpoint::EMBEDDINGS, RetryPolicy::from_env())
        .await
        .map_err(|e| {
            record_upstream_request(
                retry_endpoint::EMBEDDINGS,
                UpstreamStatus::TransportError,
                upstream_start.elapsed().as_secs_f64(),
            );
            HttpError::internal(format!("Failed to create embeddings: {e}"))
        })?;
    record_upstream_request(
        retry_endpoint::EMBEDDINGS,
        UpstreamStatus::from_code(response.status().as_u16()),
        upstream_start.elapsed().as_secs_f64(),
    );

    if !response.status().is_success() {
        return Err(http_error_from_response("Failed to create embeddings", response).await);
    }

    read_json_capped::<Value>(response)
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
