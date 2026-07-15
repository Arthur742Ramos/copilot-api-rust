# Goal: Lead on every non-GUI capability

## User Request

Other than the GUI, I want to win on everything.

The attached comparison identifies the remaining TypeScript advantages as
Responses WebSocket support, Codex Alpha Search, provider-specific route
breadth, guided provider configuration, Claude Code/OpenCode plugins, and
community/battle-testing.

## Refined Goal

Make the Rust gateway at least feature-equal to the current TypeScript
reference for every actionable non-GUI capability in the comparison, and
prefer a safer or more complete design where parity would be weaker. Close the
real protocol and route gaps, provide first-class terminal onboarding and
installable client integrations, and strengthen public conformance evidence so
the resulting advantages are reproducible rather than merely claimed. If the
comparison mischaracterizes a feature, verify the current TypeScript source and
implement the actual advantage or document why Rust already wins.

## Acceptance Criteria

- [ ] Criterion 1: Codex Alpha Search works end to end through
  `POST /alpha/search` and `POST /v1/alpha/search`, with the provider-scoped
  versioned and unversioned forms supported wherever the selected provider has
  a compatible capability. The implementation reuses existing authentication,
  HTTP, provider-resolution, error-envelope, timeout, metrics, and redaction
  conventions; unsupported providers fail explicitly; deterministic tests
  cover success, malformed input, authentication/upstream errors, and route
  aliases without live credentials.
- [ ] Criterion 2: The provider-prefixed API surface is no narrower than the
  current TypeScript reference. At minimum, versioned routes cover Messages,
  token counting, Responses, Responses compaction, Chat Completions, model
  discovery, image generation/editing, and Alpha Search where meaningful;
  current TypeScript-compatible unversioned aliases are available or a
  demonstrably safer intentional divergence is documented and tested. Existing
  provider handlers are actually registered, route precedence is unambiguous,
  provider capability checks are explicit, unknown JSON fields continue to
  round-trip, and streaming/non-streaming tests exercise the newly reachable
  routes.
- [ ] Criterion 3: Close the actual Responses WebSocket gap found in the
  current TypeScript source. If TypeScript's WebSocket is upstream-only, add
  reliable Codex upstream Responses WebSocket transport rather than inventing
  a public protocol: capability-based selection, connection reuse where safe,
  cancellation, heartbeat/silence handling, exact terminal-event accounting,
  and fallback to HTTP/SSE only before any response bytes/events are exposed.
  Preserve billable-request safety and never replay after observable progress.
  Add deterministic transport/state-machine tests plus useful metrics and
  diagnostics. If current evidence proves Rust already meets or exceeds the
  reference, codify that in tests and correct the compatibility documentation.
- [ ] Criterion 4: `copilot-api auth` provides guided terminal onboarding when
  invoked interactively without a provider, while `--provider` and all existing
  non-interactive automation remain stable. The flow discovers Copilot, Codex,
  built-in quick providers present in the current TypeScript reference
  (including DeepSeek, DashScope, OpenRouter, and OpenCode Go when still
  supported), and custom Anthropic/OpenAI Chat/OpenAI Responses-compatible
  providers; validates names, URLs, auth modes, and model/capability choices;
  offers a bounded provider health probe; writes configuration atomically; and
  provides actionable help/completion/discoverability. Secrets must never be
  emitted to logs or ordinary config/output, must use the repository's secure
  credential storage and restrictive fallback permissions, and prompts must
  never block non-TTY or preconfigured deployments. Tests cover interactive
  choices through injected I/O as well as non-interactive and failure paths.
- [ ] Criterion 5: Ship usable, documented non-GUI integrations for Claude
  Code and OpenCode rather than only internal compatibility helpers. Claude
  Code integration is installable through the current plugin/marketplace
  mechanism and exercises subagent markers and deferred tool search; OpenCode
  integration has an installable plugin or generated configuration appropriate
  to its current extension model. Existing marker parsing, propagation,
  deferred-tool behavior, plugin/client version diagnostics, and provider
  selection are verified with credential-free tests. Integration assets contain
  no machine-specific paths or secrets and have a documented install,
  uninstall, configuration, and troubleshooting path.
- [ ] Criterion 6: Strengthen the engineering-controlled portion of
  community/battle-testing. Refresh the public comparison/compatibility audit
  against the checked-out current TypeScript reference commit and the supported
  Claude Code/Codex/OpenCode client versions; add concise endpoint/provider
  support matrices and runnable examples; add structured bug, compatibility,
  and feature-request issue templates that collect redacted diagnostics; and
  ensure all new behavior is covered by deterministic cross-platform-safe tests.
  Do not claim external adoption, stars, forks, or real-world diversity that the
  repository has not earned.
- [ ] Criterion 7: Preserve all existing Rust advantages and security
  invariants: strict malformed/truncated stream failures, exact Anthropic and
  OpenAI event lifecycles, local Files API behavior and quotas, load shedding,
  remote-bind authentication safeguards, health/version/metrics endpoints,
  structured redaction, and no automatic retry of billable image or generation
  requests. New routes use the same client authentication and middleware as
  equivalent existing routes. Regression tests remain green.
- [ ] Criterion 8: All repository quality gates pass from a clean test
  invocation: `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`, and
  `cargo deny check` when the existing CI tool is available. No test requires
  network credentials, an interactive terminal, or a platform-specific secret
  service.

## Scope Boundaries

**In scope:**
- Current Rust repository code, tests, CLI UX, plugin/configuration assets,
  documentation, issue templates, metrics, and compatibility evidence.
- The current checked-out TypeScript source under `/tmp/copilot-api-orig` (or
  its locally available equivalent) as reference material, not as a parity
  constraint.
- Better-than-reference behavior where it improves correctness, security,
  client experience, performance, or clarity.
- Engineering work that makes broader external testing and useful issue reports
  easier.

**Out of scope:**
- Any Electron, desktop, web, or other graphical configuration GUI.
- Guaranteed external adoption, stars, forks, community size, or third-party
  incident discovery.
- Live credential-dependent calls in CI or committing credentials/test tokens.
- Unrelated cleanup or behavior changes that do not support the listed gaps.
- Publishing a release, marketplace listing, crate, image, or binary; all
  publishable assets and documentation should nevertheless be release-ready.

## Applicable Project Conventions

**Quality gate commands:**
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo build`
- `cargo test`
- `cargo deny check` (blocking in CI; run locally when available)

**Commit convention:**
- Semantic/conventional commit titles.
- Builder iteration commit:
  `type(scope): [B] description` (imperative, at most 72 characters).
- Inspector iteration commit:
  `chore(scope): [I] description` (imperative, at most 72 characters).
- Builder Assisted-by trailer: `Assisted-by: OpenAI:GPT-5.6-Sol`
- Inspector Assisted-by trailer: `Assisted-by: OpenAI:GPT-5.6-Luna`
- Every commit also includes:
  `Co-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>`

**Guidelines:**
- `AGENTS.md`
- `CONTRIBUTING.md`
- `.github/workflows/ci.yml`

**Rules:**
- Prefer the better engineering choice over TypeScript parity.
- Keep changes surgical but complete across `src/libs/`, `src/routes/`, and
  `src/services/`.
- Preserve unknown JSON fields and stable key ordering.
- Use `AppError` for client/internal errors and `HttpError` for upstream
  status/header/body propagation; never silently fail or synthesize success.
- Reuse existing helpers and infrastructure before adding new abstractions.
- Keep type safety; avoid broad catches, silent defaults, and unnecessary casts.
- Do not stop or casually restart the live server on port 4141; use the
  repository's scratch-server verification workflow if runtime probing is
  needed.
- Keep all tests deterministic and cross-platform; warnings are errors in CI.
