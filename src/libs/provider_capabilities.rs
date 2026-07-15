//! Explicit provider endpoint capability policy.
//!
//! A provider may opt into a precise capability list in `config.json`. When the
//! list is absent we derive a conservative default from its wire protocol type.
//! This keeps route registration broad while ensuring an incompatible provider
//! fails before any upstream request is made.

use crate::libs::config::ResolvedProviderConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCapability {
    Messages,
    CountTokens,
    Responses,
    ResponsesCompact,
    ChatCompletions,
    Models,
    Images,
    AlphaSearch,
}

impl ProviderCapability {
    pub const ALL: [Self; 8] = [
        Self::Messages,
        Self::CountTokens,
        Self::Responses,
        Self::ResponsesCompact,
        Self::ChatCompletions,
        Self::Models,
        Self::Images,
        Self::AlphaSearch,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::CountTokens => "count_tokens",
            Self::Responses => "responses",
            Self::ResponsesCompact => "responses_compact",
            Self::ChatCompletions => "chat_completions",
            Self::Models => "models",
            Self::Images => "images",
            Self::AlphaSearch => "alpha_search",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL
            .into_iter()
            .find(|capability| capability.as_str() == normalized)
    }
}

pub fn default_capabilities(provider_type: &str) -> Vec<ProviderCapability> {
    let mut capabilities = vec![
        ProviderCapability::Messages,
        ProviderCapability::CountTokens,
        ProviderCapability::Models,
    ];
    match provider_type {
        "openai-compatible" => {
            capabilities.push(ProviderCapability::ChatCompletions);
            capabilities.push(ProviderCapability::Images);
        }
        "openai-responses" => {
            capabilities.push(ProviderCapability::Responses);
            capabilities.push(ProviderCapability::ResponsesCompact);
            capabilities.push(ProviderCapability::Images);
            capabilities.push(ProviderCapability::AlphaSearch);
        }
        "anthropic" => {}
        _ => {}
    }
    capabilities
}

pub fn supports(config: &ResolvedProviderConfig, capability: ProviderCapability) -> bool {
    match config.capabilities.as_ref() {
        Some(configured) => configured
            .iter()
            .filter_map(|value| ProviderCapability::parse(value))
            .any(|candidate| candidate == capability),
        None => default_capabilities(&config.provider_type).contains(&capability),
    }
}

pub fn normalized_capability_names(
    provider_type: &str,
    values: Option<&[String]>,
) -> Result<Vec<String>, String> {
    let capabilities = match values {
        Some(values) => {
            let mut parsed = Vec::new();
            for value in values {
                let capability = ProviderCapability::parse(value)
                    .ok_or_else(|| format!("Unknown provider capability '{}'", value.trim()))?;
                if !parsed.contains(&capability) {
                    parsed.push(capability);
                }
            }
            parsed
        }
        None => default_capabilities(provider_type),
    };
    Ok(capabilities
        .into_iter()
        .map(|capability| capability.as_str().to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::config::ResolvedProviderConfig;

    fn config(provider_type: &str, capabilities: Option<Vec<&str>>) -> ResolvedProviderConfig {
        ResolvedProviderConfig {
            name: "fixture".to_string(),
            provider_type: provider_type.to_string(),
            base_url: "https://example.com".to_string(),
            api_key: "secret".to_string(),
            auth_type: "authorization".to_string(),
            models: None,
            capabilities: capabilities
                .map(|values| values.into_iter().map(str::to_string).collect()),
            adjust_input_tokens: None,
        }
    }

    #[test]
    fn conservative_defaults_match_wire_protocols() {
        assert!(supports(
            &config("openai-responses", None),
            ProviderCapability::AlphaSearch
        ));
        assert!(!supports(
            &config("openai-compatible", None),
            ProviderCapability::Responses
        ));
        assert!(!supports(
            &config("anthropic", None),
            ProviderCapability::Images
        ));
    }

    #[test]
    fn explicit_capabilities_override_defaults() {
        let config = config("openai-compatible", Some(vec!["responses", "alpha-search"]));
        assert!(supports(&config, ProviderCapability::Responses));
        assert!(supports(&config, ProviderCapability::AlphaSearch));
        assert!(!supports(&config, ProviderCapability::ChatCompletions));
    }
}
