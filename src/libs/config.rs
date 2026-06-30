//! Persisted configuration model (`config.json`): auth keys, per-model
//! overrides, and provider definitions. Mirrors the TS `src/lib/config.ts`
//! and round-trips unknown keys via `serde(flatten)`.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::RwLock;

use super::paths::PATHS;

pub type ReasoningEffort = String; // "none" | "minimal" | "low" | "medium" | "high" | "xhigh"

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "apiKeys")]
    pub api_keys: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "adminApiKey")]
    pub admin_api_key: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "topP")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "topK")]
    pub top_k: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "extraBody")]
    pub extra_body: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "contextCache")]
    pub context_cache: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "supportPdf")]
    pub support_pdf: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "toolContentSupportType"
    )]
    pub tool_content_support_type: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub provider_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "authType")]
    pub auth_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<BTreeMap<String, ModelConfig>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "adjustInputTokens")]
    pub adjust_input_tokens: Option<bool>,
    /// Preserve unknown keys so round-tripping config.json does not drop fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct ResolvedProviderConfig {
    pub name: String,
    pub provider_type: String, // anthropic | openai-compatible | openai-responses
    pub base_url: String,
    pub api_key: String,
    pub auth_type: String, // authorization | oauth2 | x-api-key
    pub models: Option<BTreeMap<String, ModelConfig>>,
    pub adjust_input_tokens: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<BTreeMap<String, ProviderConfig>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "modelMappings")]
    pub model_mappings: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "extraPrompts")]
    pub extra_prompts: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "smallModel")]
    pub small_model: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "useResponsesApiContextManagement"
    )]
    pub use_responses_api_context_management: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "modelResponsesApiCompactThresholds"
    )]
    pub model_responses_api_compact_thresholds: Option<BTreeMap<String, f64>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "modelReasoningEfforts"
    )]
    pub model_reasoning_efforts: Option<BTreeMap<String, ReasoningEffort>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "useMessagesApi")]
    pub use_messages_api: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "useResponsesApiWebSocket"
    )]
    pub use_responses_api_web_socket: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "anthropicApiKey")]
    pub anthropic_api_key: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "useResponsesApiWebSearch"
    )]
    pub use_responses_api_web_search: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "messageApiWebSearchModel"
    )]
    pub message_api_web_search_model: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "claudeTokenMultiplier"
    )]
    pub claude_token_multiplier: Option<f64>,
    /// Cap on total tokens recorded across the current local day. When exceeded,
    /// new requests are rejected with a 429 until the day rolls over. `None` or a
    /// value `<= 0` disables the cap. Enforcement gates on cumulative spend so
    /// far and can overshoot by the tokens of the requests already in flight when
    /// the cap is crossed (usage is recorded after each response completes).
    #[serde(skip_serializing_if = "Option::is_none", rename = "dailyTokenBudget")]
    pub daily_token_budget: Option<i64>,
    /// Top-level Responses model used to drive image generation over the Codex
    /// transport (the actual image model is selected via the `image_generation`
    /// tool). These model slugs drift on OpenAI's side, so they are configurable.
    /// Defaults to `gpt-5.5`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "imageChatModel")]
    pub image_chat_model: Option<String>,
    /// The image model the `image_generation` tool requests. Defaults to
    /// `gpt-image-2`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "imageModel")]
    pub image_model: Option<String>,
    /// Reject requests with a 429 once the account's cached GitHub Copilot
    /// premium-interaction quota falls strictly below this many remaining
    /// interactions. `None` (or a value `<= 0`) disables the threshold check.
    /// Coarse, account-wide, and slow-refreshing (see
    /// [`crate::libs::premium_interactions`]); always a no-op on plans that
    /// report the quota as unlimited.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "minPremiumInteractionsRemaining"
    )]
    pub min_premium_interactions_remaining: Option<f64>,
    /// Reject requests with a 429 once the account has exhausted its
    /// premium-interaction entitlement and is into overage. `None`/`false`
    /// disables the check. Always a no-op on unlimited plans.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "blockOnPremiumOverage"
    )]
    pub block_on_premium_overage: Option<bool>,
    /// Preserve unknown top-level keys (e.g. desktop-only fields).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

const GPT5_EXPLORATION_PROMPT: &str = "## Exploration and reading files\n- **Think first.** Before any tool call, decide ALL files/resources you will need.\n- **Batch everything.** If you need multiple files (even from different places), read them together.\n- **multi_tool_use.parallel** Use multi_tool_use.parallel to parallelize tool calls and only this.\n- **Only make sequential calls if you truly cannot know the next file without seeing a result first.**\n- **Workflow:** (a) plan all needed reads → (b) issue one parallel batch → (c) analyze results → (d) repeat if new, unpredictable reads arise.";

const GPT5_COMMENTARY_PROMPT: &str = "# Working with the user\n\nYou interact with the user through a terminal. You have 2 ways of communicating with the users:  \n- Share intermediary updates in `commentary` channel.  \n- After you have completed all your work, send a message to the `final` channel.  \n\n## Intermediary updates\n\n- Intermediary updates go to the `commentary` channel.\n- User updates are short updates while you are working, they are NOT final answers.\n- You use 1-2 sentence user updates to communicate progress and new information to the user as you are doing work.\n- Do not begin responses with conversational interjections or meta commentary. Avoid openers such as acknowledgements (“Done —”, “Got it”, “Great question, ”) or framing phrases.\n- You provide user updates frequently, every 20s.\n- Before exploring or doing substantial work, you start with a user update acknowledging the request and explaining your first step. You should include your understanding of the user request and explain what you will do. Avoid commenting on the request or using starters such as \"Got it -\" or \"Understood -\" etc.\n- When exploring, e.g. searching, reading files, you provide user updates as you go, every 20s, explaining what context you are gathering and what you've learned. Vary your sentence structure when providing these updates to avoid sounding repetitive - in particular, don't start each sentence the same way.\n- After you have sufficient context, and the work is substantial, you provide a longer plan (this is the only user update that may be longer than 2 sentences and can contain formatting).\n- Before performing file edits of any kind, you provide updates explaining what edits you are making.\n- As you are thinking, you very frequently provide updates even if not taking any actions, informing the user of your progress. You interrupt your thinking and send multiple updates in a row if thinking for more than 100 words.\n- Tone of your updates MUST match your personality.";

fn default_config() -> AppConfig {
    let mut extra_prompts = BTreeMap::new();
    extra_prompts.insert(
        "gpt-5-mini".to_string(),
        GPT5_EXPLORATION_PROMPT.to_string(),
    );
    extra_prompts.insert(
        "gpt-5.3-codex".to_string(),
        GPT5_COMMENTARY_PROMPT.to_string(),
    );
    extra_prompts.insert(
        "gpt-5.4-mini".to_string(),
        GPT5_COMMENTARY_PROMPT.to_string(),
    );
    extra_prompts.insert("gpt-5.4".to_string(), GPT5_COMMENTARY_PROMPT.to_string());
    extra_prompts.insert("gpt-5.5".to_string(), GPT5_COMMENTARY_PROMPT.to_string());

    let mut thresholds = BTreeMap::new();
    thresholds.insert("gpt-5.4".to_string(), 272_000.0 * 0.8);
    thresholds.insert("gpt-5.5".to_string(), 272_000.0 * 0.8);

    let mut efforts = BTreeMap::new();
    efforts.insert("gpt-5-mini".to_string(), "low".to_string());
    efforts.insert("gpt-5.3-codex".to_string(), "xhigh".to_string());
    efforts.insert("gpt-5.4-mini".to_string(), "xhigh".to_string());
    efforts.insert("gpt-5.4".to_string(), "xhigh".to_string());
    efforts.insert("gpt-5.5".to_string(), "xhigh".to_string());
    efforts.insert("claude-opus-4.8".to_string(), "max".to_string());

    AppConfig {
        auth: Some(AuthConfig {
            api_keys: Some(vec![]),
            admin_api_key: None,
        }),
        providers: Some(BTreeMap::new()),
        model_mappings: Some(BTreeMap::new()),
        extra_prompts: Some(extra_prompts),
        small_model: Some("gpt-5-mini".to_string()),
        use_responses_api_context_management: Some(true),
        model_responses_api_compact_thresholds: Some(thresholds),
        model_reasoning_efforts: Some(efforts),
        use_messages_api: Some(true),
        use_responses_api_web_socket: Some(true),
        use_responses_api_web_search: Some(true),
        message_api_web_search_model: Some("gpt-5-mini".to_string()),
        ..Default::default()
    }
}

static CACHED_CONFIG: Lazy<RwLock<Option<Arc<AppConfig>>>> = Lazy::new(|| RwLock::new(None));

/// Test seam: overwrite the process-global cached config so router/auth tests can
/// install a known `auth.apiKeys` / `auth.adminApiKey` without touching disk.
/// Tests that use this MUST run serially (the cache is a process-global).
#[doc(hidden)]
pub fn set_cached_config_for_test(config: AppConfig) {
    *CACHED_CONFIG.write().unwrap() = Some(Arc::new(config));
}

/// Test seam: clear the cached config so the next `get_config()` re-reads.
#[doc(hidden)]
pub fn reset_cached_config_for_test() {
    *CACHED_CONFIG.write().unwrap() = None;
}

fn serialize_pretty(config: &AppConfig) -> String {
    let mut s = serde_json::to_string_pretty(config).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

fn ensure_config_file() {
    let path = &PATHS.config_path;
    if std::fs::metadata(path).is_err() {
        let _ = std::fs::create_dir_all(&PATHS.app_dir);
        let _ = std::fs::write(path, serialize_pretty(&default_config()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn read_config_from_disk() -> AppConfig {
    ensure_config_file();
    match std::fs::read_to_string(&PATHS.config_path) {
        Ok(raw) => {
            if raw.trim().is_empty() {
                let _ = std::fs::write(&PATHS.config_path, serialize_pretty(&default_config()));
                return default_config();
            }
            match serde_json::from_str::<AppConfig>(&raw) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::error!("Failed to read config file, using default config: {e}");
                    default_config()
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to read config file, using default config: {e}");
            default_config()
        }
    }
}

fn read_editable_config_from_disk() -> Result<AppConfig, anyhow::Error> {
    match std::fs::read_to_string(&PATHS.config_path) {
        Ok(raw) => {
            if raw.trim().is_empty() {
                return Ok(AppConfig::default());
            }
            serde_json::from_str::<AppConfig>(&raw).map_err(|_| {
                anyhow::anyhow!(
                    "Config file is not valid JSON: {}",
                    PATHS.config_path.display()
                )
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppConfig::default()),
        Err(e) => Err(anyhow::Error::new(e)),
    }
}

fn write_config_to_disk(config: &AppConfig) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    std::fs::create_dir_all(&PATHS.app_dir)?;
    // Write to a sibling temp file then atomically rename over the target so a
    // crash mid-write can never leave a truncated/corrupt secrets file (the
    // config holds adminApiKey and provider apiKeys). std::fs::rename is an
    // atomic replace on both Unix and Windows.
    let target = &PATHS.config_path;
    // Unique per write: pid plus a monotonic counter, so two concurrent writes
    // in the same process can't collide on the same tmp path.
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = PATHS
        .app_dir
        .join(format!("config.json.tmp.{}.{seq}", std::process::id()));
    let contents = serialize_pretty(config);

    let write_result = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        // Create the tmp file with 0600 from the start so the secrets in it are
        // never briefly world/group-readable under a permissive umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, target)
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_result
}

/// Returns (merged, changed). Adds any missing default extraPrompts /
/// reasoningEfforts / compactThresholds entries without overwriting user values.
fn merge_default_config(mut config: AppConfig) -> (AppConfig, bool) {
    let def = default_config();
    let def_prompts = def.extra_prompts.clone().unwrap_or_default();
    let def_efforts = def.model_reasoning_efforts.clone().unwrap_or_default();
    let def_thresholds = def
        .model_responses_api_compact_thresholds
        .clone()
        .unwrap_or_default();

    let prompts = config.extra_prompts.get_or_insert_with(BTreeMap::new);
    let mut changed = false;
    for (k, v) in &def_prompts {
        if !prompts.contains_key(k) {
            prompts.insert(k.clone(), v.clone());
            changed = true;
        }
    }
    let efforts = config
        .model_reasoning_efforts
        .get_or_insert_with(BTreeMap::new);
    for (k, v) in &def_efforts {
        if !efforts.contains_key(k) {
            efforts.insert(k.clone(), v.clone());
            changed = true;
        }
    }
    let thresholds = config
        .model_responses_api_compact_thresholds
        .get_or_insert_with(BTreeMap::new);
    for (k, v) in &def_thresholds {
        if !thresholds.contains_key(k) {
            thresholds.insert(k.clone(), *v);
            changed = true;
        }
    }
    (config, changed)
}

fn normalize_admin_api_key(value: &Option<Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                tracing::warn!("Invalid auth.adminApiKey config. Expected a non-empty string.");
                None
            } else {
                Some(t.to_string())
            }
        }
        Some(_) => {
            tracing::warn!("Invalid auth.adminApiKey config. Expected a non-empty string.");
            None
        }
        None => None,
    }
}

fn generate_admin_api_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn ensure_admin_api_key(config: AppConfig) -> Result<(AppConfig, bool), anyhow::Error> {
    let current = config.auth.as_ref().and_then(|a| a.admin_api_key.clone());
    if let Some(normalized) = normalize_admin_api_key(&current) {
        if current == Some(Value::String(normalized.clone())) {
            return Ok((config, false));
        }
        let mut next = config;
        let auth = next.auth.get_or_insert_with(AuthConfig::default);
        auth.admin_api_key = Some(Value::String(normalized));
        return Ok((next, true));
    }

    // Need to generate. Start from editable on-disk config to avoid persisting
    // merged prompts. A malformed config.json surfaces here as an error: like
    // the TS port (readEditableConfigFromDisk throws), we propagate it so
    // startup aborts and the user's file is preserved rather than overwritten.
    let mut editable = read_editable_config_from_disk()?;
    let auth = editable.auth.get_or_insert_with(AuthConfig::default);
    auth.admin_api_key = Some(Value::String(generate_admin_api_key()));
    let (merged, _) = merge_default_config(editable);
    Ok((merged, true))
}

pub fn merge_config_with_defaults() -> Result<AppConfig, anyhow::Error> {
    let config = read_config_from_disk();
    let (merged, changed) = merge_default_config(config);
    let (merged, admin_changed) = ensure_admin_api_key(merged)?;
    let should_persist = changed || admin_changed;

    if should_persist {
        if let Err(e) = write_config_to_disk(&merged) {
            if admin_changed {
                anyhow::bail!("Failed to write merged default config: {e}");
            }
            tracing::warn!("Failed to write merged default config to config file: {e}");
        }
    }

    *CACHED_CONFIG.write().unwrap() = Some(Arc::new(merged.clone()));
    Ok(merged)
}

pub fn get_config() -> Arc<AppConfig> {
    {
        let guard = CACHED_CONFIG.read().unwrap();
        if let Some(cfg) = guard.as_ref() {
            return Arc::clone(cfg);
        }
    }
    let (merged, _) = merge_default_config(read_config_from_disk());
    let arc = Arc::new(merged);
    *CACHED_CONFIG.write().unwrap() = Some(Arc::clone(&arc));
    arc
}

pub fn reload_config() -> Result<AppConfig, anyhow::Error> {
    merge_config_with_defaults()
}

pub fn get_extra_prompt_for_model(model: &str) -> String {
    get_config()
        .extra_prompts
        .as_ref()
        .and_then(|m| m.get(model).cloned())
        .unwrap_or_default()
}

pub fn get_model_mappings() -> BTreeMap<String, String> {
    let config = get_config();
    let mut valid = BTreeMap::new();
    if let Some(mappings) = config.model_mappings.as_ref() {
        for (source, target) in mappings {
            if source.is_empty() {
                continue;
            }
            if let Value::String(t) = target {
                if !t.is_empty() {
                    valid.insert(source.clone(), t.clone());
                }
            }
        }
    }
    valid
}

fn validate_model_mappings(
    mappings: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Value>, anyhow::Error> {
    let mut validated = BTreeMap::new();
    for (source, target) in mappings {
        if source.is_empty() || target.is_empty() {
            return Err(anyhow::anyhow!(
                "Each model mapping must use non-empty source and target values."
            ));
        }
        validated.insert(source.clone(), Value::String(target.clone()));
    }
    Ok(validated)
}

pub fn set_model_mappings(
    mappings: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, anyhow::Error> {
    let validated = validate_model_mappings(mappings)?;
    let mut next = read_editable_config_from_disk()?;
    next.model_mappings = Some(validated);
    write_config_to_disk(&next)?;
    reload_config()?;
    Ok(get_model_mappings())
}

pub fn resolve_mapped_model(model: &str) -> String {
    get_config()
        .model_mappings
        .as_ref()
        .and_then(|m| match m.get(model) {
            Some(Value::String(t)) if !t.is_empty() => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_else(|| model.to_string())
}

pub fn get_small_model() -> String {
    get_config()
        .small_model
        .clone()
        .unwrap_or_else(|| "gpt-5-mini".to_string())
}

pub fn is_responses_api_context_management_enabled() -> bool {
    get_config()
        .use_responses_api_context_management
        .unwrap_or(true)
}

pub fn get_model_responses_api_compact_threshold(model: &str) -> Option<f64> {
    let threshold = get_config()
        .model_responses_api_compact_thresholds
        .as_ref()
        .and_then(|m| m.get(model).copied())?;
    if !threshold.is_finite() || threshold <= 0.0 {
        return None;
    }
    Some(threshold)
}

/// Built-in fallback reasoning effort used when neither the client nor the
/// operator's `modelReasoningEfforts` config specify one for a model.
pub const DEFAULT_REASONING_EFFORT: &str = "high";

/// Returns the explicitly-configured reasoning effort for a model from
/// `modelReasoningEfforts`, if one is present. Unlike
/// [`get_reasoning_effort_for_model`], this returns `None` (rather than the
/// built-in default) when the model has no configured override, so callers can
/// distinguish "operator forced an effort" from "fall back to default" and give
/// the configured value precedence over a client-supplied effort.
pub fn get_configured_reasoning_effort_for_model(model: &str) -> Option<String> {
    get_config()
        .model_reasoning_efforts
        .as_ref()
        .and_then(|m| m.get(model).cloned())
}

pub fn get_reasoning_effort_for_model(model: &str) -> String {
    get_configured_reasoning_effort_for_model(model)
        .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_string())
}

pub fn normalize_provider_base_url(url: &str) -> String {
    let trimmed = url.trim();
    trimmed.trim_end_matches('/').to_string()
}

fn get_default_provider_auth_type(provider_type: &str) -> String {
    if provider_type == "anthropic" {
        "x-api-key".to_string()
    } else {
        "authorization".to_string()
    }
}

pub fn resolve_provider_auth_type(
    provider_name: &str,
    auth_type: Option<&str>,
    provider_type: &str,
) -> String {
    let default_auth_type = get_default_provider_auth_type(provider_type);
    match auth_type {
        None => default_auth_type,
        Some("x-api-key") => "x-api-key".to_string(),
        Some("oauth2") => {
            if provider_name == "codex" {
                "oauth2".to_string()
            } else {
                tracing::warn!(
                    "Provider {provider_name} has authType 'oauth2', which is only supported by the builtin codex provider, falling back to {default_auth_type}"
                );
                default_auth_type
            }
        }
        Some("authorization") => "authorization".to_string(),
        Some(other) => {
            tracing::warn!(
                "Provider {provider_name} has invalid authType '{other}', falling back to {default_auth_type}"
            );
            default_auth_type
        }
    }
}

fn is_provider_api_key_required(provider_name: &str, auth_type: &str) -> bool {
    !(provider_name == "codex" && auth_type == "oauth2")
}

pub fn get_raw_provider_config(name: &str) -> Option<ProviderConfig> {
    let provider_name = name.trim();
    if provider_name.is_empty() {
        return None;
    }
    get_config()
        .providers
        .as_ref()
        .and_then(|p| p.get(provider_name).cloned())
}

pub fn set_provider_config(
    name: &str,
    provider: ProviderConfig,
) -> Result<ProviderConfig, anyhow::Error> {
    let provider_name = name.trim();
    if provider_name.is_empty() {
        return Err(anyhow::anyhow!("Provider name must be a non-empty string"));
    }
    if is_reserved_provider_name(provider_name) {
        return Err(anyhow::anyhow!(
            "Provider {provider_name} is reserved and cannot be configured in config.providers"
        ));
    }
    let mut next = read_editable_config_from_disk()?;
    next.providers
        .get_or_insert_with(BTreeMap::new)
        .insert(provider_name.to_string(), provider.clone());
    write_config_to_disk(&next)?;
    reload_config()?;
    Ok(get_raw_provider_config(provider_name).unwrap_or(provider))
}

pub fn get_provider_config(name: &str) -> Option<ResolvedProviderConfig> {
    let provider_name = name.trim();
    if provider_name.is_empty() {
        return None;
    }
    if is_reserved_provider_name(provider_name) {
        tracing::warn!(
            "Provider {provider_name} is reserved and cannot be configured in config.providers"
        );
        return None;
    }
    let provider = get_raw_provider_config(provider_name)?;
    if provider.enabled == Some(false) {
        return None;
    }
    let provider_type = provider
        .provider_type
        .clone()
        .unwrap_or_else(|| "anthropic".to_string());
    if provider_type != "anthropic"
        && provider_type != "openai-compatible"
        && provider_type != "openai-responses"
    {
        tracing::warn!(
            "Provider {provider_name} is ignored because type '{provider_type}' is not supported"
        );
        return None;
    }
    let base_url = normalize_provider_base_url(provider.base_url.as_deref().unwrap_or(""));
    let auth_type =
        resolve_provider_auth_type(provider_name, provider.auth_type.as_deref(), &provider_type);
    let api_key = provider
        .api_key
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut missing = Vec::new();
    if base_url.is_empty() {
        missing.push("baseUrl");
    }
    if is_provider_api_key_required(provider_name, &auth_type) && api_key.is_empty() {
        missing.push("apiKey");
    }
    if !missing.is_empty() {
        tracing::warn!(
            "Provider {provider_name} is enabled but missing {}",
            missing.join(" or ")
        );
        return None;
    }
    Some(ResolvedProviderConfig {
        name: provider_name.to_string(),
        provider_type,
        base_url,
        api_key,
        auth_type,
        models: provider.models,
        adjust_input_tokens: provider.adjust_input_tokens,
    })
}

pub fn list_enabled_providers() -> Vec<String> {
    let config = get_config();
    let names: Vec<String> = config
        .providers
        .as_ref()
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    names
        .into_iter()
        .filter(|name| get_provider_config(name).is_some())
        .collect()
}

pub fn is_reserved_provider_name(name: &str) -> bool {
    name.trim() == "copilot"
}

pub fn is_messages_api_enabled() -> bool {
    get_config().use_messages_api.unwrap_or(true)
}

pub fn is_responses_api_web_socket_enabled() -> bool {
    get_config().use_responses_api_web_socket.unwrap_or(true)
}

pub fn get_anthropic_api_key() -> Option<String> {
    get_config()
        .anthropic_api_key
        .clone()
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
}

pub fn is_responses_api_web_search_enabled() -> bool {
    get_config().use_responses_api_web_search.unwrap_or(true)
}

pub fn get_message_api_web_search_model() -> Option<String> {
    let model = get_config()
        .message_api_web_search_model
        .clone()
        .unwrap_or_else(|| "gpt-5-mini".to_string());
    if !model.trim().is_empty() {
        Some(model)
    } else {
        None
    }
}

pub fn get_claude_token_multiplier() -> f64 {
    get_config().claude_token_multiplier.unwrap_or(1.15)
}

/// The configured daily token budget, or `None` when unset or non-positive
/// (treated as disabled).
pub fn get_daily_token_budget() -> Option<i64> {
    get_config().daily_token_budget.filter(|&b| b > 0)
}

/// Minimum remaining premium interactions before the admission gate rejects, or
/// `None` when unset or non-positive (treated as disabled).
pub fn get_min_premium_interactions_remaining() -> Option<f64> {
    get_config()
        .min_premium_interactions_remaining
        .filter(|&v| v > 0.0)
}

/// Whether to reject requests once the account is into premium-interaction
/// overage. Defaults to `false` (disabled).
pub fn get_block_on_premium_overage() -> bool {
    get_config().block_on_premium_overage.unwrap_or(false)
}

/// Top-level Responses model that drives Codex image generation. Defaults to
/// `gpt-5.5`.
pub fn get_image_chat_model() -> String {
    get_config()
        .image_chat_model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gpt-5.5".to_string())
}

/// The image model requested by the `image_generation` tool. Defaults to
/// `gpt-image-2`.
pub fn get_image_model() -> String {
    get_config()
        .image_model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gpt-image-2".to_string())
}
