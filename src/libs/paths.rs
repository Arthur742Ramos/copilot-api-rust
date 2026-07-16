//! Filesystem paths for app data (tokens, credentials, config) resolved once
//! from the environment at startup. Mirrors the TS `src/lib/paths.ts`.

use once_cell::sync::Lazy;
use std::path::PathBuf;

/// Mirrors src/lib/paths.ts. All paths are computed once from env at startup.
pub struct Paths {
    pub app_dir: PathBuf,
    pub files_dir: PathBuf,
    pub github_token_path: PathBuf,
    pub codex_credential_path: PathBuf,
    pub provider_credentials_path: PathBuf,
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
    let files_dir = app_dir.join("files");
    let codex_credential_path = app_dir.join("codex_credentials.json");
    let provider_credentials_path = app_dir.join("provider_credentials.json");
    let config_path = app_dir.join("config.json");

    Paths {
        app_dir,
        files_dir,
        github_token_path,
        codex_credential_path,
        provider_credentials_path,
        config_path,
        auth_app,
    }
});

pub async fn ensure_paths() -> std::io::Result<()> {
    tokio::fs::create_dir_all(&PATHS.app_dir).await?;
    set_permissions_700(&PATHS.app_dir).await?;
    let auth_dir = PATHS.app_dir.join(&PATHS.auth_app);
    if auth_dir != PATHS.app_dir {
        tokio::fs::create_dir_all(&auth_dir).await?;
        set_permissions_700(&auth_dir).await?;
    }
    tokio::fs::create_dir_all(&PATHS.files_dir).await?;
    set_permissions_700(&PATHS.files_dir).await?;
    ensure_file(&PATHS.github_token_path).await?;
    ensure_file(&PATHS.config_path).await?;
    ensure_file(&PATHS.provider_credentials_path).await?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_permissions_700_sync(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("owner-only mode verification failed for {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn set_permissions_700_sync(path: &std::path::Path) -> std::io::Result<()> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$path = $env:COPILOT_API_ACL_PATH
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$sid = $identity.User
$acl = [System.Security.AccessControl.DirectorySecurity]::new()
$acl.SetOwner($sid)
$acl.SetAccessRuleProtection($true, $false)
$inheritance = [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [System.Security.AccessControl.InheritanceFlags]::ObjectInherit
$rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
  $sid,
  [System.Security.AccessControl.FileSystemRights]::FullControl,
  $inheritance,
  [System.Security.AccessControl.PropagationFlags]::None,
  [System.Security.AccessControl.AccessControlType]::Allow
)
[void]$acl.AddAccessRule($rule)
[System.IO.Directory]::SetAccessControl($path, $acl)
$check = [System.IO.Directory]::GetAccessControl($path)
$rules = @($check.GetAccessRules(
  $true,
  $false,
  [System.Security.Principal.SecurityIdentifier]
))
if (-not $check.AreAccessRulesProtected -or $rules.Count -ne 1) {
  throw 'directory ACL is not protected owner-only'
}
$ownerSid = $check.GetOwner(
  [System.Security.Principal.SecurityIdentifier]
)
if ($ownerSid.Value -ne $sid.Value) {
  throw 'directory ACL owner verification failed'
}
if (($rules[0].IdentityReference.Value -ne $sid.Value) -or ($rules[0].AccessControlType -ne [System.Security.AccessControl.AccessControlType]::Allow)) {
  throw 'directory ACL owner verification failed'
}
if (($rules[0].FileSystemRights -band [System.Security.AccessControl.FileSystemRights]::FullControl) -ne [System.Security.AccessControl.FileSystemRights]::FullControl) {
  throw 'directory ACL rights verification failed'
}
if (($rules[0].InheritanceFlags -band $inheritance) -ne $inheritance) {
  throw 'directory ACL inheritance verification failed'
}
"#;

    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("COPILOT_API_ACL_PATH", path)
        .output()?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("unknown ACL error")
            .chars()
            .take(200)
            .collect::<String>();
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "failed to enforce owner-only Windows directory ACL for {}: {detail}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn set_permissions_700_sync(path: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "owner-only directory permissions are unsupported for {}",
            path.display()
        ),
    ))
}

/// Enforce and verify owner-only directory permissions before secrets or local
/// file content are created beneath the directory.
pub async fn set_permissions_700(path: &std::path::Path) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || set_permissions_700_sync(&path))
        .await
        .map_err(|error| std::io::Error::other(format!("permission worker failed: {error}")))?
}

async fn ensure_file(path: &std::path::Path) -> std::io::Result<()> {
    if tokio::fs::metadata(path).await.is_err() {
        tokio::fs::write(path, "").await?;
    }
    set_permissions_600(path).await?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_permissions_600_sync(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("owner-only mode verification failed for {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn set_permissions_600_sync(path: &std::path::Path) -> std::io::Result<()> {
    // Build a fresh protected DACL containing exactly one allow rule for the
    // current user's SID, then read it back and verify. PowerShell is part of
    // supported Windows installations; if it is missing or ACL application/
    // verification fails, credential persistence fails closed.
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$path = $env:COPILOT_API_ACL_PATH
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$sid = $identity.User
$acl = [System.Security.AccessControl.FileSecurity]::new()
$acl.SetOwner($sid)
$acl.SetAccessRuleProtection($true, $false)
$rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
  $sid,
  [System.Security.AccessControl.FileSystemRights]::FullControl,
  [System.Security.AccessControl.AccessControlType]::Allow
)
[void]$acl.AddAccessRule($rule)
[System.IO.File]::SetAccessControl($path, $acl)
$check = [System.IO.File]::GetAccessControl($path)
$rules = @($check.GetAccessRules(
  $true,
  $false,
  [System.Security.Principal.SecurityIdentifier]
))
if (-not $check.AreAccessRulesProtected -or $rules.Count -ne 1) {
  throw 'credential ACL is not protected owner-only'
}
$ownerSid = $check.GetOwner(
  [System.Security.Principal.SecurityIdentifier]
)
if ($ownerSid.Value -ne $sid.Value) {
  throw 'credential ACL owner verification failed'
}
if (($rules[0].IdentityReference.Value -ne $sid.Value) -or ($rules[0].AccessControlType -ne [System.Security.AccessControl.AccessControlType]::Allow)) {
  throw 'credential ACL owner verification failed'
}
if (($rules[0].FileSystemRights -band [System.Security.AccessControl.FileSystemRights]::FullControl) -ne [System.Security.AccessControl.FileSystemRights]::FullControl) {
  throw 'credential ACL rights verification failed'
}
"#;

    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("COPILOT_API_ACL_PATH", path)
        .output()?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("unknown ACL error")
            .chars()
            .take(200)
            .collect::<String>();
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "failed to enforce owner-only Windows ACL for {}: {detail}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn set_permissions_600_sync(path: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "owner-only credential permissions are unsupported for {}",
            path.display()
        ),
    ))
}

/// Enforce and verify restrictive credential-file permissions. Never degrades to
/// an advisory: callers must propagate failure before reading or writing a
/// secret.
pub async fn set_permissions_600(path: &std::path::Path) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || set_permissions_600_sync(&path))
        .await
        .map_err(|error| std::io::Error::other(format!("permission worker failed: {error}")))?
}

#[cfg(not(windows))]
pub(crate) fn atomic_replace_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
pub(crate) fn atomic_replace_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_file(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "copilot-api-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn credential_permission_policy_enforces_or_fails_closed() {
        let path = temporary_file("permission-policy");
        std::fs::write(&path, "credential").unwrap();
        let result = set_permissions_600_sync(&path);
        #[cfg(any(unix, windows))]
        assert!(result.is_ok(), "{result:?}");
        #[cfg(not(any(unix, windows)))]
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn directory_permission_policy_enforces_or_fails_closed() {
        let path = temporary_file("directory-permission-policy");
        std::fs::create_dir(&path).unwrap();
        let result = set_permissions_700_sync(&path);
        #[cfg(any(unix, windows))]
        assert!(result.is_ok(), "{result:?}");
        #[cfg(not(any(unix, windows)))]
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir(path);
    }

    #[test]
    fn protected_atomic_replace_preserves_new_content() {
        let destination = temporary_file("atomic-destination");
        let source = temporary_file("atomic-source");
        std::fs::write(&destination, "old").unwrap();
        std::fs::write(&source, "new").unwrap();
        set_permissions_600_sync(&source).unwrap();
        atomic_replace_file(&source, &destination).unwrap();
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "new");
        let _ = std::fs::remove_file(destination);
    }
}
