//! Guided, testable provider onboarding.
//!
//! Selection and validation are pure apart from an injected prompt interface
//! and environment lookup. The binary performs OAuth and persistence after this
//! module returns a plan, so tests never need a terminal, credential service, or
//! network.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};

use serde_json::{Map, Value};

use crate::libs::config::{ModelConfig, ProviderConfig};
use crate::libs::provider_capabilities::normalized_capability_names;

pub const PROVIDER_TYPES: [&str; 3] = ["anthropic", "openai-compatible", "openai-responses"];
pub const AUTH_TYPES: [&str; 2] = ["x-api-key", "authorization"];
pub const AUTH_PROVIDERS: [&str; 7] = [
    "copilot",
    "codex",
    "opencode-go",
    "deepseek",
    "dashscope",
    "openrouter",
    "custom",
];

#[derive(Debug, Clone, Default)]
pub struct AuthSetupOptions {
    pub provider: Option<String>,
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub base_url: Option<String>,
    pub auth_type: Option<String>,
    pub api_key_env: Option<String>,
    pub models: Vec<String>,
    pub capabilities: Vec<String>,
    pub probe: bool,
}

pub enum AuthPlan {
    Copilot,
    Codex,
    Configure(Box<ProviderSetupPlan>),
}

/// Deliberately does not implement `Debug`: it contains a credential.
pub struct ProviderSetupPlan {
    pub provider_name: String,
    pub config: ProviderConfig,
    pub api_key: String,
    pub probe: bool,
}

pub trait PromptIo {
    fn is_interactive(&self) -> bool;
    fn choose(&mut self, prompt: &str, choices: &[(&str, &str)]) -> anyhow::Result<String>;
    fn text(&mut self, prompt: &str, default: Option<&str>) -> anyhow::Result<String>;
    fn secret(&mut self, prompt: &str) -> anyhow::Result<String>;
    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool>;
}

pub struct TerminalPrompt {
    interactive: bool,
}

impl TerminalPrompt {
    pub fn detect() -> Self {
        Self {
            interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        }
    }

    pub fn is_interactive_terminal(&self) -> bool {
        self.interactive
    }

    fn read_line(&self, prompt: &str) -> anyhow::Result<String> {
        if !self.interactive {
            anyhow::bail!("Interactive input is unavailable on this terminal");
        }
        print!("{prompt}");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().lock().read_line(&mut input)?;
        Ok(input.trim().to_string())
    }
}

pub fn require_interactive_oauth(plan: &AuthPlan, interactive: bool) -> anyhow::Result<()> {
    if interactive || matches!(plan, AuthPlan::Configure(_)) {
        return Ok(());
    }
    let provider = match plan {
        AuthPlan::Copilot => "copilot",
        AuthPlan::Codex => "codex",
        AuthPlan::Configure(_) => unreachable!(),
    };
    anyhow::bail!(
        "Built-in OAuth provider '{provider}' requires an interactive terminal. \
         Run `copilot-api auth --provider {provider}` from a TTY. Preconfigured \
         services should reuse the protected credential store instead of invoking auth."
    )
}

impl PromptIo for TerminalPrompt {
    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn choose(&mut self, prompt: &str, choices: &[(&str, &str)]) -> anyhow::Result<String> {
        if !self.interactive {
            anyhow::bail!("{prompt} requires an interactive terminal");
        }
        println!("{prompt}:");
        for (index, (value, label)) in choices.iter().enumerate() {
            println!("  {}) {label} ({value})", index + 1);
        }
        let answer = self.read_line("> ")?;
        if let Ok(index) = answer.parse::<usize>() {
            if let Some((value, _)) = choices.get(index.saturating_sub(1)) {
                return Ok((*value).to_string());
            }
        }
        choices
            .iter()
            .find(|(value, _)| answer == *value)
            .map(|(value, _)| (*value).to_string())
            .ok_or_else(|| anyhow::anyhow!("Invalid selection '{answer}'"))
    }

    fn text(&mut self, prompt: &str, default: Option<&str>) -> anyhow::Result<String> {
        let suffix = default
            .filter(|value| !value.is_empty())
            .map(|value| format!(" [{value}]: "))
            .unwrap_or_else(|| ": ".to_string());
        let answer = self.read_line(&format!("{prompt}{suffix}"))?;
        if answer.is_empty() {
            Ok(default.unwrap_or_default().to_string())
        } else {
            Ok(answer)
        }
    }

    fn secret(&mut self, prompt: &str) -> anyhow::Result<String> {
        if !self.interactive {
            anyhow::bail!("{prompt} requires an interactive terminal");
        }
        rpassword::prompt_password(format!("{prompt}: ")).map_err(Into::into)
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        let hint = if default { "Y/n" } else { "y/N" };
        let answer = self.read_line(&format!("{prompt} [{hint}]: "))?;
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.to_ascii_lowercase().as_str() {
            "y" | "yes" => Ok(true),
            "n" | "no" => Ok(false),
            _ => anyhow::bail!("Expected yes or no"),
        }
    }
}

#[derive(Clone, Copy)]
struct QuickProvider {
    provider_type: &'static str,
    base_url: &'static str,
    pricing_currency: &'static str,
    editable_type: bool,
}

fn quick_provider(name: &str) -> Option<QuickProvider> {
    match name {
        "opencode-go" => Some(QuickProvider {
            provider_type: "openai-compatible",
            base_url: "https://opencode.ai/zen/go",
            pricing_currency: "USD",
            editable_type: false,
        }),
        "deepseek" => Some(QuickProvider {
            provider_type: "anthropic",
            base_url: "https://api.deepseek.com/anthropic",
            pricing_currency: "CNY",
            editable_type: true,
        }),
        "dashscope" => Some(QuickProvider {
            provider_type: "openai-compatible",
            base_url: "https://dashscope.aliyuncs.com/compatible-mode",
            pricing_currency: "CNY",
            editable_type: true,
        }),
        "openrouter" => Some(QuickProvider {
            provider_type: "anthropic",
            base_url: "https://openrouter.ai/api",
            pricing_currency: "USD",
            editable_type: false,
        }),
        _ => None,
    }
}

fn provider_label(name: &str) -> &'static str {
    match name {
        "copilot" => "GitHub Copilot",
        "codex" => "OpenAI Codex",
        "opencode-go" => "OpenCode Go",
        "deepseek" => "DeepSeek",
        "dashscope" => "DashScope",
        "openrouter" => "OpenRouter",
        "custom" => "Custom provider",
        _ => "Provider",
    }
}

pub fn validate_provider_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Provider name must be a non-empty string".to_string());
    }
    let valid = name.chars().enumerate().all(|(index, character)| {
        character.is_ascii_alphanumeric() || (index > 0 && (character == '_' || character == '-'))
    });
    if !valid {
        return Err(
            "Provider name must start with a letter or number and contain only letters, numbers, underscores, or hyphens"
                .to_string(),
        );
    }
    if matches!(name, "copilot" | "codex") {
        return Err(format!(
            "Provider name '{name}' is reserved for a builtin provider"
        ));
    }
    Ok(name.to_string())
}

fn validate_choice(value: &str, choices: &[&str], field: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if choices.contains(&value) {
        Ok(value.to_string())
    } else {
        anyhow::bail!(
            "Invalid {field} '{value}'. Expected one of: {}",
            choices.join(", ")
        )
    }
}

fn validate_base_url(value: &str) -> anyhow::Result<String> {
    let normalized = crate::libs::config::normalize_provider_base_url(value);
    let parsed = url::Url::parse(&normalized)
        .map_err(|error| anyhow::anyhow!("Invalid provider base URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("Provider base URL must use http or https");
    }
    if parsed.host_str().is_none() || !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("Provider base URL must have a host and must not contain credentials");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("Provider base URL must not contain a query string or fragment");
    }
    Ok(normalized)
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn normalize_models(values: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut models = Vec::new();
    for value in values {
        for model in split_csv(&value) {
            if model.chars().any(char::is_control) {
                anyhow::bail!("Model names must not contain control characters");
            }
            if !models.contains(&model) {
                models.push(model);
            }
        }
    }
    Ok(models)
}

fn provider_env_name(provider: &str) -> String {
    format!(
        "COPILOT_API_PROVIDER_{}_API_KEY",
        provider
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    )
}

fn resolve_secret(
    options: &AuthSetupOptions,
    provider: &str,
    io: &mut dyn PromptIo,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<String> {
    let names = if let Some(name) = options.api_key_env.as_deref() {
        vec![name.to_string()]
    } else {
        vec![
            provider_env_name(provider),
            "COPILOT_API_PROVIDER_API_KEY".to_string(),
        ]
    };
    for name in names {
        if let Some(value) = env(&name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Ok(value);
        }
    }
    if !io.is_interactive() {
        anyhow::bail!(
            "Provider API key is required in non-interactive mode. Set {} or pass --api-key-env <NAME>",
            provider_env_name(provider)
        );
    }
    let value = io.secret(&format!("Enter {provider} API key"))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        anyhow::bail!("API key must be a non-empty string");
    }
    Ok(value)
}

fn choose_provider(options: &AuthSetupOptions, io: &mut dyn PromptIo) -> anyhow::Result<String> {
    if let Some(provider) = options.provider.as_deref() {
        let provider = provider.trim();
        if AUTH_PROVIDERS.contains(&provider) {
            return Ok(provider.to_string());
        }
        anyhow::bail!(
            "Unknown provider '{provider}'. Expected one of: {}",
            AUTH_PROVIDERS.join(", ")
        );
    }
    if !io.is_interactive() {
        // Preserve the legacy automation behavior and, critically, never prompt
        // from a pipe/service.
        return Ok("copilot".to_string());
    }
    let choices = AUTH_PROVIDERS
        .iter()
        .map(|provider| (*provider, provider_label(provider)))
        .collect::<Vec<_>>();
    io.choose("Select a provider to authenticate or configure", &choices)
}

fn prompt_or_option(
    option: Option<&str>,
    io: &mut dyn PromptIo,
    prompt: &str,
    default: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(value) = option.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(value.to_string());
    }
    if io.is_interactive() {
        return io.text(prompt, default);
    }
    default
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("{prompt} is required in non-interactive mode"))
}

pub fn build_auth_plan(
    options: &AuthSetupOptions,
    io: &mut dyn PromptIo,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<AuthPlan> {
    let selected = choose_provider(options, io)?;
    if selected == "copilot" {
        return Ok(AuthPlan::Copilot);
    }
    if selected == "codex" {
        return Ok(AuthPlan::Codex);
    }

    let quick = quick_provider(&selected);
    let provider_name = if selected == "custom" {
        validate_provider_name(&prompt_or_option(
            options.name.as_deref(),
            io,
            "Provider name",
            None,
        )?)
        .map_err(anyhow::Error::msg)?
    } else {
        selected.clone()
    };

    let default_type = quick.map(|provider| provider.provider_type);
    if let (Some(quick), Some(requested)) = (quick, options.provider_type.as_deref()) {
        if !quick.editable_type && requested.trim() != quick.provider_type {
            anyhow::bail!(
                "Provider '{selected}' uses fixed type '{}'; --type cannot override it",
                quick.provider_type
            );
        }
    }
    let provider_type = if quick.is_some_and(|provider| !provider.editable_type)
        && options.provider_type.is_none()
    {
        default_type.unwrap_or("anthropic").to_string()
    } else {
        let value = prompt_or_option(
            options.provider_type.as_deref(),
            io,
            "Provider protocol type",
            default_type,
        )?;
        validate_choice(&value, &PROVIDER_TYPES, "provider type")?
    };

    let base_url = validate_base_url(&prompt_or_option(
        options.base_url.as_deref(),
        io,
        "Provider base URL",
        quick.map(|provider| provider.base_url),
    )?)?;
    let default_auth = if provider_type == "anthropic" {
        "x-api-key"
    } else {
        "authorization"
    };
    let auth_type = if selected == "custom" || options.auth_type.is_some() {
        let value = prompt_or_option(
            options.auth_type.as_deref(),
            io,
            "Provider authentication mode",
            Some(default_auth),
        )?;
        validate_choice(&value, &AUTH_TYPES, "authentication mode")?
    } else {
        default_auth.to_string()
    };

    let mut model_values = options.models.clone();
    if model_values.is_empty() && io.is_interactive() {
        let value = io.text("Model names (comma-separated, optional)", Some(""))?;
        model_values.extend(split_csv(&value));
    }
    let models = normalize_models(model_values)?;

    let default_capabilities =
        normalized_capability_names(&provider_type, None).map_err(anyhow::Error::msg)?;
    let capability_values = if options.capabilities.is_empty() && io.is_interactive() {
        split_csv(&io.text(
            "Capabilities (comma-separated)",
            Some(&default_capabilities.join(",")),
        )?)
    } else if options.capabilities.is_empty() {
        default_capabilities
    } else {
        options
            .capabilities
            .iter()
            .flat_map(|value| split_csv(value))
            .collect()
    };
    let capabilities = normalized_capability_names(&provider_type, Some(&capability_values))
        .map_err(anyhow::Error::msg)?;
    let api_key = resolve_secret(options, &provider_name, io, &env)?;
    let probe = options.probe
        || (io.is_interactive() && io.confirm("Run a bounded provider health probe", true)?);

    let mut extra = Map::new();
    if let Some(quick) = quick {
        extra.insert(
            "pricingCurrency".to_string(),
            Value::String(quick.pricing_currency.to_string()),
        );
    }
    let model_config = if models.is_empty() {
        None
    } else {
        Some(
            models
                .into_iter()
                .map(|model| (model, ModelConfig::default()))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    Ok(AuthPlan::Configure(Box::new(ProviderSetupPlan {
        provider_name,
        config: ProviderConfig {
            provider_type: Some(provider_type),
            enabled: Some(true),
            base_url: Some(base_url),
            api_key: None,
            auth_type: Some(auth_type),
            models: model_config,
            capabilities: Some(capabilities),
            adjust_input_tokens: None,
            extra,
        },
        api_key,
        probe,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct ScriptedPrompt {
        interactive: bool,
        responses: VecDeque<String>,
    }

    impl ScriptedPrompt {
        fn interactive(responses: &[&str]) -> Self {
            Self {
                interactive: true,
                responses: responses.iter().map(|value| (*value).to_string()).collect(),
            }
        }

        fn next(&mut self) -> anyhow::Result<String> {
            self.responses
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("unexpected prompt"))
        }
    }

    impl PromptIo for ScriptedPrompt {
        fn is_interactive(&self) -> bool {
            self.interactive
        }
        fn choose(&mut self, _prompt: &str, _choices: &[(&str, &str)]) -> anyhow::Result<String> {
            self.next()
        }
        fn text(&mut self, _prompt: &str, default: Option<&str>) -> anyhow::Result<String> {
            let value = self.next()?;
            if value.is_empty() {
                Ok(default.unwrap_or_default().to_string())
            } else {
                Ok(value)
            }
        }
        fn secret(&mut self, _prompt: &str) -> anyhow::Result<String> {
            self.next()
        }
        fn confirm(&mut self, _prompt: &str, default: bool) -> anyhow::Result<bool> {
            let value = self.next()?;
            if value.is_empty() {
                return Ok(default);
            }
            Ok(matches!(value.as_str(), "y" | "yes"))
        }
    }

    #[test]
    fn non_interactive_without_provider_preserves_copilot_default() {
        let mut io = ScriptedPrompt {
            interactive: false,
            responses: VecDeque::new(),
        };
        assert!(matches!(
            build_auth_plan(&AuthSetupOptions::default(), &mut io, |_| None).unwrap(),
            AuthPlan::Copilot
        ));
    }

    #[test]
    fn guided_quick_provider_keeps_secret_out_of_config() {
        // provider, editable type, base URL, models, capabilities, secret, probe
        let mut io = ScriptedPrompt::interactive(&[
            "deepseek",
            "",
            "",
            "deepseek-chat",
            "",
            "super-secret",
            "n",
        ]);
        let AuthPlan::Configure(plan) =
            build_auth_plan(&AuthSetupOptions::default(), &mut io, |_| None).unwrap()
        else {
            panic!("expected provider plan")
        };
        assert_eq!(plan.provider_name, "deepseek");
        assert_eq!(plan.api_key, "super-secret");
        assert_eq!(plan.config.provider_type.as_deref(), Some("anthropic"));
        assert_eq!(
            plan.config.base_url.as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );
        assert!(plan.config.api_key.is_none());
        let json = serde_json::to_string(&plan.config).unwrap();
        assert!(!json.contains("super-secret"));
        assert!(plan
            .config
            .models
            .as_ref()
            .unwrap()
            .contains_key("deepseek-chat"));
    }

    #[test]
    fn custom_non_interactive_configuration_uses_named_environment_secret() {
        let options = AuthSetupOptions {
            provider: Some("custom".to_string()),
            name: Some("team-openai".to_string()),
            provider_type: Some("openai-responses".to_string()),
            base_url: Some("https://api.example.com/root".to_string()),
            api_key_env: Some("TEAM_OPENAI_KEY".to_string()),
            capabilities: vec!["responses,alpha_search".to_string()],
            ..Default::default()
        };
        let mut io = ScriptedPrompt {
            interactive: false,
            responses: VecDeque::new(),
        };
        let AuthPlan::Configure(plan) = build_auth_plan(&options, &mut io, |name| {
            (name == "TEAM_OPENAI_KEY").then(|| "secret".to_string())
        })
        .unwrap() else {
            panic!("expected provider plan")
        };
        assert_eq!(
            plan.config.capabilities.unwrap(),
            vec!["responses", "alpha_search"]
        );
    }

    #[test]
    fn non_interactive_provider_never_prompts_for_missing_secret() {
        let options = AuthSetupOptions {
            provider: Some("openrouter".to_string()),
            ..Default::default()
        };
        let mut io = ScriptedPrompt {
            interactive: false,
            responses: VecDeque::new(),
        };
        let error = match build_auth_plan(&options, &mut io, |_| None) {
            Ok(_) => panic!("missing secret should fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("non-interactive"));
    }

    #[test]
    fn rejects_credential_bearing_or_non_http_base_urls() {
        assert!(validate_base_url("file:///tmp/provider").is_err());
        assert!(validate_base_url("https://user:pass@example.com").is_err());
        assert!(validate_base_url("https://example.com?q=secret").is_err());
    }

    #[test]
    fn fixed_quick_provider_type_cannot_be_overridden() {
        let options = AuthSetupOptions {
            provider: Some("openrouter".to_string()),
            provider_type: Some("openai-responses".to_string()),
            ..Default::default()
        };
        let mut io = ScriptedPrompt {
            interactive: false,
            responses: VecDeque::new(),
        };
        let error = match build_auth_plan(&options, &mut io, |_| Some("key".to_string())) {
            Ok(_) => panic!("fixed quick-provider type should reject override"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("fixed type"));
    }

    #[test]
    fn built_in_oauth_requires_tty_but_provider_configuration_does_not() {
        assert!(require_interactive_oauth(&AuthPlan::Copilot, false).is_err());
        assert!(require_interactive_oauth(&AuthPlan::Codex, false).is_err());
        assert!(require_interactive_oauth(&AuthPlan::Copilot, true).is_ok());
        let plan = AuthPlan::Configure(Box::new(ProviderSetupPlan {
            provider_name: "fixture".to_string(),
            config: ProviderConfig::default(),
            api_key: "not-logged".to_string(),
            probe: false,
        }));
        assert!(require_interactive_oauth(&plan, false).is_ok());
    }
}
