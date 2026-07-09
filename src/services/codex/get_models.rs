//! Curated Codex model catalog.
//!
//! Codex exposes an authenticated models endpoint, but the proxy currently
//! serves a local snapshot so model listing stays deterministic and available
//! without an upstream request.

use crate::services::copilot::get_models::{
    Model, ModelCapabilities, ModelLimits, ModelSupports, ModelsResponse,
};

/// Mirrors the TS `CodexModelDefinition` interface.
struct CodexModelDefinition {
    id: &'static str,
    /// Allowed input modalities: "text" and/or "image".
    input: &'static [&'static str],
    max_context_window: i64,
    max_prompt_tokens: i64,
    name: &'static str,
    reasoning_efforts: &'static [&'static str],
}

// Reasoning levels follow openai/codex `codex-rs/models-manager/models.json`;
// limits follow the authenticated model catalog so opt-in 1M variants remain
// visible. Keep entries that are API-supported and visible.
const CODEX_MODELS: &[CodexModelDefinition] = &[
    CodexModelDefinition {
        id: "gpt-5.6-sol",
        input: &["text", "image"],
        max_context_window: 1_050_000,
        max_prompt_tokens: 922_000,
        name: "GPT-5.6-Sol",
        reasoning_efforts: &["low", "medium", "high", "xhigh", "max", "ultra"],
    },
    CodexModelDefinition {
        id: "gpt-5.6-terra",
        input: &["text", "image"],
        max_context_window: 1_050_000,
        max_prompt_tokens: 922_000,
        name: "GPT-5.6-Terra",
        reasoning_efforts: &["low", "medium", "high", "xhigh", "max", "ultra"],
    },
    CodexModelDefinition {
        id: "gpt-5.6-luna",
        input: &["text", "image"],
        max_context_window: 1_050_000,
        max_prompt_tokens: 922_000,
        name: "GPT-5.6-Luna",
        reasoning_efforts: &["low", "medium", "high", "xhigh", "max"],
    },
    CodexModelDefinition {
        id: "gpt-5.5",
        input: &["text", "image"],
        max_context_window: 1_050_000,
        max_prompt_tokens: 922_000,
        name: "GPT-5.5",
        reasoning_efforts: &["low", "medium", "high", "xhigh"],
    },
    CodexModelDefinition {
        id: "gpt-5.4",
        input: &["text", "image"],
        max_context_window: 1_050_000,
        max_prompt_tokens: 922_000,
        name: "GPT-5.4",
        reasoning_efforts: &["low", "medium", "high", "xhigh"],
    },
    CodexModelDefinition {
        id: "gpt-5.4-mini",
        input: &["text", "image"],
        max_context_window: 272_000,
        max_prompt_tokens: 272_000,
        name: "GPT-5.4-Mini",
        reasoning_efforts: &["low", "medium", "high", "xhigh"],
    },
    CodexModelDefinition {
        id: "gpt-5.2",
        input: &["text", "image"],
        max_context_window: 272_000,
        max_prompt_tokens: 272_000,
        name: "GPT-5.2",
        reasoning_efforts: &["low", "medium", "high", "xhigh"],
    },
];

/// Mirrors the TS `normalizeCodexModel`: maps a Codex definition into the
/// Copilot `/models` `Model` shape.
fn normalize_codex_model(model: &CodexModelDefinition) -> Model {
    let supports_vision = model.input.contains(&"image");

    Model {
        capabilities: ModelCapabilities {
            family: "gpt".to_string(),
            limits: ModelLimits {
                max_context_window_tokens: Some(model.max_context_window),
                max_prompt_tokens: Some(model.max_prompt_tokens),
                // Codex does not publish a separate output cap in its catalog.
                max_output_tokens: None,
                ..Default::default()
            },
            object: "model_capabilities".to_string(),
            supports: ModelSupports {
                adaptive_thinking: Some(true),
                parallel_tool_calls: Some(true),
                reasoning_effort: Some(
                    model
                        .reasoning_efforts
                        .iter()
                        .map(|effort| (*effort).to_string())
                        .collect(),
                ),
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
        supported_endpoints: Some(vec![
            "/v1/messages".to_string(),
            "/v1/responses".to_string(),
        ]),
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
        assert_eq!(
            response
                .data
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.2",
            ]
        );

        for model in &response.data {
            assert!(model.model_picker_enabled);
            assert_eq!(model.vendor, "openai");
            assert_eq!(model.version, "chatgpt-codex");
            assert_eq!(model.capabilities.family, "gpt");
            assert_eq!(model.capabilities.tokenizer, "o200k_base");
            assert_eq!(model.capabilities.model_type, "chat");
            assert_eq!(model.capabilities.supports.vision, Some(true));
            assert_eq!(model.capabilities.limits.max_output_tokens, None);
            assert_eq!(
                model.supported_endpoints.as_ref().unwrap(),
                &vec!["/v1/messages".to_string(), "/v1/responses".to_string()]
            );
        }

        let sol = &response.data[0];
        assert_eq!(sol.name, "GPT-5.6-Sol");
        assert_eq!(
            sol.capabilities.limits.max_context_window_tokens,
            Some(1_050_000)
        );
        assert_eq!(sol.capabilities.limits.max_prompt_tokens, Some(922_000));
        assert_eq!(
            sol.capabilities.supports.reasoning_effort.as_deref(),
            Some(
                ["low", "medium", "high", "xhigh", "max", "ultra"]
                    .map(String::from)
                    .as_slice()
            )
        );

        let luna = &response.data[2];
        assert_eq!(
            luna.capabilities.supports.reasoning_effort.as_deref(),
            Some(
                ["low", "medium", "high", "xhigh", "max"]
                    .map(String::from)
                    .as_slice()
            )
        );

        let gpt54 = &response.data[4];
        assert_eq!(
            gpt54.capabilities.limits.max_context_window_tokens,
            Some(1_050_000)
        );
        assert_eq!(gpt54.capabilities.limits.max_prompt_tokens, Some(922_000));
        assert_eq!(
            gpt54.capabilities.supports.reasoning_effort.as_deref(),
            Some(
                ["low", "medium", "high", "xhigh"]
                    .map(String::from)
                    .as_slice()
            )
        );
    }
}
