# Inspector Feedback — Iteration 16

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  Builder commit `a8fe46e3b501eb3bd2bed27de8fba7d4e29a6b9b`.
- Re-read the accumulated Claude Code 2.1.207 and Codex CLI 0.144.1
  compatibility evidence and the new catalog-aware tool-choice/recursive-schema
  policy.
- Verified installed versions:
  - `claude --version` → `2.1.207 (Claude Code)`
  - `codex --version` → `codex-cli 0.144.1`
- Inspected recursive schema traversal, boolean schemas, node/depth/collection
  budgets, path errors, catalog choice resolution, deferred references,
  source-type validation, and Responses conversion.
- Ran the public compatibility suite, all required repository gates, and the
  ignored Codex loopback canary. No repository product code was changed.
- The public boundary now rejects the previously identified undefined/
  incompatible tool-choice cases, recursive structural containers, bounds,
  malformed source types, and deferred references before fixture dispatch.
  Remaining gaps are semantic known-schema constraints and unknown choice
  field preservation during translation.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** Documentation now describes catalog-aware choices, recursive
  object/boolean schema validation, bounds, source support, deferred references,
  and the direct/provider/native matrix. It does not disclose that known
  schema primitive values such as an invalid `type` string are only shape
  checked, nor that unknown `tool_choice` keys are discarded by Responses
  conversion.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The public
  suite now runs 53 tests (52 passed and one ignored), through the production
  Axum router and deterministic loopback fixtures. It covers the accumulated
  Claude/Codex workflows, direct/provider compact, native bytes/headers/errors,
  request collections, catalog choices, recursive schemas/bounds, source
  validation, annotations, lifecycles, auth/routing, metrics, and the
  installed Codex canary.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** Catalog
  selection and malformed collection rejection now pass before dispatch.
  However, a valid open `tool_choice` object containing an unknown extension
  key is converted to a new Responses object containing only `type` and
  `name`; the flattened unknown key is silently discarded. The same conversion
  pattern drops unsupported/unknown fields from custom tool objects. This
  conflicts with the goal’s unknown-field preservation/no-silent-fallback
  requirement unless explicitly classified as an intentional protocol
  translation divergence.

- [ ] **Criterion 4 — Codex Responses, continuation, optional items,
  compaction, and inbound translation.** All accumulated Responses and
  compaction behavior remains green. Recursive schema traversal is bounded and
  structurally robust, but known JSON Schema primitive constraints are not
  fully validated: `type` accepts arbitrary strings (and duplicate values in
  arrays), while required-name arrays permit empty/duplicate entries. A
  malformed known schema can therefore reach the Responses/provider boundary
  despite the documented malformed-definition policy.

- [ ] **Criterion 5 — native contracts, errors, and no upstream dispatch after
  validation failure.** The tested malformed containers and bounds return
  native HTTP 400 before upstream dispatch. An invalid-but-string schema type
  such as `{"type":"not-a-json-schema-type"}` passes the request validator and
  is copied by `normalize_tool_schema` into the outgoing function definition.
  The provider then determines behavior instead of the proxy emitting a local
  validation error.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models,
  and aliases.** Gateway/provider credentials, provider-only startup, catalog
  model resolution, tool-search model capability checks, aliases, model
  discovery, and the loopback Codex canary remain green.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  new tests cover tool-choice kind/catalog/deferred/server/bridge cases,
  recursive structural containers, boolean schemas, depth/node/collection
  bounds, source forms, and no-dispatch behavior. They do not cover:

  - invalid single-string schema `type` values or duplicate/empty `type`
    arrays;
  - duplicate/empty `required` and dependent-required entries;
  - unknown `tool_choice` extension preservation through Responses capture;
  - unknown tool-definition extension preservation or an explicit documented
    rejection policy.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs
  claim malformed definitions fail before dispatch and unknown keys on open
  objects remain intact. The implementation’s unknown choice/tool conversion
  drops and known-schema primitive gaps are not reflected in the evidence
  matrix. Either preserve these fields through a representable Responses
  extension mechanism or document the intentional loss and test it.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The new
  request-validation module is bounded and path-aware, and earlier hardening
  remains intact. Remaining issues:

  1. `validate_json_schema_node` checks the shape of `type` but not its
     permitted JSON Schema values, and its string-array checks do not enforce
     uniqueness/non-empty semantics for known required/type collections.
  2. `convert_anthropic_tool_choice` reconstructs a fresh value from the
     selected kind/name and drops `AnthropicToolChoice.extra`; the complex
     schema fixture’s `future_choice` sentinel is not present in the captured
     Responses request.
  3. `convert_tool_to_function`/deferred namespace conversion similarly
     selects only known tool fields, so unknown tool extensions are not
     retained or explicitly rejected.

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

### Blocking: known schema primitive values still reach the provider

The recursive validator correctly handles object/boolean nodes, traverses the
documented structural containers, and enforces depth, node, and collection
bounds. It still accepts any string for the JSON Schema `type` keyword and any
string (including duplicates/empty entries) in known required-name arrays.

For example, `input_schema: {"type":"not-a-json-schema-type"}` satisfies the
current validator and is copied unchanged by `normalize_tool_schema` into the
Responses tool definition. This is a malformed known primitive, not an
unknown extension keyword, and should fail before provider dispatch under the
documented policy. The same concern applies to duplicate/empty `type` and
required/dependent-required entries.

### Blocking: unknown tool-choice/tool fields are silently dropped in translation

`AnthropicToolChoice` preserves an `extra` map during deserialization, but
`convert_anthropic_tool_choice` emits a new string/object containing only the
recognized choice kind/name. The public complex-schema fixture includes a
`future_choice` extension; the captured Responses choice contains only
`{"type":"function","name":"selected_tool"}`. Unknown fields in custom tool
objects are likewise reduced to name/description/parameters by
`convert_tool_to_function`.

This violates the repository convention to preserve unknown JSON fields unless
the proxy explicitly documents an intentional protocol translation loss. A
client extension can therefore disappear silently at the public provider
boundary.

## What Must Be Fixed

1. Validate known JSON Schema primitive values and required-name collection
   semantics, while keeping boolean schemas, recursive bounds, and unknown
   keyword preservation.
2. Decide and document how unknown `tool_choice`/tool-definition fields map to
   Responses. Preserve representable extensions or reject unsupported known
   extensions explicitly; never silently discard them under a blanket
   unknown-field-preservation claim.
3. Add public fixtures for invalid schema primitives, duplicate/empty required
   entries, choice/tool extension sentinels, and captured no-loss behavior.
4. Re-run all accumulated carrier, framing, lifecycle, scalar, raw, compact,
   authority, request-validation, auth/routing, native, hardening, gate, and
   canary checks.
