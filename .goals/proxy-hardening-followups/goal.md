# Goal: Complete proxy hardening follow-ups

## User Request

Implement every improvement identified in the proxy audit ("Do all"):
bounded and cancellation-safe admission, cost-safe retries, complete upstream
response-size caps, non-blocking token-budget admission, accurate stream
lifecycle accounting, secure remote exposure and internal errors,
provider-aware startup/readiness, and the related documentation corrections.

## Refined Goal

Harden the proxy end to end without regressing its OpenAI, Anthropic, Copilot,
Codex, or configured-provider behavior. The implementation must bound live work
through the full lifetime of streaming bodies, eliminate ambiguous default
replays of billable generations, keep blocking SQLite work off async workers,
make stream completion/cancellation observable and correctly finalized, close
all remaining unbounded upstream buffering paths, and make unauthenticated
remote exposure an explicit opt-in. It must also support an explicit
provider-only startup mode that does not require GitHub/Copilot initialization
and reports readiness for the selected provider.

## Acceptance Criteria

- [ ] A configurable global in-flight limit and configurable per-API-key limit
  protect billable proxy requests. Limits have documented bounded defaults (and
  an explicit documented disable mechanism if supported), overload returns a
  structured retryable response with `Retry-After`, and metrics expose active,
  queued, and rejected work using bounded labels.
- [ ] Admission permits remain held until a streaming response body completes or
  is dropped; returning response headers must not release the permit. Unit or
  integration tests prove this for both normal completion and client
  cancellation.
- [ ] Any admission queue is bounded by both waiter count and total wait time.
  Existing `--wait` rate limiting no longer leaves phantom reservations when a
  waiting request/future is cancelled, and tests cover cancellation, queue
  overflow, timeout, reject mode, and fair-enough serialized admission.
- [ ] Default retry policy for billable generation endpoints (messages, chat,
  Responses, Codex generation, images where applicable) retries connection
  failures and 429 responses but does not automatically replay ambiguous
  502/503/504 responses. Ambiguous 5xx replay is available only through an
  explicit documented opt-in or a provider-guaranteed idempotency mechanism.
  Safe/idempotent endpoints may retain bounded transient-status retries. Tests
  verify policy selection and `Retry-After` behavior.
- [ ] Every request-serving, provider, authentication, token, usage, OAuth, and
  count-token path that buffers a `reqwest::Response` uses an explicit byte cap.
  The non-streaming Codex provider Responses path is fixed. The self-updater, if
  it remains a direct asset buffer, has its own explicit maximum download size.
  No unbounded `.bytes()`, `.text()`, or `.json()` response consumption remains
  without a narrowly justified and documented reason.
- [ ] Token-budget admission performs all SQLite reads on a blocking thread (or
  uses an equally safe in-memory accounting design). No synchronous SQLite call
  can block a Tokio worker during request admission, including cache refreshes
  and per-key checks. Existing global/per-key semantics and metrics remain
  correct, with concurrency tests.
- [ ] Stream lifecycle instrumentation distinguishes `ok`, `error`, and
  `cancelled`. A stream is `ok` only after an explicit protocol terminal marker;
  dropping an unfinished downstream body records `cancelled`; malformed or
  prematurely ended upstream streams record `error`. Any already-observed usage
  is finalized at most once on completion, error, or cancellation.
- [ ] Third-party provider streaming and non-streaming routes participate in the
  same request summary, TTFT, active-stream, completion, outcome, and usage
  finalization mechanisms as builtin routes. Tests cover a dropped provider
  stream and a clean provider terminal event.
- [ ] Non-loopback startup with no general API key fails closed unless the
  operator supplies an explicit, conspicuously named unsafe opt-out. Loopback
  startup remains convenient and backward compatible. Docker examples/config
  do not silently opt out of this protection and document a usable authenticated
  startup path.
- [ ] Unexpected internal failures no longer expose raw `anyhow`, filesystem,
  parser, transport, or runtime details to clients. The full cause is logged
  under the request trace; the response is a generic 500 with a usable trace
  reference. Malformed, oversized, or unreadable upstream responses are reported
  as 502-class upstream failures, while intentional client errors retain their
  existing safe messages. Tests verify both secrecy and status mapping.
- [ ] An explicit provider-only startup mode selects a configured Codex or
  third-party provider, skips GitHub/Copilot authentication and model-cache
  bootstrap, validates the selected provider, and makes `/readyz` reflect that
  provider's usable state. Default startup behavior remains Copilot mode.
  Provider-prefixed routes work in provider-only mode, and tests cover a
  configured third-party provider, Codex credentials, invalid provider names,
  and readiness failure reasons without using live network services.
- [ ] README/CLI/config documentation describes every new limit, timeout, retry,
  remote-auth, unsafe opt-out, and provider-only setting. The README no longer
  claims `GET /token` returns a live bearer; it accurately documents
  presence/expiry only. Existing undocumented
  `COPILOT_API_UPSTREAM_MAX_RETRIES` behavior is documented or replaced by the
  new retry configuration.
- [ ] Changes are surgical, preserve unknown JSON fields and existing protocol
  compatibility, avoid silent fallbacks, and add regression tests for every
  behavior change.
- [ ] `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build --verbose`,
  `cargo test --verbose`, and `cargo deny check` all pass.

## Scope Boundaries

**In scope:**
- All eight audited follow-ups and their tests, runtime configuration,
  metrics, CLI/config wiring, Docker examples, and README updates.
- Refactoring shared admission, body-finalization, response-reading, and error
  helpers when needed to avoid duplicated or inconsistent behavior.
- Behavior-safe defaults for loopback users and explicit migration guidance for
  remotely exposed deployments.

**Out of scope:**
- TLS termination, firewall management, or a full reverse-proxy deployment
  system.
- Distributed/multi-process quota coordination or exact pre-reservation of a
  daily token budget.
- Replacing SQLite, changing provider API contracts, or redesigning public
  OpenAI/Anthropic payload schemas beyond the required safe error behavior.
- Unrelated dependency upgrades, cleanup, or parity work against the original
  TypeScript project.
- Live calls that spend real Copilot/Codex/provider quota during tests.

## Applicable Project Conventions

**Quality gate command:**
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo build --verbose`
- `cargo test --verbose`
- `cargo deny check`
- `cargo audit` is advisory/non-blocking when available.

**Commit convention:**
- Conventional-style title: `type(scope): [B/I] description`, at most 72
  characters, explaining why.
- Builder trailer: `Assisted-by: Claude:Sonnet-4.6`
- Inspector trailer: `Assisted-by: Claude:Haiku-4.5`
- Repository-required trailer:
  `Co-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>`

**Guidelines:**
- `AGENTS.md`
- `CONTRIBUTING.md`
- `.github/workflows/ci.yml`
- `deny.toml`

**Rules:**
- Prefer correctness, client experience, performance, and clarity over parity
  with the TypeScript reference.
- Preserve unknown JSON keys with Serde flattening/order conventions.
- Use `AppError` for client/internal failures and `HttpError` for real upstream
  responses; never fail silently.
- Follow the existing three-layer architecture (`libs`, `routes`, `services`).
- Run all repository CI gates before completion.
