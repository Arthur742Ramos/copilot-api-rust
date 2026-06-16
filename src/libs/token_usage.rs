use serde::Serialize;
use serde_json::Value;

use crate::libs::request_context::request_context_store;
use crate::libs::state;

/// Mirrors `UsageTokens` in src/lib/token-usage/store.ts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTokens {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i64>,
}

pub type TokenUsageEndpoint = &'static str; // chat_completions | embeddings | messages | provider_messages | responses
pub type TokenUsageSource = &'static str; // copilot | provider

fn normalize_token(value: Option<f64>) -> i64 {
    match value {
        Some(v) if v.is_finite() => v.floor().max(0.0) as i64,
        _ => 0,
    }
}

fn normalize_optional_token(value: Option<f64>) -> Option<i64> {
    value.map(|v| {
        if v.is_finite() {
            v.floor().max(0.0) as i64
        } else {
            0
        }
    })
}

fn nested_f64(usage: Option<&Value>, parent: &str, key: &str) -> Option<f64> {
    usage?.get(parent)?.get(key)?.as_f64()
}

fn top_f64(usage: Option<&Value>, key: &str) -> Option<f64> {
    usage?.get(key)?.as_f64()
}

pub fn has_any_token(tokens: &UsageTokens) -> bool {
    normalize_token(tokens.input_tokens.map(|v| v as f64)) > 0
        || normalize_token(tokens.output_tokens.map(|v| v as f64)) > 0
        || normalize_token(tokens.cache_read_input_tokens.map(|v| v as f64)) > 0
        || normalize_token(tokens.cache_creation_input_tokens.map(|v| v as f64)) > 0
        || normalize_token(tokens.total_tokens.map(|v| v as f64)) > 0
}

/// Mirrors `normalizeOpenAIUsage`.
pub fn normalize_openai_usage(usage: Option<&Value>) -> UsageTokens {
    let cached = normalize_token(nested_f64(usage, "prompt_tokens_details", "cached_tokens"));
    let cache_creation = normalize_token(nested_f64(
        usage,
        "prompt_tokens_details",
        "cache_creation_input_tokens",
    ));
    let prompt = normalize_token(top_f64(usage, "prompt_tokens"));
    UsageTokens {
        cache_creation_input_tokens: Some(cache_creation),
        cache_read_input_tokens: Some(cached),
        input_tokens: Some((prompt - cached - cache_creation).max(0)),
        output_tokens: Some(normalize_token(top_f64(usage, "completion_tokens"))),
        total_tokens: normalize_optional_token(top_f64(usage, "total_tokens")),
    }
}

/// Mirrors `normalizeResponsesUsage`.
pub fn normalize_responses_usage(usage: Option<&Value>) -> UsageTokens {
    let cached = normalize_token(nested_f64(usage, "input_tokens_details", "cached_tokens"));
    let input = normalize_token(top_f64(usage, "input_tokens"));
    UsageTokens {
        cache_creation_input_tokens: None,
        cache_read_input_tokens: Some(cached),
        input_tokens: Some((input - cached).max(0)),
        output_tokens: Some(normalize_token(top_f64(usage, "output_tokens"))),
        total_tokens: normalize_optional_token(top_f64(usage, "total_tokens")),
    }
}

/// Mirrors `normalizeAnthropicUsage`.
pub fn normalize_anthropic_usage(usage: Option<&Value>) -> UsageTokens {
    UsageTokens {
        cache_creation_input_tokens: normalize_optional_token(top_f64(
            usage,
            "cache_creation_input_tokens",
        )),
        cache_read_input_tokens: normalize_optional_token(top_f64(usage, "cache_read_input_tokens")),
        input_tokens: normalize_optional_token(top_f64(usage, "input_tokens")),
        output_tokens: normalize_optional_token(top_f64(usage, "output_tokens")),
        total_tokens: normalize_optional_token(top_f64(usage, "total_tokens")),
    }
}

/// Mirrors `mergeAnthropicUsage` (next overrides current when present).
pub fn merge_anthropic_usage(current: UsageTokens, next: UsageTokens) -> UsageTokens {
    UsageTokens {
        cache_creation_input_tokens: next
            .cache_creation_input_tokens
            .or(current.cache_creation_input_tokens),
        cache_read_input_tokens: next.cache_read_input_tokens.or(current.cache_read_input_tokens),
        input_tokens: next.input_tokens.or(current.input_tokens),
        output_tokens: next.output_tokens.or(current.output_tokens),
        total_tokens: next.total_tokens.or(current.total_tokens),
    }
}

pub fn resolve_token_usage_session_id(
    session_id: Option<&str>,
    fallback_session_id: Option<&str>,
) -> String {
    if let Some(affinity) = request_context_store().and_then(|c| c.session_affinity) {
        let trimmed = affinity.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(s) = session_id {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(s) = fallback_session_id {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    String::new()
}

/// SQLite-backed usage storage is a deferred subsystem; until it lands, storage
/// is disabled and the recorder is a no-op (matching the TS behavior when the
/// SQLite runtime is unavailable).
pub fn is_token_usage_storage_enabled() -> bool {
    false
}

/// Mirrors the closure returned by `createCopilotTokenUsageRecorder` /
/// `createProviderTokenUsageRecorder`. Holds the recorder options and records a
/// usage event when invoked.
pub struct TokenUsageRecorder {
    pub endpoint: TokenUsageEndpoint,
    pub source: TokenUsageSource,
    pub model: String,
    pub provider_name: Option<String>,
    pub session_id: Option<String>,
    pub fallback_session_id: Option<String>,
}

impl TokenUsageRecorder {
    pub fn record(&self, usage: UsageTokens) {
        if !is_token_usage_storage_enabled() {
            return;
        }
        if !has_any_token(&usage) {
            return;
        }
        // Storage is disabled in this build; the resolved identifiers are
        // computed for parity but the event is dropped.
        let _session = resolve_token_usage_session_id(
            self.session_id.as_deref(),
            self.fallback_session_id.as_deref(),
        );
        let _user = if self.source == "provider" {
            self.provider_name.clone().unwrap_or_default()
        } else {
            state::with_state(|s| s.user_name.clone().unwrap_or_default())
        };
    }
}

pub fn create_copilot_token_usage_recorder(
    endpoint: TokenUsageEndpoint,
    model: impl Into<String>,
    fallback_session_id: Option<String>,
) -> TokenUsageRecorder {
    TokenUsageRecorder {
        endpoint,
        source: "copilot",
        model: model.into(),
        provider_name: None,
        session_id: None,
        fallback_session_id,
    }
}

pub fn create_provider_token_usage_recorder(
    endpoint: TokenUsageEndpoint,
    model: impl Into<String>,
    provider_name: impl Into<String>,
    fallback_session_id: Option<String>,
) -> TokenUsageRecorder {
    TokenUsageRecorder {
        endpoint,
        source: "provider",
        model: model.into(),
        provider_name: Some(provider_name.into()),
        session_id: None,
        fallback_session_id,
    }
}
