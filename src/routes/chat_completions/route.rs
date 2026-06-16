use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::libs::error::AppError;

use super::handler::handle_completion;

/// POST /chat/completions — mirrors routes/chat-completions/route.ts.
pub async fn post_chat_completions(body: Json<Value>) -> Response {
    match handle_completion(body.0).await {
        Ok(response) => response,
        Err(error) => AppError::into_response(error),
    }
}
