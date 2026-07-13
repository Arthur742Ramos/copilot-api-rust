# Inspector Feedback — Iteration 6

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  the complete accumulated diff from
  `8b7472013665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `48aed44499a1ef173728020bb44aa4faeac1a28e`.
- Re-read the Codex 0.144.1 HTTP SSE implementation at
  `44918ea10c0f99151c6710411b4322c2f5c96bea`. Its `ResponseCompleted`
  deserializer requires `id` and optional usage/end-turn fields, not
  `response.status`; the checked-in `ev_completed` fixture emits no status.
- Independently ran the expanded public lifecycle/framing tests, Codex
  optional-item and compaction continuation tests, direct probes for
  reasoning-content events, terminal variants, lifecycle replay/conflict,
  all required quality gates, and the installed loopback Codex canary. No
  paid/external provider or port 4141 was used.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, and
  evidence matrix.** Documentation records the audited client versions,
  setup, endpoints, headers, Codex source, TypeScript reference, provider
  routing, framing policy, lifecycle policy, and bounded-state claims.
- [x] **Criterion 2 — credential-free black-box public boundary harness.**
  The public Axum harness uses ephemeral loopback fixtures, captured
  JSON/SSE, fake credentials, and no production port or paid provider. It now
  exercises reasoning summary/content deltas, replay/conflict cases, sparse
  indices, terminal mismatches, and standalone done items.
- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  The reasoning state machine now maps `response.reasoning_text.delta`,
  buffers summary/content parts, reconciles authoritative done values,
  rejects missing/out-of-order/duplicate/conflicting item events, bounds
  state, and terminates once. However, the translated Responses-to-Messages
  bridge rejects a valid Codex 0.144.1 terminal:

  ```json
  {
    "type": "response.completed",
    "response": {
      "id": "r",
      "usage": {
        "input_tokens": 1,
        "output_tokens": 1,
        "total_tokens": 2
      }
    }
  }
  ```

  Codex's checked-in `ev_completed` helper emits exactly this shape, and its
  `ResponseCompleted` type does not require a status. The proxy's
  `handle_response_completed` instead requires
  `response.status == "completed"` and emits an Anthropic `api_error`. The
  same issue applies to `response.incomplete` without a status. A direct
  probe returned `completed_without_status=[Error ... inconsistent response
  status]` and `incomplete_without_status=[Error ... inconsistent response
  status]`.
- [x] **Criterion 4 — Codex Responses and compaction compatibility.**
  Direct native Responses forwarding remains protocol-native, and optional
  `ResponseItem` variants, id-less compaction output/continuation, native
  event passthrough, usage, IDs, and function-call arguments remain covered.
  The status omission is currently a failure in the Responses-to-Anthropic
  bridge; the direct Codex `/v1/responses` guard does not impose this
  unnecessary status requirement.
- [ ] **Criterion 5 — native JSON/SSE contracts and terminal behavior.**
  Lifecycle and malformed-stream handling is substantially improved, but a
  source-valid Codex terminal is converted to an Anthropic error solely
  because an optional/unmodeled `status` field is absent. That is not a
  protocol-native error and breaks a valid successful stream in the public
  provider Messages path.
- [x] **Criterion 6 — authentication, routing, provider-only mode, and
  discovery.** Client authentication, provider credential replacement,
  mappings, `/v1/models`, explicit unsupported route/model errors, and
  provider-only startup remain covered and unchanged.
- [ ] **Criterion 7 — regression tests for every compatibility claim.**
  The expanded lifecycle tests cover content deltas, summary part ordering,
  duplicates, conflicts, sparse indices, terminal output mismatches,
  standalone items, later IDs, and bounded state. They all construct terminal
  fixtures with `status: "completed"` or `"incomplete"`, so there is no
  regression for Codex's source-backed no-status completed/incomplete
  terminal shape.
- [ ] **Criterion 8 — documentation claims are fully evidenced.**
  The lifecycle documentation claims the audited Codex event sequence and
  successful terminal handling, but does not disclose that the bridge requires
  a status field Codex's own canonical completion fixture omits. The support
  claim is broader than the source-backed behavior.
- [ ] **Criterion 9 — surgical, lossless, hardened implementation.**
  Bounded state, ID/index reconciliation, native error termination, and prior
  admission/auth/size/retry hardening are preserved. Requiring an unnecessary
  status field still rejects a valid event and violates the goal's explicit
  client-contract and no-silent-divergence requirements.
- [x] **Criterion 10 — quality gates.** All required commands passed:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks passed:
  `cargo test --test client_compatibility --verbose` (9 passed, 1 ignored),
  `cargo test --lib --verbose`, and
  `cargo test --test client_compatibility installed_codex_cli_smoke
  -- --ignored --nocapture` (1 passed). `cargo deny check` retained existing
  non-fatal unmatched-license/duplicate-dependency warnings but returned
  success.

## Issues Found

### Blocking: valid Codex terminal events without `response.status` fail

The audited Codex HTTP SSE contract and checked-in fixtures omit `status` from
`response.completed` and do not require it for `response.incomplete`. The
bridge imposes that field and returns a protocol error for valid terminal
events, while the new lifecycle fixtures mask the issue by adding status.

## What Must Be Fixed

1. Accept Codex-valid completed/incomplete terminal responses when the
   canonical required fields are present; use the event type as the terminal
   discriminator and validate status only when supplied (or explicitly
   document a justified provider-specific requirement).
2. Add public provider-fixture tests for completed and incomplete terminals
   without status, asserting exactly one successful Anthropic terminal for
   completed and the correct truncation/error semantics for incomplete.
3. Re-run all prior lifecycle, optional-item, framing, quality-gate, and
   installed-canary checks.
