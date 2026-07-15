# Non-GUI capability and compatibility audit

This is an engineering evidence matrix, not an adoption claim. It records
deterministic behavior controlled by this repository and makes no claim about
stars, forks, external traffic diversity, or provider uptime.

## Audit identity

Audited **2026-07-15**:

| Component | Exact identity |
|---|---|
| TypeScript reference | `caozhiyuan/copilot-api` `287d2d330c299bbdf3ed213a1bc05b1739aecf03`, package `1.14.9` |
| Claude Code | `2.1.210` |
| OpenAI Codex CLI | `0.144.1` |
| OpenCode | npm package `opencode-ai` `1.17.15` |
| Rust evidence | `cargo test`, especially `non_gui_features`, `integration_assets`, `client_compatibility`, and WebSocket unit tests |

The TypeScript source proves its Responses WebSocket is an **upstream
transport**, not a public WebSocket endpoint. Rust follows that design: clients
continue to use HTTP/SSE, while configured Codex streaming requests may use the
pooled upstream WebSocket. A failed handshake falls back before
`response.create` is sent. Send/read/terminal failures after that boundary never
replay billable work.

## Public endpoints

`/v1` and unversioned forms are both available where shown. Provider-scoped
aliases accept either `/{provider}/v1/...` or `/{provider}/...`.

| Capability | Public route | Provider-scoped route | Evidence |
|---|---|---|---|
| Anthropic Messages | `POST /v1/messages` | `POST /:provider[/v1]/messages` | `provider_routing`, `client_compatibility` |
| Token counting | `POST /v1/messages/count_tokens` | `POST /:provider[/v1]/messages/count_tokens` | `provider_routing` |
| Chat Completions | `POST /[v1/]chat/completions` | `POST /:provider[/v1]/chat/completions` | `non_gui_features` JSON/SSE |
| Responses | `POST /[v1/]responses` | `POST /:provider[/v1]/responses` | `non_gui_features` JSON/SSE |
| Responses compaction | `POST /[v1/]responses/compact` | `POST /:provider[/v1]/responses/compact` | compact contract tests |
| Models | `GET /[v1/]models` | `GET /:provider[/v1]/models` | route/model tests |
| Images generation/editing | `POST /[v1/]images/{generations,edits}` | `POST /:provider[/v1]/images/{generations,edits}` | image and provider tests |
| Alpha Search | `POST /[v1/]alpha/search` | `POST /:provider[/v1]/alpha/search` | `non_gui_features` success/error/alias tests |
| Local Files | `/[v1/]files...` | materialized before provider dispatch | Files API quota/lifecycle tests |

Fixed routes take precedence over `:provider` routes in Axum. Every new route
passes through the same client authentication, body limit, admission,
redaction, trace, panic, and metrics middleware as its existing counterpart.

## Provider defaults

An explicit `capabilities` array overrides these conservative defaults.
Unsupported combinations return `400` without contacting an upstream.
`models.<model>.type` may select a different effective protocol for one model;
route capabilities, endpoint dispatch, and default auth mode are resolved from
that effective type. Unknown model fields are retained.

| Provider type | messages | count | models | chat | responses | compact | images | alpha search |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `anthropic` | yes | yes | yes | no | no | no | no | no |
| `openai-compatible` | yes (translated) | yes | yes | yes | no | no | yes | no |
| `openai-responses` | yes (translated) | yes | yes | no | yes | yes | yes | yes |
| built-in `codex` | yes (translated) | yes | local catalog | no | yes | yes | yes | yes |

Provider API keys configured by `copilot-api auth` are stored in
`provider_credentials.json`. Unix mode is set and verified as `0600`; Windows
uses a protected DACL verified to contain only the current user's allow rule.
Any unsupported platform or ACL failure stops before secret/config access.
`config.json` contains protocol metadata and capability/model choices, not the
secret. Non-interactive custom setup reads a named environment variable and
never prompts; built-in OAuth requires a TTY and fails immediately otherwise.

## Runnable examples

Guided onboarding:

```sh
copilot-api auth
```

Non-interactive OpenAI Responses provider:

```sh
export TEAM_OPENAI_KEY='replace-at-runtime'
copilot-api auth --provider custom --name team-openai \
  --type openai-responses --base-url https://api.example.com \
  --api-key-env TEAM_OPENAI_KEY \
  --model gpt-example \
  --capability responses,responses_compact,models,alpha_search \
  --probe
```

Alpha Search through Codex or a provider:

```sh
curl -sS http://127.0.0.1:4141/v1/alpha/search \
  -H 'content-type: application/json' \
  -H "authorization: Bearer $COPILOT_API_KEY" \
  -d '{"query":"Rust WebSocket cancellation"}'

curl -sS http://127.0.0.1:4141/team-openai/v1/alpha/search \
  -H 'content-type: application/json' \
  -H "authorization: Bearer $COPILOT_API_KEY" \
  -d '{"query":"provider capability routing"}'
```

Generate shell completions:

```sh
copilot-api completions bash > copilot-api.bash
```

Client plugin installation and removal are documented in
[`plugin/README.md`](../plugin/README.md).

## Safer intentional differences

- No public WebSocket protocol is invented. Upstream WebSocket pooling is
  selected only for Codex Responses streams and never when proxy-from-env is
  active. Custom Codex hosts stay on the DNS-pinned HTTP client unless the
  explicit private-provider development override is set.
- Connections cancelled before a terminal event are evicted; queued frames can
  never leak into the next request.
- Every socket passes a bounded ping/pong preflight before `response.create`.
  A failed pooled socket is evicted and reopened once before the safe HTTP
  fallback boundary; ambiguous application-frame send failures are never
  replayed. An idle watcher evicts remote closes and stale post-terminal frames.
- Silence has a bounded per-frame deadline. Ping/pong frames keep a live
  connection healthy but are not exposed as model events.
- Completion, failure, incomplete, and error are terminal exactly once. Missing
  terminals and malformed/truncated streams remain failures. Only a terminal
  accepted by the shared Responses lifecycle guard makes a socket reusable.
- Alpha Search preserves caller-supplied `Accept` and `openai-beta` headers,
  defaults missing `Accept` to JSON, replaces inbound authorization, and
  preserves query parameters and unknown JSON fields.
- Generation, image, and other billable requests are not automatically replayed
  after observable progress.

## Reporting new evidence

Use the structured bug or compatibility issue form. Attach only redacted
`copilot-api debug --json`, `doctor --json`, a trace ID, and a credential-free
fixture. Never attach tokens, account IDs, prompts, uploaded file content,
private provider URLs, or machine-specific paths.
