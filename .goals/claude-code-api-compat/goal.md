# Goal: Improve Claude Code API compatibility

## User Request

Sometimes, when using the API, we run into API errors in Claude Code. Make sure
the proxy supports a broad set of Claude Code behaviors, using
https://github.com/caozhiyuan/copilot-api as a reference.

## Refined Goal

Harden the proxy's Claude Code-facing Anthropic API behavior so common request,
streaming, tool-use, model-routing, and failure paths work reliably instead of
surfacing avoidable API errors. Audit the established Rust implementation
against the current `caozhiyuan/copilot-api` reference and real Claude Code
protocol expectations, then implement every high-confidence compatibility gap
found within the in-scope surfaces. Exact TypeScript parity is not required:
intentional Rust behavior may diverge when it is more correct, robust, or
client-friendly, but each divergence must be explicit and covered by tests.

## Acceptance Criteria

- [ ] A checked-in compatibility matrix documents the Claude Code-facing
  behaviors audited against `caozhiyuan/copilot-api`, including the relevant
  Anthropic endpoints, request forms, streaming event forms, tool-use flows,
  model handling, and error semantics. Every audited item is marked supported,
  fixed by this goal, intentionally divergent, or out of scope, with a code or
  test reference.
- [ ] Common Claude Code message requests no longer fail because of avoidable
  translation or validation gaps. Coverage includes both streaming and
  non-streaming requests, string and structured content, system content,
  tool definitions and tool choice, tool-use/tool-result turns, optional
  metadata, and preservation of safe unknown JSON fields where the proxy
  round-trips payloads.
- [ ] Anthropic streaming responses used by Claude Code produce valid,
  deterministic event ordering and terminal behavior for text, thinking or
  reasoning content when supplied, tool calls (including fragmented arguments),
  usage, stop reasons, upstream error events, malformed chunks, interrupted
  streams, and normal completion. A stream must not silently end when the client
  instead needs a terminal error event.
- [ ] Claude Code-facing non-streaming and streaming failures use an
  SDK-recognizable Anthropic error envelope, preserve the appropriate HTTP
  status and safe diagnostic message, classify retryable rate-limit/overload
  failures correctly, and forward relevant retry/rate-limit/request-correlation
  headers without leaking internal details.
- [ ] Model discovery, model aliases/normalization, and Claude Code request
  headers or beta-feature headers used by the audited reference are accepted or
  intentionally handled without brittle hard-coded rejection. Unsupported
  capabilities fail clearly as client errors rather than panics, malformed
  success responses, or opaque internal errors.
- [ ] Regression tests exercise each compatibility fix and the compatibility
  matrix's critical supported paths. Tests are deterministic and credential-free
  and include representative Claude Code-style fixtures for normal messages,
  tools, streaming fragmentation, and upstream errors.
- [ ] Existing behavior outside the audited compatibility surface remains
  intact, and all repository quality gates pass:
  `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`,
  `cargo build --verbose`, and
  `cargo test --verbose`.

## Scope Boundaries

**In scope:**
- Claude Code-facing Anthropic-compatible HTTP routes and any shared
  translation, model, header, provider, or error infrastructure required to
  make those routes reliable.
- Comparison with the current `caozhiyuan/copilot-api` implementation as
  reference material, including fetching it when the documented local checkout
  is unavailable.
- Surgical compatibility improvements and deterministic regression tests.
- Documentation needed to record audited behavior and intentional divergences.

**Out of scope:**
- Exact byte-for-byte or bug-for-bug parity with the TypeScript reference.
- Rewriting the proxy architecture or unrelated OpenAI/provider features that
  are not exercised by the Claude Code-facing flow.
- Live tests requiring GitHub Copilot credentials, paid-provider credentials,
  or production quota.
- Client-side retry implementation inside Claude Code.
- Restarting or hot-swapping the dogfood server on port 4141.
- Unrelated security, deployment, UI, or performance work.

## Applicable Project Conventions

**Quality gate commands:**
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo build --verbose`
- `cargo test --verbose`
- `cargo deny check` when `cargo-deny` is available

**Commit convention:**
- Conventional commits with an explanatory message, following
  `type(scope): [B/I] description` with a title of at most 72 characters.
- Builder trailer: `Assisted-by: OpenAI:GPT-5.6-Sol`
- Inspector trailer: `Assisted-by: OpenAI:GPT-5.6-Luna`
- Include
  `Co-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>`.

**Guidelines:**
- `AGENTS.md`
- `CONTRIBUTING.md`
- `.github/workflows/ci.yml`

**Rules:**
- Prefer correctness, client experience, performance, and clarity over strict
  TypeScript parity.
- Preserve unknown JSON keys where payloads round-trip; use serde flattening or
  value-walking consistent with existing code.
- Use `AppError` for client/internal failures and `HttpError` for upstream
  responses; surface Anthropic-compatible error shapes and never fail silently.
- Keep changes surgical, reuse existing helpers, maintain type safety, and avoid
  broad catches or success-shaped fallbacks.
- Do not stop or casually restart the server on port 4141 while dogfooding.
