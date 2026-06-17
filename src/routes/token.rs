use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{Map, Value};

use crate::libs::state;

/// GET /token — returns only the presence and expiry of the live Copilot bearer,
/// never the raw token itself.
///
/// The previous implementation echoed the live Copilot bearer (mirroring the TS
/// `c.json({ token })`). With default-empty `auth.apiKeys` the auth layer
/// short-circuits and allows this route, so anyone able to reach the gateway
/// (e.g. via a 0.0.0.0 bind, or a cross-origin browser request) could lift the
/// upstream credential. Returning presence + expiry keeps the endpoint useful
/// for diagnostics ("do we hold a token, and when does it expire?") without ever
/// disclosing the secret.
pub async fn get_token() -> Response {
    let token = state::with_state(|s| s.copilot_token.clone());
    let mut body = Map::new();
    match token {
        Some(token) => {
            body.insert("hasToken".to_string(), Value::Bool(true));
            if let Some(exp) = parse_copilot_token_exp(&token) {
                body.insert("expiresAt".to_string(), Value::Number(exp.into()));
            }
        }
        None => {
            body.insert("hasToken".to_string(), Value::Bool(false));
        }
    }
    Json(Value::Object(body)).into_response()
}

/// Parse the unix-seconds `exp=` field embedded in a Copilot bearer token.
///
/// Copilot tokens are semicolon-delimited `key=value` strings, e.g.
/// `tid=...;exp=1700000000;sku=...`. Returns `None` when the field is absent or
/// not an integer, so callers can fall back to a presence-only decision.
pub fn parse_copilot_token_exp(token: &str) -> Option<i64> {
    token.split(';').find_map(|segment| {
        let (key, value) = segment.split_once('=')?;
        if key.trim() == "exp" {
            value.trim().parse::<i64>().ok()
        } else {
            None
        }
    })
}

/// Whether a held Copilot token is still fresh (not past its expiry) at
/// `now_secs`. When the token carries no parseable `exp=`, we cannot assert
/// staleness and fall back to "fresh" (presence-based readiness).
pub fn copilot_token_is_fresh(token: &str, now_secs: i64) -> bool {
    match parse_copilot_token_exp(token) {
        Some(exp) => now_secs < exp,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exp_from_copilot_token() {
        let token = "tid=abc;exp=1700000000;sku=copilot";
        assert_eq!(parse_copilot_token_exp(token), Some(1_700_000_000));
    }

    #[test]
    fn missing_exp_returns_none() {
        assert_eq!(parse_copilot_token_exp("tid=abc;sku=copilot"), None);
        assert_eq!(parse_copilot_token_exp("opaque-token"), None);
        assert_eq!(parse_copilot_token_exp("tid=abc;exp=notanumber"), None);
    }

    #[test]
    fn freshness_uses_exp_when_present() {
        let token = "tid=abc;exp=1000";
        assert!(copilot_token_is_fresh(token, 999));
        assert!(!copilot_token_is_fresh(token, 1000));
        assert!(!copilot_token_is_fresh(token, 1001));
    }

    #[test]
    fn freshness_falls_back_to_present_without_exp() {
        // No parseable exp -> cannot assert staleness, treat as fresh.
        assert!(copilot_token_is_fresh("opaque-token", i64::MAX));
    }
}
