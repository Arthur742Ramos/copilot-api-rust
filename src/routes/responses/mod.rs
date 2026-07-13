//! OpenAI `/v1/responses` endpoint: route wiring, handler, and stream-id sync.

pub mod compact;
pub mod handler;
pub mod route;
pub mod stream_guard;
pub mod stream_id_sync;
pub mod utils;
