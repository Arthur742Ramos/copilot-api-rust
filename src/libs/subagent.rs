use serde::{Deserialize, Serialize};

/// Mirrors src/lib/subagent.ts. A marker embedded in request payloads that the
/// proxy recognises to attribute work to a spawned subagent.
pub const SUBAGENT_MARKER_PREFIX: &str = "__SUBAGENT_MARKER__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentMarker {
    pub session_id: String,
    pub agent_id: String,
    pub agent_type: String,
}

/// Mirrors `parseSubagentMarkerFromSystemReminder`: scans `<system-reminder>`
/// sections in `text` for the marker prefix, JSON-parsing the trailing slice.
fn parse_subagent_marker_from_system_reminder(text: &str) -> Option<SubagentMarker> {
    const START_TAG: &str = "<system-reminder>";
    const END_TAG: &str = "</system-reminder>";
    let mut search_from = 0usize;

    while let Some(rel) = text[search_from..].find(START_TAG) {
        let reminder_start = search_from + rel;
        let content_start = reminder_start + START_TAG.len();
        let reminder_end = match text[content_start..].find(END_TAG) {
            Some(rel) => content_start + rel,
            None => break,
        };

        let reminder_content = &text[content_start..reminder_end];
        let after = reminder_end + END_TAG.len();

        match reminder_content.find(SUBAGENT_MARKER_PREFIX) {
            None => {
                search_from = after;
                continue;
            }
            Some(marker_index) => {
                let marker_json = reminder_content[marker_index + SUBAGENT_MARKER_PREFIX.len()..]
                    .trim();
                match serde_json::from_str::<SubagentMarker>(marker_json) {
                    Ok(parsed)
                        if !parsed.session_id.is_empty()
                            && !parsed.agent_id.is_empty()
                            && !parsed.agent_type.is_empty() =>
                    {
                        return Some(parsed);
                    }
                    _ => {
                        search_from = after;
                        continue;
                    }
                }
            }
        }
    }

    None
}

/// Mirrors `parseSubagentMarkerFromFirstUser`: finds the first user message with
/// array content and scans each text block for a subagent marker.
pub fn parse_subagent_marker_from_first_user(
    payload: &serde_json::Value,
) -> Option<SubagentMarker> {
    let messages = payload.get("messages")?.as_array()?;
    let first_user = messages.iter().find(|msg| {
        msg.get("role").and_then(|r| r.as_str()) == Some("user")
            && msg.get("content").map(|c| c.is_array()).unwrap_or(false)
    })?;

    let content = first_user.get("content")?.as_array()?;
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        let text = match block.get("text").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };
        if let Some(marker) = parse_subagent_marker_from_system_reminder(text) {
            return Some(marker);
        }
    }

    None
}
