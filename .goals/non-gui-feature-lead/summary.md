# Goal Completion: Lead on every non-GUI capability

## Result

The goal passed independent inspection in iteration 2. The Rust gateway now
matches or exceeds the checked-out TypeScript reference across every actionable
non-GUI gap identified by the comparison.

## Acceptance Criteria

1. **Codex Alpha Search** — Added top-level and provider-scoped, versioned and
   unversioned routes with capability checks, request/header preservation,
   redacted error propagation, metrics, and credential-free loopback tests.
2. **Provider route breadth** — Registered the complete provider route matrix,
   added compatible aliases, preserved static-route precedence, and added
   per-model effective protocol/auth/capability resolution with unknown model
   fields retained.
3. **Responses WebSocket** — Implemented safe pooled Codex upstream WebSocket
   transport with bounded preflight, idle close/error monitoring, dead-socket
   reopen, cancellation and silence handling, lifecycle-authorized reuse, and
   no replay after ambiguous or observable progress.
4. **Guided provider configuration** — Added interactive terminal onboarding,
   quick/custom provider setup, injected-I/O coverage, atomic config and
   credential writes, non-TTY OAuth fail-fast behavior, Unix `0600`
   verification, and Windows current-user-only ACL enforcement with fail-closed
   behavior.
5. **Claude Code and OpenCode integrations** — Added installable Claude Code
   marketplace/hooks/MCP assets and OpenCode plugin/configuration, backed by
   marker, deferred-tool, diagnostics, provider-selection, syntax, lifecycle,
   secret-scan, and portability tests.
6. **Battle-testing and discoverability** — Refreshed the compatibility audit
   against a pinned TypeScript commit and installed client versions, added
   endpoint/provider matrices and runnable examples, and added structured
   redacted issue forms without making unsupported adoption claims.
7. **Existing advantages and security** — Preserved strict stream termination,
   Anthropic/OpenAI lifecycle correctness, Files API quotas, admission and load
   shedding, remote-bind authentication, health/version/metrics, redaction, and
   billable-request replay safeguards.
8. **Quality gates** — Formatting, clippy with warnings denied, build, the full
   test suite, `cargo deny check`, targeted feature/resolver/onboarding suites,
   and 20 repeated WebSocket lifecycle runs all passed without credentials or
   touching the live server on port 4141.

## Iteration History

### Iteration 1 — FAIL

The first implementation delivered the broad feature surface, but the Inspector
found five blocking gaps:

- pooled WebSockets could reuse dead or lifecycle-invalid connections;
- provider models could not override the provider-level wire protocol;
- Alpha Search changed reference-visible headers;
- non-missing Codex resolver failures were misreported as provider-not-found;
- explicit OAuth could block without a TTY, and non-Unix credential permissions
  were only advisory.

### Iteration 2 — PASS

The Builder added state-aware WebSocket preflight/reopen/eviction and
lifecycle-authorized reuse, effective per-model protocol/auth/capability
selection with flattened unknown fields, Alpha header preservation, explicit
resolver error propagation, non-TTY OAuth rejection, and enforced Windows
credential ACLs. The Inspector independently verified all original and repaired
criteria and found no remaining blocker.

## Inspector Issues and Resolutions

| Issue | Resolution |
| --- | --- |
| Dead or malformed pooled WebSocket reuse | Bounded ping/pong preflight, idle monitoring, one safe reopen, lifecycle guard approval before reuse, and deterministic race/error tests |
| Missing model-level provider type | Effective model/provider type, auth, and capability resolution plus unknown-field round trips |
| Alpha Search header drift | Preserve caller `Accept` and `openai-beta`; default only a missing `Accept` |
| Codex setup errors collapsed to 404 | Propagate non-missing resolver failures through redacted OpenAI server errors |
| Non-TTY OAuth and weak non-Unix permissions | Fail fast for non-interactive OAuth; verify Unix `0600`; enforce Windows user-only DACLs or fail closed |

## Recommendations

- Let the existing Linux/macOS/Windows CI matrix exercise the new Windows ACL
  path on a native Windows runner; the macOS Inspector could not complete an
  MSVC cross-build because the host lacks the Windows SDK and `ring` toolchain.
- Keep the pinned TypeScript and client-version compatibility audit current as
  those upstreams evolve.
- Use the new compatibility issue form and redacted diagnostics to turn
  external reports into deterministic regression fixtures.
