# Goal: Perfect Claude Code and Codex integration

## User Request

Our proxy should be as good as https://github.com/caozhiyuan/copilot-api when
working with claude code and codex, specially compatibility, deal with issues,
making sure it has perfect integration with those tools. Let's go.

## Refined Goal

Make the proxy a reliable drop-in API endpoint for the latest stable Claude Code
and OpenAI Codex CLI releases, while preserving the independently verified Claude
Code hardening already present at the initial SHA. Audit the current clients'
actual public HTTP contracts and the TypeScript reference for useful behavior,
then fix every reproducible compatibility gap at the proxy boundary. Prefer
correct, explicit, well-tested behavior over TypeScript parity.

The user was unavailable for follow-up questions, so the selected compatibility
bar is the recommended one: latest stable releases, credential-free deterministic
regression coverage for core agent workflows, and opt-in live canaries that never
consume paid quota during normal tests.

## Acceptance Criteria

- [ ] Record the exact Claude Code and OpenAI Codex CLI stable versions audited,
  how each client is configured to use this proxy, the endpoints and headers each
  exercises, and a feature/transport matrix backed by source, captured fixtures,
  or reproducible commands. Compare relevant behavior with the current
  `caozhiyuan/copilot-api` reference, but classify intentional divergences by
  correctness and client impact rather than treating parity as the objective.
- [ ] Add a credential-free black-box compatibility harness at the public Axum
  boundary. It must exercise realistic client-shaped requests and responses
  rather than only unit-testing translation helpers, use local deterministic
  upstream fixtures, avoid port 4141, and cover both clients without network or
  paid-provider access in normal CI.
- [ ] Claude Code compatibility remains complete for the audited stable client:
  Messages streaming and non-streaming, system/content variants, tool
  definitions and multi-turn tool results, parallel tool use where supported,
  prompt-cache markers, thinking/reasoning blocks, token counting, model aliases,
  beta/version headers, unknown-field preservation, cancellation/truncation, and
  retryable failures all produce SDK-recognizable Anthropic behavior. Existing
  verified behavior must not regress.
- [ ] Codex CLI compatibility is audited and implemented against the protocol the
  audited client actually uses, including the OpenAI Responses API rather than
  assuming Chat Completions is sufficient. Cover streaming and non-streaming
  Responses, instructions/input forms, reasoning items, function calls and
  outputs, parallel/interleaved calls, multi-turn continuation fields, usage,
  model selection, request metadata and unknown fields, cancellation/truncation,
  and any compaction or auxiliary endpoint that the audited stable client
  requires. Retain Chat Completions compatibility where it remains part of the
  supported surface.
- [ ] Each public API emits its native client contract: Anthropic JSON/SSE for
  Claude Code and OpenAI JSON/SSE for Codex. Streams have deterministic event
  ordering, preserve IDs and call arguments, terminate exactly once with a valid
  success terminal event or a protocol-native error, never fabricate success
  after malformed input or premature EOF, and do not leak one protocol's event or
  error shape into the other.
- [ ] Authentication and routing work with the documented client configuration:
  client API keys are validated consistently, relevant safe client headers are
  accepted or forwarded, provider/Codex credentials are selected without
  requiring unrelated GitHub initialization in provider-only mode, model aliases
  resolve consistently across `/v1/models`, Messages, Chat Completions, and
  Responses, and unsupported models or routes fail explicitly.
- [ ] HTTP and in-stream failures use the envelope, status, and retry metadata
  recognized by the calling client. Tests cover at least malformed JSON,
  validation failures, authentication/permission failures, unknown models or
  routes, oversized requests/responses, rate limits with `Retry-After`, overload
  or transient transport failures, upstream 5xx failures, malformed stream
  frames, and premature EOF. Internal details remain sanitized while request IDs
  remain usable for diagnosis.
- [ ] Add regression tests for every gap fixed and for all compatibility claims in
  the matrix. Tests must be deterministic, credential-free, and assert both
  positive workflows and failure semantics. If actual installed CLIs can be run
  safely against local fixtures, include reproducible opt-in smoke commands; do
  not substitute a live paid call for deterministic coverage.
- [ ] Update user-facing documentation with exact setup examples for Claude Code
  and Codex CLI, supported/unsupported features, model and provider selection,
  authentication expectations, troubleshooting guidance, and the audited client
  versions. Do not claim support that the tests or reproducible evidence do not
  establish.
- [ ] Keep changes surgical, preserve unknown JSON fields and stable key order,
  reuse shared routing/error/stream helpers, avoid silent fallbacks, and do not
  weaken the proxy's admission, response-size, retry-safety, remote-auth, or
  internal-error hardening.
- [ ] `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build --verbose`,
  `cargo test --verbose`, and `cargo deny check` all pass.

## Scope Boundaries

**In scope:**
- Public API behavior and configuration needed by the latest stable Claude Code
  and OpenAI Codex CLI clients.
- Anthropic Messages and token-counting routes, OpenAI Responses and Chat
  Completions routes, model discovery, protocol-native streaming and errors,
  authentication/routing, deterministic fixtures, integration tests, and setup
  and troubleshooting documentation.
- Fixes for reproducible compatibility defects found in this repository, in the
  clients' current contracts, or in relevant behavior from the TypeScript
  reference.
- Shared refactoring only where necessary to make behavior consistent and
  testable across these client surfaces.

**Out of scope:**
- Exact token-fragment cadence, undocumented UI behavior, or blind parity with
  ChatGPT Web, Claude.ai, or the TypeScript implementation.
- Modifying Claude Code or Codex CLI, client-side retry policy, provider billing
  accuracy, quota availability, or live calls that spend paid quota.
- TLS termination, firewall/deployment management, distributed quotas, unrelated
  performance/security work, dependency upgrades, or broad cleanup.
- Restarting or hot-swapping the dogfooded proxy on port 4141.
- Supporting every historical client release; compatibility targets the audited
  latest stable releases, with recent versions preserved where existing tests
  establish it.

## Applicable Project Conventions

**Quality gate command:**
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo build --verbose`
- `cargo test --verbose`
- `cargo deny check`

**Commit convention:**
- Conventional commit title: `type(scope): [B/I] description`, imperative mood,
  at most 72 characters.
- Builder trailer: `Assisted-by: OpenAI:GPT-5.6-Sol`
- Inspector trailer: `Assisted-by: OpenAI:GPT-5.6-Luna`
- Repository trailer:
  `Co-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>`

**Guidelines:**
- `AGENTS.md`
- `CONTRIBUTING.md`
- `.github/workflows/ci.yml`
- `deny.toml`
- `docs/claude-code-api-compatibility.md`

**Rules:**
- Prefer correctness, client experience, performance, and clarity over parity
  with the TypeScript reference at `/tmp/copilot-api-orig`.
- Preserve unknown JSON keys with Serde flattening and stable key order.
- Use `AppError` for client/internal failures and `HttpError` for upstream
  responses; surface failures explicitly in the calling protocol's native shape.
- Follow the existing `libs`, `routes`, and `services` architecture and reuse
  shared helpers before adding parallel implementations.
- Keep normal tests credential-free and never disrupt the live server on port
  4141; use a scratch port for any opt-in runtime probe.
- Run every repository CI gate before completion.
