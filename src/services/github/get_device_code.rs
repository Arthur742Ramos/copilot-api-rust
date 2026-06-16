use serde::Deserialize;

use crate::libs::api_config::{get_oauth_app_config, get_oauth_urls};
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::client;

/// Mirrors services/github/get-device-code.ts. Starts GitHub's OAuth device flow.
pub async fn get_device_code() -> Result<DeviceCodeResponse, HttpError> {
    let app_config = get_oauth_app_config();
    let urls = get_oauth_urls();

    let body = serde_json::json!({
        "client_id": app_config.client_id,
        "scope": app_config.scope,
    });

    let response = client()
        .post(&urls.device_code_url)
        .headers(app_config.headers)
        .body(serde_json::to_vec(&body).unwrap_or_default())
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to get device code: {e}")))?;

    if !response.status().is_success() {
        return Err(http_error_from_response("Failed to get device code", response).await);
    }

    response
        .json::<DeviceCodeResponse>()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to parse device code: {e}")))
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[allow(dead_code)]
    pub expires_in: u64,
    pub interval: u64,
}
