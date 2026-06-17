use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::sync::Mutex;

use crate::libs::state;
use crate::services::copilot::get_models::{get_models, ModelsResponse};
use crate::services::get_vscode_version::get_vscode_version;

pub async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Mirrors `findLastUserContent`: scans messages from the end for the last
/// user message with content, returning its text (stringifying array content
/// with `tool_result` parts removed and `cache_control` stripped).
fn find_last_user_content(messages: &[serde_json::Value]) -> Option<String> {
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let content = match msg.get("content") {
            Some(c) if !c.is_null() => c,
            _ => continue,
        };
        if let Some(s) = content.as_str() {
            if !s.is_empty() {
                return Some(s.to_string());
            }
            continue;
        }
        if let Some(arr) = content.as_array() {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter(|n| n.get("type").and_then(|t| t.as_str()) != Some("tool_result"))
                .map(|n| {
                    let mut cloned = n.clone();
                    if let Some(obj) = cloned.as_object_mut() {
                        obj.insert("cache_control".to_string(), serde_json::Value::Null);
                    }
                    cloned
                })
                .collect();
            if !filtered.is_empty() {
                return serde_json::to_string(&filtered).ok();
            }
        }
    }
    None
}

/// Mirrors `generateRequestIdFromPayload`. Derives a deterministic request id
/// from the last user content (+ session id + mac machine id), or a random
/// UUID when there is no user content.
pub fn generate_request_id_from_payload(
    messages: &[serde_json::Value],
    session_id: Option<&str>,
) -> String {
    let last_user_content = find_last_user_content(messages);
    if let Some(content) = last_user_content {
        let mac = state::with_state(|s| s.mac_machine_id.clone().unwrap_or_default());
        let seed = format!("{}{}{}", session_id.unwrap_or(""), mac, content);
        return get_uuid(&seed);
    }
    uuid::Uuid::new_v4().to_string()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// --- Models cache + refresh loop -------------------------------------------

const MODELS_REFRESH_BASE_MS: u64 = 30 * 60 * 1000;

static MODELS_REFRESH: Lazy<Mutex<Option<RefreshHandle>>> = Lazy::new(|| Mutex::new(None));

struct RefreshHandle {
    aborted: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

pub fn stop_models_refresh_loop() {
    if let Some(h) = MODELS_REFRESH.lock().unwrap().take() {
        h.aborted.store(true, Ordering::SeqCst);
        h.handle.abort();
    }
}

/// Filter + store models, logging newly-added ids. Mirrors `refreshModels`.
async fn refresh_models() -> Result<(), anyhow::Error> {
    let prev_ids: std::collections::HashSet<String> = state::with_state(|s| {
        s.models
            .as_ref()
            .map(|m| m.data.iter().map(|model| model.id.clone()).collect())
            .unwrap_or_default()
    });

    let models = get_models().await.map_err(|e| anyhow::anyhow!(e.message))?;
    let filtered: Vec<_> = models
        .data
        .into_iter()
        .filter(|model| model.model_picker_enabled || model.capabilities.model_type == "embeddings")
        .collect();
    let next_ids: Vec<String> = filtered.iter().map(|m| m.id.clone()).collect();

    state::with_state_mut(|s| {
        s.models = Some(std::sync::Arc::new(ModelsResponse {
            data: filtered,
            object: models.object,
        }));
    });

    let added: Vec<String> = next_ids
        .iter()
        .filter(|id| !prev_ids.contains(*id))
        .cloned()
        .collect();
    if !added.is_empty() {
        tracing::info!(
            "Models refresh: {} new -- {}",
            added.len(),
            added.join(", ")
        );
    } else {
        tracing::debug!("Models refresh: no changes ({} total)", next_ids.len());
    }
    Ok(())
}

pub async fn cache_models() -> Result<(), anyhow::Error> {
    refresh_models().await?;
    schedule_models_refresh(MODELS_REFRESH_BASE_MS);
    Ok(())
}

fn schedule_models_refresh(interval_ms: u64) {
    stop_models_refresh_loop();
    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_clone = aborted.clone();
    let handle = tokio::spawn(async move {
        loop {
            let jitter = rand::random::<u64>() % (interval_ms / 6).max(1);
            let delay = interval_ms + jitter;
            tracing::debug!("Scheduling next models refresh in {} seconds", delay / 1000);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if aborted_clone.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = refresh_models().await {
                tracing::warn!("Failed to refresh models, keeping previous cache. {e}");
            }
        }
    });
    *MODELS_REFRESH.lock().unwrap() = Some(RefreshHandle { aborted, handle });
}

pub async fn cache_vscode_version() {
    let response = get_vscode_version().await;
    state::with_state_mut(|s| s.vscode_version = Some(response.clone()));
    tracing::info!("Using VSCode version: {response}");
}

// --- MAC machine id ---------------------------------------------------------

fn is_valid_mac_address(candidate: &str) -> bool {
    let normalized = candidate.replace('-', ":").to_lowercase();
    !matches!(
        normalized.as_str(),
        "00:00:00:00:00:00" | "ff:ff:ff:ff:ff:ff" | "ac:de:48:00:11:22"
    )
}

pub fn get_mac() -> Option<String> {
    match mac_address::get_mac_address() {
        Ok(Some(addr)) => {
            // Node's os.networkInterfaces() yields lowercase-hex MACs; the
            // mac_address crate's Display is uppercase. Lowercase so the hashed
            // machine id (and the request-id seed) match the TS reference.
            let s = addr.to_string().to_lowercase();
            if is_valid_mac_address(&s) {
                Some(s)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn cache_mac_machine_id() {
    let mac = get_mac().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut hasher = Sha256::new();
    hasher.update(mac.as_bytes());
    let digest = hex::encode(hasher.finalize());
    state::with_state_mut(|s| s.mac_machine_id = Some(digest.clone()));
    tracing::debug!("Using machine ID: {digest}");
}

pub async fn cache_vscode_device_id() {
    let id = crate::libs::deviceid::get_vscode_device_id().await;
    state::with_state_mut(|s| s.vscode_device_id = id.clone());
    tracing::debug!("Using VSCode device ID: {id}");
}

// --- VSCode session id + refresh loop ---------------------------------------

const SESSION_REFRESH_BASE_MS: u64 = 60 * 60 * 1000;
const SESSION_REFRESH_JITTER_MS: u64 = 20 * 60 * 1000;

static SESSION_REFRESH: Lazy<Mutex<Option<RefreshHandle>>> = Lazy::new(|| Mutex::new(None));

fn generate_session_id() {
    let session_id = format!("{}{}", uuid::Uuid::new_v4(), now_millis());
    state::with_state_mut(|s| s.vscode_session_id = Some(session_id.clone()));
    tracing::debug!("Generated VSCode session ID: {session_id}");
}

pub fn stop_vscode_session_refresh_loop() {
    if let Some(h) = SESSION_REFRESH.lock().unwrap().take() {
        h.aborted.store(true, Ordering::SeqCst);
        h.handle.abort();
    }
}

pub fn cache_vscode_session_id() {
    stop_vscode_session_refresh_loop();
    generate_session_id();
    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_clone = aborted.clone();
    let handle = tokio::spawn(async move {
        loop {
            let random_delay = rand::random::<u64>() % SESSION_REFRESH_JITTER_MS;
            let delay = SESSION_REFRESH_BASE_MS + random_delay;
            tracing::debug!(
                "Scheduling next VSCode session ID refresh in {} seconds",
                delay / 1000
            );
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if aborted_clone.load(Ordering::SeqCst) {
                return;
            }
            generate_session_id();
        }
    });
    *SESSION_REFRESH.lock().unwrap() = Some(RefreshHandle { aborted, handle });
}

// --- Deterministic UUID (sha256-based, mirrors getUUID) ---------------------

pub fn get_uuid(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[0..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

// --- user_id metadata parsing ----------------------------------------------

pub struct UserIdMetadata {
    pub safety_identifier: Option<String>,
    pub session_id: Option<String>,
}

fn get_user_id_json_field(payload: &serde_json::Value, field: &str) -> Option<String> {
    payload
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

pub fn parse_user_id_metadata(user_id: Option<&str>) -> UserIdMetadata {
    let user_id = match user_id {
        Some(s) if !s.is_empty() => s,
        _ => {
            return UserIdMetadata {
                safety_identifier: None,
                session_id: None,
            }
        }
    };

    let legacy_safety = regex_capture(user_id, r"user_([^_]+)_account");
    let legacy_session = regex_capture(user_id, r"_session_(.+)$");

    let parsed: Option<serde_json::Value> = if legacy_safety.is_some() && legacy_session.is_some() {
        None
    } else {
        serde_json::from_str(user_id)
            .ok()
            .filter(|v: &serde_json::Value| v.is_object())
    };

    let safety_identifier = legacy_safety.or_else(|| {
        parsed.as_ref().and_then(|p| {
            get_user_id_json_field(p, "device_id")
                .or_else(|| get_user_id_json_field(p, "account_uuid"))
        })
    });
    let session_id = legacy_session.or_else(|| {
        parsed
            .as_ref()
            .and_then(|p| get_user_id_json_field(p, "session_id"))
    });

    UserIdMetadata {
        safety_identifier,
        session_id,
    }
}

/// Mirrors `getRootSessionId`. Resolves the session id from
/// `metadata.user_id` (via `parse_user_id_metadata`) or, when absent, the
/// `x-session-id` request header, then maps it through `get_uuid`.
pub fn get_root_session_id(
    payload: &serde_json::Value,
    headers: &axum::http::HeaderMap,
) -> Option<String> {
    let user_id = payload
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str());

    let session_id = match user_id {
        Some(uid) => parse_user_id_metadata(Some(uid))
            .session_id
            .filter(|s| !s.is_empty()),
        None => headers
            .get("x-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
    };

    session_id.map(|s| get_uuid(&s))
}

fn regex_capture(haystack: &str, pattern: &str) -> Option<String> {
    let re = regex::Regex::new(pattern).ok()?;
    re.captures(haystack)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}
