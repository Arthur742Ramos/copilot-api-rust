use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::routes::parse_json_body;

use super::handler::handle_completion;

/// POST /chat/completions — mirrors routes/chat-completions/route.ts.
pub async fn post_chat_completions(headers: HeaderMap, body: Bytes) -> Response {
    let payload = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_openai_response(),
    };
    match handle_completion(payload, headers).await {
        Ok(response) => response,
        Err(error) => error.into_openai_response(),
    }
}
