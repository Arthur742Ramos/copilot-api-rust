# Inspector Feedback — Iteration 3

## Verdict: FAIL

## Inspection basis

- Goal: `.goals/claude-code-api-compat/goal.md` (read in full; immutable).
- Iteration: 3.
- Builder commit inspected: `ded1cfc78bbe184e2b4af9d7c72de8ccd5c1750c`.
- Baseline inspected: `status.json.initial_sha` =
  `2c8a37436462c808830c62c11ea367cd9f53817b`.
- Reviewed the complete accumulated change set from the baseline, the latest
  Builder diff, the compatibility matrix, the prior Inspector feedback, and
  the relevant request, error, provider, and stream translation paths.
- No credentials were used and port `4141` was not accessed. The isolated,
  credential-free runtime checks used port `4142` only.

## Acceptance Criteria Check

- [ ] **Criterion 1 — FAILED.** The checked-in matrix is broad and has useful
  code/test references, but it does not accurately account for top-level
  Chat Completions error payloads delivered as translated SSE chunks. Rows
  55, 62, 63, and 76 collectively claim supported/fixed lifecycle,
  translated-terminal, malformed/error, and Anthropic-error behavior without
  identifying this still-broken upstream-error form. The matrix therefore
  overstates the audited streaming contract. It needs an explicit row for
  top-level translated upstream error events (including pending and
  non-pending state), with the actual status and regression references.

- [x] **Criterion 2 — VERIFIED.** The accumulated implementation and tests
  cover the requested common message shapes: streaming and non-streaming
  dispatch, string and structured content, system content, tools and tool
  choice, tool-use/tool-result turns, metadata, model/provider aliases, and
  flattened safe unknown fields. The public and provider count-token
  validation/body-limit fixes from iteration 3 were also reviewed; no
  additional request/validation gap was found in this inspection.

- [ ] **Criterion 3 — FAILED.** The translated Chat Completions state machine
  still mishandles an upstream SSE event such as:

  ```json
  {"error":{"type":"server_error","message":"upstream boom"}}
  ```

  In `src/routes/messages/stream_translation.rs:92-99`, any chunk without a
  non-empty `choices` array is treated as the legitimate empty-choice/usage
  case and calls `complete_pending_message`. Consequently:

  1. If `pending_message_delta` exists (the normal deferred-usage state after a
     finish chunk), the error produces a successful `message_delta` followed
     by `message_stop`, sets the terminal-success state, and never emits an
     Anthropic `error` event.
  2. If no pending delta exists, the error produces no event at all, leaves the
     stream non-terminal, and loses both the upstream error type and diagnostic.
     A later `[DONE]`/EOF only produces the generic “ended before a finish
     reason” error, or a later normal chunk can continue into a success-shaped
     completion.

  This affects both public translated Chat Completions flow
  (`src/routes/messages/api_flows.rs:280-305`) and the provider translated
  flow (`src/routes/provider/messages.rs:1147-1175`). The flow drivers parse
  the JSON and immediately pass it to the same translator; neither detects a
  top-level upstream error first. Thus the required terminal error ordering,
  diagnostic preservation, and “must not silently end” behavior are not
  reliable for a real upstream error event.

- [ ] **Criterion 4 — FAILED.** The non-streaming/shared error-envelope and
  count-token fixes now produce SDK-recognizable Anthropic errors, and the
  malformed/transport/truncation paths tested by the Builder are sound.
  However, the streaming failure path above can either return a successful
  `message_stop` or no event, rather than the required complete Anthropic
  `error` event with a safe diagnostic. A Claude Code client cannot reliably
  recognize or retry that upstream failure.

- [x] **Criterion 5 — VERIFIED.** Model discovery and normalization, provider
  and model aliases, `[1m]` handling, Claude Code headers/beta filtering, and
  clear client-side rejection paths were reviewed with their associated
  tests. No new blocking model/header gap was found.

- [ ] **Criterion 6 — FAILED.** The deterministic suite covers malformed JSON,
  transport failures, truncated streams, and the legitimate empty-choice
  usage-completion case (`empty_choices_completes_pending_with_usage`), but
  it has no regression for a valid JSON top-level `error` object in a
  translated Chat Completions chunk. In particular, there are no tests for
  the pending-message and no-pending-message states, no assertion that an
  open block is closed before the terminal error, and no assertion that later
  chunks cannot produce a success terminal event. The matrix's streaming
  support claims are therefore not backed by coverage of this critical
  Claude Code failure path.

- [x] **Criterion 7 — VERIFIED.** Existing audited behavior and the required
  quality gates completed successfully; details are below. The remaining
  defect is behavioral rather than a compile, lint, formatting, dependency,
  or test-suite failure.

## Quality Gate

- `cargo fmt --all -- --check` — **PASS** (exit 0).
- `cargo clippy --all-targets -- -D warnings` — **PASS** (exit 0).
- `cargo build --verbose` — **PASS** (exit 0).
- `cargo test --verbose` — **PASS** (all tests passed; no failures).
- `cargo deny check` — **PASS** (exit 0; only the repository's existing
  non-blocking unmatched-license warnings were reported).

## Runtime and code-path evidence

- Credential-free checks on isolated port `4142` confirmed the iteration 3
  count-token fixes for valid requests, unknown providers, malformed JSON,
  invalid payloads, oversized bodies, and public provider/model-alias routing.
- The same translated stream translator is reached by both the public
  Chat Completions flow and provider OpenAI-compatible flow. There is no
  alternate upstream-error handling between JSON parsing and
  `translate_chunk_to_anthropic_events`.
- The existing `empty_choices_completes_pending_with_usage` test demonstrates
  why a broad “no choices means error” change would be insufficient: legitimate
  usage chunks must remain successful, while an object containing a top-level
  `error` must take the terminal-error path first.

## Issues Found

### 1. Top-level translated upstream errors are mistaken for empty-choice chunks

The branch at `stream_translation.rs:92-99` only asks whether `choices` is
present and non-empty. It does not inspect `chunk.error` before invoking the
empty-choice completion path. This is a reachable compatibility defect, not
just an undocumented edge case: OpenAI-compatible Chat Completions streams
commonly encode provider failures as a JSON error object in an SSE `data`
record.

The pending case is especially damaging because it fabricates a clean
Anthropic completion after an upstream failure. The non-pending case is
silent and can subsequently be turned into a generic EOF error or a normal
completion, so it neither preserves the upstream diagnostic nor terminates
deterministically.

### 2. The matrix and regression evidence overstate streaming-error support

The matrix correctly records many fixed lifecycle and failure paths, but it
does not distinguish a top-level translated upstream error from malformed
JSON, transport failure, truncation, or a legitimate usage-only chunk. As
written, a reader would infer that the audited streaming/error surface is
complete, while this valid error payload is not handled. That makes the
compatibility documentation inaccurate until the implementation and tests are
fixed (or the behavior is explicitly recorded as an intentional limitation,
which would not satisfy the goal's hardening request).

## What Must Be Fixed

1. Detect a top-level upstream error object before empty-choice handling in
   `stream_translation.rs`, for both public and provider translated flows.
   Convert it to exactly one SDK-recognizable Anthropic terminal `error` event,
   retaining only a safe upstream type/message (with a safe fallback when the
   payload is malformed or unsafe).
2. Close any active thinking/content block safely, discard deferred successful
   completion state, mark the stream terminal, and prevent subsequent chunks
   or EOF flushing from emitting `message_delta`/`message_stop` or a second
   terminal event. Preserve the existing legitimate empty-choice usage path.
3. Add deterministic, credential-free tests for at least:
   - a top-level upstream error while `pending_message_delta` is present;
   - the same error with no pending delta and an open block;
   - terminal-event uniqueness and rejection of later success chunks;
   - safe error type/message mapping and block-close ordering;
   - no regression to empty-choice usage completion.
4. Update the compatibility matrix with an explicit upstream translated
   streaming-error row and precise code/test references.
5. Re-run every quality gate above after the fix.

