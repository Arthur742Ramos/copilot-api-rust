# Inspector Feedback — Iteration 1

## Verdict: FAIL

## Acceptance Criteria Check

- [ ] Criterion 1 — **FAILED**: The Alpha Search routes, aliases, capability
  checks, local fixture coverage, error envelopes, and metrics are present, but
  the Codex forwarding contract is not fully compatible with the checked-out
  TypeScript reference, and non-missing Codex setup failures are collapsed into
  a misleading 404. Details are below.
- [ ] Criterion 2 — **FAILED**: The provider route matrix is broad and the
  aliases are registered, but the Rust provider handlers use only the
  provider-level type/capabilities. They do not implement the TypeScript
  `models[model].type` effective-provider override, and that unknown model
  configuration field is dropped.
- [ ] Criterion 3 — **FAILED**: The upstream Codex WebSocket transport has
  good handshake-only fallback, cancellation eviction, silence bounds, and
  deterministic state tests. However, pooled reuse does not detect a socket
  closed while idle and marks a connection reusable from only the event-type
  predicate before the Responses lifecycle guard validates the terminal event.
  A subsequent request can therefore use a dead or protocol-invalid socket
  after the handshake phase, with no safe HTTP fallback.
- [ ] Criterion 4 — **FAILED**: Custom/quick-provider onboarding correctly
  separates secrets and avoids prompting for missing custom credentials in
  non-TTY mode. Explicit `auth --provider codex`, however, still enters the
  interactive OAuth callback wait without a TTY guard; a credential-free
  non-TTY invocation remained running after a three-second timeout. The new
  provider credential file also has no restrictive permission fallback on
  non-Unix platforms; the implementation only emits a warning there.
- [x] Criterion 5 — **VERIFIED**: Marketplace assets, Claude hooks/MCP
  configuration, OpenCode plugin/configuration, marker parsing, deferred-tool
  checks, version diagnostics, uninstall instructions, and secret/path scans
  are present. Node syntax checks and an injected OpenCode lifecycle check
  passed without credentials.
- [x] Criterion 6 — **VERIFIED**: The audit pins the checked-out TypeScript
  commit and package version, the installed Claude Code/Codex/OpenCode
  versions match the recorded values, endpoint/provider matrices and runnable
  examples are present, issue forms request redacted diagnostics, and no
  external adoption claim was found.
- [x] Criterion 7 — **VERIFIED**: Existing strict stream, authentication,
  middleware, local Files API, load-shedding, error-redaction, and
  no-billable-replay regression suites remained green. The new WebSocket pool
  defect is tracked under Criterion 3 rather than treated as an existing
  regression.
- [x] Criterion 8 — **VERIFIED**: All requested local gates passed:
  formatting, clippy with `-D warnings`, build, the complete test suite, and
  `cargo deny check` (available locally; warnings did not make it fail).

## Quality Gate

- Command: `cargo fmt --all -- --check` — **PASS**
- Command: `cargo clippy --all-targets -- -D warnings` — **PASS**
- Command: `cargo build` — **PASS**
- Command: `cargo test` — **PASS** (all reported unit and integration suites)
- Command: `cargo deny check` — **PASS** (advisories, bans, licenses, and
  sources passed)
- Additional credential-free checks: plugin syntax/import/lifecycle checks,
  installed client-version checks, and `git diff --check` — **PASS**
- No live server on port 4141 was stopped or restarted, and no credentials were
  used.

## Issues Found

### 1. Pooled Codex WebSockets can reuse a dead or unvalidated connection

`src/services/responses_websocket.rs:297-307` reuses any pool entry whose
boolean `closed` flag is false. A remote close that occurs while the socket is
idle is not observed because Rust has no background close/error listener and no
ready-state check in the reuse branch. The next request reaches
`ws.send(...)` at lines 513-524 only after
`forward_codex_responses_selected` has already received `Ok(stream)`, so the
handshake-only fallback at `create_responses.rs:335-353` cannot run. The
request consequently fails instead of evicting/reopening or falling back at the
safe pre-send boundary.

There is a second reuse-safety problem in the same engine:
`responses_websocket.rs:562-569` marks the connection reusable from
`is_terminal_chunk` before `ResponsesStreamGuard` validates the event's
`response.created` ordering, response object, response id, and event name.
Malformed `response.completed`/`error` frames can therefore leave an invalid
connection in the pool. The existing tests cover clean reuse and close-before-
terminal, but not remote close-after-terminal or malformed-terminal reuse.

This contradicts the documentation's claim that only terminal-clean sockets
are reused and is a functional Codex transport/fallback gap.

### 2. Rust is missing the current TypeScript model-level provider type
override

The checked-out TypeScript reference defines `ModelConfig.type` and
`resolveEffectiveProviderType` (`/tmp/copilot-api-orig/src/lib/config.ts:41-50`
and `709-717`). Its provider Messages, Chat Completions, and Responses handlers
select the wire protocol from that effective per-model type.

Rust's `src/libs/config.rs:27-46` has no model `type` field or flattened
unknown-field map. The provider handlers branch on
`provider_config.provider_type`/provider-level capability only
(`src/routes/provider/messages.rs:181`,
`src/routes/provider/chat_completions.rs:40`, and
`src/routes/provider/responses.rs:54`). A valid TypeScript configuration such
as an Anthropic provider with one model configured as
`{"type":"openai-responses"}` is silently deserialized without that field and
then dispatches the wrong protocol or returns unsupported-capability. This is
not documented as an intentional divergence or covered by a test, so the
provider surface is still narrower than the reference.

### 3. Codex Alpha Search changes a reference-visible request header

The TypeScript Alpha Search transport preserves an inbound `Accept` header and
only supplies `application/json` when it is absent
(`/tmp/copilot-api-orig/src/services/codex/alpha-search.ts:17-25`). Rust's
`src/services/codex/alpha_search.rs:18-24` unconditionally overwrites `Accept`
with `application/json` and also removes `openai-beta`. The Alpha route tests
cover bodies, aliases, and errors but do not assert the forwarded Codex header
contract. If this is an intentional security/protocol divergence, it must be
documented and tested; otherwise the Rust transport should retain the
reference behavior.

### 4. Codex credential setup errors are misreported as provider-not-found

The TypeScript resolver returns `null` only for the specific missing-credential
case and rethrows other setup/refresh errors
(`/tmp/copilot-api-orig/src/lib/provider-resolver.ts:31-38`). Rust's
`src/libs/provider_resolver.rs:40-49` logs every non-missing error and returns
`None`. All affected Codex routes then produce
`Provider 'codex' not found or disabled` with a 404, hiding malformed
credential files, refresh failures, and upstream authentication/network
failures behind the wrong provider-resolution result. This weakens the
authentication/upstream error contract and operator diagnostics.

### 5. Explicit Codex auth still blocks without a TTY, and non-Unix secret
permissions are only advisory

`run_auth` dispatches `AuthPlan::Codex` directly to `run_codex_login`
(`src/main.rs:728-730`), while `login_codex` waits for the local OAuth callback
and only reads stdin after its callback timeout (`src/libs/oauth/codex.rs:343-
375`). Running `copilot-api auth --provider codex` with stdin set to
`/dev/null` and no credentials remained active after three seconds. This
violates the acceptance requirement that onboarding prompts never block a
non-TTY deployment; it should fail fast with actionable non-interactive
guidance (or use an explicitly configured non-interactive credential path).

Additionally, `set_permissions_600` is a no-op on non-Unix targets
(`src/libs/paths.rs:91-108`), and the only fallback is a warning
(`110-123`). The new plaintext `provider_credentials.json` therefore has no
owner-restrictive ACL enforced by the implementation on Windows or another
non-Unix target. The acceptance criterion asks for restrictive fallback
permissions, not only an advisory.

## What Must Be Fixed

1. Make pooled WebSocket reuse state-aware: evict entries on remote
   close/error while idle, verify/reopen a connection before sending, and
   ensure any failure before an observable response event can take the safe
   HTTP fallback. Do not mark a connection reusable until the Responses
   lifecycle guard has accepted a valid terminal event. Add deterministic
   close-after-terminal and malformed-terminal reuse tests.
2. Add the TypeScript-compatible per-model provider type (and any needed
   preserved model fields), resolve the effective type before capability
   checks/dispatch, and add credential-free mixed-provider model tests.
3. Preserve or intentionally specify the Alpha Search header divergence, with
   tests for `Accept`, `openai-beta`, auth, query, and unknown JSON fields.
4. Preserve non-missing Codex resolver errors as explicit internal/upstream
   failures instead of returning provider-not-found.
5. Make explicit built-in OAuth auth reject non-TTY execution before waiting,
   and implement/enforce a restrictive non-Unix credential-file fallback (or
   fail closed when it cannot be established).
6. Re-run every quality gate and the credential-free compatibility tests after
   these fixes.
