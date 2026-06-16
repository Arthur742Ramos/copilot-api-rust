//! Error types and Axum responses: `AppError` for internal failures and
//! `HttpError` for forwarding an upstream response's status/headers/body.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Mirrors the TS `HTTPError` class: wraps an upstream response so the status,
/// headers, and body can be forwarded to the client. We capture the parts we
/// need eagerly because reqwest responses are not cloneable.
#[derive(Debug)]
pub struct HttpError {
    pub message: String,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl HttpError {
    pub fn new(
        message: impl Into<String>,
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    ) -> Self {
        HttpError {
            message: message.into(),
            status,
            headers,
            body,
        }
    }

    /// A synthetic 500 with no upstream headers/body — used when a request fails
    /// before we have a response (network errors, JSON parse failures).
    pub fn internal(message: impl Into<String>) -> Self {
        HttpError::new(
            message,
            StatusCode::INTERNAL_SERVER_ERROR,
            HeaderMap::new(),
            String::new(),
        )
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (status {})", self.message, self.status)
    }
}

impl std::error::Error for HttpError {}

/// Build an HttpError from a non-OK reqwest response, consuming its body.
pub async fn http_error_from_response(
    message: impl Into<String>,
    response: reqwest::Response,
) -> HttpError {
    let status = StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers().iter() {
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::from_bytes(name.as_str().as_bytes()),
            axum::http::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.insert(n, v);
        }
    }
    let body = response.text().await.unwrap_or_default();
    HttpError::new(message, status, headers, body)
}

/// The crate-wide error type that route handlers return. Mirrors `forwardError`.
#[derive(Debug)]
pub enum AppError {
    Http(HttpError),
    Other(anyhow::Error),
}

impl From<HttpError> for AppError {
    fn from(e: HttpError) -> Self {
        AppError::Http(e)
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Other(e)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Other(anyhow::Error::new(e))
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::Other(anyhow::Error::new(e))
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Http(e) => write!(f, "{e}"),
            AppError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Mirrors `forwardError` in src/lib/error.ts: 429s forward retry-after / x-*
/// headers; HTTP errors return `{ error: { message, type } }` with the upstream
/// status; everything else is a 500 with the error message.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Http(e) => {
                tracing::error!("Error occurred: {}", e.message);
                let mut out_headers = HeaderMap::new();
                if e.status == StatusCode::TOO_MANY_REQUESTS {
                    for (name, value) in e.headers.iter() {
                        let lower = name.as_str().to_lowercase();
                        if lower == "retry-after" || lower.starts_with("x-") {
                            out_headers.insert(name.clone(), value.clone());
                        }
                    }
                }
                // TS forwardError renders an upstream HTTPError from its response
                // body text, and a synthetic Error from its `.message`. Our
                // synthetic errors (HttpError::internal) carry an empty body, so
                // fall back to the message text in that case.
                let message = if e.body.is_empty() {
                    e.message.clone()
                } else {
                    e.body.clone()
                };
                let body = Json(json!({
                    "error": {
                        "message": message,
                        "type": "error",
                    }
                }));
                (e.status, out_headers, body).into_response()
            }
            AppError::Other(e) => {
                tracing::error!("Error occurred: {}", e);
                let body = Json(json!({
                    "error": {
                        "message": e.to_string(),
                        "type": "error",
                    }
                }));
                (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
            }
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;
