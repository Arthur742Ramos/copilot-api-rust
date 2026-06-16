use serde::Serialize;

use crate::libs::config;
use crate::libs::paths::PATHS;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProvidersInfo {
    codex_configured: bool,
    enabled: Vec<String>,
}

#[derive(Serialize)]
struct RuntimeInfo {
    name: String,
    version: String,
    platform: String,
    arch: String,
}

#[derive(Serialize)]
struct PathsInfo {
    #[serde(rename = "APP_DIR")]
    app_dir: String,
    #[serde(rename = "CONFIG_PATH")]
    config_path: String,
    #[serde(rename = "GITHUB_TOKEN_PATH")]
    github_token_path: String,
}

#[derive(Serialize)]
struct DebugInfo {
    providers: ProvidersInfo,
    version: String,
    runtime: RuntimeInfo,
    paths: PathsInfo,
    #[serde(rename = "tokenExists")]
    token_exists: bool,
}

fn get_runtime_info() -> RuntimeInfo {
    RuntimeInfo {
        name: "rust".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

async fn check_file_exists(path: &std::path::Path) -> bool {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => !content.trim().is_empty(),
        Err(_) => false,
    }
}

async fn get_debug_info() -> DebugInfo {
    let token_exists = check_file_exists(&PATHS.github_token_path).await;

    DebugInfo {
        providers: ProvidersInfo {
            codex_configured: config::get_raw_provider_config("codex").is_some(),
            enabled: config::list_enabled_providers(),
        },
        version: env!("CARGO_PKG_VERSION").to_string(),
        runtime: get_runtime_info(),
        paths: PathsInfo {
            app_dir: PATHS.app_dir.display().to_string(),
            config_path: PATHS.config_path.display().to_string(),
            github_token_path: PATHS.github_token_path.display().to_string(),
        },
        token_exists,
    }
}

fn print_debug_info_plain(info: &DebugInfo) {
    let enabled = if info.providers.enabled.is_empty() {
        "none".to_string()
    } else {
        info.providers.enabled.join(", ")
    };
    let codex_configured = if info.providers.codex_configured {
        "Yes"
    } else {
        "No"
    };
    let token_exists = if info.token_exists { "Yes" } else { "No" };

    println!(
        "copilot-api debug

Version: {version}
Runtime: {rt_name} {rt_version} ({rt_platform} {rt_arch})

Providers:
- enabled: {enabled}
- codex configured: {codex_configured}

Paths:
- APP_DIR: {app_dir}
- CONFIG_PATH: {config_path}
- GITHUB_TOKEN_PATH: {github_token_path}

GitHub token exists: {token_exists}",
        version = info.version,
        rt_name = info.runtime.name,
        rt_version = info.runtime.version,
        rt_platform = info.runtime.platform,
        rt_arch = info.runtime.arch,
        enabled = enabled,
        codex_configured = codex_configured,
        app_dir = info.paths.app_dir,
        config_path = info.paths.config_path,
        github_token_path = info.paths.github_token_path,
        token_exists = token_exists,
    );
}

fn print_debug_info_json(info: &DebugInfo) {
    match serde_json::to_string_pretty(info) {
        Ok(s) => println!("{s}"),
        Err(e) => tracing::error!("Failed to serialize debug info: {e}"),
    }
}

pub async fn run_debug(json: bool) {
    let debug_info = get_debug_info().await;

    if json {
        print_debug_info_json(&debug_info);
    } else {
        print_debug_info_plain(&debug_info);
    }
}
