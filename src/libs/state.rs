//! Process-global mutable application state (tokens, account info, cached
//! models) behind an `RwLock`, mirroring the TS singleton `state` object.

use once_cell::sync::Lazy;
use std::sync::RwLock;

use crate::services::copilot::get_models::ModelsResponse;

/// Mutable global application state. The TS version uses a single mutable
/// `state` object; we model it as a process-global behind an RwLock.
#[derive(Debug, Default, Clone)]
pub struct State {
    pub github_token: Option<String>,
    pub user_name: Option<String>,
    pub copilot_token: Option<String>,
    pub codex_access_token: Option<String>,
    pub codex_refresh_token: Option<String>,
    pub codex_expires_at: Option<i64>,
    pub codex_account_id: Option<String>,

    pub account_type: String,
    pub models: Option<ModelsResponse>,
    pub vscode_version: Option<String>,

    pub mac_machine_id: Option<String>,
    pub vscode_session_id: Option<String>,
    pub vscode_device_id: String,

    pub manual_approve: bool,
    pub rate_limit_wait: bool,
    pub show_token: bool,

    pub rate_limit_seconds: Option<u64>,
    pub last_request_timestamp: Option<i64>,
    pub verbose: bool,

    pub copilot_api_url: Option<String>,
    pub token_based_billing: Option<bool>,
}

impl State {
    fn initial() -> Self {
        State {
            account_type: "individual".to_string(),
            manual_approve: false,
            rate_limit_wait: false,
            show_token: false,
            verbose: false,
            vscode_device_id: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        }
    }
}

pub static STATE: Lazy<RwLock<State>> = Lazy::new(|| RwLock::new(State::initial()));

/// Read a clone of the current state. Mirrors the many `state.foo` reads in TS;
/// callers that need a consistent snapshot for header building clone once.
pub fn snapshot() -> State {
    STATE.read().unwrap().clone()
}

/// Mutate the global state under the write lock.
pub fn with_state_mut<F, R>(f: F) -> R
where
    F: FnOnce(&mut State) -> R,
{
    let mut guard = STATE.write().unwrap();
    f(&mut guard)
}

/// Read the global state under the read lock.
pub fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&State) -> R,
{
    let guard = STATE.read().unwrap();
    f(&guard)
}
