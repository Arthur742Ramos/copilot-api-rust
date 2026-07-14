use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::libs::config::{
    get_model_mappings, get_provider_config, get_raw_provider_config, is_reserved_provider_name,
    list_enabled_providers, reload_config, set_model_mappings, set_provider_config, ProviderConfig,
};
use crate::libs::paths::PATHS;
use crate::services::providers::provider_proxy::{probe_provider_models, ProbeOutcome};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelMappingsRequest {
    model_mappings: BTreeMap<String, String>,
}

fn config_path_string() -> String {
    PATHS.config_path.to_string_lossy().into_owned()
}

/// GET /admin/config/model-mappings — mirrors routes/admin/config/route.ts.
pub async fn get_model_mappings_route() -> Response {
    Json(json!({
        "configPath": config_path_string(),
        "modelMappings": get_model_mappings(),
    }))
    .into_response()
}

/// POST /admin/config/model-mappings — validates and persists the mapping table.
pub async fn post_model_mappings_route(body: Bytes) -> Response {
    // Mirror `await c.req.json()`: an unparseable body throws -> forwardError (500).
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return forwarded_error(&error.to_string()),
    };

    // Mirror the zod safeParse gate: valid JSON of the wrong shape -> 400.
    let parsed: ModelMappingsRequest = match serde_json::from_value(value) {
        Ok(parsed) => parsed,
        Err(_) => return invalid_request("Invalid request body."),
    };

    if parsed
        .model_mappings
        .iter()
        .any(|(source, target)| source.trim().is_empty() || target.trim().is_empty())
    {
        return invalid_request("Model mapping source and target must be non-empty strings.");
    }

    // Persistence/reload failures remain server errors after request validation.
    match set_model_mappings(&parsed.model_mappings) {
        Ok(updated) => Json(json!({
            "configPath": config_path_string(),
            "modelMappings": updated,
        }))
        .into_response(),
        Err(error) => forwarded_error(&error.to_string()),
    }
}

/// POST /admin/config/reload — rebuilds the cached config from disk so a manual
/// edit to config.json applies without restarting and dropping in-flight streams
/// or the warm Copilot token.
pub async fn post_reload_route() -> Response {
    match reload_config() {
        Ok(_) => Json(json!({
            "configPath": config_path_string(),
            "reloaded": true,
            "modelMappings": get_model_mappings(),
            "providers": provider_summaries(),
        }))
        .into_response(),
        Err(error) => forwarded_error(&error.to_string()),
    }
}

/// A single provider's state with the `apiKey` secret redacted to a boolean.
fn provider_summary(name: &str) -> Value {
    // `get_provider_config` resolves+validates this one provider, so it is the
    // O(1) per-provider enabled check (list_enabled_providers would re-scan all
    // providers on every call, making the listing O(n^2)).
    let enabled = get_provider_config(name).is_some();
    match get_raw_provider_config(name) {
        Some(p) => json!({
            "name": name,
            "type": p.provider_type,
            "baseUrl": p.base_url,
            "authType": p.auth_type,
            "enabled": enabled,
            "apiKeySet": p.api_key.as_deref().map(|k| !k.trim().is_empty()).unwrap_or(false),
        }),
        None => json!({ "name": name, "enabled": enabled }),
    }
}

/// Build the redacted provider list from the raw config keys (so disabled and
/// misconfigured providers are still visible to an operator), never echoing the
/// stored `apiKey`.
fn provider_summaries() -> Vec<Value> {
    let raw = crate::libs::config::get_config()
        .providers
        .as_ref()
        .map(|p| p.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    raw.iter().map(|name| provider_summary(name)).collect()
}

/// GET /admin/config/providers — list configured providers with secrets redacted.
pub async fn get_providers_route() -> Response {
    Json(json!({
        "configPath": config_path_string(),
        "providers": provider_summaries(),
    }))
    .into_response()
}

/// Map a probe outcome to the redacted per-provider JSON fields. The raw HTTP
/// status is preserved (not collapsed to a boolean) so an operator can tell a
/// bad apiKey (401/403) from a wrong baseUrl (404) from an unreachable host.
fn probe_result_json(outcome: &ProbeOutcome, latency_ms: u128) -> Value {
    match outcome {
        // `reachable` means "we got an HTTP response", independent of whether the
        // credentials were accepted. A 401 is reachable-but-unauthorized.
        ProbeOutcome::Status(status) => json!({
            "reachable": true,
            "status": status,
            "latencyMs": latency_ms,
        }),
        ProbeOutcome::Unreachable(reason) => json!({
            "reachable": false,
            "status": Value::Null,
            "error": reason,
            "latencyMs": latency_ms,
        }),
    }
}

/// Freshness summary for a builtin (copilot / codex) credential. These are
/// probed by token expiry rather than an HTTP round-trip: the gateway already
/// holds and refreshes them, so an HTTP call would just re-test our own warm
/// token. Never echoes the token itself.
fn builtin_summary(name: &str, present: bool, fresh: bool) -> Value {
    json!({
        "name": name,
        "tokenPresent": present,
        "tokenFresh": fresh,
    })
}

/// GET /admin/providers/health — actively probe every ENABLED third-party
/// provider so a bad apiKey / wrong baseUrl / unreachable host surfaces here
/// instead of on the first real generation. Each provider gets a lightweight,
/// timeout-bounded `GET {baseUrl}/v1/models` probe (issued concurrently) and is
/// reported with name, `reachable`, the raw HTTP `status`, and `latencyMs`. The
/// builtin copilot/codex credentials are reported by token freshness instead of
/// an HTTP round-trip. No secrets (apiKeys, tokens) appear in the response.
pub async fn get_providers_health_route() -> Response {
    // Resolve each enabled provider to its full config up front. Disabled /
    // misconfigured providers are intentionally skipped — `list_enabled_providers`
    // already filters them, and there is nothing to probe.
    let configs: Vec<_> = list_enabled_providers()
        .into_iter()
        .filter_map(|name| get_provider_config(&name))
        .collect();

    // Probe all providers concurrently; a single slow upstream must not serialize
    // behind the others. Each probe is independently bounded by PROBE_TIMEOUT.
    let probes = configs.iter().map(|cfg| async move {
        let (outcome, latency_ms) = probe_provider_models(cfg).await;
        let mut entry = probe_result_json(&outcome, latency_ms);
        if let Value::Object(map) = &mut entry {
            map.insert("name".to_string(), json!(cfg.name));
            map.insert("type".to_string(), json!(cfg.provider_type));
            map.insert("baseUrl".to_string(), json!(cfg.base_url));
            map.insert("authType".to_string(), json!(cfg.auth_type));
        }
        entry
    });
    let providers: Vec<Value> = futures::future::join_all(probes).await;

    Json(json!({
        "configPath": config_path_string(),
        "builtins": builtin_health(),
        "providers": providers,
    }))
    .into_response()
}

/// Build the builtin (copilot / codex) freshness section. Reuses the same
/// freshness predicates as `/readyz` (`copilot_token_is_fresh`) and the codex
/// refresh loop (`is_codex_credentials_expired`).
fn builtin_health() -> Vec<Value> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now_millis = now_secs.saturating_mul(1000);

    let (copilot_token, codex_token, codex_expires_at) = crate::libs::state::with_state(|s| {
        (
            s.copilot_token.clone(),
            s.codex_access_token.clone(),
            s.codex_expires_at,
        )
    });

    let copilot_present = copilot_token.as_deref().is_some_and(|t| !t.is_empty());
    let copilot_fresh = copilot_token
        .as_deref()
        .is_some_and(|t| crate::routes::token::copilot_token_is_fresh(t, now_secs));

    let codex_present = codex_token.as_deref().is_some_and(|t| !t.is_empty());
    // Fresh only when a token is held AND its expiry is not within the refresh
    // buffer. A missing expiry (no token) is treated as not-fresh.
    let codex_fresh = codex_present
        && codex_expires_at.is_some_and(|exp| {
            !crate::libs::oauth::codex::is_codex_credentials_expired(exp, Some(now_millis))
        });

    vec![
        builtin_summary("copilot", copilot_present, copilot_fresh),
        builtin_summary("codex", codex_present, codex_fresh),
    ]
}

/// Strip a secret string key from a config object and report whether it held a
/// non-empty value. The indicator is returned (not written back into the config
/// object) so it can't collide with a user-supplied `#[serde(flatten)] extra`
/// key of the same name.
fn strip_secret(obj: &mut serde_json::Map<String, Value>, key: &str) -> bool {
    obj.remove(key)
        .and_then(|v| v.as_str().map(|s| !s.trim().is_empty()))
        .unwrap_or(false)
}

/// GET /admin/config — the effective merged runtime config with all secrets
/// redacted, so an operator can confirm what the gateway is actually running
/// (budget, model slugs, feature flags, providers) without filesystem access to
/// config.json.
///
/// Secret values are STRIPPED from the returned `config` object entirely; the
/// presence indicators (which secrets are set, the apiKeys count, which
/// providers have a key) are reported in a separate top-level `secrets` object,
/// not as sibling keys inside `config` — `AppConfig`/`ProviderConfig` round-trip
/// unknown keys via `#[serde(flatten)]`, so writing synthetic `*Set` keys inside
/// the config could collide with a real user-supplied key.
pub async fn get_effective_config_route() -> Response {
    let cfg = crate::libs::config::get_config();
    let mut value = match serde_json::to_value(&*cfg) {
        Ok(Value::Object(map)) => map,
        _ => {
            return forwarded_error("Failed to serialize effective config");
        }
    };

    let mut admin_key_set = false;
    let mut api_keys_count = 0usize;
    if let Some(Value::Object(auth)) = value.get_mut("auth") {
        admin_key_set = strip_secret(auth, "adminApiKey");
        api_keys_count = auth
            .remove("apiKeys")
            .and_then(|v| v.as_array().map(|a| a.len()))
            .unwrap_or(0);
    }
    let anthropic_key_set = strip_secret(&mut value, "anthropicApiKey");

    // Strip each provider's apiKey and record which providers have one set.
    let mut providers_with_key: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Some(Value::Object(providers)) = value.get_mut("providers") {
        for (name, prov) in providers.iter_mut() {
            if let Value::Object(p) = prov {
                let set = strip_secret(p, "apiKey");
                providers_with_key.insert(name.clone(), json!(set));
            }
        }
    }

    Json(json!({
        "configPath": config_path_string(),
        "config": value,
        "secrets": {
            "adminApiKeySet": admin_key_set,
            "apiKeysCount": api_keys_count,
            "anthropicApiKeySet": anthropic_key_set,
            "providerApiKeySet": providers_with_key,
        },
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpsertProviderRequest {
    name: String,
    config: ProviderConfig,
}

/// POST /admin/config/providers — upsert a single provider. Delegates to
/// set_provider_config (which rejects the reserved `copilot` name and
/// persists+reloads). The response redacts the stored apiKey.
pub async fn post_providers_route(body: Bytes) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return forwarded_error(&error.to_string()),
    };

    let parsed: UpsertProviderRequest = match serde_json::from_value(value) {
        Ok(parsed) => parsed,
        Err(_) => return invalid_request("Invalid request body."),
    };

    let name = parsed.name.trim();
    if name.is_empty() {
        return invalid_request("Provider name must be a non-empty string.");
    }
    // Reserved-name rejection is a client error, not a server fault. Checked on
    // the trimmed name so a value like "copilot " is rejected as 400 here rather
    // than slipping through to set_provider_config and surfacing as a 500.
    if is_reserved_provider_name(name) {
        return invalid_request(&format!(
            "Provider {name} is reserved and cannot be configured."
        ));
    }

    match set_provider_config(name, parsed.config) {
        Ok(_) => Json(json!({
            "configPath": config_path_string(),
            "provider": provider_summary(name),
        }))
        .into_response(),
        Err(error) => forwarded_error(&error.to_string()),
    }
}

fn invalid_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
            }
        })),
    )
        .into_response()
}

/// Mirrors forwardError() for a non-HTTPError throw: 500 with `type: "error"`.
fn forwarded_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "message": message,
                "type": "error",
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reachable_probe_preserves_raw_status() {
        // A 401 means "reachable, but rejected our apiKey" — reachable must be
        // true and the raw status preserved, NOT collapsed to a boolean.
        let v = probe_result_json(&ProbeOutcome::Status(401), 12);
        assert_eq!(v["reachable"], json!(true));
        assert_eq!(v["status"], json!(401));
        assert_eq!(v["latencyMs"], json!(12));
        assert!(v.get("error").is_none());
    }

    #[test]
    fn not_found_probe_is_reachable_with_404() {
        // A wrong baseUrl typically yields 404 — still reachable, distinct from
        // a connect failure.
        let v = probe_result_json(&ProbeOutcome::Status(404), 5);
        assert_eq!(v["reachable"], json!(true));
        assert_eq!(v["status"], json!(404));
    }

    #[test]
    fn unreachable_probe_reports_false_with_reason() {
        let v = probe_result_json(&ProbeOutcome::Unreachable("connect".to_string()), 4000);
        assert_eq!(v["reachable"], json!(false));
        assert_eq!(v["status"], Value::Null);
        assert_eq!(v["error"], json!("connect"));
        assert_eq!(v["latencyMs"], json!(4000));
    }

    #[test]
    fn builtin_summary_redacts_token_to_booleans() {
        let v = builtin_summary("copilot", true, false);
        assert_eq!(v["name"], json!("copilot"));
        assert_eq!(v["tokenPresent"], json!(true));
        assert_eq!(v["tokenFresh"], json!(false));
        // The token value itself must never appear.
        assert!(v.as_object().unwrap().len() == 3);
    }
}
