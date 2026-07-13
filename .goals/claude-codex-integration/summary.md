# Verified Summary: Claude Code and Codex Integration

## Result

**PASS after 27 Builder/Inspector iterations.**

The proxy is now independently verified against Claude Code 2.1.207 and OpenAI
Codex CLI 0.144.1 contracts, using Codex source commit
`44918ea10c0f99151c6710411b4322c2f5c96bea` and deterministic public-boundary
fixtures. The implementation preserves native Anthropic behavior for Claude Code,
native OpenAI Responses behavior for Codex, and strict translation behavior when a
configured provider requires a protocol bridge.

## Acceptance Criteria

1. **Audited clients and reproducible matrix — PASS.** The exact client versions,
   setup, endpoints, headers, source evidence, TypeScript-reference comparison,
   and intentional divergences are documented in
   `docs/claude-code-codex-compatibility.md`.
2. **Credential-free public harness — PASS.** Tests enter the production Axum
   router and use ephemeral loopback upstreams, fake credentials, and no port
   4141 or paid provider. Provider and direct-Copilot paths are both exercised.
3. **Claude Code compatibility — PASS.** Messages JSON/SSE, structured system and
   content blocks, tool definitions/results, parallel tools, prompt caching,
   thinking/reasoning carriers, token counting, aliases, beta/version headers,
   extensions, cancellation, truncation, and retryable failures are covered.
4. **Codex compatibility — PASS.** Native Responses JSON/SSE, instructions/input,
   optional continuation items, reasoning, function/custom/tool-search calls,
   parallel calls, usage, metadata, raw variants, id-less compaction, and
   continuation after compaction are covered.
5. **Native protocol and terminal semantics — PASS.** Anthropic and OpenAI
   envelopes remain protocol-native. Streams preserve identities, ordering,
   arguments, usage, and exactly one success or error terminal; malformed or
   incomplete input cannot fabricate success.
6. **Authentication, routing, and models — PASS.** Client authentication,
   provider credentials, safe headers, provider-only startup, aliases, model
   discovery, and explicit unsupported model/route errors are verified.
7. **Regression evidence — PASS.** The final full run passed 565 tests with 2
   ignored; the public compatibility suite passed 69 with 1 opt-in canary
   ignored; 38 transactional Responses accounting tests passed. The Builder ran
   the installed Codex loopback canary successfully. The final Inspector could
   not rerun that optional canary because its environment lacked the `codex`
   executable; deterministic gates were unaffected.
8. **User documentation — PASS.** Setup, supported behavior, validation policy,
   transport-specific controls, extension handling, compaction, failure
   semantics, troubleshooting, and audited versions match tested behavior.
9. **Surgical hardening — PASS.** Unknown fields and stable key order are
   preserved where representable; unrepresentable or malformed known fields fail
   explicitly. Existing admission, retry, size, remote-auth, and internal-error
   hardening remains intact.
10. **Repository gates — PASS.** Formatting, Clippy with warnings denied, build,
    full tests, and cargo-deny passed. Both online and reproducible offline
    cargo-deny checks passed in the final inspection.

## Iteration History

- **1–4:** Fixed Codex-valid optional continuation shapes, id-less compaction,
  optional reasoning carriers, empty-summary preservation, and JSON/SSE reasoning
  framing.
- **5–8:** Completed reasoning-event lifecycle handling, statusless Codex
  terminals, stable response identity, and strict usage validation.
- **9–14:** Removed scalar coercion across stream/non-stream translation, aligned
  provider and direct compaction, reconciled web-search snapshots, classified raw
  output variants, and normalized annotation semantics.
- **15–19:** Added fail-closed inbound request validation, bounded recursive JSON
  Schema checks, catalog-aware tool choices, extension preservation, route-aware
  controls, and lossless Chat request translation.
- **20–24:** Hardened non-stream and streaming Chat responses, optional tool-call
  fragments, refusal/logprobs/service-tier behavior, refusal accumulation, and
  source-ordered tool/text/reasoning scheduling.
- **25–27:** Unified aggregate output bounds, corrected retained-state ownership,
  added public Responses/web overflow fixtures, made cross-budget updates
  transactional, and verified observed upstream usage is recorded once while
  failed translations remain error outcomes.

## Key Inspector Findings Resolved

- Valid Codex optional fields and statusless events were initially rejected.
- Reasoning carriers, summary parts, and tool fragments could be dropped,
  duplicated, reordered, or completed incorrectly.
- Chat and Responses JSON/SSE paths had inconsistent scalar validation, usage,
  extension, and terminal behavior.
- Direct/provider compact and web-search snapshot paths diverged in validation,
  byte preservation, metrics, and errors.
- Malformed request collections and nested schemas could be silently normalized.
- Output and retained-state limits initially double-counted or committed
  non-atomically.

All were fixed and independently rechecked.

## Recommendations

- Keep the opt-in installed-client canaries available in a dedicated environment
  with pinned Claude Code and Codex binaries to detect client drift.
- Add property-based or fuzz testing around Chat and Responses stream state
  machines to supplement the deterministic adversarial matrix.
- Periodically refresh the audited client versions and source commit in the
  compatibility guide.
- Address existing non-fatal cargo-deny duplicate/unmatched-license warnings as a
  separate maintenance task.
