//! Local Anthropic/OpenAI Files API compatibility routes and request expansion.

mod api;
mod materialize;

pub use api::router;
pub use materialize::{materialize_anthropic_file_sources, materialize_responses_file_references};
