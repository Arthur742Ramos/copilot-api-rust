use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::libs::oauth::codex::CodexCredentials;
use crate::libs::paths::{set_permissions_600, PATHS};

// Mirrors src/lib/credential-store.ts. Reads/writes the GitHub token and Codex
// credential files, with 0600 permissions on write.

async fn read_optional_file(path: &std::path::Path) -> Result<Option<String>, anyhow::Error> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn write_protected_file(path: &std::path::Path, content: &str) -> Result<(), anyhow::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, content).await?;
    set_permissions_600(path).await;
    Ok(())
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ProviderCredentials {
    #[serde(default)]
    providers: BTreeMap<String, String>,
}

fn read_provider_credentials_sync() -> Result<ProviderCredentials, anyhow::Error> {
    match std::fs::read_to_string(&PATHS.provider_credentials_path) {
        Ok(raw) if raw.trim().is_empty() => Ok(ProviderCredentials::default()),
        Ok(raw) => serde_json::from_str(&raw).map_err(|error| {
            anyhow::anyhow!(
                "Provider credentials file is not valid JSON: {} ({error})",
                PATHS.provider_credentials_path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ProviderCredentials::default())
        }
        Err(error) => Err(error.into()),
    }
}

fn write_provider_credentials_sync(credentials: &ProviderCredentials) -> Result<(), anyhow::Error> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    std::fs::create_dir_all(&PATHS.app_dir)?;
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = PATHS.app_dir.join(format!(
        "provider_credentials.json.tmp.{}.{}",
        std::process::id(),
        sequence
    ));
    let result = (|| -> Result<(), anyhow::Error> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let json = serde_json::to_string_pretty(credentials)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);

        #[cfg(windows)]
        if PATHS.provider_credentials_path.exists() {
            // Windows' std::fs::rename cannot replace an existing file. The
            // temporary file still prevents a partial/truncated credential set;
            // the short remove/rename window is the platform fallback.
            std::fs::remove_file(&PATHS.provider_credentials_path)?;
        }
        std::fs::rename(&temporary, &PATHS.provider_credentials_path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Read one provider key from the repository credential store. This synchronous
/// form is used by the synchronous config resolver and never logs the value.
pub fn read_provider_api_key_sync(provider: &str) -> Result<Option<String>, anyhow::Error> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Ok(None);
    }
    Ok(read_provider_credentials_sync()?
        .providers
        .get(provider)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

/// Atomically persist one provider key in the owner-only credential store.
pub async fn write_provider_api_key(provider: &str, api_key: &str) -> Result<(), anyhow::Error> {
    let provider = provider.trim().to_string();
    let api_key = api_key.trim().to_string();
    if provider.is_empty() || api_key.is_empty() {
        anyhow::bail!("Provider name and API key must be non-empty");
    }
    tokio::task::spawn_blocking(move || {
        let mut credentials = read_provider_credentials_sync()?;
        credentials.providers.insert(provider, api_key);
        write_provider_credentials_sync(&credentials)
    })
    .await
    .map_err(|error| anyhow::anyhow!("Provider credential writer failed: {error}"))?
}

pub async fn read_github_token() -> Result<Option<String>, anyhow::Error> {
    let token = read_optional_file(&PATHS.github_token_path).await?;
    Ok(token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty()))
}

pub async fn write_github_token(token: &str) -> Result<(), anyhow::Error> {
    write_protected_file(&PATHS.github_token_path, token.trim()).await
}

#[derive(Debug, Deserialize, Serialize)]
struct RawCodexCredentials {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "accountId")]
    account_id: Option<String>,
}

fn normalize_codex_credentials(raw: RawCodexCredentials) -> Option<CodexCredentials> {
    Some(CodexCredentials {
        access_token: raw.access_token?,
        refresh_token: raw.refresh_token?,
        expires_at: raw.expires_at?,
        account_id: raw.account_id?,
    })
}

pub async fn read_codex_credentials() -> Result<Option<CodexCredentials>, anyhow::Error> {
    let raw = read_optional_file(&PATHS.codex_credential_path).await?;
    let raw = match raw {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Ok(None),
    };

    let parsed: RawCodexCredentials = serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "Codex credentials file is not valid JSON: {} ({e})",
            PATHS.codex_credential_path.display()
        )
    })?;

    match normalize_codex_credentials(parsed) {
        Some(c) => Ok(Some(c)),
        None => Err(anyhow::anyhow!(
            "Codex credentials file is missing required fields: {}",
            PATHS.codex_credential_path.display()
        )),
    }
}

pub async fn write_codex_credentials(credentials: &CodexCredentials) -> Result<(), anyhow::Error> {
    let json = serde_json::to_string_pretty(credentials)?;
    write_protected_file(&PATHS.codex_credential_path, &format!("{json}\n")).await
}

#[allow(dead_code)]
pub async fn clear_codex_credentials() -> Result<(), anyhow::Error> {
    write_protected_file(&PATHS.codex_credential_path, "").await
}

#[allow(dead_code)]
pub async fn has_codex_credentials() -> bool {
    matches!(read_codex_credentials().await, Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_credentials_default_to_missing() {
        let parsed: ProviderCredentials = serde_json::from_str("{}").unwrap();
        assert!(parsed.providers.is_empty());
    }

    #[test]
    fn provider_credentials_round_trip_without_config_shape() {
        let credentials = ProviderCredentials {
            providers: BTreeMap::from([("fixture".to_string(), "top-secret".to_string())]),
        };
        let value = serde_json::to_value(credentials).unwrap();
        assert_eq!(value["providers"]["fixture"], "top-secret");
        assert!(value.get("baseUrl").is_none());
    }
}
