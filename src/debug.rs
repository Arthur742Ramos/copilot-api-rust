use once_cell::sync::Lazy;
use serde::Serialize;

use crate::libs::config;
use crate::libs::paths::PATHS;

static CLIENT_VERSION_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"(?i)\bv?\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?\b")
        .expect("static client version regex")
});

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
#[serde(rename_all = "camelCase")]
struct IntegrationInfo {
    claude_code: Option<String>,
    codex_cli: Option<String>,
    open_code: Option<String>,
    agent_inject_plugin: &'static str,
    tool_search_plugin: &'static str,
    open_code_marker: &'static str,
}

#[derive(Serialize)]
struct DebugInfo {
    providers: ProvidersInfo,
    version: String,
    runtime: RuntimeInfo,
    paths: PathsInfo,
    #[serde(rename = "tokenExists")]
    token_exists: bool,
    integrations: IntegrationInfo,
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

fn sanitize_version_output(output: &[u8]) -> Option<String> {
    let first_line = String::from_utf8_lossy(output)
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect::<String>();
    let version = CLIENT_VERSION_RE
        .find(&first_line)?
        .as_str()
        .trim_start_matches(['v', 'V'])
        .to_string();
    Some(version)
}

async fn detect_client_version(command: &str) -> Option<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::process::Command::new(command)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    sanitize_version_output(&output.stdout).or_else(|| sanitize_version_output(&output.stderr))
}

async fn get_debug_info() -> DebugInfo {
    let token_exists = check_file_exists(&PATHS.github_token_path).await;
    let (claude_code, codex_cli, open_code) = tokio::join!(
        detect_client_version("claude"),
        detect_client_version("codex"),
        detect_client_version("opencode")
    );

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
        integrations: IntegrationInfo {
            claude_code,
            codex_cli,
            open_code,
            agent_inject_plugin: "1.0.0",
            tool_search_plugin: "1.0.0",
            open_code_marker: "1.0.0",
        },
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
    let detected =
        |value: &Option<String>| value.clone().unwrap_or_else(|| "not detected".to_string());

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
    println!(
        "\nIntegrations:\n- Claude Code: {}\n- Codex CLI: {}\n- OpenCode: {}\n- agent-inject plugin: {}\n- tool-search plugin: {}\n- OpenCode marker: {}",
        detected(&info.integrations.claude_code),
        detected(&info.integrations.codex_cli),
        detected(&info.integrations.open_code),
        info.integrations.agent_inject_plugin,
        info.integrations.tool_search_plugin,
        info.integrations.open_code_marker,
    );
}

fn print_debug_info_json(info: &DebugInfo) {
    match serde_json::to_string_pretty(info) {
        Ok(s) => println!("{s}"),
        Err(e) => tracing::error!("Failed to serialize debug info: {e}"),
    }
}

pub async fn run_debug(json: bool) -> anyhow::Result<()> {
    crate::libs::paths::ensure_paths().await?;
    let debug_info = get_debug_info().await;

    if json {
        print_debug_info_json(&debug_info);
    } else {
        print_debug_info_plain(&debug_info);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_output_is_single_line_and_bounded() {
        assert_eq!(
            sanitize_version_output(b"client 1.2.3\nignored token-like detail"),
            Some("1.2.3".to_string())
        );
        assert!(sanitize_version_output(b"\n").is_none());
        assert!(sanitize_version_output(b"token=private-value").is_none());
    }
}
