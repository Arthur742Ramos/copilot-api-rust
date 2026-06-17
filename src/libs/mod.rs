//! Shared infrastructure: configuration, global state, HTTP client, auth and
//! token handling, error types, and other cross-cutting utilities.

pub mod api_config;
pub mod approval;
pub mod codex_rate_limit;
pub mod compact;
pub mod config;
pub mod copilot_rate_limit;
pub mod credential_store;
pub mod deviceid;
pub mod error;
pub mod http;
pub mod logger;
pub mod metrics;
pub mod models;
pub mod oauth;
pub mod opencode;
pub mod paths;
pub mod provider_model;
pub mod provider_resolver;
pub mod rate_limit;
pub mod request_auth;
pub mod request_context;
pub mod shell;
pub mod sqlite;
pub mod sse;
pub mod state;
pub mod stream_metrics;
pub mod subagent;
pub mod token;
pub mod token_usage;
pub mod tokenizer;
pub mod tool_search;
pub mod utils;
pub mod zstd_request;
