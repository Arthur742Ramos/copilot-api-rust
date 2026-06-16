use serde::Deserialize;

use crate::libs::api_config::{get_github_api_base_url, github_user_headers};
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::client;
use crate::libs::state::State;

/// Mirrors services/github/get-user.ts. Fetches the authenticated GitHub user's
/// login. `github_token` overrides `state.github_token` (nullish coalescing).
pub async fn get_github_user(
    state: &State,
    github_token: Option<&str>,
) -> Result<GithubUserResponse, HttpError> {
    let resolved = github_token
        .map(|s| s.to_string())
        .or_else(|| state.github_token.clone());
    let resolved = match resolved {
        Some(t) if !t.is_empty() => t,
        _ => return Err(HttpError::internal("GitHub token not found")),
    };

    let mut auth_state = state.clone();
    auth_state.github_token = Some(resolved);

    let response = client()
        .get(format!("{}/user", get_github_api_base_url()))
        .headers(github_user_headers(&auth_state))
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to get GitHub user: {e}")))?;

    if !response.status().is_success() {
        return Err(http_error_from_response("Failed to get GitHub user", response).await);
    }

    response
        .json::<GithubUserResponse>()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to parse GitHub user: {e}")))
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubUserResponse {
    #[serde(default)]
    pub login: String,
}
