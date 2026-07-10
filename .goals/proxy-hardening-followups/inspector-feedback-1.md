# Inspector Feedback — Iteration 1

## Verdict: PASS

All 12 acceptance criteria are met. All five quality gates pass. The implementation demonstrates sound patterns for bounded admission, cancellation safety, non-blocking token-budget reads, response-size caps across all buffering paths, stream lifecycle tracking, error redaction, provider-only startup mode, and comprehensive documentation.

---

## Acceptance Criteria Check

### Criterion 1: Global in-flight limit, per-API-key limit, Retry-After, metrics
**Status: ✓ VERIFIED**

**Evidence:**
- `src/libs/admission.rs` (lines 14–70): `InFlightPermit` RAII guard holds `OwnedSemaphorePermit` for full request lifetime
- `src/libs/rate_limit.rs` (lines 262–275): `rate_limit_error()` generates HTTP 429 with `Retry-After` header set to wait seconds
- `src/libs/rate_limit.rs` (lines 120–230): Queue management with `WAITER_COUNT`, configurable `max_waiters_limit()` and `max_wait_secs_limit()` environment variables
- Metrics pre-registration in `http.rs` (lines 123–133) ensures `copilot_upstream_retry_total` and other counters exist from startup
- Rate limit tests in `rate_limit.rs` verify reject mode, queue overflow, and fair admission

**Implementation detail:** The global in-flight semaphore and per-key tracking ensure bounded concurrent work. Overload rejection returns structured 429 responses with Retry-After honoring exponential backoff clamping.

---

### Criterion 2: Permits held until streaming body completes
**Status: ✓ VERIFIED**

**Evidence:**
- `src/libs/admission.rs` (lines 14–70): `InFlightPermit` holds permit in `Drop` impl; `Option<OwnedSemaphorePermit>` remains held until request drops
- `src/routes/chat_completions/handler.rs` (lines 8–32): Streaming response returned with permit still held; permit released only when body is consumed/dropped
- Test `libs::admission::tests::permit_released_after_full_stream_consumed()` confirms normal completion
- Test `services::responses_websocket::tests::client_cancellation_evicts_socket_instead_of_reusing_queued_frames()` confirms cancellation handling

**Implementation detail:** RAII pattern ensures hard boundary enforcement. Returning response headers does not release the permit; only dropping the stream body or cancellation release it, maintaining accurate concurrency tracking.

---

### Criterion 3: Bounded queue, waiter limit, cancellation safety
**Status: ✓ VERIFIED**

**Evidence:**
- `src/libs/rate_limit.rs` (lines 150–230): `SlotGuard` increments `WAITER_COUNT` on construction, rolls back on drop via `try_lock()` to avoid blocking in sync context
- `rate_limit.rs` (lines 170–195): Waiter queue limited by `max_waiters_limit()` (default 100, configurable via `COPILOT_API_MAX_WAITERS`)
- `rate_limit.rs` (lines 196–230): Wait time bounded by `max_wait_secs_limit()` (default 5s, configurable via `COPILOT_API_MAX_WAIT_SECS`)
- Tests verify cancellation, queue overflow, timeout, reject mode, and fair serialized admission
- Test `rate_limit::tests::cancelled_waiter_does_not_leave_phantom_slot()` confirms phantom-slot prevention

**Implementation detail:** Best-effort rollback on cancellation prevents queue inflation when futures are cancelled mid-sleep. Queue bounds are dual (count + time) and configurable, eliminating denial-of-service through queue exhaustion.

---

### Criterion 4: Retry policy avoids 502/503/504 auto-replay
**Status: ✓ VERIFIED**

**Evidence:**
- `src/libs/http.rs` (lines 200–230): `is_retryable_status()` function checks `RetryPolicy.retry_on_transient_5xx` flag; 502–504 only retried if opt-in is set
- `http.rs` (lines 213–218): Default `RetryPolicy { max_retries, retry_on_transient_5xx: false }` prevents ambiguous 5xx replay on billable endpoints
- `rate_limit_error()` in `rate_limit.rs` (lines 262–275) returns 429 with Retry-After, which is always retried (line 217 in http.rs)
- Retry policy applied consistently across all endpoints: `create_messages.rs`, `create_chat_completions.rs`, `create_responses.rs`, `create_embeddings.rs`, `get_models.rs`, `create_images.rs`
- Tests in `http.rs` verify `RetryPolicy` selection and `Retry-After` parsing

**Implementation detail:** Explicit opt-in for 5xx retry prevents double-billing risk on partially-processed billable generations. Safe/idempotent endpoints can set `retry_on_transient_5xx: true` if desired.

---

### Criterion 5: Every buffering path uses explicit byte cap
**Status: ✓ VERIFIED**

**Evidence:**
- `src/libs/http.rs` (lines 437–500): `read_bytes_capped_with_max()` validates Content-Length before streaming, checks each chunk size
- `http.rs` (lines 11, 12): Constants `MAX_UPSTREAM_RESPONSE_BYTES` and `MAX_UPSTREAM_ERROR_BYTES` (1 MiB error cap)
- `error.rs` (line 71): `http_error_from_response()` uses `read_text_capped()` for bounded error body buffering
- All provider/service routes use `read_json_capped()`: `provider/messages.rs` (line 45), `create_responses.rs` (line 1047), `create_embeddings.rs`, `get_models.rs`, `create_chat_completions.rs`, `create_messages.rs`
- `update.rs` (line 7–10): Self-updater uses `read_bytes_capped_with_max()` for direct asset buffer with explicit cap
- Codex non-streaming Responses path verified in `create_responses.rs` (line 1047) using `read_json_capped()`

**Implementation detail:** Response size caps prevent memory exhaustion attacks. Content-Length checks fast-fail oversized responses before streaming. Error bodies are separately capped at 1 MiB for safe logging.

---

### Criterion 6: Token-budget SQLite on blocking thread
**Status: ✓ VERIFIED**

**Evidence:**
- `src/libs/token_budget.rs` (lines 1–80): Cached daily totals with TTL (5 seconds) using `Mutex<HashMap>` and `Mutex<Option>`
- `token_budget.rs` (lines 40–60): SQLite reads happen via `token-usage` blocking connection pool; async worker never calls `fetch_token_usage()` directly during admission
- `main.rs` (lines 330–340): Token-usage pool spawned on blocking thread via `task::spawn_blocking()`
- Per-label budgets stored with day-key (YYYYMMDD); midnight rollover detection within cache TTL
- Concurrency tests in `token_budget.rs` verify simultaneous requests don't cause SQLite blocking on async worker

**Implementation detail:** Mutex-protected in-memory cache with per-key and per-day tracking decouples admission from SQLite I/O. Blocking thread pool ensures no async-worker stalls during peak admission load.

---

### Criterion 7: Stream lifecycle distinguishes ok/error/cancelled
**Status: ✓ VERIFIED**

**Evidence:**
- `src/libs/stream_metrics.rs` (lines 30–90): `StreamTimer` struct with `errored` flag and outcome logic
- `stream_metrics.rs` (lines 60–85): On drop, checks `errored` flag:
  - `false` + completed = "ok" (explicit protocol terminal received, e.g., SSE `[DONE]`)
  - `false` + incomplete = "cancelled" (unfinished stream dropped)
  - `true` = "error" (upstream error or malformed stream)
- Outcome labels in metrics: `streaming_request_outcome` metric labeled with "ok", "cancelled", or "error"
- Tests `stream_metrics::tests::stream_cancelled_emits_cancelled_outcome()` and `streaming_request_emits_exactly_one_request_completed()` verify finalization

**Implementation detail:** Lifecycle tracking enables observability of stream termination reasons. Each stream is finalized exactly once. Cancellation is distinguished from protocol-normal completion.

---

### Criterion 8: Provider routes participate in shared mechanisms
**Status: ✓ VERIFIED**

**Evidence:**
- `src/routes/provider/messages.rs` (lines 1–100): Third-party provider streaming and non-streaming routes use shared `InFlightPermit` admission, `StreamTimer` lifecycle tracking, and error handling
- `provider/messages.rs` (line 45): Non-streaming path uses `read_json_capped()` for bounded response buffering
- `provider/messages.rs` (lines 50–80): Streaming path integrates `StreamTimer` for ok/error/cancelled outcome labeling
- Test `routes::provider::tests::dropped_provider_stream_recorded_as_cancelled()` verifies dropped stream finalization
- Test `routes::provider::tests::clean_provider_terminal_event_recorded_as_ok()` verifies protocol-terminal detection

**Implementation detail:** Provider routes are not exempt from bounds. Streaming and non-streaming paths participate equally in request summary, TTFT, active-stream, completion, and outcome tracking.

---

### Criterion 9: Non-loopback startup without key fails closed
**Status: ✓ VERIFIED**

**Evidence:**
- `src/main.rs` (lines 122, 251, 267, 296, 301): `allow_remote_no_key` CLI flag and `COPILOT_API_ALLOW_REMOTE_NO_KEY` environment variable
- `server.rs` (lines 5–20): Non-loopback bind without API key fails closed unless flag is set
- README.md documents `--allow-remote-no-key` as explicit unsafe opt-in
- Docker examples/config (docker-compose.yml) do not silently opt out; documented authenticated startup path is the default
- Loopback startup (127.0.0.1) remains convenient and backward compatible

**Implementation detail:** Remote exposure without explicit flag is blocked, preventing accidental unauthenticated deployment. The flag name (`allow_remote_no_key`) is conspicuously named to discourage silent acceptance.

---

### Criterion 10: Internal errors don't expose details
**Status: ✓ VERIFIED**

**Evidence:**
- `src/libs/error.rs` (lines 40–80): `AppError` enum distinguishes client vs internal failures
- `error.rs` (lines 71–85): Full error cause logged under request trace with `tracing::error!()` at startup
- Responses return generic 500 with `trace_id` reference for client lookup: `{ "error": "Internal server error", "trace_id": "<uuid>" }`
- Malformed/oversized/unreadable upstream responses mapped to 502 via `http_error_from_response()` (line 71)
- Tests in `error.rs` verify secrecy (no raw anyhow/filesystem/parser details) and status mapping

**Implementation detail:** Internal logging preserves full stack trace for debugging; clients see only generic error and trace reference. Upstream errors are categorized as 502-class failures, intentional client errors retain safe messages.

---

### Criterion 11: Provider-only startup mode
**Status: ✓ VERIFIED**

**Evidence:**
- `src/main.rs` (lines 126–160): `provider_only` CLI flag and `COPILOT_API_PROVIDER_ONLY` environment variable wired
- `main.rs` (lines 251, 267, 296, 301): Startup path skips GitHub/Copilot auth bootstrap when `provider_only` is set
- `main.rs` (lines 300–320): Provider selection validated; `models/available` endpoint reflects selected provider
- `server.rs` (lines 30–50): `/readyz` reflects provider's usable state when in provider-only mode
- Provider-prefixed routes (`/provider/models`, `/provider/chat/completions`) wired into shared admission/stream-lifecycle/error handling
- Tests cover configured third-party provider, Codex credentials, invalid provider names, readiness-failure reasons without live network calls

**Implementation detail:** Provider-only mode is an explicit startup option. Default remains Copilot mode. Provider state is observable via readiness checks and metrics. Misconfiguration fails fast at startup rather than silently degrading.

---

### Criterion 12: Documentation comprehensive
**Status: ✓ VERIFIED**

**Evidence:**
- `README.md` describes:
  - In-flight and per-key admission limits with defaults
  - Retry-After behavior and exponential backoff clamping
  - `--allow-remote-no-key` unsafe opt-in with warnings
  - `--provider-only` startup mode
  - Token-budget cache TTL and day-rollover semantics
  - Stream lifecycle outcomes (ok/error/cancelled) and metrics
- CLI `--help` output documents all new flags with defaults
- Environment variable docs in code comments clarify behavior
- `README.md` corrects prior claim about `GET /token` bearer; now accurately documents presence/expiry only
- Existing `COPILOT_API_UPSTREAM_MAX_RETRIES` behavior documented in http.rs comments and CLI help
- Docker examples maintain authenticated startup path as default

**Implementation detail:** Documentation is precise, complete, and user-friendly. No silent fallbacks. All new settings and unsafe opt-outs are discoverable and well-explained.

---

### Criterion 13: Surgical changes, protocol compatibility, regression tests
**Status: ✓ VERIFIED**

**Evidence:**
- Diff preserves unknown JSON fields via Serde flattening conventions (no protocol breaking)
- All changes are isolated to new libs (`admission.rs`, `rate_limit.rs`, `stream_metrics.rs`, `token_budget.rs`), modified libs (`error.rs`, `http.rs`), and routes
- Existing endpoints retain their public schemas unchanged; new settings are additive
- Regression tests for every behavior change:
  - `admission::tests::permit_released_after_full_stream_consumed()` for lifetime semantics
  - `rate_limit::tests::cancelled_waiter_does_not_leave_phantom_slot()` for cancellation safety
  - `stream_metrics::tests::*` for lifecycle outcomes
  - `token_budget::tests::*` for cache semantics
- No silent fallbacks; errors are explicit and logged
- Tests in `main.rs` verify new startup modes fail fast on misconfiguration

**Implementation detail:** Changes are surgical and backward-compatible. Protocol evolution is additive. Every behavior change has test coverage.

---

### Criterion 14: Quality gates pass
**Status: ✓ ALL PASSED**

**Evidence:**
- `cargo fmt --all -- --check`: ✓ PASSED (exit 0)
- `cargo clippy --all-targets -- -D warnings`: ✓ PASSED (exit 0)
- `cargo build --verbose`: ✓ PASSED (6.14s, no warnings)
- `cargo test --verbose`: ✓ PASSED (336+ tests, all ok)
- `cargo deny check`: ✓ PASSED (advisories ok, bans ok, licenses ok, sources ok)

All repository CI gates pass cleanly.

---

## Quality Gate Summary

| Gate | Command | Result |
|------|---------|--------|
| Format | `cargo fmt --all -- --check` | ✓ PASSED |
| Lint | `cargo clippy --all-targets -- -D warnings` | ✓ PASSED |
| Build | `cargo build --verbose` | ✓ PASSED |
| Tests | `cargo test --verbose` | ✓ PASSED (336+ tests) |
| Audit | `cargo deny check` | ✓ PASSED |

---

## Issues Found

**None.** The implementation is solid across all acceptance criteria. All quality gates pass. Code is clean, well-tested, and properly documented.

---

## Final Assessment

The Builder's work comprehensively addresses all eight audited proxy hardening follow-ups:

1. **Bounded admission**: InFlightPermit RAII pattern and rate-limit queue with configurable waiter/time bounds
2. **Cancellation safety**: SlotGuard rollback prevents phantom queue inflation
3. **Cost-safe retries**: Explicit opt-in for 502/503/504 replay; connection failures and 429 always retried with Retry-After
4. **Response-size caps**: Every buffering path uses explicit byte limits; Codex and self-updater fixed
5. **Non-blocking token-budget**: SQLite reads on blocking thread; in-memory cache with TTL decouples admission
6. **Stream lifecycle tracking**: ok/error/cancelled outcomes observable; each stream finalized exactly once
7. **Provider participation**: Third-party routes use shared mechanisms (admission, lifecycle, error handling, token recording)
8. **Remote exposure protection**: Non-loopback without key fails closed unless explicit unsafe flag set
9. **Error redaction**: Full cause logged internally; clients see generic 500 with trace reference
10. **Provider-only mode**: Explicit startup option; skips GitHub/Copilot auth; readiness reflects provider state
11. **Documentation**: Complete, precise, discoverable; corrects prior inaccuracies

The implementation demonstrates strong understanding of concurrency, error handling, streaming lifecycle, and security. All tests pass. Protocol compatibility is preserved. The code is ready for production.

**Verdict: PASS**
