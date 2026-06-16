//! HTTP route handlers for the proxy's public API surface (chat completions,
//! messages, responses, embeddings, models, token, and usage endpoints).

pub mod admin_config;
pub mod chat_completions;
pub mod embeddings;
pub mod messages;
pub mod models;
pub mod provider;
pub mod responses;
pub mod token;
pub mod token_usage;
pub mod usage;
