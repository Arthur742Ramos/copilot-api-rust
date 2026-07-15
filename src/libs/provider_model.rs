use serde_json::json;

use crate::libs::models::CONTEXT_1M_SUFFIX;
use crate::services::copilot::get_models::Model;

// Mirrors src/lib/provider-model.ts.

pub const CLAUDE_CODE_DISCOVERY_ALIAS_PREFIX: &str = "claude-copilot:";

pub struct ProviderModelAlias {
    pub model: String,
    pub provider: String,
}

/// Build an ID that passes Claude Code's gateway-discovery filter while
/// retaining the real Copilot model ID verbatim after a reversible prefix.
pub fn create_claude_code_discovery_alias(model_id: &str, context_1m: bool) -> String {
    let suffix = if context_1m { CONTEXT_1M_SUFFIX } else { "" };
    format!("{CLAUDE_CODE_DISCOVERY_ALIAS_PREFIX}{model_id}{suffix}")
}

/// Resolve a picker-safe Claude Code discovery alias back to the model ID the
/// proxy already understands. The optional 1M suffix remains attached so normal
/// context-beta handling still applies when the client sends it verbatim.
pub fn resolve_claude_code_discovery_alias(model_id: &str) -> Option<String> {
    let target = model_id.strip_prefix(CLAUDE_CODE_DISCOVERY_ALIAS_PREFIX)?;
    if target.trim().is_empty() {
        return None;
    }
    Some(target.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_discovery_alias_round_trips() {
        let alias = create_claude_code_discovery_alias("gpt-5.6-sol", false);
        assert_eq!(alias, "claude-copilot:gpt-5.6-sol");
        assert_eq!(
            resolve_claude_code_discovery_alias(&alias).as_deref(),
            Some("gpt-5.6-sol")
        );
    }

    #[test]
    fn claude_code_discovery_alias_preserves_1m_suffix() {
        let alias = create_claude_code_discovery_alias("gpt-5.6-sol", true);
        assert_eq!(alias, "claude-copilot:gpt-5.6-sol[1m]");
        assert_eq!(
            resolve_claude_code_discovery_alias(&alias).as_deref(),
            Some("gpt-5.6-sol[1m]")
        );
    }

    #[test]
    fn rejects_empty_claude_code_discovery_alias() {
        assert_eq!(
            resolve_claude_code_discovery_alias(CLAUDE_CODE_DISCOVERY_ALIAS_PREFIX),
            None
        );
    }
}
