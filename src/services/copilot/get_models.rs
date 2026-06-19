use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::libs::api_config::{copilot_base_url, copilot_models_headers};
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::{client, read_json_capped, retry_endpoint, send_with_retry, RetryPolicy};
use crate::libs::metrics::{record_upstream_request, UpstreamStatus};
use crate::libs::state;

pub async fn get_models() -> Result<ModelsResponse, HttpError> {
    let st = state::snapshot();
    let base = copilot_base_url(&st);
    tracing::info!("Fetching models from {base}/models");
    let headers = copilot_models_headers(&st);
    let upstream_start = std::time::Instant::now();
    let request = client().get(format!("{base}/models")).headers(headers);
    let response = send_with_retry(request, retry_endpoint::MODELS, RetryPolicy::from_env())
        .await
        .map_err(|e| {
            record_upstream_request(
                retry_endpoint::MODELS,
                UpstreamStatus::TransportError,
                upstream_start.elapsed().as_secs_f64(),
            );
            HttpError::internal(format!("Failed to get models: {e}"))
        })?;
    record_upstream_request(
        retry_endpoint::MODELS,
        UpstreamStatus::from_code(response.status().as_u16()),
        upstream_start.elapsed().as_secs_f64(),
    );

    if !response.status().is_success() {
        let err = http_error_from_response("Failed to get models", response).await;
        tracing::error!("Failed to get models response body {}", err.body);
        return Err(err);
    }

    read_json_capped::<ModelsResponse>(response)
        .await
        .map_err(|e| HttpError::internal(format!("Failed to parse models: {e}")))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub data: Vec<Model>,
    pub object: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelVisionLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_prompt_image_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_prompt_images: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported_media_types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context_window_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_prompt_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_inputs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<ModelVisionLimits>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelSupports {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_thinking_budget: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_thinking_budget: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_outputs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub limits: ModelLimits,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub supports: ModelSupports,
    #[serde(default)]
    pub tokenizer: String,
    #[serde(rename = "type", default)]
    pub model_type: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelPolicy {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub terms: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Mirrors the Copilot `/models` model shape. TS forwards models verbatim (a
/// runtime-no-op `as` cast), so every field is lenient (`#[serde(default)]`,
/// keeping a malformed model from failing the whole list) and unknown upstream
/// fields (e.g. `billing`, `is_chat_default`) flow through `extra`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Model {
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model_picker_enabled: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub preview: bool,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ModelPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_endpoints: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
