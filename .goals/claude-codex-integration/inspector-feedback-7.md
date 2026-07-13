# Inspector Feedback — Iteration 7

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  the complete accumulated diff from
  `8b7472013665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `47a42037179e1a506a580efcfff3ca999c439b43`.
- Re-read Codex 0.144.1 terminal definitions and fixtures at
  `44918ea10c0f99151c6710411b4322c2f5c96bea`. The canonical
  `response.created` fixture contains only an id; `ResponseCompleted` has
  required id and optional usage/end-turn fields, with no status requirement.
- Independently probed the translated bridge for created-without-model,
  empty/mismatched terminal IDs, and malformed/partial/wrong-typed usage.
  Also ran the expanded public lifecycle/terminal suite, all optional-item,
  framing, and compaction tests, every quality gate, and the installed
  loopback Codex canary. No paid/external provider or port 4141 was used.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, and
  evidence matrix.** The documentation records current client versions,
  setup, endpoints/headers, Codex source/fixture links, provider routing,
  framing, lifecycle, terminal, and bounded-state policy.
- [x] **Criterion 2 — credential-free black-box public boundary harness.**
  Public Axum tests use ephemeral loopback fixtures, fake credentials,
  captured JSON/SSE, and no external provider or production port. The harness
  now covers statusless terminals, failed/error follow-ons, lifecycle
  conflicts, usage presence, and terminal output state.
- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  The bridge now handles reasoning content deltas, bounded lifecycle state,
  statusless completed/incomplete terminals, repeated/contradictory terminals,
  and exact usage mapping for valid fixtures. It still has client-contract
  failures:

  1. `handle_response_created` requires both `response.id` and
     `response.model`. Codex's source-backed `ev_response_created` contains
     only `{"id": ...}`. A direct probe produced
     `created_without_model=[Error ... missing its response id or model]`.
  2. The bridge requires terminal id presence only as a string type and never
     compares terminal id to the id from `response.created`. Direct probes
     with `id: ""` and `id: "different"` both emitted successful
     `message_delta`/`message_stop`.
  3. Terminal usage is parsed with `as_i64().unwrap_or(0)`. Wrong-typed,
     null, partial, or otherwise malformed usage fields are silently mapped
     to zero and still produce successful Anthropic completion. Codex's
     typed `ResponseCompletedUsage` would reject wrong types and missing
     required fields when a usage object is present.
- [x] **Criterion 4 — Codex Responses and compaction compatibility.**
  The direct native `/v1/responses` path forwards statusless Codex terminals
  unchanged, and optional items, reasoning/summary content, function calls,
  id-less compaction continuation, usage, and native event termination remain
  covered. The remaining failures are in the Responses-to-Anthropic bridge,
  not the direct OpenAI Responses contract.
- [ ] **Criterion 5 — native JSON/SSE contracts and terminal behavior.**
  Lifecycle and terminal event handling is substantially improved, but the
  public translated Messages stream rejects a source-valid created event
  without a model and can report successful completion for a terminal whose
  id conflicts with the created response. Malformed usage can also fabricate
  a success with zero usage instead of a protocol-native error.
- [x] **Criterion 6 — authentication, routing, provider-only mode, and
  discovery.** Client keys, provider credential replacement, mappings,
  `/v1/models`, explicit unsupported route/model errors, and provider-only
  startup remain covered and unchanged.
- [ ] **Criterion 7 — regression tests for every compatibility claim.**
  The new terminal tests cover statusless/matching/mismatched status,
  absent/present usage, failed/error follow-ons, pending items, and terminal
  truncation. They do not cover Codex's model-less `response.created`,
  terminal id mismatch/empty id, or malformed/partial usage field types.
  The public fixtures consequently do not catch the direct probe failures.
- [ ] **Criterion 8 — documentation claims are fully evidenced.**
  Documentation correctly describes statusless terminal handling but does
  not disclose the bridge's extra model requirement, lack of terminal-id
  reconciliation, or silent usage coercion. Its broad current-client support
  claim remains ahead of source-backed bridge behavior.
- [ ] **Criterion 9 — surgical, lossless, hardened implementation.**
  Prior hardening, bounded state, native errors, and Codex request-field
  preservation remain intact. Requiring a non-contract model field,
  accepting conflicting response identities, and coercing malformed usage to
  zero violate explicit client-contract, lossless, and no-silent-fallback
  requirements.
- [x] **Criterion 10 — quality gates.** All required commands passed:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks passed:
  `cargo test --test client_compatibility --verbose` (18 passed, 1 ignored),
  `cargo test --lib --verbose`, and
  `cargo test --test client_compatibility installed_codex_cli_smoke
  -- --ignored --nocapture` (1 passed). `cargo deny check` retained existing
  non-fatal unmatched-license/duplicate-dependency warnings but returned
  success.

## Issues Found

### Blocking: bridge rejects or fabricates success for valid/malformed terminal data

The translated bridge still requires `response.model` on `response.created`,
does not bind terminal ids to the created response id, and treats malformed or
partial terminal usage as zero. These behaviors disagree with the audited
Codex terminal structs/fixtures and can either reject a valid stream or return
an incorrect successful Anthropic usage/identity.

## What Must Be Fixed

1. Require only the Codex-contract fields on `response.created` (id); retain
   model when present but do not reject its absence.
2. Store the created response id and require non-empty matching terminal ids
   for completed/incomplete events, with tests for empty and conflicting ids.
3. Validate terminal usage objects against the Codex shape when present:
   accept omission, reject wrong types and missing required token fields, and
   preserve valid cached/reasoning usage semantics.
4. Add public provider fixtures for all three cases and rerun the full gates,
   optional-item/framing/lifecycle tests, and installed canary.
