use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{Map, Value};

use crate::libs::state;

/// GET /token — mirrors routes/token/route.ts. TS does `c.json({ token })` where
/// an undefined token is dropped by JSON.stringify, yielding `{}`; mirror that by
/// omitting the key when no token is set rather than emitting `{"token":null}`.
pub async fn get_token() -> Response {
    let token = state::with_state(|s| s.copilot_token.clone());
    let mut body = Map::new();
    if let Some(token) = token {
        body.insert("token".to_string(), Value::String(token));
    }
    Json(Value::Object(body)).into_response()
}
