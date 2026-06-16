//! Shared helpers for the `/v1/messages` route.
//!
//! Mirrors `src/routes/messages/utils.ts`.

/// Maps an OpenAI `finish_reason` to the Anthropic `stop_reason`.
///
/// Mirrors `mapOpenAIStopReasonToAnthropic`:
/// - `stop`           -> `end_turn`
/// - `length`         -> `max_tokens`
/// - `tool_calls`     -> `tool_use`
/// - `content_filter` -> `end_turn`
/// - `null` / unknown -> `None`
pub fn map_openai_stop_reason_to_anthropic(finish_reason: Option<&str>) -> Option<&'static str> {
    match finish_reason {
        Some("stop") => Some("end_turn"),
        Some("length") => Some("max_tokens"),
        Some("tool_calls") => Some("tool_use"),
        Some("content_filter") => Some("end_turn"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_known_finish_reasons() {
        assert_eq!(
            map_openai_stop_reason_to_anthropic(Some("stop")),
            Some("end_turn")
        );
        assert_eq!(
            map_openai_stop_reason_to_anthropic(Some("length")),
            Some("max_tokens")
        );
        assert_eq!(
            map_openai_stop_reason_to_anthropic(Some("tool_calls")),
            Some("tool_use")
        );
        assert_eq!(
            map_openai_stop_reason_to_anthropic(Some("content_filter")),
            Some("end_turn")
        );
        assert_eq!(map_openai_stop_reason_to_anthropic(None), None);
        assert_eq!(map_openai_stop_reason_to_anthropic(Some("other")), None);
    }
}
