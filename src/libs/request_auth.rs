use std::collections::BTreeSet;

use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use serde_json::Value;

use crate::libs::config::get_config;

// Mirrors src/lib/request-auth.ts. Provides API-key extraction and the auth
// decision used by the server middleware layers.

/// A normalized API key plus its optional human-readable label. This is the unit
/// of per-key identity: the label (never the raw key) is what gets attributed in
/// usage records and, in a later phase, looked up for per-key budgets. Designed
/// so a budget lookup can reuse the same `label`/`attribution` resolution.
#[derive(Debug, Clone)]
pub struct ApiKeyConfig {
    pub key: String,
    pub label: Option<String>,
    /// Optional per-key daily token cap. When set and positive, requests
    /// attributed to this key (by [`ApiKeyConfig::attribution`]) are rejected with
    /// a 429 once the key's own recorded spend for the local day reaches it,
    /// independently of the global `dailyTokenBudget`. `None` or `<= 0` disables
    /// the per-key cap. Phase 2 of per-key multi-tenancy.
    pub daily_token_budget: Option<i64>,
}

impl ApiKeyConfig {
    /// The stable attribution token for this key: its label when set, otherwise a
    /// short, deterministic fingerprint of the key. Never the raw key — safe to
    /// persist and surface as a bounded metric label.
    pub fn attribution(&self) -> String {
        match self.label.as_deref().map(str::trim) {
            Some(label) if !label.is_empty() => label.to_string(),
            _ => key_fingerprint(&self.key),
        }
    }
}

/// A short, stable fingerprint of a raw key used to distinguish unlabeled clients
/// without ever persisting or logging the secret itself. `key-<first 12 hex of
/// sha256>` is collision-resistant enough to separate a handful of keys while
/// revealing nothing about the original value.
pub fn key_fingerprint(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    format!("key-{}", &hex::encode(digest)[..12])
}

/// One configured `auth.apiKeys` entry: either a bare string or an object with a
/// `key` and an optional `label`. Untagged so existing string-only config keeps
/// parsing unchanged while the richer object form is also accepted.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ApiKeyEntry {
    Plain(String),
    Labeled {
        key: String,
        #[serde(default)]
        label: Option<String>,
        /// Per-key daily token cap (object form only). Renamed to match the
        /// top-level `dailyTokenBudget` config key.
        #[serde(default, rename = "dailyTokenBudget")]
        daily_token_budget: Option<i64>,
    },
}

/// Normalize raw `auth.apiKeys` values (each an arbitrary JSON `Value`, so the
/// surrounding config round-trips unknown shapes) into deduplicated key->label
/// pairs. Accepts either a plain string or `{ "key": ..., "label": ... }`; empty
/// keys and unparseable entries are dropped with a warning.
pub fn normalize_api_keys(api_keys: Option<&Vec<Value>>) -> Vec<ApiKeyConfig> {
    let api_keys = match api_keys {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut had_invalid = false;
    for entry in api_keys {
        let parsed = match serde_json::from_value::<ApiKeyEntry>(entry.clone()) {
            Ok(parsed) => parsed,
            Err(_) => {
                had_invalid = true;
                continue;
            }
        };
        let (key, label, daily_token_budget) = match parsed {
            ApiKeyEntry::Plain(s) => (s, None, None),
            ApiKeyEntry::Labeled {
                key,
                label,
                daily_token_budget,
            } => (key, label, daily_token_budget),
        };
        let trimmed = key.trim();
        if trimmed.is_empty() {
            had_invalid = true;
            continue;
        }
        let label = label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty());
        // A non-positive per-key cap is treated as "no cap", mirroring the
        // global `dailyTokenBudget` semantics, so config can disable it inline.
        let daily_token_budget = daily_token_budget.filter(|&b| b > 0);
        if seen.insert(trimmed.to_string()) {
            out.push(ApiKeyConfig {
                key: trimmed.to_string(),
                label,
                daily_token_budget,
            });
        }
    }
    if had_invalid {
        tracing::warn!(
            "Invalid auth.apiKeys entries found. Each entry must be a non-empty string or an object with a non-empty 'key'."
        );
    }
    out
}

/// Normalized api-key entries, memoized against the identity of the current
/// cached config `Arc`. The untagged string|object parse in [`normalize_api_keys`]
/// then runs once per config (re)load instead of on every auth/budget check on
/// the hot path. `reload_config` swaps the config `Arc`, so its pointer changes
/// and the cache is transparently rebuilt on the next call.
pub fn get_configured_api_keys() -> Vec<ApiKeyConfig> {
    let config = get_config();
    normalize_api_keys(config.auth.as_ref().and_then(|a| a.api_keys.as_ref()))
}

fn normalize_api_key(value: Option<&Value>) -> Option<String> {
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

pub fn get_configured_admin_api_keys() -> Vec<ApiKeyConfig> {
    let config = get_config();
    match normalize_api_key(config.auth.as_ref().and_then(|a| a.admin_api_key.as_ref())) {
        Some(k) => vec![ApiKeyConfig {
            key: k,
            label: Some("admin".to_string()),
            daily_token_budget: None,
        }],
        None => Vec::new(),
    }
}

/// Look up the configured per-key daily token budget for a given attribution
/// token (the same `label`/fingerprint Phase 1 attributes usage under). Returns
/// the first matching positive cap, or `None` when the label is unknown or has no
/// per-key budget. Reuses [`ApiKeyConfig::attribution`] so the key used for the
/// budget lookup is exactly the key used for usage attribution.
///
/// This intentionally re-derives the key list per call rather than caching it:
/// `auth.apiKeys` is a small, operator-controlled list and this runs only for
/// labeled requests that already passed the global-budget check, so the scan is
/// negligible — and a pointer/identity cache over the config `Arc` is unsafe
/// across `reload_config` (a recycled allocation address would serve stale keys).
pub fn get_api_key_daily_budget(attribution: &str) -> Option<i64> {
    get_configured_api_keys()
        .into_iter()
        .find(|entry| entry.attribution() == attribution)
        .and_then(|entry| entry.daily_token_budget)
        .filter(|&b| b > 0)
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

fn unauthorized_response(path: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        "WWW-Authenticate",
        axum::http::HeaderValue::from_static("Bearer realm=\"copilot-api\""),
    );
    let mut response = if crate::libs::error::is_openai_native_path(path) {
        crate::libs::error::openai_error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            Some("invalid_api_key"),
            "Unauthorized",
        )
    } else {
        crate::libs::error::anthropic_error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Unauthorized",
        )
    };
    response.headers_mut().extend(headers);
    response
}

/// Options matching `AuthMiddlewareOptions`.
pub struct AuthOptions {
    pub get_api_keys: fn() -> Vec<ApiKeyConfig>,
    pub allow_unauthenticated_paths: &'static [&'static str],
    pub allow_options_bypass: bool,
    pub allow_when_no_api_keys: bool,
    pub skip_admin_prefix: bool,
}

impl AuthOptions {
    /// The general auth middleware: skips `/admin/` (handled by the admin layer)
    /// and allows the unauthenticated landing / usage-viewer paths.
    ///
    /// `/metrics` is intentionally NOT listed here: it would otherwise leak LAN
    /// traffic patterns (request counts / latency labels) to any unauthenticated
    /// client. By relying on the normal key check instead, `/metrics` stays open
    /// only when no API keys are configured (`allow_when_no_api_keys`) and
    /// requires a valid key once keys are set. `/readyz` remains always-open so
    /// orchestrators can probe readiness without credentials.
    pub fn general() -> Self {
        AuthOptions {
            get_api_keys: get_configured_api_keys,
            allow_unauthenticated_paths: &[
                "/",
                "/usage-viewer",
                "/usage-viewer/",
                "/readyz",
                "/version",
            ],
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

/// The result of an auth check. `Reject` carries the 401 response; `Allow`
/// carries the matched key's attribution token (its label, or a stable
/// fingerprint when unlabeled) when a configured key matched, or `None` for
/// unauthenticated-but-permitted requests (allowlisted paths, OPTIONS bypass, or
/// no keys configured).
pub enum AuthOutcome {
    Reject(Response),
    Allow(Option<String>),
}

/// Returns `Reject(response)` when the request should be rejected, `Allow(label)`
/// when it is authorized to proceed. Mirrors `createAuthMiddleware`'s decision
/// tree. On a successful key match the matched entry's attribution token is
/// returned so the caller can attribute usage to the named key.
pub fn check_auth(
    options: &AuthOptions,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    is_admin_layer: bool,
) -> AuthOutcome {
    if options.allow_options_bypass && method == Method::OPTIONS {
        return AuthOutcome::Allow(None);
    }
    // The general layer skips /admin/ paths so the admin layer can handle them.
    if options.skip_admin_prefix && path.starts_with("/admin/") {
        return AuthOutcome::Allow(None);
    }
    // The admin layer only applies to /admin/* paths.
    if is_admin_layer && !path.starts_with("/admin/") {
        return AuthOutcome::Allow(None);
    }
    if options.allow_unauthenticated_paths.contains(&path) {
        return AuthOutcome::Allow(None);
    }

    let api_keys = (options.get_api_keys)();
    if api_keys.is_empty() {
        return if options.allow_when_no_api_keys {
            AuthOutcome::Allow(None)
        } else {
            AuthOutcome::Reject(unauthorized_response(path))
        };
    }

    let request_api_key = match extract_request_api_key(headers) {
        Some(key) => key,
        None => return AuthOutcome::Reject(unauthorized_response(path)),
    };

    // Compare against every configured key without short-circuiting, so neither
    // the per-byte comparison (constant_time_eq) nor the loop reveals which key
    // matched or how far a near-miss got. The matched entry's attribution token
    // is captured for usage accounting.
    let mut matched: Option<String> = None;
    for entry in &api_keys {
        if constant_time_eq(&entry.key, &request_api_key) {
            matched = Some(entry.attribution());
        }
    }
    match matched {
        Some(attribution) => AuthOutcome::Allow(Some(attribution)),
        None => AuthOutcome::Reject(unauthorized_response(path)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    fn keys(values: Vec<Value>) -> Vec<ApiKeyConfig> {
        normalize_api_keys(Some(&values))
    }

    #[test]
    fn normalize_accepts_plain_string_and_labeled_object() {
        let parsed = keys(vec![
            json!("plain-key"),
            json!({ "key": "labeled-key", "label": "team-a" }),
            json!({ "key": "unlabeled-obj" }),
        ]);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].key, "plain-key");
        assert_eq!(parsed[0].label, None);
        assert_eq!(parsed[1].key, "labeled-key");
        assert_eq!(parsed[1].label.as_deref(), Some("team-a"));
        assert_eq!(parsed[2].key, "unlabeled-obj");
        assert_eq!(parsed[2].label, None);
    }

    #[test]
    fn normalize_parses_per_key_daily_budget() {
        let parsed = keys(vec![
            json!({ "key": "k1", "label": "team-a", "dailyTokenBudget": 1_000_000 }),
            json!({ "key": "k2", "label": "team-b" }), // no budget -> None
            json!({ "key": "k3", "label": "team-c", "dailyTokenBudget": 0 }), // non-positive -> disabled
            json!("plain"),                                                   // string form -> None
        ]);
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].daily_token_budget, Some(1_000_000));
        assert_eq!(parsed[1].daily_token_budget, None);
        assert_eq!(parsed[2].daily_token_budget, None);
        assert_eq!(parsed[3].daily_token_budget, None);
    }

    #[test]
    fn normalize_trims_dedupes_and_drops_invalid() {
        let parsed = keys(vec![
            json!("  spaced  "),
            json!("spaced"),                      // duplicate after trim -> dropped
            json!(""),                            // empty -> dropped
            json!({ "key": "  ", "label": "x" }), // empty key -> dropped
            json!(42),                            // wrong type -> dropped
            json!({ "label": "no-key" }),         // missing key -> dropped
        ]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].key, "spaced");
    }

    #[test]
    fn attribution_prefers_label_then_fingerprint() {
        let labeled = ApiKeyConfig {
            key: "secret".to_string(),
            label: Some("frontend".to_string()),
            daily_token_budget: None,
        };
        assert_eq!(labeled.attribution(), "frontend");

        let unlabeled = ApiKeyConfig {
            key: "secret".to_string(),
            label: None,
            daily_token_budget: None,
        };
        let fp = unlabeled.attribution();
        assert!(fp.starts_with("key-"));
        // Deterministic and never the raw key.
        assert_eq!(fp, key_fingerprint("secret"));
        assert_ne!(fp, "secret");
        assert!(!fp.contains("secret"));
    }

    fn options_for(keys: fn() -> Vec<ApiKeyConfig>) -> AuthOptions {
        AuthOptions {
            get_api_keys: keys,
            allow_unauthenticated_paths: &[],
            allow_options_bypass: true,
            allow_when_no_api_keys: false,
            skip_admin_prefix: false,
        }
    }

    fn bearer(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
        );
        headers
    }

    fn two_keys() -> Vec<ApiKeyConfig> {
        vec![
            ApiKeyConfig {
                key: "key-one".to_string(),
                label: Some("alice".to_string()),
                daily_token_budget: None,
            },
            ApiKeyConfig {
                key: "key-two".to_string(),
                label: None,
                daily_token_budget: None,
            },
        ]
    }

    #[test]
    fn check_auth_returns_matched_label() {
        let options = options_for(two_keys);
        match check_auth(
            &options,
            &Method::GET,
            "/v1/models",
            &bearer("key-one"),
            false,
        ) {
            AuthOutcome::Allow(Some(label)) => assert_eq!(label, "alice"),
            _ => panic!("expected Allow with label"),
        }
    }

    #[test]
    fn check_auth_unlabeled_match_returns_fingerprint() {
        let options = options_for(two_keys);
        match check_auth(
            &options,
            &Method::GET,
            "/v1/models",
            &bearer("key-two"),
            false,
        ) {
            AuthOutcome::Allow(Some(label)) => {
                assert_eq!(label, key_fingerprint("key-two"));
            }
            _ => panic!("expected Allow with fingerprint"),
        }
    }

    #[test]
    fn check_auth_rejects_unknown_key() {
        let options = options_for(two_keys);
        match check_auth(&options, &Method::GET, "/v1/models", &bearer("nope"), false) {
            AuthOutcome::Reject(_) => {}
            _ => panic!("expected Reject"),
        }
    }
}
