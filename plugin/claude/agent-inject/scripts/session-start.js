async function readInput() {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  return input;
}

await readInput();
process.stdout.write(
  `${JSON.stringify({
    hookSpecificOutput: {
      hookEventName: "SessionStart",
      additionalContext:
        "copilot-api integration active: preserve synthetic subagent markers.",
    },
  })}\n`,
);
