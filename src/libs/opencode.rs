use once_cell::sync::Lazy;
use std::sync::RwLock;

/// Mirrors src/lib/opencode.ts. Resolves the globally-installed opencode-ai
/// package version (only when the oauth app is "opencode"), caching the result.
static OPENCODE_VERSION_CACHE: Lazy<RwLock<Option<String>>> = Lazy::new(|| RwLock::new(None));

async fn get_global_npm_root() -> Result<String, anyhow::Error> {
    let output = tokio::process::Command::new("npm")
        .arg("root")
        .arg("-g")
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn resolve_opencode_version() {
    match get_global_npm_root().await {
        Ok(npm_root) => {
            let pkg_path = std::path::Path::new(&npm_root)
                .join("opencode-ai")
                .join("package.json");
            match tokio::fs::read_to_string(&pkg_path).await {
                Ok(contents) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                        if let Some(version) = json.get("version").and_then(|v| v.as_str()) {
                            *OPENCODE_VERSION_CACHE.write().unwrap() = Some(version.to_string());
                            return;
                        }
                    }
                    tracing::warn!("Failed to resolve opencode version: no version field");
                }
                Err(e) => tracing::warn!("Failed to resolve opencode version: {e}"),
            }
        }
        Err(e) => tracing::warn!("Failed to resolve opencode version: {e}"),
    }
}

pub async fn init_opencode_version() {
    if std::env::var("COPILOT_API_OAUTH_APP")
        .map(|s| s.trim() == "opencode")
        .unwrap_or(false)
    {
        resolve_opencode_version().await;
    }
}

pub fn get_cached_opencode_version() -> Option<String> {
    OPENCODE_VERSION_CACHE.read().unwrap().clone()
}
