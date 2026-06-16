use std::path::PathBuf;

// Mirrors src/lib/deviceid.ts. Reads (or lazily creates + persists) the stable
// VSCode "device id" GUID used in the `editor-device-id` Copilot header,
// matching VSCode's own storage location per-platform.

fn get_posix_home_dir() -> Result<PathBuf, anyhow::Error> {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => Ok(PathBuf::from(home)),
        _ => Err(anyhow::anyhow!("Home directory not found")),
    }
}

#[cfg(target_os = "macos")]
fn get_device_id_file_path() -> Result<PathBuf, anyhow::Error> {
    let folder = get_posix_home_dir()?
        .join("Library")
        .join("Application Support");
    Ok(folder
        .join("Microsoft")
        .join("DeveloperTools")
        .join("deviceid"))
}

#[cfg(target_os = "linux")]
fn get_device_id_file_path() -> Result<PathBuf, anyhow::Error> {
    let folder = match std::env::var("XDG_CACHE_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => get_posix_home_dir()?.join(".cache"),
    };
    Ok(folder
        .join("Microsoft")
        .join("DeveloperTools")
        .join("deviceid"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn get_device_id_file_path() -> Result<PathBuf, anyhow::Error> {
    Err(anyhow::anyhow!("Unsupported platform"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn read_stored_device_id_file(
    path: &std::path::Path,
) -> Result<Option<String>, anyhow::Error> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn write_stored_device_id_file(
    path: &std::path::Path,
    device_id: &str,
) -> Result<(), anyhow::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, device_id).await?;
    Ok(())
}

/// `getStoredVSCodeDeviceId` — reads the persisted device id, or None if absent.
pub async fn get_stored_vscode_device_id() -> Result<Option<String>, anyhow::Error> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let path = get_device_id_file_path()?;
        read_stored_device_id_file(&path).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // Windows registry storage is not ported; degrade to ephemeral id.
        Err(anyhow::anyhow!("Unsupported platform"))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn set_stored_vscode_device_id(device_id: &str) -> Result<(), anyhow::Error> {
    let path = get_device_id_file_path()?;
    write_stored_device_id_file(&path, device_id).await
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn set_stored_vscode_device_id(_device_id: &str) -> Result<(), anyhow::Error> {
    Err(anyhow::anyhow!("Unsupported platform"))
}

fn create_vscode_device_id() -> String {
    uuid::Uuid::new_v4().to_string().to_lowercase()
}

/// `getVSCodeDeviceId` — resilient wrapper. Never fails; degrades to an
/// ephemeral UUID if reading or persisting the stored id errors.
pub async fn get_vscode_device_id() -> String {
    let stored = match get_stored_vscode_device_id().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("Failed to read VSCode device id {e}");
            None
        }
    };

    if let Some(id) = stored {
        if !id.is_empty() {
            return id;
        }
    }

    let new_device_id = create_vscode_device_id();
    if let Err(e) = set_stored_vscode_device_id(&new_device_id).await {
        tracing::warn!("Failed to persist VSCode device id, using ephemeral id {e}");
    }
    new_device_id
}
