# Terminal client integrations

These assets contain no credentials or machine-specific paths. Install
`copilot-api` on `PATH` before enabling them.

## Claude Code

In Claude Code, add this repository as a marketplace and install both plugins:

```text
/plugin marketplace add https://github.com/Arthur742Ramos/copilot-api-rust.git
/plugin install agent-inject@copilot-api-rust-marketplace
/plugin install tool-search@copilot-api-rust-marketplace
```

`agent-inject` emits the `__SUBAGENT_MARKER__` context consumed by the gateway.
`tool-search` starts `copilot-api mcp` from `PATH` for deferred Responses tools.
Configure Claude Code to use `http://127.0.0.1:4141` as `ANTHROPIC_BASE_URL`;
see the main README for the complete environment block.

Uninstall:

```text
/plugin uninstall agent-inject@copilot-api-rust-marketplace
/plugin uninstall tool-search@copilot-api-rust-marketplace
/plugin marketplace remove copilot-api-rust-marketplace
```

## OpenCode

Copy the plugin and merge the example configuration:

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/opencode/plugins"
cp plugin/opencode/subagent-marker.js \
  "${XDG_CONFIG_HOME:-$HOME/.config}/opencode/plugins/"
```

Merge `plugin/opencode/opencode.example.json` into your OpenCode configuration,
then select the `copilot-api` provider. Change model names to models exposed by
your gateway. The placeholder API key authenticates only when you explicitly
configure that same local gateway key; do not put upstream provider credentials
in the OpenCode file.

Uninstall by deleting `subagent-marker.js`, the `copilot-api` provider, and the
`tool_search` MCP entry from the OpenCode configuration.

## Troubleshooting

- Run `copilot-api doctor` and `copilot-api debug --json`; both redact secrets.
- Run `copilot-api completions <shell>` to install CLI completions.
- If `tool_search` cannot start, verify `copilot-api` is on `PATH` in the client
  process, not only in an interactive shell.
- If subagent calls are billed as user calls, confirm the marker plugin is
  enabled and that no client middleware removes synthetic system reminders.
- For a provider-specific route, use the endpoint matrix in
  `docs/non-gui-compatibility.md` and confirm the configured capability list.
