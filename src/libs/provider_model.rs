use serde_json::json;

use crate::services::copilot::get_models::Model;

/// Mirrors src/lib/provider-model.ts.

pub struct ProviderModelAlias {
    pub model: String,
    pub provider: String,
}

pub fn parse_provider_model_alias(model: &str) -> Option<ProviderModelAlias> {
    let separator_index = model.find('/')?;
    if separator_index == 0 || separator_index == model.len() - 1 {
        return None;
    }
    let provider = model[..separator_index].trim().to_string();
    let provider_model = model[separator_index + 1..].trim().to_string();
    if provider.is_empty() || provider_model.is_empty() {
        return None;
    }
    Some(ProviderModelAlias {
        model: provider_model,
        provider,
    })
}

/// Build a minimal fallback Model for provider-routed model IDs. Deserializes
/// from the same JSON shape the TS `createFallbackModel` produces.
pub fn create_fallback_model(model_id: &str) -> Model {
    serde_json::from_value(json!({
        "capabilities": {
            "family": "provider",
            "limits": {},
            "object": "model_capabilities",
            "supports": {},
            "tokenizer": "o200k_base",
            "type": "chat",
        },
        "id": model_id,
        "model_picker_enabled": false,
        "name": model_id,
        "object": "model",
        "preview": false,
        "vendor": "provider",
        "version": "unknown",
    }))
    .expect("fallback model is valid")
}
