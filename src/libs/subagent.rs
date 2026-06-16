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
