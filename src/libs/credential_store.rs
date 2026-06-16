use serde::{Deserialize, Serialize};

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
