# Inspector Feedback — Iteration 4

## Verdict: FAIL

## Inspection basis

- Read the immutable goal in `.goals/claude-code-api-compat/goal.md` and
  `status.json` in full.
- Inspected the accumulated change set from `status.json.initial_sha`
  (`2c8a37436462c808830c62c11ea367cd9f53817b`) through Builder commit
  `6311aee8e90f280ed00292b46acec57cac5e2115`.
- Re-read Inspector feedback for iterations 1–3 and reviewed the current
  message, provider, error, stream-translation, and compatibility-matrix
  implementations.
- No credentials were used and no command accessed or restarted port `4141`.
  The checks below are credential-free; no live upstream request was needed.

## Acceptance Criteria Check

- [ ] **Criterion 1 — FAILED.** The checked-in matrix is broad and has useful
  code/test references. It now accurately documents the fixed top-level
  translated Chat error path and the legitimate empty-choice usage path, but
  it still overstates the translated streaming contract. Row 63 only covers
  malformed SSE JSON, while row 64 claims the translated upstream-error
  surface is fixed; neither records that a valid JSON chunk with a missing or
  non-array `choices` field is still accepted as an empty-choice completion.
  That is a reachable malformed-chunk behavior covered by the goal, so the
  matrix is not accurate as a complete audit until the behavior is fixed and
  regression-tested (or explicitly documented as an intentional limitation).

- [x] **Criterion 2 — VERIFIED.** The accumulated implementation and tests
  cover the common Claude Code request shapes: streaming and non-streaming
  dispatch, string and structured content, system content, tool definitions
  and tool choice, tool-use/tool-result turns, metadata, model/provider
  aliases, and flattened unknown JSON fields. The provider/public count-token
  envelope, malformed-body, validation, and body-limit fixes also remain
  covered.

- [ ] **Criterion 3 — FAILED.** The new top-level `error` handling is ordered
  correctly before the legitimate `choices: []` path. It safely maps the
  upstream type/message, closes thinking/content blocks before the error,
  discards pending success, emits one terminal error, and the public and
  provider translated drivers stop after that error. The existing tests cover
  those cases, including later chunks and EOF.

  However, `stream_translation.rs:155-162` still treats every chunk whose
  `choices` is absent, non-array, or empty as the same case:

  ```rust
  let choices = chunk.get("choices").and_then(|v| v.as_array());
  match choices {
      Some(arr) if !arr.is_empty() => ...
      _ => complete_pending_message(state, &mut events, Some(chunk)),
  }
  ```

  Thus a structurally malformed but valid JSON event such as
  `{"usage":{...}}`, `{"choices":{}}`, `null`, or `[]` can silently take the
  success/empty-choice path. With a deferred `pending_message_delta`, it
  emits `message_delta` and `message_stop` and marks success. Without one, it
  emits no event and leaves the stream non-terminal, allowing a later normal
  chunk to continue into a success-shaped response. This violates the
  requirement that malformed chunks never silently end or fabricate success.
  The explicit `choices: []` plus `error: null` usage-only completion must
  remain supported, but missing/non-array `choices` must take a terminal
  malformed-stream error path.

- [ ] **Criterion 4 — FAILED.** The shared non-streaming error renderer and
  provider count-token paths now produce complete SDK-recognizable Anthropic
  envelopes with status preservation, header allowlisting, retry/overload
  classification, and internal-detail sanitization. The translated top-level
  Chat error mapping is also bounded and safe. Nevertheless, the malformed
  translated chunks described above can produce a successful terminal
  `message_stop` or no failure event at all, so Claude Code cannot reliably
  receive an SDK-recognizable error for every in-scope streaming failure.

- [x] **Criterion 5 — VERIFIED.** Model discovery and normalization, aliases and
  `[1m]` handling, Claude Code initiator/editor and beta headers, provider
  resolution, and clear client-side rejection paths were reviewed with their
  associated tests. No additional blocking model/header gap was found.

- [ ] **Criterion 6 — FAILED.** The deterministic suite contains strong
  regressions for top-level upstream errors in pending and non-pending states,
  safe mappings, open-block close ordering, terminal uniqueness, later-chunk
  suppression, EOF behavior, and the legitimate empty-choice usage case.
  It also covers malformed JSON, transport failures, truncation, public route
  smoke behavior, and provider routing. It has no regression for a valid JSON
  chunk with missing/non-array `choices`, and no flow-level assertion that the
  public and provider translated drivers reject that structural malformed
  shape. The compatibility matrix consequently claims more malformed-stream
  coverage than the implementation and tests provide.

- [x] **Criterion 7 — VERIFIED.** All required quality gates passed in this
  inspection; the failure is a behavioral coverage/handling gap rather than a
  formatting, compilation, lint, dependency, or existing-test failure.

## Quality Gate

- `cargo fmt --all -- --check` — **PASS**.
- `cargo clippy --all-targets -- -D warnings` — **PASS**.
- `cargo build --verbose` — **PASS**.
- `cargo test --verbose` — **PASS**; all unit and integration suites passed.
- `cargo deny check` — **PASS**; only the repository's existing unmatched
  license allowances and duplicate dependency warnings were reported.
- Targeted streaming, Responses, router-smoke, and provider-routing tests —
  **PASS**. The passing suite does not include the missing/non-array
  `choices` regression described above.

## Issues Found

### 1. Missing or non-array `choices` is still a fabricated success path

The top-level error fix is correct for a chunk containing `error`, but the
translator still conflates an explicitly empty `choices` array with an absent
or wrongly typed field. In the pending state this is especially damaging:
after a finish chunk has queued `pending_message_delta`, a malformed
`{"usage":{...}}` chunk calls `complete_pending_message` and produces a clean
Anthropic completion even though the upstream event was not a valid Chat
Completions chunk. In the non-pending state the same event produces no output
and does not set `terminal_event_emitted`, so a later chunk can turn the
malformed stream into a success.

This is distinct from the valid usage-only fixture at
`stream_translation.rs:1741-1777`, which correctly uses
`"choices": []` and `"error": null`. The fix must preserve that case while
rejecting missing/non-array `choices`.

### 2. Matrix and regression evidence overstate malformed-stream support

The matrix's “Malformed translated SSE JSON” and “Top-level translated Chat
Completions upstream error objects” rows accurately describe their individual
tests, but together imply that the translated stream cannot silently accept a
malformed event. The missing structural-shape case is not documented or
tested, and the public/provider driver claims are not exercised for that
case. The matrix and tests should be updated after the implementation makes
the distinction.

## What Must Be Fixed

1. In `translate_chunk_to_anthropic_events`, distinguish an explicit
   `choices: []` usage-only record (with `error` absent or `null`) from a
   missing, null, non-array, or otherwise malformed `choices` field.
2. Route the latter through the same terminal malformed-stream path used for
   malformed JSON: close open thinking/content blocks first, clear deferred
   success/tool state, emit exactly one safe Anthropic `error`, mark the stream
   terminal, and suppress later chunks and EOF flushing.
3. Add deterministic tests for missing and non-array `choices` in both pending
   and non-pending/open-block states, including later success/error/usage
   chunks and EOF. Exercise the public and provider translated drivers or
   otherwise assert their shared terminal handling directly.
4. Update the compatibility matrix and references so malformed translated
   chunk coverage is precise.
5. Re-run every quality gate after the fix.
