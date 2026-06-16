//! Filesystem paths for app data (tokens, credentials, config) resolved once
//! from the environment at startup. Mirrors the TS `src/lib/paths.ts`.

use once_cell::sync::Lazy;
use std::path::PathBuf;

/// Mirrors src/lib/paths.ts. All paths are computed once from env at startup.
pub struct Paths {
    pub app_dir: PathBuf,
    pub github_token_path: PathBuf,
    pub codex_credential_path: PathBuf,
    pub config_path: PathBuf,
    pub auth_app: String,
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub static PATHS: Lazy<Paths> = Lazy::new(|| {
    let auth_app = std::env::var("COPILOT_API_OAUTH_APP")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let enterprise_prefix = if std::env::var("COPILOT_API_ENTERPRISE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some()
    {
        "ent_"
    } else {
        ""
    };

    let default_dir = home_dir().join(".local").join("share").join("copilot-api");
    let app_dir = std::env::var("COPILOT_API_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default_dir);

    let github_token_path = app_dir
        .join(&auth_app)
        .join(format!("{enterprise_prefix}github_token"));
    let codex_credential_path = app_dir.join("codex_credentials.json");
    let config_path = app_dir.join("config.json");

    Paths {
        app_dir,
        github_token_path,
        codex_credential_path,
        config_path,
        auth_app,
    }
});

pub async fn ensure_paths() -> std::io::Result<()> {
    tokio::fs::create_dir_all(PATHS.app_dir.join(&PATHS.auth_app)).await?;
    ensure_file(&PATHS.github_token_path).await?;
    ensure_file(&PATHS.config_path).await?;
    Ok(())
}

async fn ensure_file(path: &std::path::Path) -> std::io::Result<()> {
    if tokio::fs::metadata(path).await.is_err() {
        tokio::fs::write(path, "").await?;
        set_permissions_600(path).await;
    }
    Ok(())
}

#[cfg(unix)]
pub async fn set_permissions_600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
}

#[cfg(not(unix))]
pub async fn set_permissions_600(_path: &std::path::Path) {}
