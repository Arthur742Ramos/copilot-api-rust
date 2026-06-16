use crate::libs::config::{get_provider_config, get_raw_provider_config, ResolvedProviderConfig};
use crate::libs::state;
use crate::libs::token::setup_codex_token;

/// Mirrors the TS `isMissingCodexCredentialsError`: only the specific
/// "Codex credentials not found ..." error is swallowed into `None`.
fn is_missing_codex_credentials_error(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("Codex credentials not found")
}

/// Port of `resolveProviderConfig` from `lib/provider-resolver.ts`.
pub async fn resolve_provider_config(provider_name: &str) -> Option<ResolvedProviderConfig> {
    let normalized_provider_name = provider_name.trim();
    if normalized_provider_name.is_empty() {
        return None;
    }

    if normalized_provider_name == "codex" {
        let raw_provider_config = get_raw_provider_config(normalized_provider_name);
        if let Some(raw) = raw_provider_config {
            if raw.enabled == Some(false) {
                return None;
            }
        }

        if let Err(error) = setup_codex_token().await {
            if is_missing_codex_credentials_error(&error) {
                return None;
            }
            // TS rethrows non-credentials errors. This function returns Option,
            // so we log and fall through to None instead of propagating.
            tracing::error!("Failed to set up Codex token: {error}");
            return None;
        }

        let provider_config = get_provider_config(normalized_provider_name)?;

        // TS: `state.codexAccessToken ?? providerConfig.apiKey`.
        // Override the api key with the live codex access token when available.
        let codex_access_token = state::with_state(|s| s.codex_access_token.clone());
        let api_key = match codex_access_token {
            Some(token) if !token.is_empty() => token,
            _ => provider_config.api_key.clone(),
        };

        return Some(ResolvedProviderConfig {
            api_key,
            ..provider_config
        });
    }

    get_provider_config(normalized_provider_name)
}
