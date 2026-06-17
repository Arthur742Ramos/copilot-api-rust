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

/// If `body` is a JSON object that already looks like an error envelope, return
/// it parsed so it can be forwarded to the client verbatim (avoids the
/// double-encoding where a JSON error body is stringified into our `message`).
/// Recognizes both `{"error": {...}}` and `{"type":"error","error":{...}}`.
fn parse_upstream_error_envelope(body: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let obj = value.as_object()?;
    if obj.get("error").map(|e| e.is_object()).unwrap_or(false) {
        Some(value)
    } else {
        None
    }
}

/// Best-effort extraction of a human-readable message from a non-envelope JSON
/// error body (e.g. `{"message": "..."}`), so we don't embed raw JSON in our
/// `message` field. Returns `None` when no string message is found.
fn lift_upstream_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    value
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| value.pointer("/error/message").and_then(|m| m.as_str()))
        .map(|s| s.to_string())
}

/// Map an upstream HTTP status to the Anthropic-recognized `error.type` string,
/// so clients can classify a failure (permanent vs retryable, needs re-auth,
/// etc.) instead of seeing the generic `"error"`. 429/529 are handled separately
/// (rate_limit_error / overloaded_error); this covers everything else.
fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 408 | 409 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        _ => "api_error",
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
                // The upstream error body (when present) is forwarded to the
                // client. Rather than stringifying a JSON error body into our
                // `message` field (which double-encodes it as an escaped string),
                // surface it directly: if the upstream already sent an Anthropic-
                // style error envelope, forward it verbatim; if it's some other
                // JSON, lift a human message out of it; otherwise use the raw text.
                let fallback_message = if e.body.is_empty() {
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
                            "message": fallback_message,
                        }
                    }))
                } else if let Some(envelope) = parse_upstream_error_envelope(&e.body) {
                    // Upstream sent a recognizable error envelope — forward it
                    // verbatim so the client sees the real structured error.
                    Json(envelope)
                } else {
                    // Non-envelope JSON or plain text: synthesize the Anthropic
                    // error envelope with a status-derived `error.type` so clients
                    // can classify the failure (a 401 vs a transient 500), and the
                    // top-level `type: "error"` wrapper matching the rate-limit path.
                    let message = lift_upstream_error_message(&e.body).unwrap_or(fallback_message);
                    Json(json!({
                        "type": "error",
                        "error": {
                            "type": anthropic_error_type(e.status),
                            "message": message,
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
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": e.to_string(),
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
    async fn non_rate_limit_500_renders_api_error_shape() {
        let err = AppError::Http(HttpError::new(
            "boom",
            status(500),
            HeaderMap::new(),
            "internal upstream error".to_string(),
        ));
        let (status, _headers, body) = render(err).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // A 5xx without a recognizable upstream envelope maps to api_error and
        // carries the full Anthropic `{type:"error", error:{...}}` envelope.
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["message"], "internal upstream error");
    }

    #[tokio::test]
    async fn upstream_status_maps_to_anthropic_error_type() {
        // Non-envelope upstream errors derive error.type from the status so
        // clients can classify the failure instead of seeing generic "error".
        for (code, expected) in [
            (400u16, "invalid_request_error"),
            (408, "invalid_request_error"),
            (409, "invalid_request_error"),
            (422, "invalid_request_error"),
            (401, "authentication_error"),
            (403, "permission_error"),
            (404, "not_found_error"),
            (413, "request_too_large"),
        ] {
            let err = AppError::Http(HttpError::new(
                "upstream said no",
                status(code),
                HeaderMap::new(),
                "plain text error".to_string(),
            ));
            let (rstatus, _headers, body) = render(err).await;
            assert_eq!(rstatus.as_u16(), code);
            assert_eq!(body["type"], "error");
            assert_eq!(
                body["error"]["type"], expected,
                "status {code} should map to {expected}"
            );
        }
    }

    #[tokio::test]
    async fn app_error_other_renders_500_api_error() {
        let err = AppError::Other(anyhow::anyhow!("something broke internally"));
        let (status, _headers, body) = render(err).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["message"], "something broke internally");
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
    async fn upstream_json_error_envelope_is_forwarded_verbatim() {
        // A JSON error envelope from upstream must be forwarded as structured
        // JSON, not stringified into our `message` (the double-encoding bug).
        let err = AppError::Http(HttpError::new(
            "Failed",
            status(400),
            HeaderMap::new(),
            r#"{"error":{"type":"invalid_request_error","message":"The requested model is not supported."}}"#.to_string(),
        ));
        let (status, _headers, body) = render(err).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(
            body["error"]["message"],
            "The requested model is not supported."
        );
        // The message must be a string, not escaped JSON.
        assert!(body["error"]["message"].is_string());
    }

    #[tokio::test]
    async fn upstream_non_envelope_json_lifts_message() {
        // A JSON body that isn't an error envelope: lift its message string.
        let err = AppError::Http(HttpError::new(
            "Failed",
            status(500),
            HeaderMap::new(),
            r#"{"message":"upstream exploded","code":42}"#.to_string(),
        ));
        let (_status, _headers, body) = render(err).await;
        assert_eq!(body["error"]["message"], "upstream exploded");
        // 500 with no recognizable envelope -> api_error.
        assert_eq!(body["error"]["type"], "api_error");
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
        let (status, out_headers, body) = render(err).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Only 429/529 forward retry-after; other errors do not.
        assert!(out_headers.get("retry-after").is_none());
        // 403 maps to permission_error.
        assert_eq!(body["error"]["type"], "permission_error");
    }
}
