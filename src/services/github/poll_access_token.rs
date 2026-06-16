use std::time::Duration;

use serde::Deserialize;

use super::get_device_code::DeviceCodeResponse;
use crate::libs::api_config::{get_oauth_app_config, get_oauth_urls};
use crate::libs::http::client;

/// Mirrors services/github/poll-access-token.ts. Polls GitHub's access-token
/// endpoint on the device-code interval until the user authorizes, then returns
/// the access token string. Loops forever on transient errors / pending state.
pub async fn poll_access_token(device_code: &DeviceCodeResponse) -> String {
    let app_config = get_oauth_app_config();
    let urls = get_oauth_urls();

    // Interval is seconds; +1 second of safety margin, then to milliseconds.
    let sleep_duration = Duration::from_millis((device_code.interval + 1) * 1000);
    tracing::debug!("Polling access token with interval of {}ms", sleep_duration.as_millis());

    let body = serde_json::json!({
        "client_id": app_config.client_id,
        "device_code": device_code.device_code,
        "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
    });
    let body_bytes = serde_json::to_vec(&body).unwrap_or_default();

    loop {
        let response = client()
            .post(&urls.access_token_url)
            .headers(app_config.headers.clone())
            .body(body_bytes.clone())
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                tokio::time::sleep(sleep_duration).await;
                tracing::error!("Failed to poll access token: {e}");
                continue;
            }
        };

        if !response.status().is_success() {
            tokio::time::sleep(sleep_duration).await;
            let text = response.text().await.unwrap_or_default();
            tracing::error!("Failed to poll access token: {text}");
            continue;
        }

        let json: AccessTokenResponse = match response.json().await {
            Ok(j) => j,
            Err(e) => {
                tracing::debug!("Polling access token parse error: {e}");
                tokio::time::sleep(sleep_duration).await;
                continue;
            }
        };

        match json.access_token {
            Some(token) if !token.is_empty() => return token,
            _ => tokio::time::sleep(sleep_duration).await,
        }
    }
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    token_type: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    scope: Option<String>,
}
