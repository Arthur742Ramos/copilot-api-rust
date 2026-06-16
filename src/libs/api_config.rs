use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::libs::opencode::get_cached_opencode_version;
use crate::libs::request_context::request_context_store;
use crate::libs::state::State;

// --- Version / UA constants (mirror src/lib/api-config.ts) ---------------
const OPENCODE_VERSION: &str = "opencode/1.14.29";
const OPENCODE_LLM_USER_AGENT: &str =
    "opencode/1.14.29 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13, opencode/1.14.29";

const COPILOT_VERSION: &str = "0.52.0";
// EDITOR_PLUGIN_VERSION = `copilot-chat/${COPILOT_VERSION}`
fn editor_plugin_version() -> String {
    format!("copilot-chat/{COPILOT_VERSION}")
}
fn user_agent() -> String {
    format!("GitHubCopilotChat/{COPILOT_VERSION}")
}
const CLAUDE_AGENT_USER_AGENT: &str =
    "vscode_claude_code/2.1.112 (external, sdk-ts, agent-sdk/0.2.112)";
fn editor_websocket_plugin_version() -> String {
    format!("copilot-chat/{COPILOT_VERSION}")
}

const API_VERSION: &str = "2026-06-01";
const WEBSOCKET_API_VERSION: &str = API_VERSION;

pub const GITHUB_API_BASE_URL: &str = "https://api.github.com";
pub const GITHUB_BASE_URL: &str = "https://github.com";
pub const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const OPENCODE_GITHUB_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
pub fn github_app_scopes() -> String {
    "read:user".to_string()
}

pub fn is_opencode_oauth_app() -> bool {
    std::env::var("COPILOT_API_OAUTH_APP")
        .map(|s| s.trim() == "opencode")
        .unwrap_or(false)
}

pub fn normalize_domain(input: &str) -> String {
    let trimmed = input.trim();
    let no_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    no_scheme.trim_end_matches('/').to_string()
}

pub fn get_enterprise_domain() -> Option<String> {
    let raw = std::env::var("COPILOT_API_ENTERPRISE_URL").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let normalized = normalize_domain(raw);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub fn get_github_base_url() -> String {
    match get_enterprise_domain() {
        Some(d) => format!("https://{d}"),
        None => GITHUB_BASE_URL.to_string(),
    }
}

pub fn get_github_api_base_url() -> String {
    match get_enterprise_domain() {
        Some(d) => format!("https://api.{d}"),
        None => GITHUB_API_BASE_URL.to_string(),
    }
}

pub fn get_opencode_version() -> String {
    match get_cached_opencode_version() {
        Some(v) => format!("opencode/{v}"),
        None => OPENCODE_VERSION.to_string(),
    }
}

fn normalize_opencode_user_agent(user_agent: &str) -> String {
    let candidate = user_agent.trim();
    // match /^opencode\/[^\s,]+/
    let opencode_product = candidate.split_whitespace().next().and_then(|first| {
        let token: String = first.chars().take_while(|c| *c != ',').collect();
        if token.starts_with("opencode/") {
            Some(token)
        } else {
            None
        }
    });
    match opencode_product {
        Some(product) if !candidate.contains(&format!(", {product}")) => {
            format!("{candidate}, {product}")
        }
        _ => candidate.to_string(),
    }
}

pub struct OauthUrls {
    pub device_code_url: String,
    pub access_token_url: String,
}

pub fn get_oauth_urls() -> OauthUrls {
    let base = get_github_base_url();
    OauthUrls {
        device_code_url: format!("{base}/login/device/code"),
        access_token_url: format!("{base}/login/oauth/access_token"),
    }
}

pub struct OauthAppConfig {
    pub client_id: String,
    pub headers: HeaderMap,
    pub scope: String,
}

fn opencode_oauth_headers() -> HeaderMap {
    build_headers(&[
        ("Accept", "application/json".to_string()),
        ("Content-Type", "application/json".to_string()),
        ("User-Agent", get_opencode_version()),
    ])
}

fn opencode_llm_headers() -> HeaderMap {
    build_headers(&[
        ("Accept", "application/json".to_string()),
        ("Content-Type", "application/json".to_string()),
        ("User-Agent", OPENCODE_LLM_USER_AGENT.to_string()),
    ])
}

pub fn standard_headers() -> HeaderMap {
    build_headers(&[
        ("content-type", "application/json".to_string()),
        ("accept", "application/json".to_string()),
    ])
}

pub fn get_oauth_app_config() -> OauthAppConfig {
    if is_opencode_oauth_app() {
        OauthAppConfig {
            client_id: OPENCODE_GITHUB_CLIENT_ID.to_string(),
            headers: opencode_oauth_headers(),
            scope: github_app_scopes(),
        }
    } else {
        OauthAppConfig {
            client_id: GITHUB_CLIENT_ID.to_string(),
            headers: standard_headers(),
            scope: github_app_scopes(),
        }
    }
}

/// `prepareForCompact`: compactType is non-null when the request is a
/// compaction. COMPACT_REQUEST == 1 in the TS enum.
pub fn prepare_for_compact(headers: &mut HeaderMap, compact_type: Option<i32>) {
    if let Some(ct) = compact_type {
        set_header(headers, "x-initiator", "agent");
        if !is_opencode_oauth_app() && ct == crate::libs::compact::COMPACT_REQUEST {
            set_header(headers, "x-interaction-type", "conversation-compaction");
            set_header(headers, "openai-intent", "conversation-agent");
        }
    }
}

pub fn prepare_interaction_headers(
    session_id: Option<&str>,
    is_subagent: bool,
    headers: &mut HeaderMap,
) {
    let send_interaction_headers = !is_opencode_oauth_app();
    if is_subagent {
        set_header(headers, "x-initiator", "agent");
        if send_interaction_headers {
            set_header(headers, "x-interaction-type", "conversation-subagent");
        }
    }
    if let Some(sid) = session_id {
        if send_interaction_headers {
            set_header(headers, "x-interaction-id", sid);
        }
    }
}

pub fn copilot_base_url(state: &State) -> String {
    if let Some(domain) = get_enterprise_domain() {
        return format!("https://copilot-api.{domain}");
    }
    if is_opencode_oauth_app() {
        return "https://api.githubcopilot.com".to_string();
    }
    if let Some(url) = &state.copilot_api_url {
        return url.clone();
    }
    if state.account_type == "individual" {
        "https://api.githubcopilot.com".to_string()
    } else {
        format!("https://api.{}.githubcopilot.com", state.account_type)
    }
}

/// `prepareMessageProxyHeaders`: aligns headers with the vscode copilot claude
/// agent. No-op under the opencode oauth app.
pub fn prepare_message_proxy_headers(headers: &mut HeaderMap) {
    if is_opencode_oauth_app() {
        return;
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    set_header(headers, "x-agent-task-id", &request_id);
    set_header(headers, "x-request-id", &request_id);
    set_header(headers, "x-interaction-type", "messages-proxy");
    set_header(headers, "openai-intent", "messages-proxy");
    set_header(headers, "user-agent", CLAUDE_AGENT_USER_AGENT);
    headers.remove("copilot-integration-id");
}

pub fn github_user_headers(state: &State) -> HeaderMap {
    if is_opencode_oauth_app() {
        return build_headers(&[
            (
                "Authorization",
                format!("Bearer {}", state.github_token.clone().unwrap_or_default()),
            ),
            ("User-Agent", get_opencode_version()),
        ]);
    }
    build_headers(&[
        ("accept", "application/vnd.github+json".to_string()),
        (
            "authorization",
            format!("token {}", state.github_token.clone().unwrap_or_default()),
        ),
        ("user-agent", user_agent()),
        ("x-github-api-version", "2022-11-28".to_string()),
        (
            "x-vscode-user-agent-library-version",
            "electron-fetch".to_string(),
        ),
    ])
}

pub fn github_headers(state: &State) -> HeaderMap {
    if is_opencode_oauth_app() {
        let mut h = opencode_oauth_headers();
        set_header(
            &mut h,
            "Authorization",
            &format!("Bearer {}", state.github_token.clone().unwrap_or_default()),
        );
        return h;
    }
    build_headers(&[
        (
            "authorization",
            format!("token {}", state.github_token.clone().unwrap_or_default()),
        ),
        ("user-agent", user_agent()),
        ("x-github-api-version", "2025-04-01".to_string()),
        (
            "x-vscode-user-agent-library-version",
            "electron-fetch".to_string(),
        ),
    ])
}

pub fn copilot_models_headers(state: &State) -> HeaderMap {
    if is_opencode_oauth_app() {
        return build_headers(&[
            (
                "Authorization",
                format!("Bearer {}", state.copilot_token.clone().unwrap_or_default()),
            ),
            ("User-Agent", get_opencode_version()),
        ]);
    }
    let mut headers = github_copilot_headers(state, None, false);
    set_header(&mut headers, "x-interaction-type", "model-access");
    set_header(&mut headers, "openai-intent", "model-access");
    headers.remove("x-interaction-id");
    headers.remove("content-type");
    headers
}

pub fn copilot_headers(state: &State, request_id: Option<&str>, vision: bool) -> HeaderMap {
    if is_opencode_oauth_app() {
        let mut headers = opencode_llm_headers();
        set_header(
            &mut headers,
            "Authorization",
            &format!("Bearer {}", state.copilot_token.clone().unwrap_or_default()),
        );
        set_header(&mut headers, "Openai-Intent", "conversation-edits");

        if let Some(store) = request_context_store() {
            let ua = store.user_agent.trim().to_string();
            if ua.starts_with("opencode/") {
                set_header(
                    &mut headers,
                    "User-Agent",
                    &normalize_opencode_user_agent(&ua),
                );
            }
            if let Some(sa) = &store.session_affinity {
                set_header(&mut headers, "x-session-affinity", sa);
            }
            if let Some(ps) = &store.parent_session_id {
                set_header(&mut headers, "x-parent-session-id", ps);
            }
        }

        if vision {
            set_header(&mut headers, "Copilot-Vision-Request", "true");
        }
        return headers;
    }
    github_copilot_headers(state, request_id, vision)
}

fn github_copilot_headers(state: &State, request_id: Option<&str>, vision: bool) -> HeaderMap {
    let request_id_value = request_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut headers = build_headers(&[
        (
            "Authorization",
            format!("Bearer {}", state.copilot_token.clone().unwrap_or_default()),
        ),
        ("content-type", "application/json".to_string()),
        ("copilot-integration-id", "vscode-chat".to_string()),
        ("editor-device-id", state.vscode_device_id.clone()),
        (
            "editor-version",
            format!(
                "vscode/{}",
                state.vscode_version.clone().unwrap_or_default()
            ),
        ),
        ("editor-plugin-version", editor_plugin_version()),
        ("user-agent", user_agent()),
        ("openai-intent", "conversation-agent".to_string()),
        ("x-github-api-version", API_VERSION.to_string()),
        ("x-request-id", request_id_value.clone()),
        (
            "x-vscode-user-agent-library-version",
            "electron-fetch".to_string(),
        ),
        ("x-agent-task-id", request_id_value),
        ("x-interaction-type", "conversation-agent".to_string()),
    ]);

    if vision {
        set_header(&mut headers, "copilot-vision-request", "true");
    }
    if let Some(mac) = &state.mac_machine_id {
        set_header(&mut headers, "vscode-machineid", mac);
    }
    if let Some(sid) = &state.vscode_session_id {
        set_header(&mut headers, "vscode-sessionid", sid);
    }
    headers
}

// --- Header helpers -------------------------------------------------------

pub fn build_headers(pairs: &[(&str, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        set_header(&mut map, name, value);
    }
    map
}

pub fn set_header(map: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        map.insert(n, v);
    }
}
