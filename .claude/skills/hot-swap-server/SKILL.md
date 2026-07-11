---
name: hot-swap-server
description: Deploy a freshly built copilot-api release binary while the proxy on port 4141 is actively serving the running Claude Code session (dogfooding), without dropping the model connection. Use when asked to "dogfood the new build", swap the running server to the latest code, or deploy a rebuilt binary in place.
user-invocable: true
allowed-tools:
  - Read
  - Bash
  - Bash(git *)
  - Bash(curl *)
---

# hot-swap-server — swap the running proxy to a new build without cutting the session

## Why this is delicate

The `copilot-api` proxy on **port 4141** is the backend for the Claude Code
session running this skill. Your model calls route through it. Stopping 4141
severs your own connection to the user mid-task. Local shell commands keep
working (they don't go through the proxy), so the swap must be a **single
detached command** that brings the new server fully up before the old one is
gone — and rolls back if the new build fails.

## Default runtime limits

Unless the user explicitly requests different values, **every scratch launch,
production launch, and rollback launch** must use:

- **Unlimited proxy requests:** remove
  `COPILOT_API_MAX_CONCURRENT_REQUESTS` **and the deprecated
  `COPILOT_API_MAX_IN_FLIGHT`** from the child environment, and do not pass
  `--max-concurrent-requests`.
- **4096 soft file descriptors on POSIX:** run `ulimit -Sn 4096` in the exact
  shell that `exec`s the server. Fail the launch rather than silently continuing
  if the hard limit is below 4096.

Do not inherit a stale concurrency limit from the agent environment. Apply the
same policy to the scratch probe and rollback so validation matches production.
Windows has no POSIX `RLIMIT_NOFILE`; the 4096 rule is not applicable there,
but the unlimited-request rule still is.

Cargo note: use the native shell for the host. On Windows, `cargo` may not be on
the Bash tool's PATH; run it via PowerShell, prepending `~/.cargo/bin` to
`$env:Path` if needed.

## Procedure

### 1. Confirm what's running and that you're on the intended commit

```bash
# macOS/Linux:
pid=$(lsof -nP -t -iTCP:4141 -sTCP:LISTEN | head -1)
ps -p "$pid" -o pid=,ppid=,command=
printf 'shell soft/hard FD limit: '; ulimit -Sn; ulimit -Hn
```

```
# What is actually serving 4141 (PowerShell):
Get-NetTCPConnection -LocalPort 4141 -State Listen | ForEach-Object {
  Get-CimInstance Win32_Process -Filter "ProcessId=$($_.OwningProcess)" |
    Select-Object ProcessId, CommandLine
}
```
```
git rev-parse --short HEAD          # the commit you intend to ship
curl -s http://127.0.0.1:4141/version   # what's live now (git_sha + build_timestamp)
```
If `/version`'s `git_sha` already matches HEAD, there's nothing to swap.
When the working tree contains the intended uncommitted changes, compare
`build_timestamp` too: `git_sha` alone cannot distinguish two builds of the
same commit.

### 2. Preserve the old binary, then build the new one

On Windows a *running* `.exe` can be renamed (not overwritten) — the live
process keeps its handle. This frees the path for a fresh build and gives you a
rollback artifact.

```bash
# macOS/Linux. Preserve the exact executable currently serving 4141, which may
# be ~/.cargo/bin/copilot-api rather than target/release/copilot-api.
live_exe=$(lsof -nP -a -p "$pid" -d txt -Fn | sed -n 's/^n//p' | head -1)
cp "$live_exe" "${live_exe}.old"
cargo build --release --bin copilot-api
```

```
mv target/release/copilot-api.exe target/release/copilot-api.old.exe
```
```
# PowerShell:
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"; cargo build --release --bin copilot-api
```

### 3. Validate the new binary on a SCRATCH port first (non-disruptive)

Never trust an unvalidated binary on 4141. Launch it detached on 4142, poll
`/readyz`, confirm `/version`, then stop it. The old server on 4141 is untouched
throughout.

```bash
# macOS/Linux: enforce the production defaults on the scratch process too.
(
  set -eu
  ulimit -Sn 4096
  unset COPILOT_API_MAX_CONCURRENT_REQUESTS COPILOT_API_MAX_IN_FLIGHT
  target/release/copilot-api start --port 4142 >target/release/scratch.log 2>&1 &
  p=$!
  trap 'kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true' EXIT
  ok=0
  for _ in $(seq 1 40); do
    sleep 1
    if curl -fsS http://127.0.0.1:4142/readyz >/dev/null; then ok=1; break; fi
  done
  [ "$ok" -eq 1 ] || { echo "PROBE FAILED"; exit 1; }
  curl -fsS http://127.0.0.1:4142/version
)
```

```
# PowerShell: start on 4142, wait for ready, check version, stop.
$env:COPILOT_API_MAX_CONCURRENT_REQUESTS = $null
$env:COPILOT_API_MAX_IN_FLIGHT = $null
$exe = "Q:\src\copilot-api-rust\target\release\copilot-api.exe"
$p = Start-Process $exe -ArgumentList "start","--port","4142" -PassThru -WindowStyle Hidden
$ok = $false
for ($i=0; $i -lt 40; $i++) { Start-Sleep 1; try { if ((Invoke-WebRequest http://127.0.0.1:4142/readyz -UseBasicParsing -TimeoutSec 2).StatusCode -eq 200) { $ok=$true; break } } catch {} }
if ($ok) { (Invoke-WebRequest http://127.0.0.1:4142/version -UseBasicParsing).Content } else { "PROBE FAILED" }
Stop-Process -Id $p.Id -Force
```
If the probe fails, **stop here** — do not swap. Restore the old binary name
(`mv copilot-api.old.exe copilot-api.exe`) and investigate.

### 4. Atomic detached swap with auto-rollback

Run this as a **background/detached** command so it completes even if your model
connection blips while 4141 restarts. It stops the old server, starts the new
one on 4141, waits for `/readyz`, and rolls back to the old binary if the new
one doesn't come up.

```bash
# macOS/Linux: write this to a script and launch that script detached. Resolve
# all paths and the old PID before detaching. The script must log the effective
# defaults, atomically install the candidate, and restore the canonical binary
# as well as the process on rollback.
set -eu
ulimit -Sn 4096
unset COPILOT_API_MAX_CONCURRENT_REQUESTS COPILOT_API_MAX_IN_FLIGHT
echo "$(date +%T) swap start max_requests=unlimited soft_fd=$(ulimit -Sn)" >>"$log"
cp "$candidate" "${exe}.new"
chmod 755 "${exe}.new"
mv -f "${exe}.new" "$exe"
kill "$old_pid"
for _ in $(seq 1 20); do
  kill -0 "$old_pid" 2>/dev/null || break
  sleep 0.1
done
"$exe" start --port 4141 >>"$server_log" 2>&1 &
p=$!
ok=0
for _ in $(seq 1 45); do
  sleep 1
  if curl -fsS http://127.0.0.1:4141/readyz >/dev/null; then ok=1; break; fi
done
if [ "$ok" -eq 1 ]; then
  echo "$(date +%T) READY: $(curl -fsS http://127.0.0.1:4141/version)" >>"$log"
else
  echo "$(date +%T) NEW BUILD FAILED - rolling back" >>"$log"
  kill "$p" 2>/dev/null || true
  cp "$old" "${exe}.rollback"
  mv -f "${exe}.rollback" "$exe"
  "$exe" start --port 4141 >>"$server_log" 2>&1 &
fi
```

```powershell
$dir = "Q:\src\copilot-api-rust\target\release"
$exe = "$dir\copilot-api.exe"; $old = "$dir\copilot-api.old.exe"; $log = "$dir\swap.log"
function L($m){ "$([DateTime]::Now.ToString('HH:mm:ss')) $m" | Out-File -Append $log }
$env:COPILOT_API_MAX_CONCURRENT_REQUESTS = $null
$env:COPILOT_API_MAX_IN_FLIGHT = $null
L "swap start max_requests=unlimited"
Get-NetTCPConnection -LocalPort 4141 -State Listen -EA SilentlyContinue |
  ForEach-Object { Stop-Process -Id $_.OwningProcess -Force -EA SilentlyContinue }
Start-Sleep 2
$p = Start-Process $exe -ArgumentList "start","--port","4141" -PassThru -WindowStyle Hidden
$ok = $false
for ($i=0; $i -lt 45; $i++) { Start-Sleep 1; try { if ((Invoke-WebRequest http://127.0.0.1:4141/readyz -UseBasicParsing -TimeoutSec 2).StatusCode -eq 200) { $ok=$true; break } } catch {} }
if ($ok) { L ("READY: " + (Invoke-WebRequest http://127.0.0.1:4141/version -UseBasicParsing).Content) }
else {
  L "NEW BUILD FAILED — rolling back"
  Stop-Process -Id $p.Id -Force -EA SilentlyContinue
  if (Test-Path $old) { Start-Process $old -ArgumentList "start","--port","4141" -WindowStyle Hidden; L "rolled back to old binary" }
}
```

Launch it detached (e.g. the Bash tool's `run_in_background`, or
`powershell -File swap.ps1` backgrounded).

### 5. Verify and clean up

```
cat target/release/swap.log            # expect "READY: ...git_sha=<HEAD>"
curl -s http://127.0.0.1:4141/version  # confirm the new git_sha is live
curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:4141/readyz   # 200
```
Also confirm the swap log says `max_requests=unlimited soft_fd=4096` on POSIX
(`max_requests=unlimited` on Windows).
Your next reply flows through the new server — that's the proof it worked. Keep
`copilot-api.old.exe` as a rollback until the new build is confirmed healthy,
then remove it.

## Notes

- This swaps the *manually launched* server. If 4141 is managed by another
  supervisor/script, that path picks up the new `copilot-api.exe` on its next
  restart too (same file).
- The new binary reuses the cached GitHub/Copilot credentials under
  `COPILOT_API_HOME`, so `/readyz` should reach 200 in ~1s with no re-auth.
