use std::collections::BTreeSet;

use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::libs::config::get_config;

// Mirrors src/lib/request-auth.ts. Provides API-key extraction and the auth
// decision used by the server middleware layers.

pub fn normalize_api_keys(api_keys: Option<&Vec<Value>>) -> Vec<String> {
    let api_keys = match api_keys {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut had_invalid = false;
    for key in api_keys {
        match key {
            Value::String(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    had_invalid = true;
                    continue;
                }
                if seen.insert(trimmed.to_string()) {
                    out.push(trimmed.to_string());
                }
            }
            _ => had_invalid = true,
        }
    }
    if had_invalid {
        tracing::warn!("Invalid auth.apiKeys entries found. Only non-empty strings are allowed.");
    }
    out
}

pub fn get_configured_api_keys() -> Vec<String> {
    let config = get_config();
    normalize_api_keys(config.auth.and_then(|a| a.api_keys).as_ref())
}

fn normalize_api_key(value: &Option<Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

pub fn get_configured_admin_api_keys() -> Vec<String> {
    let config = get_config();
    match normalize_api_key(&config.auth.and_then(|a| a.admin_api_key)) {
        Some(k) => vec![k],
        None => Vec::new(),
    }
}

/// Mirrors `extractRequestApiKey`: prefer `x-api-key`, else a Bearer token from
/// `authorization`.
pub fn extract_request_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(x_api_key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let trimmed = x_api_key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let authorization = headers.get("authorization").and_then(|v| v.to_str().ok())?;
    let mut parts = authorization.split_whitespace();
    let scheme = parts.next().unwrap_or("");
    if scheme.to_lowercase() != "bearer" {
        return None;
    }
    let bearer: Vec<&str> = parts.collect();
    let token = bearer.join(" ");
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn unauthorized_response() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        "WWW-Authenticate",
        axum::http::HeaderValue::from_static("Bearer realm=\"copilot-api\""),
    );
    let body = Json(json!({
        "error": {
            "message": "Unauthorized",
            "type": "authentication_error",
        }
    }));
    (StatusCode::UNAUTHORIZED, headers, body).into_response()
}

/// Options matching `AuthMiddlewareOptions`.
pub struct AuthOptions {
    pub get_api_keys: fn() -> Vec<String>,
    pub allow_unauthenticated_paths: &'static [&'static str],
    pub allow_options_bypass: bool,
    pub allow_when_no_api_keys: bool,
    pub skip_admin_prefix: bool,
}

impl AuthOptions {
    /// The general auth middleware: skips `/admin/` (handled by the admin layer)
    /// and allows the unauthenticated landing / usage-viewer paths.
    pub fn general() -> Self {
        AuthOptions {
            get_api_keys: get_configured_api_keys,
            allow_unauthenticated_paths: &["/", "/usage-viewer", "/usage-viewer/"],
            allow_options_bypass: true,
            allow_when_no_api_keys: true,
            skip_admin_prefix: true,
        }
    }

    /// The admin auth middleware: enforces admin keys on `/admin/*`.
    pub fn admin() -> Self {
        AuthOptions {
            get_api_keys: get_configured_admin_api_keys,
            allow_unauthenticated_paths: &[],
            allow_options_bypass: true,
            allow_when_no_api_keys: false,
            skip_admin_prefix: false,
        }
    }
}

/// Returns `Some(response)` when the request should be rejected, `None` when it
/// is authorized to proceed. Mirrors `createAuthMiddleware`'s decision tree.
pub fn check_auth(
    options: &AuthOptions,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    is_admin_layer: bool,
) -> Option<Response> {
    if options.allow_options_bypass && method == Method::OPTIONS {
        return None;
    }
    // The general layer skips /admin/ paths so the admin layer can handle them.
    if options.skip_admin_prefix && path.starts_with("/admin/") {
        return None;
    }
    // The admin layer only applies to /admin/* paths.
    if is_admin_layer && !path.starts_with("/admin/") {
        return None;
    }
    if options.allow_unauthenticated_paths.contains(&path) {
        return None;
    }

    let api_keys = (options.get_api_keys)();
    if api_keys.is_empty() {
        return if options.allow_when_no_api_keys {
            None
        } else {
            Some(unauthorized_response())
        };
    }

    let request_api_key = extract_request_api_key(headers);
    match request_api_key {
        Some(key) if api_keys.iter().any(|valid| constant_time_eq(valid, &key)) => None,
        _ => Some(unauthorized_response()),
    }
}

/// Compare two strings without short-circuiting on the first differing byte, to
/// avoid leaking key contents through response timing. Not branch-perfect, but
/// removes the obvious early-exit timing signal of `==` / `Vec::contains`.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
