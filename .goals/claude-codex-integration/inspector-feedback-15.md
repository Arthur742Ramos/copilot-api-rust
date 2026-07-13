# Inspector Feedback — Iteration 15

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  Builder commit `30abeb5088460c3c6b39a50b8f2076bbaf4a4a43`.
- Re-read the accumulated Claude Code 2.1.207 and Codex CLI 0.144.1
  compatibility evidence and the new inbound request-validation policy.
- Verified installed versions:
  - `claude --version` → `2.1.207 (Claude Code)`
  - `codex --version` → `codex-cli 0.144.1`
- Inspected all known inbound Messages collections and object validators:
  tools, custom/deferred schemas, tool choice, messages/content, tool results,
  cache controls, system blocks, metadata, thinking/output configuration,
  stop sequences, image/document sources, web-search fields, and deferred
  references.
- Ran the public compatibility suite, all required repository gates, and the
  ignored Codex loopback canary. No repository product code was changed.
- Independently re-probed malformed web-search request collections after the
  Builder change; they now return HTTP 400 before the fixture receives a
  request. The remaining gaps below are in tool-choice reference validation and
  nested JSON Schema shape validation.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** Documentation now records the exact clients, setup, endpoint and
  header matrix, compact/native behavior, output authority, and inbound
  request-validation policy. It does not currently document that `tool_choice`
  names need to reference a declared tool or that nested schema values are
  only shallowly validated.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The public
  harness now runs 44 tests (43 passed and one ignored), uses the production
  Axum router and ephemeral fixtures, and covers direct/provider compact,
  raw/native Responses, web authority/annotations, carriers, lifecycle,
  request collections, auth/routing, metrics, and the Codex canary without
  network, paid credentials, or port 4141.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The new
  request validator correctly rejects malformed web domain arrays/location,
  tool references, tool results, cache controls, content, sources, and most
  tool/schema scalar cases before admission/provider dispatch. It does not
  verify that `tool_choice.type == "tool"` names an entry in the declared
  `tools` catalog. Such a request passes the shape validator and is converted
  to a Responses function tool choice, allowing provider dispatch with an
  undefined tool name. Nested malformed JSON Schema values likewise pass the
  validator and can be dispatched.

- [ ] **Criterion 4 — Codex Responses, continuation, optional items,
  compaction, and inbound translation.** All accumulated Responses,
  compaction, optional continuation, carrier, raw variant, and native direct/
  provider paths remain green. Inbound Anthropic tool definitions are not
  completely fail-closed: `input_schema.properties` values, nested schema
  nodes, and other consumed schema collections are not recursively validated,
  despite the documented claim that malformed schemas fail before dispatch.

- [ ] **Criterion 5 — native JSON/SSE contracts, errors, and no upstream
  dispatch after validation failure.** The tested malformed collections now
  return native Anthropic HTTP 400 and do not hit the fixture. The undefined
  tool-choice and malformed nested-schema cases are different: they pass the
  current preflight validator, so the request can reach the provider rather
  than producing one local validation error. This leaves an avoidable
  malformed-request path with provider-dependent behavior.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models,
  and aliases.** Gateway/provider credentials, provider-only startup, aliases,
  model discovery, direct/provider compact routing, and the installed canary
  remain green.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  suite covers a large set of malformed request collections and verifies no
  upstream dispatch. It does not cover:

  - `tool_choice: {"type":"tool","name":"undefined"}` against a defined tools
    catalog;
  - a schema with a scalar property schema, malformed `items`, malformed
    `additionalProperties`, or invalid nested `required`/property nodes;
  - valid JSON Schema boolean sub-schemas versus invalid scalar sub-schemas.

  These are public-boundary cases in the newly claimed request-validation
  contract.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs say
  malformed known fields never become omitted/defaulted values and that
  malformed definitions/schemas fail before dispatch. The unreferenced
  `tool_choice` name and shallow schema cases contradict those claims. The
  docs should either narrow the schema policy or the implementation should
  validate the consumed schema recursively and tie tool choice to the tool
  catalog.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  request-validation module improves early rejection and preserves unknown
  keys, but:

  1. `validate_tool_choice` receives only the payload and cannot consult the
     `ToolCatalog` returned by `validate_tools`; an undefined selected tool is
     accepted and forwarded.
  2. `validate_json_schema` validates only top-level `type`, `properties`
     container type, and `required` string entries. It accepts malformed
     nested property values and does not validate other consumed schema
     collections. `normalize_tool_schema` then preserves/forwards those
     malformed values.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS
  - `cargo clippy --all-targets -- -D warnings` — PASS
  - `cargo build --verbose` — PASS
  - `cargo test --verbose` — PASS
  - `cargo deny check` — PASS, with the repository's existing non-fatal
    unmatched-license/duplicate-dependency warnings
  - `cargo test --test client_compatibility --verbose` — PASS, 43 passed and
    1 ignored
  - `cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored
    --nocapture` — PASS; Codex 0.144.1 loopback-only canary

## Issues Found

### Blocking: undefined `tool_choice` names are not rejected before dispatch

`validate_tools` builds a `ToolCatalog`, but `validate_tool_choice` is called
without that catalog and only checks the choice's own type/name scalar shape.
For `{"type":"tool","name":"missing"}`, the request passes validation even when
the declared tools contain no `missing` entry. The Responses translator then
emits `{"type":"function","name":"missing"}` rather than producing an
Anthropic `400 invalid_request_error`.

This violates the new documented “before admission/provider dispatch” policy
and leaves behavior dependent on the upstream provider. The tool name should
be checked against the declared catalog (with the intentional bridge
tool-search exception handled explicitly).

### Blocking: JSON Schema validation is shallow and forwards malformed nested schemas

`validate_json_schema` only checks that the schema is an object, that an
optional top-level `type` is a string, that `properties` is an object, and that
`required` is an array of strings. It does not validate each property schema or
other consumed schema collections such as `items` and
`additionalProperties`. A schema such as:

```json
{
  "type": "object",
  "properties": {"path": 42},
  "items": "not-a-schema"
}
```

passes request validation and is retained by `normalize_tool_schema`, allowing
the malformed definition to reach a Responses/provider boundary. This
contradicts the docs and the goal's strict malformed-vs-optional request
contract. The implementation needs a recursive JSON Schema shape policy,
including the explicitly allowed boolean-schema form if that is intended.

### Additional analogous request-boundary gap

`validate_source` accepts unknown non-null image/document `source.type` values
in its default branch, although the Responses translator later rejects them.
That is not a silent provider dispatch today, but it is inconsistent with the
new claim that known source objects fail closed during the validation phase and
consumes admission before the later translation error. The source type policy
should be made explicit and tested alongside the recursive schema policy.

## What Must Be Fixed

1. Pass the `ToolCatalog` into `validate_tool_choice` and reject undefined
   names before admission/provider dispatch, preserving the deliberate
   tool-search bridge exception.
2. Recursively validate the consumed JSON Schema shape, including nested
   properties/items/additionalProperties and valid boolean schemas, while
   preserving unknown schema keys.
3. Reject unsupported image/document source types in request validation rather
   than deferring the error until translation.
4. Add public fixtures proving no upstream request is captured for all these
   cases, then rerun all accumulated Claude/Codex, compact/native, authority,
   auth/routing, hardening, quality-gate, and canary checks.
