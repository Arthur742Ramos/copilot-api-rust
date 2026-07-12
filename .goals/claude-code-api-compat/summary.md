# Claude Code API compatibility — completion summary

## Outcome

The goal passed independent inspection in iteration 6. The proxy now has a
documented and regression-tested Claude Code compatibility surface covering
Anthropic Messages and count-token routing, common request forms, model and
provider aliases, tool flows, deterministic streaming lifecycle behavior, and
SDK-recognizable error handling.

## Acceptance criteria

1. **Compatibility audit:** Completed in
   `docs/claude-code-api-compatibility.md`. The matrix records audited
   endpoints, request forms, content and tool behavior, stream events, model
   handling, headers, errors, intentional divergences, and code/test evidence.
2. **Common Claude Code requests:** Streaming and non-streaming messages,
   string and structured content, system prompts, metadata and safe unknown
   fields, tool definitions/choice, tool-use/tool-result turns, caching, and
   provider/model aliases are supported by deterministic tests.
3. **Streaming correctness:** Text, thinking/reasoning, fragmented and
   interleaved tool calls, usage, stop reasons, normal completion, malformed
   input, transport interruption, truncation, and upstream error events now
   terminate in deterministic Anthropic event order. Invalid chunks cannot
   silently disappear or fabricate success.
4. **Error compatibility:** Shared HTTP failures, direct and aliased
   count-token failures, provider routing failures, and translated stream
   failures use SDK-recognizable Anthropic envelopes with safe diagnostics,
   status-derived types, retry/overload semantics, and allowed correlation or
   rate-limit headers.
5. **Model and header behavior:** Model discovery, aliases, `[1m]`
   normalization, Claude Code initiator/editor headers, beta filtering,
   provider selection, and unsupported-capability errors are documented and
   covered.
6. **Regression coverage:** The credential-free suite includes representative
   Claude Code requests, tool flows, stream fragmentation, malformed nested
   fields, upstream failures, count-token aliases, and public/provider
   translated drivers. Independent inspection reported 433 passing tests,
   including 51 targeted translated-stream tests.
7. **Quality gates:** Independent inspection passed formatting, clippy with
   warnings denied, build, full tests, and `cargo deny check`.

## Iteration history

1. **FAIL:** Nested-only upstream and unknown-provider errors lacked the
   top-level Anthropic `type: error` discriminator.
2. **FAIL:** Provider-scoped count-token unknown/malformed requests still
   bypassed complete Anthropic error rendering.
3. **FAIL:** Top-level translated Chat Completions error objects were mistaken
   for legitimate empty-choice chunks.
4. **FAIL:** Missing or non-array `choices` values could still silently finish
   or continue a malformed stream.
5. **FAIL:** Wrong-typed nested delta, tool-call, and usage fields could be
   ignored, defaulted, or truncated into success-shaped output.
6. **PASS:** Nested validation, ordered terminal cleanup, driver agreement,
   matrix accuracy, and every acceptance criterion were independently verified.

## Key resolutions

- Normalized nested and locally generated errors into complete Anthropic
  envelopes without losing intended statuses, safe messages, or allowed
  headers.
- Reworked count-token body parsing so malformed, invalid, oversized, and
  unknown-provider requests use the same client-recognizable failure contract.
- Made translated upstream error events terminal before empty-choice usage
  handling and prevented deferred success from flushing after failure.
- Distinguished explicit valid `choices: []` usage records from structurally
  malformed chunks.
- Added strict type validation for consumed content/reasoning, tool-call, and
  token-accounting fields while preserving legitimate null, omitted, and
  fragmented tool updates.
- Centralized malformed-stream cleanup so open blocks close in order, deferred
  state is cleared, exactly one terminal error is emitted, and later chunks or
  EOF cannot fabricate success.

## Recommendations

- Add an opt-in credentialed canary outside normal CI to detect upstream
  protocol changes without making the deterministic suite depend on live
  services.
- Consider property-based or fuzz testing for translated SSE chunks to extend
  malformed-input coverage beyond curated fixtures.
- Clean up the existing duplicate-dependency and license-allowance warnings
  reported by `cargo deny` in a separate maintenance change.
