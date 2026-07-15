# copilot-api

An OpenAI- and Anthropic-compatible API gateway for GitHub Copilot, Codex, and
third-party LLM providers.

> **A Rust port of [`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api).**
> This is an independent Rust re-implementation of the upstream TypeScript
> project. See [`NOTICE.md`](./NOTICE.md) for attribution.

## What it does

`copilot-api` runs a local HTTP server that translates standard LLM API calls
into GitHub Copilot requests, using your own Copilot subscription. It exposes:

- **OpenAI-compatible** endpoints: `POST /v1/chat/completions`,
  `GET /v1/models`, `POST /v1/embeddings`, `POST /v1/images/generations`,
  `POST /v1/images/edits`
- **Anthropic-compatible** endpoints: `POST /v1/messages` and
  `POST /v1/messages/count_tokens`
- **Local Files API compatibility**: upload once through `/v1/files`, then use
  the returned `file_id` from Anthropic Messages or OpenAI Responses requests
- **OpenAI Responses API**: `POST /v1/responses` and Codex compaction at
  `POST /v1/responses/compact`
- **Provider routing** for Codex and third-party providers via a
  `provider/model` alias syntax and per-provider `/:provider/v1/...` routes

This lets you point existing OpenAI or Anthropic clients (including Claude Code)
at a local endpoint backed by GitHub Copilot.

See the [Claude Code / Anthropic API compatibility
matrix](./docs/claude-code-api-compatibility.md) for audited behaviors,
intentional divergences, and explicit scope limits.
The combined [Claude Code 2.1.209 and Codex CLI 0.144.1 compatibility
guide](./docs/claude-code-codex-compatibility.md) includes exact setup,
headers, transport evidence, failure behavior, and an opt-in local canary.

## Install / build

This is a Rust project. Build from source with Cargo:

```sh
cargo build --release
```

The binary is produced at `target/release/copilot-api` (on Windows,
`target\release\copilot-api.exe`). Run it directly or copy it onto your `PATH`.

All examples below assume `copilot-api` is on your `PATH`; otherwise invoke it
via the full path to the built binary.

## Updating

If you are running a release binary, update it in place with the built-in
self-updater:

```sh
copilot-api update          # check, then prompt before replacing the binary
copilot-api update --yes    # update immediately, no prompt (for scripts/CI)
copilot-api update --check  # report whether a newer release exists; change nothing
```

`update` queries the latest [GitHub release][releases], compares it against the
running version, and — if newer — downloads the binary for your platform and
atomically swaps the running executable. Restart `copilot-api` afterward for the
new version to take effect. You can confirm the running build at any time with
`copilot-api --version` or the `/version` endpoint.

> The self-updater fetches prebuilt release assets, so it works only on the
> platforms the [release workflow][releases] publishes (Linux x86-64, macOS
> Apple Silicon, Windows x86-64). On other targets, build from source instead.
>
> **Docker users:** don't use `update` inside a container — pull the new image
> instead (`docker compose pull` / `docker pull ghcr.io/arthur742ramos/copilot-api-rust:latest`).

> **Maintainer note:** `update` needs published GitHub releases to exist. Each
> release is built by `.github/workflows/release.yml` when a `v*` tag is pushed,
> so cut releases by tagging: `git tag v1.12.5 && git push origin v1.12.5`. The
> same tag push also publishes the crate to [crates.io][cratesio] (so
> `cargo install copilot-api` gets the new version): the `publish-crate` job
> verifies the tag matches the `Cargo.toml` version, then publishes — skipping
> cleanly if that version is already on crates.io, so re-running a release is
> safe. Bump the `version` in `Cargo.toml` before tagging, or the job fails the
> version-match check.

[releases]: https://github.com/Arthur742Ramos/copilot-api-rust/releases
[cratesio]: https://crates.io/crates/copilot-api

## Quick start

1. **Start the server:**

   ```sh
   copilot-api start
   ```

   On first run, if you are not already authenticated, this kicks off the GitHub
   device-login flow automatically: it prints a code, opens your browser to
   GitHub's device page (best-effort), and waits for you to authorize. The
   GitHub token is then stored locally and reused on subsequent runs. By default
   the server listens on `127.0.0.1:4141` (loopback only).

   On startup it prints a ready banner showing the exact base URLs to point your
   clients at.

2. **Point a client at it.** OpenAI-style clients use the base URL
   `http://localhost:4141/v1`.

### Pre-authenticating separately

If you prefer to run the GitHub login as its own step (for example before
scripting `start`), use the `auth` subcommand:

```sh
copilot-api auth
```

This runs the same device-login flow and stores your GitHub token locally,
without starting the server.

### Non-interactive auth

If you already have a GitHub token (e.g. one generated by `copilot-api auth`),
you can pass it directly and skip the interactive login:

```sh
copilot-api start -g <github-token>
```

### Launching Claude Code against the gateway

The `--claude-code` (`-c`) flag prompts you to pick a primary and a small model,
then builds a shell command that sets the right `ANTHROPIC_*` environment
variables and launches `claude`. The command is copied to your clipboard (and
printed if clipboard access fails):

```sh
copilot-api start --claude-code
```

> Tip: `--claude-code` only generates a convenience command. All models remain
> usable without it — you can also set the model IDs directly in your client
> configuration.

## Pointing clients at it

- **OpenAI clients:** set `base_url` to `http://localhost:4141/v1` (any API key
  string works unless you have configured `auth.apiKeys` — see
  [Security](#security-warning)).
- **Claude Code / Anthropic clients:** set
  `ANTHROPIC_BASE_URL=http://localhost:4141` and
  `ANTHROPIC_AUTH_TOKEN` to any non-empty value (e.g. `dummy`).
- **Codex CLI 0.144.1:** configure a custom model provider with
  `base_url = "http://localhost:4141/v1"` and `wire_api = "responses"`.
  See the [audited client guide](./docs/claude-code-codex-compatibility.md#codex-cli-01441-setup)
  for the complete `config.toml`.

## Running with Docker

A multi-stage [`Dockerfile`](./Dockerfile) builds a slim runtime image. The
server persists its GitHub token and config under `COPILOT_API_HOME` (set to
`/data` in the image and exposed as a volume), so mount a volume there to keep
state across restarts.

The image sets `COPILOT_API_HOST=0.0.0.0` so the server binds all interfaces
inside the container and Docker's published port (`-p`) is reachable. (The
binary itself defaults to `127.0.0.1`; see [Security](#security-warning).)

### With Docker Compose (recommended)

A [`docker-compose.yml`](./docker-compose.yml) is provided:

```sh
# First run: authenticate (device-code flow). State is kept in a named volume.
docker compose run --rm copilot-api auth

# Start the server (published on http://localhost:4141).
docker compose up
```

### With plain `docker run`

```sh
# Build the image
docker build -t copilot-api .

# First run: authenticate (device-code flow). Keep state in a named volume.
docker run -it --rm -v copilot-api-data:/data copilot-api auth

# Start the server, publishing the port.
docker run --rm -p 4141:4141 -v copilot-api-data:/data copilot-api
```

The container `HEALTHCHECK` probes `/readyz`, so Docker only reports the
container healthy once a Copilot token and the model list are loaded.

Configuration is available via environment variables, which is convenient in
containers (the `start` flags below have matching `COPILOT_API_*` variables):

| Variable | Equivalent flag | Description |
| -------- | --------------- | ----------- |
| `COPILOT_API_PORT` | `--port` | Port to listen on (also used by the healthcheck). |
| `COPILOT_API_HOST` | `--host` | Interface to bind. The image defaults this to `0.0.0.0`. |
| `COPILOT_API_ACCOUNT_TYPE` | `--account-type` | Account type: `individual`, `business`, `enterprise`. |
| `COPILOT_API_GITHUB_TOKEN` | `--github-token` | GitHub token for non-interactive startup. |
| `COPILOT_API_HOME` | `--api-home` | App data / token directory. |
| `COPILOT_API_OAUTH_APP` | `--oauth-app` | OAuth app identifier. |
| `COPILOT_API_ENTERPRISE_URL` | `--enterprise-url` | Enterprise URL for GitHub. |
| `COPILOT_API_LOG_FORMAT` | _(env only)_ | Set to `json` for structured JSON logs; defaults to the human-readable format. |
| `COPILOT_API_TOKEN_USAGE_RETENTION_DAYS` | _(env only)_ | Days of `token_usage_events` to retain before pruning (default `45`; `<= 0` disables pruning). |
| `COPILOT_API_FILE_MAX_BYTES` | _(env only)_ | Maximum size of one local Files API upload (default `20971520`, 20 MiB; capped below the gateway's 32 MiB request limit). |
| `COPILOT_API_FILE_MAX_OWNER_BYTES` | _(env only)_ | Maximum stored Files API bytes per API-key identity (default `536870912`, 512 MiB). |
| `COPILOT_API_FILE_MAX_OWNER_COUNT` | _(env only)_ | Maximum number of stored files per API-key identity (default and hard ceiling `1000`; lower values are accepted). |
| `COPILOT_API_FILE_RETENTION_DAYS` | _(env only)_ | Days to retain local Files API uploads (default `30`; `0` disables expiry). Expired content is removed lazily during upload/list operations. |
| `COPILOT_API_UPSTREAM_READ_TIMEOUT_SECS` | _(env only)_ | Max seconds of silence on an upstream HTTP or WebSocket stream before the connection is treated as stalled and dropped (default `120`; `0` disables the read timeout). Raise it if legitimately slow generations are being cut off mid-stream. |
| `COPILOT_API_SSE_HEARTBEAT_SECS` | _(env only)_ | Idle window (seconds) after which the proxy injects a keep-alive frame into a streaming response so long "thinking" gaps survive intermediaries (nginx/ALB) with sub-120s idle timeouts (default `15`; `0` disables heartbeats). A heartbeat is a no-op ping (Anthropic `event: ping` on `/v1/messages`, an SSE comment on `/responses`) and never affects token-usage or latency metrics. |
| `COPILOT_API_MAX_CONCURRENT_REQUESTS` | `--max-concurrent-requests` | Optional fail-fast cap on concurrent upstream-facing proxy requests. Unset (default) means unlimited; `64` is recommended for a desktop process with a 256-FD soft limit. When configured, excess requests receive a retryable `503 Service Unavailable` with `Retry-After: 1`. |
| `COPILOT_API_RATE_LIMIT_MAX_WAITERS` | _(env only)_ | In rate-limit wait mode, the maximum number of requests allowed to be queued (sleeping) at once. When the queue is full, new arrivals are rejected with `429 Too Many Requests` rather than enqueued. Unset (default) means unlimited. |
| `COPILOT_API_RATE_LIMIT_MAX_WAIT_SECS` | _(env only)_ | In rate-limit wait mode, the maximum number of seconds a request is allowed to wait in the queue. Requests whose projected wait would exceed this limit are rejected with `429 Too Many Requests`. Unset (default) means no limit. |
| `COPILOT_API_UPSTREAM_RETRY_5XX` | _(env only)_ | Set to `true` to retry transient upstream 5xx errors on **non-billable** routes (e.g. model list, version checks). Billable generation routes (`/v1/messages`, `/v1/responses`, `/v1/chat/completions`) always use a no-retry policy regardless of this setting to avoid double-billing. Default `false`. |
| `COPILOT_API_ALLOW_REMOTE_NO_KEY` | `--allow-remote-no-key` | Allow binding to a non-loopback interface without any API key configured. Without this flag (the default), starting the server on a publicly-reachable address with no authentication is a hard error. |
| `COPILOT_API_PROVIDER_ONLY` | `--provider-only` | Start the proxy in provider-only mode, routing all traffic through the named provider (e.g. `openai`). In this mode the proxy skips GitHub/Copilot token acquisition, model cache priming, and token-budget checks. `/readyz` returns immediately with `{"status":"ready","mode":"provider_only","provider":"<name>"}`. |

Example:

```sh
docker run --rm -p 8080:8080 \
  -e COPILOT_API_PORT=8080 \
  -e COPILOT_API_GITHUB_TOKEN=<github-token> \
  -e COPILOT_API_MAX_CONCURRENT_REQUESTS=64 \
  -v copilot-api-data:/data copilot-api
```

### Concurrency and file-descriptor headroom

Concurrency is unlimited by default. For long-running desktop services, set
`COPILOT_API_MAX_CONCURRENT_REQUESTS=64` (or
`--max-concurrent-requests 64`) so a client burst cannot consume every process
file descriptor. The admission check is fail-fast: it never queues excess
inbound requests, never terminates an already-admitted stream, and holds each
slot until the complete response body reaches EOF or is dropped because of an
upstream error or client disconnect. Liveness, readiness, version, metrics,
usage, and admin/control routes remain outside the cap.
The former `COPILOT_API_MAX_IN_FLIGHT` variable is still accepted as a
deprecated fallback.

The shared HTTP clients also retain at most eight idle connections per upstream
host. On Unix, server startup makes a best-effort attempt to raise the process
soft file-descriptor limit to `4096`, never exceeding the inherited hard limit.
If launchd/systemd supplies a lower hard limit, configure
`NumberOfFiles`/`LimitNOFILE` in the deployment that owns the service. OS limits
provide secondary headroom; they do not replace application-level admission.
This repository does not generate launchd service files.

For a macOS LaunchAgent, add resource limits such as:

```xml
<key>SoftResourceLimits</key>
<dict>
  <key>NumberOfFiles</key>
  <integer>4096</integer>
</dict>
<key>HardResourceLimits</key>
<dict>
  <key>NumberOfFiles</key>
  <integer>8192</integer>
</dict>
```

Reload the launchd job after editing its plist; changing `ulimit -n` in an
interactive shell does not update an already-running service.

Prometheus exports `proxy_upstream_concurrency_limit` (`0` means unlimited),
`proxy_upstream_requests_active`, and
`proxy_upstream_overload_rejections_total`.

## Logging

Logging uses the [`tracing`](https://docs.rs/tracing) ecosystem and is
controlled by environment variables:

- `RUST_LOG` sets the log filter (e.g. `RUST_LOG=debug`,
  `RUST_LOG=copilot_api=debug,hyper=warn`). When unset, the level defaults to
  `info` (or `debug` when `--verbose` / `-v` is passed).
- `COPILOT_API_LOG_FORMAT=json` switches log output from the default
  human-readable format to structured JSON lines, which is handy for shipping
  logs to a collector. Any other value keeps the default format.

## CLI reference

Global usage: `copilot-api [GLOBAL OPTIONS] <SUBCOMMAND>`

### Subcommands

| Subcommand     | Description |
| -------------- | ----------- |
| `start`        | Start the Copilot API server. |
| `auth`         | Run authentication flows without starting the server. |
| `check-usage`  | Show current GitHub Copilot usage / quota information. |
| `debug`        | Print environment, provider, and path diagnostics. Add `--json` for JSON output. |
| `doctor`       | Run a one-shot preflight over auth, providers, and config; exits non-zero on any `FAIL`. Add `--json` for JSON output. |
| `mcp`          | Start the MCP bridge server over stdio (`tool_search` + `generate_image` tools). |
| `update`       | Update copilot-api in place to the latest GitHub release. |

### Global options

These apply to every subcommand:

| Flag                       | Description |
| -------------------------- | ----------- |
| `--api-home <PATH>`        | Path to the API home directory (sets `COPILOT_API_HOME`). |
| `--oauth-app <NAME>`       | OAuth app identifier (sets `COPILOT_API_OAUTH_APP`). |
| `--enterprise-url <URL>`   | Enterprise URL for GitHub (sets `COPILOT_API_ENTERPRISE_URL`). |

### `start` flags

| Flag | Alias | Default | Description |
| ---- | ----- | ------- | ----------- |
| `--port <PORT>` | `-p` | `4141` | Port to listen on. Env: `COPILOT_API_PORT`. |
| `--host <HOST>` | `-H` | `127.0.0.1` | Interface to bind. Accepts an IP literal or `localhost`. Use `0.0.0.0` to expose on the LAN / make Docker `-p` work. Env: `COPILOT_API_HOST`. |
| `--verbose` | `-v` | `false` | Enable verbose (debug) logging. |
| `--account-type <TYPE>` | `-a` | `individual` | Account type: `individual`, `business`, or `enterprise`. Env: `COPILOT_API_ACCOUNT_TYPE`. |
| `--manual` | | `false` | Enable manual request approval. |
| `--rate-limit <SECONDS>` | `-r` | (none) | Minimum seconds between requests. |
| `--max-concurrent-requests <COUNT>` | | (unlimited) | Fail-fast cap for upstream-facing requests. Slots are held for the complete response-body/stream lifetime. Env: `COPILOT_API_MAX_CONCURRENT_REQUESTS`. |
| `--wait` | `-w` | `false` | Wait instead of erroring when the rate limit is hit. |
| `--github-token <TOKEN>` | `-g` | (none) | Provide a GitHub token directly (non-interactive). Env: `COPILOT_API_GITHUB_TOKEN`. |
| `--claude-code` | `-c` | `false` | Generate a clipboard command to launch Claude Code against the gateway. |
| `--show-token` | | `false` | Show GitHub and Copilot tokens on fetch and refresh. |
| `--proxy-env` | | `false` | Initialize the HTTP proxy from environment variables. Responses requests use HTTP instead of WebSocket so they also traverse the proxy. |

### `auth` flags

| Flag | Default | Description |
| ---- | ------- | ----------- |
| `--provider <NAME>` | `copilot` | Provider to log in with: `copilot` or `codex`. |
| `--verbose` / `-v` | `false` | Enable verbose logging. |
| `--show-token` | `false` | Show the provider access token on auth. |

### `doctor` preflight

`copilot-api doctor` runs a one-shot health check and exits non-zero when any
check reports `FAIL`, so it can gate a CI step or a deployment script:

```sh
copilot-api doctor          # human-readable summary
copilot-api doctor --json   # machine-readable report (checks, summary, exitCode)
```

Each check reports `OK | WARN | FAIL` with a short, secret-free message (tokens
and apiKeys are never printed). Checks performed:

- **Auth** — GitHub token present and usable; Copilot token obtained and fresh;
  Codex credentials present and unexpired (only when a `codex` provider is
  configured). A missing GitHub token `FAIL`s without starting the interactive
  device-code login.
- **Providers** — every enabled third-party provider is actively probed
  (`GET {baseUrl}/v1/models`). `200`/`404` is `OK`; `401`/`403` is `FAIL` (bad
  apiKey); an SSRF-blocked base URL or a connect/timeout failure is `FAIL`.
- **Config model-id drift** — `smallModel`, `messageApiWebSearchModel`,
  `imageChatModel`, and the model-keyed maps (`modelMappings` targets,
  `modelReasoningEfforts`, `extraPrompts`) are cross-checked against the model
  catalog they belong to. A dangling id silently no-ops at runtime, so it is
  reported as a `WARN` (it never fails the preflight).

The exit code is `0` when no check `FAIL`s (WARNs are advisory) and `1`
otherwise.

## MCP bridge (image generation in Claude Code)

The `mcp` subcommand runs a stdio MCP server exposing a `generate_image` tool, so
Claude Code (or any MCP host) can generate images natively — you ask in plain
language and the model calls the tool.

It uses the same Codex (Sign in with ChatGPT) backend as
`POST /v1/images/generations`, so it requires `copilot-api auth --provider codex`
first.

Register it with Claude Code once:

```sh
claude mcp add copilot-images -- copilot-api mcp
```

Then in a session, just ask: *"generate an image of a red fox in a snowy
forest"*. The tool returns two things:

- a **saved file** under `<app-data>/images/` (always reliable — you get a PNG on
  disk regardless of how the host renders it), and
- the **inline image** as MCP image content, which the host converts into a
  vision block so the model can see the result and you can iterate ("make it
  wider", "now at night").

> Note: whether the host forwards the full inline image to the model (vs.
> truncating it on its MCP output-token cap) is host-dependent; the saved file
> path is the reliable signal that generation succeeded. As with the HTTP image
> route, this rides an undocumented Codex backend and may change without notice.

## Endpoints

By default the server binds to `127.0.0.1:<port>` (loopback only). Pass
`-H/--host 0.0.0.0` to expose it on all interfaces (LAN). Routes (from
`src/server.rs`):

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET`  | `/` | Liveness check (`Server running`). |
| `GET`  | `/readyz` | Readiness probe; `200` only once a Copilot token and the model list are loaded, `503` otherwise. |
| `GET`  | `/version` | Build metadata: crate version, git SHA, and build timestamp. |
| `GET`  | `/metrics` | Prometheus metrics (text exposition). Subject to the normal API-key check: open only when `auth.apiKeys` is empty, otherwise a key is required. |
| `GET`  | `/usage-viewer`, `/usage-viewer/` | Self-contained usage dashboard (renders the `/token-usage` data). |
| `POST` | `/chat/completions`, `/v1/chat/completions` | OpenAI-compatible chat completions. |
| `GET`  | `/models`, `/v1/models` | List available models. |
| `POST` | `/embeddings`, `/v1/embeddings` | OpenAI-compatible embeddings. |
| `POST` | `/images/generations`, `/v1/images/generations` | OpenAI-compatible image generation proxied to the native Codex Images API (requires Codex / Sign in with ChatGPT credentials). |
| `POST` | `/images/edits`, `/v1/images/edits` | OpenAI-compatible multipart image edits proxied to the native Codex Images API. |
| `POST` | `/:provider/v1/images/generations`, `/:provider/v1/images/edits` | Provider-scoped image generation or edits. |
| `GET`  | `/usage` | Copilot usage data. |
| `GET`  | `/token` | Returns the live Copilot bearer token (see [Security](#security-warning)). |
| `*`    | `/token-usage`, `/token-usage/` | Token-usage subsystem routes. |
| `POST` | `/responses`, `/v1/responses` | OpenAI Responses API. |
| `POST` | `/responses/compact`, `/v1/responses/compact` | Unary Responses compaction used by Codex CLI. |
| `POST` | `/v1/messages` | Anthropic-compatible messages. |
| `POST` | `/v1/messages/count_tokens` | Anthropic token counting. |
| `GET` / `POST` | `/files`, `/v1/files` | List or upload locally stored files. Anthropic headers select Anthropic metadata; other requests use OpenAI metadata and require `purpose` on upload. |
| `GET` / `DELETE` | `/files/:id`, `/v1/files/:id` | Retrieve metadata or delete an owner-scoped local file. |
| `GET` | `/files/:id/content`, `/v1/files/:id/content` | Download local file content. |
| `GET` / `POST` | `/admin/config/model-mappings` | Read / write the model-mapping table (admin auth). |
| `GET` | `/admin/config` | Read the effective merged runtime config with all secrets stripped from `config`; presence indicators (which secrets are set, the `apiKeys` count, which providers have a key) are reported under a separate `secrets` object (admin auth). |
| `GET` / `POST` | `/admin/config/providers` | List / upsert third-party providers; `apiKey` is redacted to `apiKeySet` in responses (admin auth). |
| `GET` | `/admin/providers/health` | Actively probe every enabled third-party provider (`GET {baseUrl}/v1/models`, concurrent, 4s per-probe timeout) so a bad `apiKey` / wrong `baseUrl` / unreachable host surfaces immediately; reports per provider `reachable`, the raw HTTP `status` (distinguishes 401 vs 404 vs connect error), and `latencyMs`. Builtin copilot/codex are reported by token freshness instead of an HTTP probe. No secrets are returned (admin auth). |
| `POST` | `/admin/config/reload` | Re-read `config.json` from disk without restarting; returns a secret-redacted summary (admin auth). |
| `POST` | `/:provider/v1/messages` | Provider-routed Anthropic messages. |
| `POST` | `/:provider/v1/messages/count_tokens` | Provider-routed token counting. |
| `GET`  | `/:provider/v1/models` | Provider-routed model list. |

### Local Files API

Copilot has no upstream Files API, so uploads are kept locally under
`COPILOT_API_HOME/files` with SQLite metadata and owner-only filesystem
permissions where the platform supports them. File IDs are scoped to a stable
fingerprint of the authenticated API key (labels are not authorization
principals); when API authentication is disabled, all callers share the
`unauthenticated-local` scope.

Anthropic image/document file references and OpenAI Responses
`input_image.file_id`/`input_file.file_id` references are expanded to inline
base64 immediately before dispatch. Only the inline data reaches the selected
provider. Anthropic image references support JPEG, PNG, GIF, and WebP;
document references support PDF and plain text. `container_upload` and
provider-hosted file IDs are not supported.

## Configuration

On first run a `config.json` is created in the app data directory. By default
this is:

```
<home>/.local/share/copilot-api/config.json
```

(`<home>` is your OS home directory.) Override the directory with
`--api-home <PATH>` or the `COPILOT_API_HOME` environment variable. The GitHub
token and Codex credentials live in the same directory
(`github_token`, `codex_credentials.json`).

### Schema

Key fields (from `src/libs/config.rs`):

```jsonc
{
  "auth": {
    "apiKeys": [],          // client API keys accepted by the server (see Security)
    "adminApiKey": "..."    // auto-generated key required for /admin/* routes
  },
  "providers": {            // map of name -> provider config (the "copilot" name is reserved)
    "<name>": {
      "type": "anthropic",  // anthropic | openai-compatible | openai-responses
      "enabled": true,
      "baseUrl": "https://...",
      "apiKey": "...",
      "authType": "x-api-key", // authorization | x-api-key | oauth2 (oauth2 only for builtin codex)
      "models": { "<model-id>": { /* per-model overrides */ } },
      "adjustInputTokens": false
    }
  },
  "modelMappings": {        // map source model id -> target model id
    "<source>": "<target>"
  },
  "smallModel": "gpt-5-mini",
  "extraPrompts": { "<model>": "..." },
  "modelReasoningEfforts": { "<model>": "high" },
  "anthropicApiKey": "...",
  "dailyTokenBudget": 5000000, // reject new requests with 429 once this many
                               // tokens are recorded in the local day (omit or
                               // <= 0 to disable)
  "imageChatModel": "gpt-5.5",  // Responses model used by the MCP image tool
  "imageModel": "gpt-image-2"   // default HTTP/MCP image model
}
```

Notes:

- A provider is only used if it is enabled and has a valid `baseUrl` (and an
  `apiKey`, unless it is the builtin `codex` provider using `oauth2`).
- `adminApiKey` is generated automatically on startup if not set; it is required
  to call the `/admin/*` routes.
- `dailyTokenBudget` is a coarse spend guardrail: usage is recorded after each
  response completes, so enforcement gates on cumulative spend so far and can
  overshoot by at most the requests already in flight when the cap is crossed.
- `POST /v1/images/generations` and `/v1/images/edits` proxy the native Codex
  Images API using your Codex (Sign in with ChatGPT) credentials, so they require
  `copilot-api auth --provider codex`. Generation requests default `model` to
  `imageModel` when omitted; edits preserve the incoming multipart content type
  and bytes. Query parameters, compatible request headers, and the upstream
  response contract are preserved. Both routes use the same rate-limit /
  `dailyTokenBudget` admission gates and record upstream image usage when the
  response is small enough to inspect. The MCP `generate_image` tool still uses
  `imageChatModel` plus `imageModel` through Responses because it needs to save
  the returned image locally. These Codex endpoints are undocumented and may
  change without notice.

### Exact token counts for Claude models

Set the top-level `anthropicApiKey` field (or the `ANTHROPIC_API_KEY`
environment variable) to your Anthropic API key to get **exact** token counts
for Claude models on `POST /v1/messages/count_tokens`. With it set, the gateway
forwards count requests to Anthropic's free `count_tokens` endpoint, so Claude
Code's context-window bar and auto-compact thresholds are accurate.

Without it, counts are **estimated** with a tokenizer approximation, which is
slightly off. The key is used **only** for the `count_tokens` endpoint and is
never used for generation — generation always goes through GitHub Copilot.

### `provider/model` alias routing

Any model ID containing a `/` is parsed as `provider/model`. For example,
requesting the model `myprovider/claude-3.5-sonnet` routes the request to the
provider named `myprovider` (from `providers` in `config.json`) using the model
`claude-3.5-sonnet`. The part before the first `/` is the provider name; the
rest is the model ID passed to that provider. The name `copilot` is reserved.

## Security warning

> **By default, the server is UNAUTHENTICATED.**

The generated `config.json` ships with `auth.apiKeys` set to an empty list.
When `auth.apiKeys` is empty, the general auth layer allows **every** request
through with no API key. That means anyone who can reach the listening port can:

- use your GitHub Copilot subscription through the gateway, and
- read your **live GitHub Copilot bearer token** via `GET /token`.

By default the server binds to loopback (`127.0.0.1`), so it is only reachable
from the same machine. If you pass `-H 0.0.0.0` (or another non-loopback host —
as the Docker image does), it becomes reachable from other machines on your
network; in that case configure `auth.apiKeys` and/or a firewall. The server
logs a warning whenever it binds to a non-loopback interface.

If you are on a shared or untrusted machine/network:

- Set `auth.apiKeys` to one or more non-empty strings in `config.json`. Clients
  must then send a matching key via the `x-api-key` header or an
  `Authorization: Bearer <key>` header.
- Restrict access to the port (firewall, or only expose it on `localhost`).

The `/admin/*` routes are separately protected by `auth.adminApiKey`, which is
always required regardless of `auth.apiKeys`.

## Acknowledgements

This project is a Rust port of
[`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api)
(npm: `@jeffreycao/copilot-api`). All credit for the original design and
behavior belongs to its authors and contributors. See [`NOTICE.md`](./NOTICE.md)
for full attribution.

## Disclaimer

This is unofficial community software. It is **not affiliated with or endorsed
by GitHub, Microsoft, OpenAI, or Anthropic.** Using it to access GitHub Copilot
through non-official clients may violate GitHub Copilot's Terms of Service and
could put your account at risk. Use it responsibly and at your own risk — see
[`NOTICE.md`](./NOTICE.md) for the GitHub Copilot security notice.

## License

[MIT](./LICENSE) © 2026 Arthur Freitas Ramos
