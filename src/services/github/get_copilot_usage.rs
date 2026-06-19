use serde::{Deserialize, Serialize};

use crate::libs::api_config::{get_github_api_base_url, github_headers};
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::client;
use crate::libs::state::State;
use crate::services::copilot::create_responses::null_to_default;

/// Coarse account tier derived from the Copilot plan name. Distinct from
/// `State.account_type` (a plain string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotAccountType {
    Individual,
    Business,
    Enterprise,
}

impl CopilotAccountType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CopilotAccountType::Individual => "individual",
            CopilotAccountType::Business => "business",
            CopilotAccountType::Enterprise => "enterprise",
        }
    }
}

/// Mirrors services/github/get-copilot-usage.ts `getCopilotUsage`.
pub async fn get_copilot_usage(
    state: &State,
    github_token: Option<&str>,
) -> Result<CopilotUsageResponse, HttpError> {
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
        .get(format!(
            "{}/copilot_internal/user",
            get_github_api_base_url()
        ))
        .headers(github_headers(&auth_state))
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to get Copilot usage: {e}")))?;

    if !response.status().is_success() {
        return Err(http_error_from_response("Failed to get Copilot usage", response).await);
    }

    response
        .json::<CopilotUsageResponse>()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to parse Copilot usage: {e}")))
}

/// Mirrors `getCopilotAccountType`. Substring match, enterprise before business.
pub async fn get_copilot_account_type(
    state: &State,
    github_token: Option<&str>,
) -> Result<CopilotAccountType, HttpError> {
    let usage = get_copilot_usage(state, github_token).await?;
    let plan = usage.copilot_plan.unwrap_or_default().to_lowercase();
    if plan.contains("enterprise") {
        Ok(CopilotAccountType::Enterprise)
    } else if plan.contains("business") {
        Ok(CopilotAccountType::Business)
    } else {
        Ok(CopilotAccountType::Individual)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaDetail {
    // `#[serde(default)]` only covers an *absent* key; an explicit JSON `null`
    // is a present value and would fail (`invalid type: null, expected f64`),
    // 500-ing the whole /usage response. `null_to_default` coerces null to the
    // default so plan/account variation degrades gracefully (cf. commit d28a472).
    #[serde(default, deserialize_with = "null_to_default")]
    pub entitlement: f64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub overage_count: f64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub overage_permitted: bool,
    #[serde(default, deserialize_with = "null_to_default")]
    pub percent_remaining: f64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub quota_id: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub quota_remaining: f64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub remaining: f64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub unlimited: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaSnapshots {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<QuotaDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completions: Option<QuotaDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premium_interactions: Option<QuotaDetail>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Mirrors get-copilot-usage.ts's `CopilotUsageResponse`. The TS code uses a
/// runtime-no-op `as` cast, so the `/usage` route forwards whatever GitHub
/// returns. To match that — and avoid a 500 on plan/account variation — only
/// the fields our own code reads are modeled; every other upstream field flows
/// through `extra` and is re-serialized verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotUsageResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copilot_plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_reset_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_snapshots: Option<QuotaSnapshots>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Endpoints>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_based_billing: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
