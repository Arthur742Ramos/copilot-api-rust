//! Hardcoded Codex model catalog.
//!
//! Ported from services/codex/get-models.ts. The TS module keeps a small static
//! list of Codex model definitions and normalizes each into the Copilot
//! `/models` `Model` shape, exposing them through `getModels()`.

use once_cell::sync::Lazy;

use crate::services::copilot::get_models::{
    Model, ModelCapabilities, ModelLimits, ModelSupports, ModelsResponse,
};

/// Mirrors the TS `CodexModelDefinition` interface.
struct CodexModelDefinition {
    context_window: i64,
    id: &'static str,
    /// Allowed input modalities: "text" and/or "image".
    input: &'static [&'static str],
    max_tokens: i64,
    name: &'static str,
}

static CODEX_MODELS: Lazy<Vec<CodexModelDefinition>> = Lazy::new(|| {
    vec![
        CodexModelDefinition {
            context_window: 100_000,
            id: "gpt-5.3-codex-spark",
            input: &["text"],
            max_tokens: 32_000,
            name: "GPT-5.3 Codex Spark",
        },
        CodexModelDefinition {
            context_window: 400_000,
            id: "gpt-5.4",
            input: &["text", "image"],
            max_tokens: 128_000,
            name: "GPT-5.4",
        },
        CodexModelDefinition {
            context_window: 400_000,
            id: "gpt-5.4-mini",
            input: &["text", "image"],
            max_tokens: 128_000,
            name: "GPT-5.4 mini",
        },
        CodexModelDefinition {
            context_window: 272_000,
            id: "gpt-5.5",
            input: &["text", "image"],
            max_tokens: 128_000,
            name: "GPT-5.5",
        },
    ]
});

/// Mirrors the TS `normalizeCodexModel`: maps a Codex definition into the
/// Copilot `/models` `Model` shape.
fn normalize_codex_model(model: &CodexModelDefinition) -> Model {
    let supports_vision = model.input.contains(&"image");

    Model {
        capabilities: ModelCapabilities {
            family: "gpt".to_string(),
            limits: ModelLimits {
                max_context_window_tokens: Some(model.context_window),
                max_output_tokens: Some(model.max_tokens),
                max_prompt_tokens: Some(model.context_window),
                ..Default::default()
            },
            object: "model_capabilities".to_string(),
            supports: ModelSupports {
                adaptive_thinking: Some(true),
                parallel_tool_calls: Some(true),
                reasoning_effort: Some(vec![
                    "minimal".to_string(),
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string(),
                ]),
                streaming: Some(true),
                tool_calls: Some(true),
                vision: Some(supports_vision),
                ..Default::default()
            },
            tokenizer: "o200k_base".to_string(),
            model_type: "chat".to_string(),
            ..Default::default()
        },
        id: model.id.to_string(),
        model_picker_enabled: true,
        name: model.name.to_string(),
        object: "model".to_string(),
        preview: false,
        supported_endpoints: Some(vec!["/v1/messages".to_string(), "/v1/responses".to_string()]),
        vendor: "openai".to_string(),
        version: "chatgpt-codex".to_string(),
        policy: None,
        ..Default::default()
    }
}

/// Mirrors the TS `getModels()`: `{ object: "list", data: [...] }`.
pub fn get_codex_models() -> ModelsResponse {
    ModelsResponse {
        object: "list".to_string(),
        data: CODEX_MODELS.iter().map(normalize_codex_model).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_models_have_expected_shape() {
        let response = get_codex_models();
        assert_eq!(response.object, "list");
        assert_eq!(response.data.len(), 4);

        let spark = &response.data[0];
        assert_eq!(spark.id, "gpt-5.3-codex-spark");
        assert_eq!(spark.name, "GPT-5.3 Codex Spark");
        assert!(spark.model_picker_enabled);
        assert_eq!(spark.vendor, "openai");
        assert_eq!(spark.version, "chatgpt-codex");
        assert_eq!(spark.capabilities.family, "gpt");
        assert_eq!(spark.capabilities.tokenizer, "o200k_base");
        assert_eq!(spark.capabilities.model_type, "chat");
        assert_eq!(spark.capabilities.limits.max_context_window_tokens, Some(100_000));
        assert_eq!(spark.capabilities.limits.max_output_tokens, Some(32_000));
        assert_eq!(spark.capabilities.limits.max_prompt_tokens, Some(100_000));
        // spark is text-only -> vision false.
        assert_eq!(spark.capabilities.supports.vision, Some(false));
        assert_eq!(
            spark.supported_endpoints.as_ref().unwrap(),
            &vec!["/v1/messages".to_string(), "/v1/responses".to_string()]
        );

        // gpt-5.4 supports image input -> vision true.
        let gpt54 = &response.data[1];
        assert_eq!(gpt54.id, "gpt-5.4");
        assert_eq!(gpt54.capabilities.supports.vision, Some(true));
        assert_eq!(
            gpt54.capabilities.supports.reasoning_effort.as_ref().unwrap(),
            &vec![
                "minimal".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string()
            ]
        );
    }
}
