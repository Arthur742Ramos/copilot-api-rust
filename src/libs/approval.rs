use axum::http::{HeaderMap, StatusCode};

use crate::libs::error::HttpError;

/// Mirrors src/lib/approval.ts. When `--manual` approval is enabled, the proxy
/// blocks each incoming request on an interactive y/N confirmation. Rejecting
/// raises a 403 that the route layer forwards to the client.
pub async fn await_approval() -> Result<(), HttpError> {
    let approved = tokio::task::spawn_blocking(|| {
        use std::io::{self, Write};
        print!("Accept incoming request? (y/N) ");
        let _ = io::stdout().flush();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return false;
        }
        matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
    })
    .await
    .unwrap_or(false);

    if !approved {
        let body = serde_json::json!({ "message": "Request rejected" }).to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        return Err(HttpError::new(
            "Request rejected",
            StatusCode::FORBIDDEN,
            headers,
            body,
        ));
    }
    Ok(())
}
