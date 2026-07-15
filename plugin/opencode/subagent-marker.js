const MARKER_PREFIX = "__SUBAGENT_MARKER__";
const subagents = new Set();
const marked = new Set();
const parents = new Map();

function sessionInfo(event) {
  return event?.properties?.info;
}

export const CopilotApiSubagentMarker = async () => ({
  event: async ({ event }) => {
    const info = sessionInfo(event);
    if (event.type === "session.created" && info?.id) {
      if (info.parentID) subagents.add(info.id);
      parents.set(info.id, info.parentID ?? info.id);
    } else if (event.type === "session.deleted" && info?.id) {
      subagents.delete(info.id);
      marked.delete(info.id);
      parents.delete(info.id);
    }
  },
  "chat.message": async (input, output) => {
    const sessionID = input.sessionID;
    if (!subagents.has(sessionID) || marked.has(sessionID)) return;
    if (!output.message?.id || !output.message?.sessionID) return;

    const marker = `${MARKER_PREFIX}${JSON.stringify({
      session_id: sessionID,
      agent_id: sessionID,
      agent_type: input.agent ?? "opencode-subagent",
    })}`;
    output.parts.unshift({
      id: `prt-${output.message.id}-copilot-api-subagent`,
      sessionID: output.message.sessionID,
      messageID: output.message.id,
      type: "text",
      text: `<system-reminder>\nSubagentStart hook additional context: ${marker}\n</system-reminder>`,
      synthetic: true,
      time: { start: Date.now(), end: Date.now() },
    });
    marked.add(sessionID);
  },
  "chat.headers": async (input, output) => {
    const session = parents.get(input.sessionID);
    if (session) output.headers["x-session-id"] = session;
  },
});
