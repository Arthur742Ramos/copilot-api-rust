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
    /// A client-side input error: rendered as HTTP 400 with the Anthropic
    /// `invalid_request_error` type so clients (e.g. Claude Code) treat it as a
    /// permanent failure and do NOT retry, unlike a 500.
    BadRequest(String),
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
            AppError::BadRequest(m) => write!(f, "{m}"),
            AppError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Mirrors `forwardError` in src/lib/error.ts: 429/529s forward retry-after /
/// x-* headers and are reshaped into the Anthropic-recognized rate-limit shape
/// (`{ type: "error", error: { type: "rate_limit_error"|"overloaded_error",
/// message } }`) so the Claude Code SDK's retry/backoff engages; other HTTP
/// errors return the TS-parity `{ error: { message, type } }` with the upstream
/// status; everything else is a 500 with the error message.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Http(e) => {
                tracing::error!("Error occurred: {}", e.message);
                let is_rate_limit = e.status == StatusCode::TOO_MANY_REQUESTS;
                // 529 (overloaded) is not a named StatusCode constant.
                let is_overloaded = e.status.as_u16() == 529;
                let mut out_headers = HeaderMap::new();
                if is_rate_limit || is_overloaded {
                    // Forward Copilot's retry-after (so the SDK backs off the
                    // right amount) plus any x-ratelimit-* headers. We do NOT
                    // synthesize a retry-after if absent — Anthropic clients
                    // apply their own default backoff in that case.
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
                let body = if is_rate_limit || is_overloaded {
                    // Reshape rate-limit/overload errors into the Anthropic shape
                    // the Claude Code SDK recognizes as retryable: top-level
                    // `type: "error"` with a nested `error.type` of
                    // `rate_limit_error` (429) or `overloaded_error` (529).
                    let error_type = if is_overloaded {
                        "overloaded_error"
                    } else {
                        "rate_limit_error"
                    };
                    Json(json!({
                        "type": "error",
                        "error": {
                            "type": error_type,
                            "message": message,
                        }
                    }))
                } else {
                    // Non-rate-limit HTTP errors keep the existing TS-parity shape.
                    Json(json!({
                        "error": {
                            "message": message,
                            "type": "error",
                        }
                    }))
                };
                (e.status, out_headers, body).into_response()
            }
            AppError::BadRequest(message) => {
                tracing::warn!("Bad request: {message}");
                let body = Json(json!({
                    "error": {
                        "message": message,
                        "type": "invalid_request_error",
                    }
                }));
                (StatusCode::BAD_REQUEST, body).into_response()
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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn render(err: AppError) -> (StatusCode, HeaderMap, serde_json::Value) {
        let resp = err.into_response();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("body is json");
        (status, headers, json)
    }

    fn status(code: u16) -> StatusCode {
        StatusCode::from_u16(code).unwrap()
    }

    #[tokio::test]
    async fn rate_limit_429_renders_anthropic_shape() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert(
            "x-usage-ratelimit-session",
            "rem=0&rst=123".parse().unwrap(),
        );
        let err = AppError::Http(HttpError::new(
            "Failed to create messages",
            status(429),
            headers,
            r#"{"message":"rate limited by copilot"}"#.to_string(),
        ));
        let (status, out_headers, body) = render(err).await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(
            body["error"]["message"],
            r#"{"message":"rate limited by copilot"}"#
        );
        // retry-after and x-* headers are forwarded so the SDK backs off.
        assert_eq!(out_headers.get("retry-after").unwrap(), "30");
        assert_eq!(
            out_headers.get("x-usage-ratelimit-session").unwrap(),
            "rem=0&rst=123"
        );
    }

    #[tokio::test]
    async fn overloaded_529_renders_overloaded_error() {
        let err = AppError::Http(HttpError::new(
            "Overloaded",
            status(529),
            HeaderMap::new(),
            "upstream overloaded".to_string(),
        ));
        let (status, _headers, body) = render(err).await;

        assert_eq!(status.as_u16(), 529);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "overloaded_error");
        assert_eq!(body["error"]["message"], "upstream overloaded");
    }

    #[tokio::test]
    async fn rate_limit_empty_body_uses_message() {
        let err = AppError::Http(HttpError::new(
            "quota exhausted",
            status(429),
            HeaderMap::new(),
            String::new(),
        ));
        let (_status, _headers, body) = render(err).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "quota exhausted");
    }

    #[tokio::test]
    async fn non_rate_limit_500_renders_generic_shape() {
        let err = AppError::Http(HttpError::new(
            "boom",
            status(500),
            HeaderMap::new(),
            "internal upstream error".to_string(),
        ));
        let (status, _headers, body) = render(err).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // Unchanged TS-parity shape: outer `error` object, type "error".
        assert_eq!(body["error"]["type"], "error");
        assert_eq!(body["error"]["message"], "internal upstream error");
        assert!(body.get("type").is_none());
    }

    #[tokio::test]
    async fn bad_request_renders_400_invalid_request_error() {
        let err =
            AppError::BadRequest("Invalid request payload: missing field `model`".to_string());
        let (status, _headers, body) = render(err).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(
            body["error"]["message"],
            "Invalid request payload: missing field `model`"
        );
    }

    #[tokio::test]
    async fn non_rate_limit_does_not_forward_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        let err = AppError::Http(HttpError::new(
            "forbidden",
            status(403),
            headers,
            "nope".to_string(),
        ));
        let (status, out_headers, _body) = render(err).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Only 429/529 forward retry-after; other errors keep TS parity.
        assert!(out_headers.get("retry-after").is_none());
    }
}
