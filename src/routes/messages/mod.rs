//! Anthropic `/v1/messages` endpoint: request preprocessing and bidirectional
//! translation between the Anthropic Messages API and the Copilot backend.

pub mod anthropic_types;
pub mod api_flows;
pub mod count_tokens_handler;
pub mod handler;
pub mod non_stream_translation;
pub mod preprocess;
pub mod responses_stream_translation;
pub mod responses_translation;
pub mod route;
pub mod stream_translation;
pub mod utils;
pub mod web_search;
