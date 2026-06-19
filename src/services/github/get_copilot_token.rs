use serde::Deserialize;

use crate::libs::api_config::{get_github_api_base_url, github_headers};
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::client;
use crate::libs::state::State;
use crate::services::copilot::create_responses::null_to_default;

/// Mirrors services/github/get-copilot-token.ts. Exchanges the stored GitHub
/// token for a short-lived Copilot API token. Uses the global state directly.
pub async fn get_copilot_token(state: &State) -> Result<GetCopilotTokenResponse, HttpError> {
    let response = client()
        .get(format!(
            "{}/copilot_internal/v2/token",
            get_github_api_base_url()
        ))
        .headers(github_headers(state))
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to get Copilot token: {e}")))?;

    if !response.status().is_success() {
        let err = http_error_from_response("Failed to get Copilot token", response).await;
        tracing::error!("Failed to get Copilot token response body {}", err.body);
        return Err(err);
    }

    response
        .json::<GetCopilotTokenResponse>()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to parse Copilot token: {e}")))
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetCopilotTokenResponse {
    // Coerce explicit JSON null to default (see null_to_default): `#[serde(default)]`
    // alone fails on a present null, which would 500 the token fetch.
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "null_to_default")]
    pub expires_at: i64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub refresh_in: i64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub token: String,
}
