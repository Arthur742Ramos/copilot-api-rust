use crate::libs::config::{
    get_provider_config_checked, get_raw_provider_config, ResolvedProviderConfig,
};
use crate::libs::state;
use crate::libs::token::setup_codex_token;

const MISSING_CODEX_CREDENTIALS: &str =
    "Codex credentials not found. Run `copilot-api auth login --provider codex` first.";

/// Mirrors the TS `isMissingCodexCredentialsError`: only the exact missing
/// credential error is swallowed into `None`.
fn is_missing_codex_credentials_error(error: &anyhow::Error) -> bool {
    error.to_string() == MISSING_CODEX_CREDENTIALS
}

/// Port of `resolveProviderConfig` from `lib/provider-resolver.ts`.
pub async fn resolve_provider_config(
    provider_name: &str,
) -> anyhow::Result<Option<ResolvedProviderConfig>> {
    resolve_provider_config_with_setup(provider_name, setup_codex_token).await
}

async fn resolve_provider_config_with_setup<F, Fut>(
    provider_name: &str,
    setup_codex: F,
) -> anyhow::Result<Option<ResolvedProviderConfig>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), anyhow::Error>>,
{
    let normalized_provider_name = provider_name.trim();
    if normalized_provider_name.is_empty() {
        return Ok(None);
    }

    if normalized_provider_name == "codex" {
        let raw_provider_config = get_raw_provider_config(normalized_provider_name);
        if let Some(raw) = raw_provider_config {
            if raw.enabled == Some(false) {
                return Ok(None);
            }
        }

        let loaded_codex_auth = state::with_state(|state| {
            let token_loaded = state
                .codex_access_token
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let account_loaded = state
                .codex_account_id
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let unexpired = state
                .codex_expires_at
                .is_some_and(|expires| expires > chrono::Utc::now().timestamp_millis());
            token_loaded && account_loaded && unexpired
        });
        if !loaded_codex_auth {
            if let Err(error) = setup_codex().await {
                if is_missing_codex_credentials_error(&error) {
                    return Ok(None);
                }
                return Err(error.context("Failed to set up Codex credentials"));
            }
        }

        let Some(provider_config) = get_provider_config_checked(normalized_provider_name)? else {
            return Ok(None);
        };

        // TS: `state.codexAccessToken ?? providerConfig.apiKey`.
        // Override the api key with the live codex access token when available.
        let codex_access_token = state::with_state(|s| s.codex_access_token.clone());
        let api_key = match codex_access_token {
            Some(token) if !token.is_empty() => token,
            _ => provider_config.api_key.clone(),
        };

        return Ok(Some(ResolvedProviderConfig {
            api_key,
            ..provider_config
        }));
    }

    get_provider_config_checked(normalized_provider_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::config::{
        reset_cached_config_for_test, set_cached_config_for_test, AppConfig, ProviderConfig,
    };
    use serde_json::Map;
    use std::collections::BTreeMap;

    fn codex_config() -> AppConfig {
        AppConfig {
            providers: Some(BTreeMap::from([(
                "codex".to_string(),
                ProviderConfig {
                    provider_type: Some("openai-responses".to_string()),
                    enabled: Some(true),
                    base_url: Some("https://chatgpt.com/backend-api".to_string()),
                    api_key: None,
                    auth_type: Some("oauth2".to_string()),
                    models: None,
                    capabilities: None,
                    adjust_input_tokens: None,
                    extra: Map::new(),
                },
            )])),
            ..Default::default()
        }
    }

    fn clear_loaded_codex() {
        state::with_state_mut(|state| {
            state.codex_access_token = None;
            state.codex_account_id = None;
            state.codex_expires_at = None;
        });
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn only_exact_missing_credentials_becomes_not_found() {
        set_cached_config_for_test(codex_config());
        clear_loaded_codex();
        let result = resolve_provider_config_with_setup("codex", || async {
            Err(anyhow::anyhow!(MISSING_CODEX_CREDENTIALS))
        })
        .await
        .unwrap();
        assert!(result.is_none());
        reset_cached_config_for_test();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn malformed_or_refresh_setup_error_is_preserved() {
        set_cached_config_for_test(codex_config());
        clear_loaded_codex();
        let error = resolve_provider_config_with_setup("codex", || async {
            Err(anyhow::anyhow!("credential file is malformed"))
        })
        .await
        .expect_err("non-missing setup errors must propagate");
        assert!(error
            .to_string()
            .contains("Failed to set up Codex credentials"));
        assert!(format!("{error:#}").contains("credential file is malformed"));
        reset_cached_config_for_test();
    }
}
