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

/// Restrict a credential file to owner-only access (`0600`) on unix.
///
/// On non-unix targets (notably win32) the unix permission bits do not apply,
/// so this is a no-op: the github token / admin key are written with whatever
/// ACL the parent directory grants (typically inheriting broader access). Call
/// [`warn_if_file_perms_unrestricted`] once at startup so operators know the
/// on-disk secrets are NOT locked down on those platforms.
#[cfg(unix)]
pub async fn set_permissions_600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
}

/// No-op on non-unix targets: unix `0600` mode bits do not exist on win32, so
/// credential files inherit the directory's ACL rather than being restricted to
/// the current user. See [`warn_if_file_perms_unrestricted`].
#[cfg(not(unix))]
pub async fn set_permissions_600(_path: &std::path::Path) {}

/// Emit a one-line startup warning on Windows, where [`set_permissions_600`]
/// cannot restrict file permissions via unix mode bits. No-op on unix and on
/// other non-unix targets (the message is Windows/NTFS-specific).
pub fn warn_if_file_perms_unrestricted() {
    #[cfg(windows)]
    {
        tracing::warn!(
            "File permissions are not restricted on this platform (win32): the GitHub token and \
             admin key in {} are stored without owner-only (0600) access control. Protect this \
             directory with NTFS ACLs if other users share the machine.",
            PATHS.app_dir.display()
        );
    }
}
