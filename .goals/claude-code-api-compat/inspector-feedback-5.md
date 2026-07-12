# Inspector Feedback — Iteration 5

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, the current `status.json`, and the prior Inspector
  feedback, including `inspector-feedback-4.md`.
- Reviewed the complete accumulated change set from the recorded initial SHA
  `2c8a37436462c808830c62c11ea367cd9f53817b` through Builder commit
  `b871cf7c9316015245d270355b92806989f767d1`.
- Re-read the stream state machine, both translated streaming drivers, the
  Anthropic error renderer, provider count-token route, regression tests, and
  the compatibility matrix.
- No credentials were used. No live upstream request was required, and port
  `4141` was not accessed, restarted, or hot-swapped.

## Acceptance Criteria Check

- [ ] **Criterion 1 — FAILED.** The checked-in matrix is broad and has code and
  test references, but row 64 overstates the translated-stream guarantee. The
  implementation rejects malformed containers, while the row also claims that
  malformed delta, tool-call, and usage structures are terminal. Present scalar
  values of the wrong type inside those structures are still accepted and
  silently ignored or converted to zero. The matrix is therefore not an
  accurate audit of the supported behavior.

- [x] **Criterion 2 — VERIFIED.** The accumulated implementation covers the
  audited common request forms: streaming and non-streaming dispatch, string
  and structured content, system content, tools and tool choice, tool-use and
  tool-result turns, metadata/unknown fields, model aliases, and provider
  routing. The count-token validation and error-envelope regressions also
  remain covered.

- [ ] **Criterion 3 — FAILED.** The iteration-5 distinction between an explicit
  `choices: []` usage record and missing/null/non-array `choices` is correct.
  The pending-success, open-thinking/open-tool, later-chunk, EOF, terminal
  uniqueness, and public/provider driver paths for those structural shapes are
  also correctly handled. However, malformed scalar fields inside an otherwise
  structurally valid chunk still bypass the terminal malformed-stream path:

  - `{"choices":[{"delta":{"content":42},"finish_reason":null}]}`
    passes `validate_chat_chunk`; `DeltaView::from_delta` drops the value
    because `opt_string` only returns strings. The stream can continue with
    missing content, or a finish chunk can produce a success-shaped completion
    for data that was not valid.
  - A tool entry such as
    `{"index":"bad","id":123,"function":{"name":99,"arguments":{}}}` passes
    the object-only tool-call validation. `handle_tool_calls` defaults the
    index to `0` and ignores the invalid id, name, and arguments. With an
    existing tool block this can mutate/finish the wrong logical call without
    an error; with no existing call it can disappear and allow a later success.
  - A usage object such as
    `{"prompt_tokens":"bad","completion_tokens":[]}` is accepted because only
    the outer `usage` object shape is checked. `usage_num` then returns `0`
    (and also truncates fractional numbers), so a pending finish can be
    completed with fabricated zero counts instead of a terminal error.

  These cases violate the requirement that malformed chunks never silently
  complete or leave the client without a terminal error. They also affect the
  requested pending/non-pending and open-block cleanup guarantees because no
  malformed error is emitted and `close_stream_for_error` is not called.

- [ ] **Criterion 4 — FAILED.** The shared error renderer and the top-level
  upstream-error path produce SDK-recognizable Anthropic errors with the
  expected status/header behavior. Nevertheless, the malformed scalar cases
  above can produce a normal `message_delta`/`message_stop` or continue into a
  later success instead of producing an Anthropic `error` event. Thus the
  streaming failure contract is still incomplete.

- [x] **Criterion 5 — VERIFIED.** Model discovery/normalization, aliases,
  `[1m]` handling, Claude Code initiator/editor headers, beta allowlisting,
  provider resolution, and clear client-error paths were reviewed with their
  associated tests. No additional blocking model or header gap was found.

- [ ] **Criterion 6 — FAILED.** The suite has strong coverage for structural
  malformed choices, top-level upstream errors, thinking/content/tool cleanup,
  pending success suppression, later chunks, EOF, terminal uniqueness, public
  and provider drivers, and valid usage-only completion. It does not test
  wrong-typed consumed delta fields, wrong-typed tool-call fields, or
  malformed token-count fields nested inside an object-shaped usage record.
  Consequently, the matrix and regression evidence do not cover every
  malformed-chunk behavior they claim to support.

- [x] **Criterion 7 — VERIFIED.** All required repository quality gates passed.
  The remaining failure is behavioral rather than a build, lint, formatting,
  dependency, or existing-test failure.

## Quality Gate

- `cargo fmt --all -- --check` — **PASS**.
- `cargo clippy --all-targets -- -D warnings` — **PASS**.
- `cargo build --verbose` — **PASS**.
- `cargo test --verbose` — **PASS**; all unit and integration tests passed.
- `cargo deny check` — **PASS**; it reported the repository's existing
  duplicate `base64` entries and license-allowance warnings but exited
  successfully.
- Additional credential-free targeted checks — **PASS**:
  stream-translation tests (47 passed), provider-routing tests (10 passed),
  router-smoke tests (9 passed), and the translated public/provider driver
  regressions in the full suite.

## Issues Found

### 1. Wrong-typed delta scalar values are silently discarded

`src/routes/messages/stream_translation.rs:46-57` constructs `DeltaView`
through `opt_string`, and `:73-77` maps every present non-string content or
reasoning value to `None`. The validator at `:143-197` checks that `delta` is
an object but does not validate the types of fields consumed by the state
machine. A numeric/array/object `content` or reasoning field can therefore
produce no output rather than the required terminal malformed-stream error.

### 2. Wrong-typed tool-call fields are silently normalized or ignored

The validator at `:182-188` checks only that `tool_calls` is an array of
objects. At `:559-635`, `index` defaults to zero and invalid `id`, function
`name`, and `arguments` values are ignored. This is especially unsafe while a
tool block is open: malformed input can be associated with an existing index
or disappear, and a later finish can still be rendered as a successful
Anthropic response instead of closing state through the error cleanup path.

### 3. Object-shaped usage with malformed token fields fabricates accounting

`validate_chat_chunk` accepts any object-valued `usage` at `:158-165` and
accepts any object-valued usage-only record at `:147-155`. The accounting
helper at `:932-943` converts unsupported values to zero and accepts/truncates
floating-point values. A malformed usage record can consequently complete a
deferred success with incorrect counts, rather than discarding pending state
and emitting one terminal error.

### 4. Matrix and regression claims are broader than the verified behavior

Row 64 of `docs/claude-code-api-compatibility.md` explicitly claims terminal
handling for malformed delta/tool-call/usage structures, but the tests only
exercise their outer/container shapes. The implementation must either reject
the malformed scalar cases and add deterministic pending/open-block,
later-chunk, EOF, and driver coverage, or explicitly narrow the matrix claim.
Given the goal's requirement for reliable malformed-stream handling, the
correct resolution is to validate the consumed nested field types and use the
existing terminal cleanup path.

## What Must Be Fixed

1. Extend translated Chat chunk validation to reject present consumed delta
   fields whose types are not the accepted string/null forms, without
   disallowing legitimate fragmented optional tool-call fields.
2. Validate present tool-call `index`, `id`, function object/name, and
   arguments field types; do not default malformed values or silently drop
   them. Preserve legitimate omission of fields on later argument fragments.
3. Validate present usage token fields and nested token-detail containers
   before accounting; reject wrong-typed and fractional token values rather
   than converting them to zero/truncating them.
4. Route all of those failures through `malformed_stream_error_events`, so
   thinking/content/tool blocks close in order, deferred content and pending
   success are cleared, tool state is cleared, exactly one error is emitted,
   later chunks are suppressed, and EOF cannot flush a success.
5. Add credential-free regression tests for each malformed nested shape in
   pending, non-pending, and open-tool/open-thinking states, including later
   success/error/usage chunks, EOF, and both translated drivers. Update row 64
   and its references to match the resulting coverage.
6. Re-run every quality gate after the behavioral fix.
