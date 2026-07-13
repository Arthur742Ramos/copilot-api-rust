# Inspector Feedback — Iteration 17

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  Builder commit `984f5748df5944c3a0e596e115e28bb25a7dd9df`.
- Re-read the accumulated Claude Code 2.1.207 and Codex CLI 0.144.1
  compatibility evidence and the new open-object extension/collision policy.
- Verified installed versions:
  - `claude --version` → `2.1.207 (Claude Code)`
  - `codex --version` → `codex-cli 0.144.1`
- Inspected recursive schema permitted types/unions, boolean schemas,
  dependent-required/name arrays, bounds, collision checks, tool-choice
  conversion, content/source/message extension handling, and the final
  Messages-to-Responses payload construction.
- Ran the public compatibility suite, all required repository gates, and the
  ignored Codex loopback canary. No repository product code was changed.
- The public suite now rejects invalid schema types/collections, verifies
  catalog-aware tool choices, and captures many tool/content/config extensions.
  The remaining issues below are visible in the final request payload
  construction and are not covered by equivalent public fixtures.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** Documentation now records catalog choice semantics, recursive
  schema bounds/types, extension collision policy, supported source forms, and
  the client matrix. It does not disclose that top-level/message open fields
  and the known `stop_sequences` field are not carried into the Responses
  request payload.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The public
  suite now runs 53 tests (52 passed and one ignored), through the production
  Axum router and deterministic loopback fixtures. It covers direct/provider
  native and compact paths, request validation, schemas, catalog choices,
  sources, extensions, carriers, lifecycle, raw variants, authentication,
  metrics, and the installed Codex canary.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** Catalog
  choice validation, recursive structural schemas, source validation,
  deferred references, and extension collision checks now pass. The final
  Responses request builder still initializes `ResponsesPayload.extra` to an
  empty map and does not copy `AnthropicMessagesPayload.extra`; it also
  ignores `AnthropicInputMessage.extra`. A valid Claude request extension is
  therefore silently lost on the supported Responses provider path.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** Native
  Responses/compaction, carriers, continuation, raw output, routing, and
  schemas remain green. Inbound Messages-to-Responses translation does not
  preserve every open request object: top-level and message-level extras are
  discarded, and `stop_sequences` is accepted/validated but has no Responses
  representation or explicit rejection. This can alter client instructions or
  generation semantics before provider dispatch.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** Invalid
  schema types/containers and catalog choices fail before upstream dispatch.
  However, a valid request with `stop_sequences` and unknown top-level/message
  extensions reaches the provider with those fields absent. The request
  succeeds rather than preserving the open fields or returning a documented
  unsupported-feature error.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models,
  and aliases.** Existing auth, provider-only startup, model/alias resolution,
  tool-search capability routing, direct/provider compact, and canary checks
  remain green.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  new tests cover permitted schema types, recursive containers, bounds,
  boolean schemas, tool choices, collision rejection, and many nested
  extension sentinels. Missing public regressions remain for:

  - top-level `AnthropicMessagesPayload.extra` reaching Responses;
  - `AnthropicInputMessage.extra` on user/assistant messages;
  - a valid `stop_sequences` request through the Responses provider capture;
  - empty `required`/`dependentRequired`/dependency name arrays;
  - interaction of preserved message extensions with split tool-result/message
    items.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs
  state that unknown keys on open objects remain intact and that malformed
  known fields never become omitted/defaulted values. The implementation still
  drops payload/message extras and silently drops `stop_sequences` in the
  Responses path. The docs need either a complete preservation/unsupported
  policy or narrower claims.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** Recursive
  schema traversal, path errors, bounds, collision checks, and source
  validation are substantially improved. Remaining loss:

  1. `translate_anthropic_messages_to_responses_payload` constructs
     `ResponsesPayload { extra: Map::new(), ... }` instead of merging
     top-level payload extensions with canonical collision checks.
  2. `create_message` always sets `extra: Default::default()`, so open
     message-level fields disappear during user/assistant message translation.
  3. `stop_sequences` is validated by `validate_messages_request_shape` but
     not represented in `ResponsesPayload` and is silently ignored for the
     Responses bridge.
  4. `validate_schema_name_array` enforces string/nonempty/unique entries but
     permits empty arrays for `required`, `dependentRequired`, and dependency
     name lists; these known JSON Schema collections have minimum-name
     semantics that are not tested or documented.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS
  - `cargo clippy --all-targets -- -D warnings` — PASS
  - `cargo build --verbose` — PASS
  - `cargo test --verbose` — PASS
  - `cargo deny check` — PASS, with the repository's existing non-fatal
    unmatched-license/duplicate-dependency warnings
  - `cargo test --test client_compatibility --verbose` — PASS, 52 passed and
    1 ignored
  - `cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored
    --nocapture` — PASS; Codex 0.144.1 loopback-only canary

## Issues Found

### Blocking: open top-level and message fields are still dropped

`AnthropicMessagesPayload` and `AnthropicInputMessage` both intentionally
capture unknown keys with flattened `extra` maps. The final
`translate_anthropic_messages_to_responses_payload` creates
`ResponsesPayload.extra` as an empty map, and `create_message` always uses an
empty `ResponseInputMessage.extra`. Thus a valid request such as:

```json
{
  "model": "provider/model",
  "messages": [
    {
      "role": "user",
      "content": "hello",
      "future_message_extension": {"keep": true}
    }
  ],
  "future_request_extension": {"keep": true}
}
```

is accepted but those fields are absent from the captured Responses request.
This is silent loss through a supported Messages-to-Responses path and is not
covered by the current extension fixtures, which focus on tools/content/
metadata/config.

### Blocking: `stop_sequences` is validated then silently ignored

The inbound validator accepts `stop_sequences` as a valid non-null/empty
string array, but `ResponsesPayload` has no corresponding field and the
translation builder does not copy it into `extra` or reject it. A Claude
request can therefore ask for stop sequences, receive a successful Responses
turn, and never have the constraint sent upstream. The proxy should map it if
the selected Responses provider supports an equivalent, or reject it
explicitly rather than claiming complete Messages compatibility.

### Additional schema collection gap: empty required-name arrays

The recursive schema validator now enforces permitted `type` values, unique
type arrays, and nonempty/unique name entries. It does not reject an empty
`required: []`, `dependentRequired: {"x":[]}`, or dependency string array.
These are known JSON Schema collections with minimum-name semantics, and the
new tests do not probe them. If the implementation intentionally uses a
shape-only exception, the exception must be documented; otherwise reject
before dispatch.

## What Must Be Fixed

1. Merge top-level and message-level open extensions into Responses input with
   canonical-field collision checks, or explicitly reject unsupported
   extensions; do not silently drop them.
2. Map `stop_sequences` to an equivalent Responses control where supported, or
   reject it with a native Anthropic unsupported-feature error before dispatch.
3. Enforce/document minimum-name semantics for required/dependent schema
   arrays and add public no-dispatch fixtures.
4. Re-run all accumulated carrier, framing, lifecycle, scalar, raw, compact,
   authority, request-validation, extension, auth/routing, native,
   hardening, quality-gate, and canary checks.
