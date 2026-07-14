---
name: verify
description: Verify copilot-api runtime changes through an isolated scratch server without disturbing the active proxy on port 4141.
user-invocable: true
allowed-tools:
  - Read
  - Bash
  - Bash(curl *)
---

# Verify copilot-api through its HTTP surface

1. Build the release binary with `cargo build --release`.
2. Confirm port 4142 is free, then start `target/release/copilot-api start --port 4142` in the background. Do not stop or reconfigure port 4141.
3. Poll `http://127.0.0.1:4142/readyz`, then capture `/version`.
4. Derive the local client key from `ANTHROPIC_API_KEY`, falling back to `ANTHROPIC_AUTH_TOKEN`; never print it. Send it as `Authorization: Bearer ...` when present.
5. Drive the changed route with `curl` against port 4142. For Messages compatibility, prefer a native Claude model advertised by `/v1/models`; exercise both a normal request and an adjacent invalid input.
6. For reasoning-history changes, request adaptive thinking with `display:"omitted"`, save the complete response content, and replay that content unchanged in the assistant turn of a second request.
7. Capture sanitized response fields with `jq`: status/type/model/stop reason, text, thinking length, signature presence, and usage. Never print signatures or credentials.
8. Stop the scratch process, remove temporary request/response files, verify port 4142 is free, and confirm port 4141 is still listening.

If a runtime flow would consume a paid or externally visible action beyond a minimal model request, ask first or use a loopback provider fixture. Tests and Clippy are CI evidence, not runtime verification.
