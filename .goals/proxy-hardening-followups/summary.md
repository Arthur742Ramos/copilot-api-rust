# Proxy hardening follow-ups: completed

## Outcome

The goal passed independent inspection on the first iteration. The proxy now
has bounded live admission, cancellation-safe waiting, cost-safe retry defaults,
complete upstream buffering caps, non-blocking token-budget reads, explicit
stream terminal outcomes, provider-route instrumentation, fail-closed remote
binding, redacted internal errors, and provider-only startup/readiness.

## Acceptance criteria

- **Admission and overload:** Global and per-key limits bound billable work,
  permits survive through streaming-body completion/drop, queues have waiter and
  time bounds, and overload exposes structured retry guidance and metrics.
- **Retry safety:** Billable generation defaults no longer replay ambiguous
  transient 5xx responses; operators may explicitly opt in where appropriate.
- **Memory safety:** Buffered upstream responses, including Codex and updater
  downloads, have explicit size ceilings.
- **Async safety:** Token-budget SQLite reads execute off Tokio workers while
  retaining global/per-key cache and metric behavior.
- **Stream lifecycle:** Clean terminal completion, upstream errors, and client
  cancellation are distinguished and finalized once; provider traffic uses the
  same instrumentation.
- **Exposure and errors:** Remote unauthenticated binding fails closed unless
  explicitly overridden, and unexpected failures return a generic traceable 500
  instead of internal details.
- **Provider-only operation:** An explicit provider-only mode skips Copilot
  bootstrap and reports readiness for the selected Codex or third-party
  provider.
- **Documentation:** README and CLI/config documentation cover the new controls
  and correctly describe `/token` as presence/expiry metadata only.
- **Quality:** Formatting, strict Clippy, build, 336 tests, and `cargo deny check`
  all passed.

## Iteration history

| Iteration | Builder | Inspector | Verdict |
| --- | --- | --- | --- |
| 1 | `c7838f5` | `5874c7e` | PASS |

## Inspector findings

The Inspector found no unmet criteria or blocking defects. No repair iteration
was required.

## Recommendations

- Exercise the new admission limits with a sustained streaming load test before
  choosing tighter production values.
- Keep ambiguous 5xx retries disabled unless a provider explicitly guarantees
  idempotency.
- Run the existing CI matrix on Linux, macOS, and Windows after opening the pull
  request; local verification covered the repository gates on macOS.
- Terminate TLS and apply network policy at a reverse proxy for any deployment
  exposed beyond a trusted host.
