# Inspector Feedback — Iteration 2

## Verdict: PASS

## Acceptance Criteria Check

- [x] Criterion 1 — **VERIFIED**: `/alpha/search`, `/v1/alpha/search`,
  provider-scoped versioned/unversioned aliases, capability rejection,
  malformed JSON, client authentication failures, upstream authentication
  failures, query/body preservation, header replacement, and upstream error
  propagation are covered by the loopback fixture tests. Codex Alpha Search
  now preserves caller `Accept`/`openai-beta` headers while defaulting missing
  `Accept` to JSON, and resolver failures are rendered as redacted OpenAI
  server errors rather than provider-not-found responses.
- [x] Criterion 2 — **VERIFIED**: The provider route matrix and aliases remain
  registered with unambiguous static-route precedence. Model-level
  `models.<model>.type` overrides now select the effective protocol, auth
  default, and capability set; model unknown fields round-trip through the
  flattened configuration map. Mixed-provider fixtures cover Chat,
  Responses, Messages, unsupported combinations, headers, JSON/SSE, and
  unknown request fields.
- [x] Criterion 3 — **VERIFIED**: The WebSocket pool now performs a bounded
  ping/pong preflight before `response.create`, watches idle sockets for
  close/error/stale application frames, reopens failed reused sockets once,
  and keeps ambiguous request-frame failures inside the returned stream so
  they cannot trigger a billable replay. The lifecycle guard authorizes reuse
  only after a valid terminal event. Handshake, stale-preflight,
  close-after-terminal, dead-socket reopen, malformed-terminal,
  cancellation, heartbeat, silence, reuse, and truncated-stream tests pass;
  the WebSocket suite also passed in 20 repeated runs.
- [x] Criterion 4 — **VERIFIED**: Guided provider selection and injected-I/O
  tests remain green. Custom provider credentials stay out of config/log
  output, configuration and credential writes use atomic replacement, Unix
  permissions are verified as `0600`, and Windows uses a protected,
  current-user-only DACL with fail-closed behavior. Copilot and Codex OAuth
  entry points now reject non-TTY execution immediately; the CLI test covers
  no-provider, explicit Copilot, and explicit Codex paths.
- [x] Criterion 5 — **VERIFIED**: Claude Code marketplace/hooks/MCP assets and
  OpenCode plugin/configuration remain installable and documented. Marker
  propagation, deferred tool selection, diagnostics, provider selection,
  asset secret/path scans, Node syntax/import, and an injected OpenCode
  lifecycle check pass without credentials.
- [x] Criterion 6 — **VERIFIED**: The compatibility audit still pins the
  checked-out TypeScript reference commit/package and the installed Claude
  Code, Codex CLI, and OpenCode versions. Endpoint/provider matrices,
  runnable examples, redacted issue forms, and the no-adoption-claims
  boundary remain present. Historical older-client fixtures are clearly
  separate from the current version identity.
- [x] Criterion 7 — **VERIFIED**: Existing strict malformed/truncated stream
  behavior, Anthropic/OpenAI lifecycles, Files API quotas, admission/load
  shedding, remote-bind authentication, health/version/metrics, redaction,
  and billable-request replay safeguards remain covered and green.
- [x] Criterion 8 — **VERIFIED**: All requested native gates pass from the
  clean iteration-2 tree. The optional Windows cross-target check was attempted
  but could not reach Rust code compilation because this macOS host lacks the
  Windows/MSVC C/SDK environment required by `ring`; the checked-in Windows
  ACL/atomic-replace branches were reviewed and are covered by `cfg(windows)`
  tests.

## Quality Gate

- `cargo fmt --all -- --check` — **PASS**
- `cargo clippy --all-targets -- -D warnings` — **PASS**
- `cargo build` — **PASS**
- `cargo test` — **PASS** (536 library tests plus all integration suites)
- `cargo deny check` — **PASS** (advisories, bans, licenses, and sources)
- Targeted `cargo test --test non_gui_features` — **PASS**
- Targeted `cargo test --test codex_resolver_errors` — **PASS**
- Targeted `cargo test --test cli_onboarding` — **PASS**
- WebSocket lifecycle suite repeated 20 times — **PASS**
- No live server on port 4141 was stopped or restarted, and no credentials were
  used.

## Issues Found

No blocking functional, security, protocol, compatibility, packaging,
documentation, or test gap was found in iteration 2.

## What Must Be Fixed

None.
