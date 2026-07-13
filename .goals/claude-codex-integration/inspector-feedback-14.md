# Inspector Feedback — Iteration 14

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  Builder commit `9e8987fa049adec4a42bee69fdcffbe2cbebd9da`.
- Re-read the audited Claude Code 2.1.207 and Codex CLI 0.144.1 contracts and
  the Codex compact mock/source shape. Direct and provider compact responses
  now share an output-only buffered contract, strict usage validation, safe
  headers, exact bytes, and bad-gateway handling.
- Verified installed versions:
  - `claude --version` → `2.1.207 (Claude Code)`
  - `codex --version` → `codex-cli 0.144.1`
- Inspected all direct/provider compact readers, response status/header
  handling, token-usage record construction, bounded upstream metrics, and
  annotation canonicalization.
- Ran the public compatibility suite, all required repository gates, and the
  ignored loopback Codex canary. No repository product code was changed.
- Independently probed the public provider Messages web-search path with:
  `allowed_domains: [42, "example.com"]`, `blocked_domains: "not-an-array"`,
  and `user_location: "not-an-object"`. The forwarded Responses tool contained
  only `allowed_domains: ["example.com"]`, omitted the malformed blocked list,
  and forwarded the invalid scalar user location. This is silent request-side
  normalization outside the tested output-snapshot authority logic.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documentation now covers direct/provider compact schemas,
  byte/header/error behavior, usage/metric labels, and web annotation
  authority. It does not document the malformed request-side web-search
  collection normalization observed in the public probe.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The suite now
  runs 44 tests (43 passed and one ignored), through the production Axum
  router with deterministic ephemeral fixtures. It covers direct and provider
  compact parity, exact bytes, headers, error status/envelopes, usage records,
  upstream metrics, web annotation directions, accumulated Claude/Codex
  lifecycles, raw variants, and the loopback canary.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  accumulated Messages behavior and all tested web output annotations pass.
  However, malformed Anthropic web-search request options are silently
  filtered before provider dispatch: non-string domain entries are dropped,
  non-array domain fields become absent, and a scalar `user_location` is
  forwarded. This violates the no-silent-fallback/lossless request boundary
  for a supported Claude server tool.

- [x] **Criterion 4 — Codex Responses, continuation, optional items,
  compaction, and native transport.** Direct and provider compact now share
  `ResponsesCompactResult` parsing, validate output/usage consistently,
  preserve output-only/id-less shapes and exact bytes, forward allowlisted
  request IDs/state headers, record `responses_compact` usage, and emit
  bounded direct/provider metrics. Native Responses SSE, raw variants,
  continuation, reasoning, and compaction remain green.

- [ ] **Criterion 5 — native JSON/SSE contracts, errors, and termination.**
  Direct/provider compact malformed JSON, wrong output/item, malformed or
  inconsistent usage, oversized successful bodies, upstream 4xx/5xx, retry
  metadata, and safe headers now match in tests. The request-side web-search
  malformed collection case still produces a provider request and can proceed
  with altered semantics instead of one native Anthropic validation error.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models,
  and aliases.** Gateway/provider credentials, direct Copilot setup, provider
  compact routing, model discovery/mappings, provider-only startup, and the
  installed canary remain green.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  suite now covers every direct/provider compact body and metric case and all
  listed annotation missing/null/empty/unknown/mixed/known/malformed
  directions. It does not cover malformed inbound web-search configuration:
  mixed-type or scalar `allowed_domains`/`blocked_domains`, malformed
  `user_location`, or malformed tool-reference collections. The independent
  public probe demonstrates a real silent transformation.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** Compact and
  annotation documentation is now supported by the expanded tests. The
  documentation does not state how malformed web-search request options are
  handled, while the implementation silently omits invalid domain values and
  forwards an invalid location shape. A supported request boundary needs an
  explicit reject-or-preserve policy and evidence.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  compact reader/metrics/header refactor is shared and bounded, and prior
  hardening remains intact. `extract_web_search_config` still uses
  `as_array().map(...filter_map(as_str))`: invalid arrays/entries are silently
  converted to partial valid configuration, and `user_location` is copied
  without object/null validation. This is an avoidable silent fallback and
  can change the user's requested search restrictions.

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

### Blocking: malformed web-search request collections are silently normalized

`extract_web_search_config` currently defines `extra_array` as:

- `as_array()` failure → `None`;
- mixed array values → non-string entries are discarded;
- valid strings → retained;
- `user_location` → copied as any `Value`.

An independent public provider Messages probe sent:

```json
{
  "allowed_domains": [42, "example.com"],
  "blocked_domains": "not-an-array",
  "user_location": "not-an-object"
}
```

The captured Responses request contained only
`filters.allowed_domains: ["example.com"]`, no blocked-domain restriction, and
`user_location: "not-an-object"`. The request boundary therefore changed the
user's search policy without an error. This conflicts with the repository
conventions to avoid silent fallbacks and with robust Claude server-tool
compatibility.

The fix should either reject malformed option fields with an Anthropic
validation error or preserve and forward only a source-valid shape under a
documented policy; it must not silently drop mixed values.

### Additional analogous gap: malformed tool-reference collections are ignored

`extract_tool_reference_names` in the Responses request translation uses
`filter_map` for `tool_reference.tool_name`. A malformed/missing tool name is
silently omitted, potentially turning a requested deferred-tool selection
into an empty selection. This is the same request-side collection-normalization
pattern and needs a deliberate validation policy or explicit documentation.

## What Must Be Fixed

1. Validate `allowed_domains` and `blocked_domains` as arrays of non-empty
   strings, validate `user_location` as the supported object/null shape, and
   return a native Anthropic request error for malformed values.
2. Audit and test malformed/missing `tool_reference` collections rather than
   silently filtering invalid names.
3. Add credential-free public fixtures asserting no malformed search policy is
   dispatched and that valid empty/mixed/unknown cases follow the documented
   policy.
4. Re-run all accumulated carrier, framing, lifecycle, scalar, raw variant,
   web authority/annotation, compact direct/provider, metrics, auth/routing,
   native SSE/JSON, hardening, quality-gate, and canary checks.
