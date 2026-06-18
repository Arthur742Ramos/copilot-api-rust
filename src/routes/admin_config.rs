use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::libs::config::{
    get_model_mappings, get_provider_config, get_raw_provider_config, is_reserved_provider_name,
    reload_config, set_model_mappings, set_provider_config, ProviderConfig,
};
use crate::libs::paths::PATHS;

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

    // Mirror setModelMappings(): validation/persistence failures throw -> 500.
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

/// Redact a secret string value in-place to a boolean `<key>Set` flag (true when
/// it is a non-empty string) and remove the raw value, so the effective-config
/// view never echoes a secret.
fn redact_secret(obj: &mut serde_json::Map<String, Value>, key: &str, set_key: &str) {
    let present = obj
        .get(key)
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    obj.remove(key);
    obj.insert(set_key.to_string(), json!(present));
}

/// GET /admin/config — the effective merged runtime config with all secrets
/// redacted, so an operator can confirm what the gateway is actually running
/// (budget, model slugs, feature flags, providers) without filesystem access to
/// config.json. Secrets become booleans: `auth.adminApiKey`/`anthropicApiKey` ->
/// `*Set`, `auth.apiKeys` -> a count, and each provider `apiKey` -> `apiKeySet`.
pub async fn get_effective_config_route() -> Response {
    let cfg = crate::libs::config::get_config();
    let mut value = match serde_json::to_value(&*cfg) {
        Ok(Value::Object(map)) => map,
        _ => {
            return forwarded_error("Failed to serialize effective config");
        }
    };

    // Redact auth.adminApiKey and auth.apiKeys (count only).
    if let Some(Value::Object(auth)) = value.get_mut("auth") {
        redact_secret(auth, "adminApiKey", "adminApiKeySet");
        let key_count = auth
            .get("apiKeys")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        auth.remove("apiKeys");
        auth.insert("apiKeysCount".to_string(), json!(key_count));
    }
    // Redact the top-level anthropicApiKey.
    redact_secret(&mut value, "anthropicApiKey", "anthropicApiKeySet");
    // Redact each provider's apiKey.
    if let Some(Value::Object(providers)) = value.get_mut("providers") {
        for (_name, prov) in providers.iter_mut() {
            if let Value::Object(p) = prov {
                redact_secret(p, "apiKey", "apiKeySet");
            }
        }
    }

    Json(json!({
        "configPath": config_path_string(),
        "config": value,
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
