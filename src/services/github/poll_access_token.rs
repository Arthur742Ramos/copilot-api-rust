use std::time::{Duration, Instant};

use serde::Deserialize;

use super::get_device_code::DeviceCodeResponse;
use crate::libs::api_config::{get_oauth_app_config, get_oauth_urls};
use crate::libs::error::HttpError;
use crate::libs::http::client;

/// Mirrors services/github/poll-access-token.ts. Polls GitHub's access-token
/// endpoint on the device-code interval until the user authorizes, then returns
/// the access token string. Bounded by the device code's `expires_in`: if the
/// user does not authorize before the code expires, returns an error instead of
/// looping forever, so a missed/ignored prompt fails fast with guidance rather
/// than hanging the process indefinitely.
pub async fn poll_access_token(device_code: &DeviceCodeResponse) -> Result<String, HttpError> {
    let app_config = get_oauth_app_config();
    let urls = get_oauth_urls();

    // Interval is seconds; +1 second of safety margin, then to milliseconds.
    let sleep_duration = Duration::from_millis((device_code.interval + 1) * 1000);
    // GitHub's verification code expires after `expires_in` seconds; stop polling
    // once it elapses so we never wait on a code the user can no longer redeem.
    let deadline = Instant::now() + Duration::from_secs(device_code.expires_in);
    tracing::debug!(
        "Polling access token with interval of {}ms (expires in {}s)",
        sleep_duration.as_millis(),
        device_code.expires_in
    );

    let body = serde_json::json!({
        "client_id": app_config.client_id,
        "device_code": device_code.device_code,
        "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
    });
    let body_bytes = serde_json::to_vec(&body).unwrap_or_default();

    loop {
        if Instant::now() >= deadline {
            // A user-visible timeout, not an internal failure — use 408 so the
            // CLI error line doesn't read as a server bug ("status 500").
            return Err(HttpError::new(
                format!(
                    "Device code expired after {}s without authorization. \
                     Re-run the command and complete the GitHub login prompt.",
                    device_code.expires_in
                ),
                axum::http::StatusCode::REQUEST_TIMEOUT,
                axum::http::HeaderMap::new(),
                String::new(),
            ));
        }

        let response = client()
            .post(&urls.access_token_url)
            .headers(app_config.headers.clone())
            .body(body_bytes.clone())
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to poll access token: {e}");
                tokio::time::sleep(sleep_duration).await;
                continue;
            }
        };

        if !response.status().is_success() {
            let text = response.text().await.unwrap_or_default();
            tracing::error!("Failed to poll access token: {text}");
            tokio::time::sleep(sleep_duration).await;
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
            Some(token) if !token.is_empty() => return Ok(token),
            _ => {
                // Heartbeat at info level so non-verbose users see progress while
                // GitHub reports the authorization is still pending.
                tracing::info!("Still waiting for GitHub authorization...");
                tokio::time::sleep(sleep_duration).await;
            }
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
