//! Error types and Axum responses: `AppError` for internal failures and
//! `HttpError` for forwarding an upstream response's status/headers/body.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// Mirrors the TS `HTTPError` class: wraps an upstream response so the status,
/// headers, and body can be forwarded to the client. We capture the parts we
/// need eagerly because reqwest responses are not cloneable.
#[derive(Debug)]
pub struct HttpError {
    pub message: String,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
    /// Whether `message` is safe to expose when there is no upstream response
    /// body. Synthetic transport/parse failures keep their detailed diagnostic
    /// in logs but return only an opaque reference to clients.
    pub expose_message: bool,
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
            expose_message: true,
        }
    }

    /// A synthetic 500 with no upstream headers/body — used when a request fails
    /// before we have a response (network errors, JSON parse failures).
    pub fn internal(message: impl Into<String>) -> Self {
        let mut error = HttpError::new(
            message,
            StatusCode::INTERNAL_SERVER_ERROR,
            HeaderMap::new(),
            String::new(),
        );
        error.expose_message = false;
        error
    }

    /// A sanitized synthetic failure for a successful upstream HTTP response
    /// whose body violates the selected protocol contract.
    pub fn bad_gateway(message: impl Into<String>) -> Self {
        HttpError::new(
            message,
            StatusCode::BAD_GATEWAY,
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

/// Render a locally generated failure in the complete Anthropic error envelope.
///
/// Use this helper for local route and middleware failures that do not have an
/// upstream response to normalize. Anthropic SDKs require the top-level
/// `type: "error"` discriminator in addition to the nested error type.
pub fn anthropic_error_response(
    status: StatusCode,
    error_type: impl Into<String>,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": {
                "type": error_type.into(),
                "message": message.into(),
            }
        })),
    )
        .into_response()
}

/// Whether `path` belongs to an OpenAI-native public API surface.
///
/// Messages endpoints deliberately keep Anthropic envelopes. Responses and Chat
/// Completions must not leak those envelopes into Codex/OpenAI clients, including
/// middleware failures that occur before a route handler runs.
pub fn is_openai_native_path(path: &str) -> bool {
    matches!(
        path,
        "/responses"
            | "/v1/responses"
            | "/responses/compact"
            | "/v1/responses/compact"
            | "/chat/completions"
            | "/v1/chat/completions"
            | "/models"
            | "/v1/models"
    ) || path.starts_with("/responses/")
        || path.starts_with("/v1/responses/")
        || path.starts_with("/chat/completions/")
        || path.starts_with("/v1/chat/completions/")
        || path.starts_with("/models/")
        || path.starts_with("/v1/models/")
        || path.ends_with("/v1/models")
}

/// Render a locally generated failure using the OpenAI error envelope consumed
/// by Codex and OpenAI SDKs.
pub fn openai_error_response(
    status: StatusCode,
    error_type: impl Into<String>,
    code: Option<&str>,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message.into(),
                "type": error_type.into(),
                "param": Value::Null,
                "code": code,
            }
        })),
    )
        .into_response()
}

/// Build an HttpError from a non-OK reqwest response, consuming its body.
pub async fn http_error_from_response(
    message: impl Into<String>,
    response: reqwest::Response,
) -> HttpError {
    let status = StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = upstream_response_headers(&response);
    let body = crate::libs::http::read_text_capped(response).await;
    HttpError::new(message, status, headers, body)
}

/// Copy upstream headers into Axum's HTTP types. Error rendering applies the
/// final correlation/retry allowlist, so callers can retain diagnostic headers
/// even when a successful HTTP response later fails body validation.
pub fn upstream_response_headers(response: &reqwest::Response) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers().iter() {
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::from_bytes(name.as_str().as_bytes()),
            axum::http::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.insert(n, v);
        }
    }
    headers
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

/// If `body` is a JSON object that looks like an error envelope, normalize it to
/// the complete Anthropic shape and return it parsed (avoids the double-encoding
/// where a JSON error body is stringified into our `message`).
///
/// Upstreams commonly omit the top-level discriminator and return only
/// `{"error": {...}}`. Anthropic SDKs require
/// `{"type":"error","error":{...}}`, so add or correct that discriminator while
/// preserving the nested error and any other upstream fields. An already
/// complete Anthropic envelope is unchanged.
fn parse_upstream_error_envelope(body: &str) -> Option<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let obj = value.as_object_mut()?;
    obj.get("error").filter(|error| error.is_object())?;
    obj.insert(
        "type".to_string(),
        serde_json::Value::String("error".to_string()),
    );
    Some(value)
}

/// Parse an OpenAI `{"error": {...}}` envelope without adding Anthropic's
/// top-level `type` discriminator or otherwise rewriting unknown fields.
fn parse_openai_error_envelope(body: &str, status: StatusCode) -> Option<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let object = value.as_object_mut()?;

    // A Messages upstream can return Anthropic's top-level discriminator.
    // Responses/Chat clients must never receive it.
    if object.get("type").and_then(Value::as_str) == Some("error") {
        object.remove("type");
    }
    let error = object.get_mut("error")?.as_object_mut()?;

    let normalized_type = match error.get("type").and_then(Value::as_str) {
        Some("overloaded_error" | "api_error") => Some("server_error"),
        Some("request_too_large") => Some("invalid_request_error"),
        _ => None,
    };
    if let Some(error_type) = normalized_type {
        error.insert("type".to_string(), json!(error_type));
    }
    error.entry("param".to_string()).or_insert(Value::Null);
    error
        .entry("code".to_string())
        .or_insert_with(|| openai_error_code(status).map_or(Value::Null, Value::from));
    Some(value)
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

/// Headers that are useful to a client diagnosing or retrying a failed request.
///
/// Never forward arbitrary `x-*` headers: upstreams frequently attach internal
/// routing/debug metadata that is neither stable nor intended for callers.
fn should_forward_error_header(name: &str, retryable_status: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    let correlation = matches!(
        lower.as_str(),
        "request-id"
            | "x-request-id"
            | "x-correlation-id"
            | "x-github-request-id"
            | "x-ms-request-id"
            | "x-vss-e2eid"
            | "x-azure-ref"
            | "openai-request-id"
    );
    if correlation {
        return true;
    }
    retryable_status
        && (lower == "retry-after"
            || lower.starts_with("x-ratelimit-")
            || lower.starts_with("x-usage-")
            || lower.starts_with("anthropic-ratelimit-")
            || lower.starts_with("ratelimit-"))
}

fn internal_reference_message() -> String {
    let trace_id = crate::libs::request_context::request_context_store()
        .map(|ctx| ctx.trace_id.clone())
        .unwrap_or_else(crate::libs::request_context::generate_trace_id);
    format!("An internal error occurred. Reference: {trace_id}")
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

fn openai_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        429 => "rate_limit_error",
        500..=599 => "server_error",
        _ => "invalid_request_error",
    }
}

fn openai_error_code(status: StatusCode) -> Option<&'static str> {
    match status.as_u16() {
        401 => Some("invalid_api_key"),
        403 => Some("insufficient_permissions"),
        404 => Some("not_found"),
        413 => Some("request_too_large"),
        429 => Some("rate_limit_exceeded"),
        500..=599 => Some("server_error"),
        _ => None,
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
                let retryable_status = is_rate_limit || is_overloaded || e.status.is_server_error();
                // Correlation IDs are useful on every status. Retry/backoff
                // metadata is additionally forwarded for retryable 429/529/5xx
                // failures. We do NOT synthesize retry-after when it is absent.
                for (name, value) in e.headers.iter() {
                    if should_forward_error_header(name.as_str(), retryable_status) {
                        out_headers.insert(name.clone(), value.clone());
                    }
                }
                // The upstream error body (when present) is forwarded to the
                // client. Rather than stringifying a JSON error body into our
                // `message` field (which double-encodes it as an escaped string),
                // surface it directly: if the upstream already sent an Anthropic-
                // style error envelope, forward it verbatim; if it's some other
                // JSON, lift a human message out of it; otherwise use the raw text.
                let fallback_message = if e.body.is_empty() && !e.expose_message {
                    internal_reference_message()
                } else if e.body.is_empty() {
                    e.message.clone()
                } else {
                    e.body.clone()
                };
                let safe_message =
                    lift_upstream_error_message(&e.body).unwrap_or(fallback_message.clone());
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
                            "message": safe_message,
                        }
                    }))
                } else if let Some(envelope) = parse_upstream_error_envelope(&e.body) {
                    // Preserve the real structured upstream error while ensuring
                    // the top-level discriminator required by Anthropic SDKs.
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
                anthropic_error_response(StatusCode::BAD_REQUEST, "invalid_request_error", message)
            }
            AppError::Other(e) => {
                let message = internal_reference_message();
                tracing::error!(
                    error = ?e,
                    "internal error"
                );
                anthropic_error_response(StatusCode::INTERNAL_SERVER_ERROR, "api_error", message)
            }
        }
    }
}

impl AppError {
    /// Render this failure for an OpenAI-native endpoint.
    ///
    /// This intentionally lives alongside the Anthropic `IntoResponse`
    /// implementation rather than changing the crate-wide default: Messages
    /// handlers and middleware still rely on Anthropic retry/error semantics.
    pub fn into_openai_response(self) -> Response {
        match self {
            AppError::Http(e) => {
                tracing::error!("Error occurred: {}", e.message);
                let retryable_status =
                    e.status == StatusCode::TOO_MANY_REQUESTS || e.status.is_server_error();
                let mut out_headers = HeaderMap::new();
                for (name, value) in e.headers.iter() {
                    if should_forward_error_header(name.as_str(), retryable_status) {
                        out_headers.insert(name.clone(), value.clone());
                    }
                }

                let fallback_message = if e.body.is_empty() && !e.expose_message {
                    internal_reference_message()
                } else if e.body.is_empty() {
                    e.message.clone()
                } else {
                    e.body.clone()
                };

                let body = if let Some(envelope) = parse_openai_error_envelope(&e.body, e.status) {
                    Json(envelope)
                } else {
                    let message = lift_upstream_error_message(&e.body).unwrap_or(fallback_message);
                    Json(json!({
                        "error": {
                            "message": message,
                            "type": openai_error_type(e.status),
                            "param": Value::Null,
                            "code": openai_error_code(e.status),
                        }
                    }))
                };
                (e.status, out_headers, body).into_response()
            }
            AppError::BadRequest(message) => {
                tracing::warn!("Bad request: {message}");
                openai_error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    None,
                    message,
                )
            }
            AppError::Other(error) => {
                let message = internal_reference_message();
                tracing::error!(error = ?error, "internal error");
                openai_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    Some("server_error"),
                    message,
                )
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

    async fn render_openai(err: AppError) -> (StatusCode, HeaderMap, serde_json::Value) {
        let resp = err.into_openai_response();
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
        assert_eq!(body["error"]["message"], "rate limited by copilot");
        // Retry and rate-limit headers are forwarded so the SDK backs off.
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
        // The raw error message must NOT leak to the client; a sanitized
        // "An internal error occurred. Reference: <trace_id>" is expected.
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.starts_with("An internal error occurred. Reference:"),
            "expected sanitized message, got: {msg}"
        );
        assert!(
            !msg.contains("something broke internally"),
            "raw error must not appear in client response, got: {msg}"
        );
    }

    #[tokio::test]
    async fn bad_request_renders_400_invalid_request_error() {
        let err =
            AppError::BadRequest("Invalid request payload: missing field `model`".to_string());
        let (status, _headers, body) = render(err).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["type"], "error");
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
        // A nested-only envelope must also gain the SDK-required discriminator.
        let err = AppError::Http(HttpError::new(
            "Failed",
            status(400),
            HeaderMap::new(),
            r#"{"error":{"type":"invalid_request_error","message":"The requested model is not supported."}}"#.to_string(),
        ));
        let (status, _headers, body) = render(err).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(
            body["error"]["message"],
            "The requested model is not supported."
        );
        // The message must be a string, not escaped JSON.
        assert!(body["error"]["message"].is_string());
    }

    #[tokio::test]
    async fn complete_upstream_error_envelope_is_preserved() {
        let upstream = json!({
            "type": "error",
            "error": {
                "type": "permission_error",
                "message": "Access denied",
                "upstream_code": "forbidden",
            },
            "request_id": "req-upstream",
        });
        let err = AppError::Http(HttpError::new(
            "Failed",
            StatusCode::FORBIDDEN,
            HeaderMap::new(),
            upstream.to_string(),
        ));

        let (status, _headers, body) = render(err).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, upstream);
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
    async fn error_headers_use_explicit_allowlist() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert("x-request-id", "req-123".parse().unwrap());
        headers.insert("x-internal-secret", "do-not-leak".parse().unwrap());
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
        // Correlation headers are useful on every failure status.
        assert_eq!(out_headers.get("x-request-id").unwrap(), "req-123");
        // Arbitrary x-* metadata must never be reflected.
        assert!(out_headers.get("x-internal-secret").is_none());
        // 403 maps to permission_error.
        assert_eq!(body["error"]["type"], "permission_error");
    }

    #[tokio::test]
    async fn rate_limit_does_not_forward_arbitrary_x_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-internal-secret", "do-not-leak".parse().unwrap());
        let err = AppError::Http(HttpError::new(
            "limited",
            status(429),
            headers,
            String::new(),
        ));
        let (_status, out_headers, _body) = render(err).await;
        assert_eq!(out_headers.get("x-ratelimit-remaining").unwrap(), "0");
        assert!(out_headers.get("x-internal-secret").is_none());
    }

    #[tokio::test]
    async fn synthetic_internal_http_error_hides_diagnostic() {
        let err = AppError::Http(HttpError::internal(
            "connection to private-host.internal:443 failed with token abc",
        ));
        let (_status, _headers, body) = render(err).await;
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("An internal error occurred. Reference:"));
        assert!(!message.contains("private-host"));
        assert!(!message.contains("token abc"));
    }

    #[tokio::test]
    async fn openai_renderer_removes_anthropic_discriminator_and_normalizes_fields() {
        let err = AppError::Http(HttpError::new(
            "overloaded",
            StatusCode::SERVICE_UNAVAILABLE,
            HeaderMap::new(),
            json!({
                "type":"error",
                "error":{
                    "type":"overloaded_error",
                    "message":"try later",
                    "upstream_extension":true
                },
                "request_id":"req_fixture"
            })
            .to_string(),
        ));
        let (status, _, body) = render_openai(err).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.get("type").is_none());
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(body["error"]["code"], "server_error");
        assert!(body["error"]["param"].is_null());
        assert_eq!(body["error"]["upstream_extension"], true);
        assert_eq!(body["request_id"], "req_fixture");
    }
}
