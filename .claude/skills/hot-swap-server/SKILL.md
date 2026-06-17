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

Cargo note: on this Windows-primary repo `cargo` may not be on the Bash tool's
PATH. Run cargo via PowerShell, prepending `~/.cargo/bin` to `$env:Path` if
needed. Use Bash for `git`/`curl`.

## Procedure

### 1. Confirm what's running and that you're on the intended commit

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

### 2. Preserve the old binary, then build the new one

On Windows a *running* `.exe` can be renamed (not overwritten) — the live
process keeps its handle. This frees the path for a fresh build and gives you a
rollback artifact.

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

```
# PowerShell: start on 4142, wait for ready, check version, stop.
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

```powershell
$dir = "Q:\src\copilot-api-rust\target\release"
$exe = "$dir\copilot-api.exe"; $old = "$dir\copilot-api.old.exe"; $log = "$dir\swap.log"
function L($m){ "$([DateTime]::Now.ToString('HH:mm:ss')) $m" | Out-File -Append $log }
L "swap start"
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
Your next reply flows through the new server — that's the proof it worked. Keep
`copilot-api.old.exe` as a rollback until the new build is confirmed healthy,
then remove it.

## Notes

- This swaps the *manually launched* server. If 4141 is managed by another
  supervisor/script, that path picks up the new `copilot-api.exe` on its next
  restart too (same file).
- The new binary reuses the cached GitHub/Copilot credentials under
  `COPILOT_API_HOME`, so `/readyz` should reach 200 in ~1s with no re-auth.
