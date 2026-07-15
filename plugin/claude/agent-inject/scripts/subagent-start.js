async function readInput() {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  return input.trim();
}

let input = {};
const raw = await readInput();
if (raw) {
  try {
    input = JSON.parse(raw);
  } catch {
    input = {};
  }
}

const marker = `__SUBAGENT_MARKER__${JSON.stringify({
  session_id: input.session_id ?? null,
  agent_id: input.agent_id ?? null,
  agent_type: input.agent_type ?? null,
})}`;
process.stdout.write(
  `${JSON.stringify({
    hookSpecificOutput: {
      hookEventName: "SubagentStart",
      additionalContext: marker,
    },
  })}\n`,
);
