# Operator checklist — verifying SystemContext (pre-logon control) on a Windows host

How to confirm a perMachine + SystemContext host can be remote-controlled
**pre-logon** (at the Windows logon / lock screen with no user signed in).
The fixes this verifies landed in rc.51/rc.52 — any current release includes
them; the per-step "rc.52" anchors below identify *which* behaviour is being
checked, not a version you should install. The easiest way to set up such a
host today is the `roomler-setup` wizard's **Daemon — system** role
([installation.md](installation.md)).

This procedure assumes you have remote-shell access to the controlled
host (a field host). All PowerShell snippets must run from an
**elevated** prompt (Run as administrator).

---

## 0. Background: what the fix does

- **rc.51** — supervisor's `consecutive_failures` counter now climbs on a
  crash-loop; backoff escalates `2 → 4 → 8 → 16 → 32 → 60 s` instead of
  being pinned at 2 s. A throttled `ERROR` alarm fires at ≥8 failures.
  Infinite respawn is retained (a logon / config-fix recovers the host).
- **rc.52** — the SystemContext worker reads its config from a
  **machine-global** location (`%PROGRAMDATA%\roomler\roomler\config.toml`),
  which is reachable by LocalSystem **before any user logs in**. The
  parent dir gets an inheritable SYSTEM + Administrators DACL so the
  Agent JWT inside is not world-readable.
- **Self-heal** — the first healthy post-logon run of an rc.50-or-earlier
  SystemContext host copies its existing `%APPDATA%` config to
  `%PROGRAMDATA%`, preserving `machine_id`.

---

## 1. Confirm the host is on rc.52+

```powershell
& "C:\Program Files\Roomler\roomlerd.exe" --version
# expect: 0.3.0-rc.52 (or later)
```

If older, force the update (must be from an **elevated** prompt — the
auto-updater needs admin to write `%ProgramFiles%`):

```powershell
& "C:\Program Files\Roomler\roomlerd.exe" self-update
```

(If the host crash-loops too fast for the worker's in-process updater
to fire, install the rc.52 perMachine MSI from the GitHub release
manually.)

---

## 2. Confirm the machine-global config exists

This is the **crux** of the fix.

```powershell
Test-Path "C:\ProgramData\roomler\roomler\config.toml"
# expect: True
```

If **False**, populate it via one of:

### 2a. Self-heal (automatic, requires one ≥5 min logged-in healthy run)

1. Log in to the host as the normal user.
2. Confirm the agent service is running and connecting:
   ```powershell
   Get-Service Roomler     # Status: Running
   ```
3. **Leave it idle for ≥5 minutes.** The agent's `clean_run_task` fires
   at the threshold; its log line is:
   ```
   config: self-healed perUser config to machine-global path
       (machine_id preserved; next boot is pre-logon-controllable)
   ```
   When the person's config has device keys, the line before it is
   `config: self-heal carried the device keys (peer-auth) to the machine-global path`.
   The keys go first (#3096), so browsers that pinned the device keep
   knowing it after the move.
4. Recheck step 2.

### 2b. Direct write via `enroll --machine-global` (no logon required)

If self-heal isn't viable (kiosk that never gets logged into, or you
can't wait), enroll directly to the machine-global path. **Get a fresh
enrollment token from the admin UI first** — this counts as a new
enrollment for that host:

```powershell
& "C:\Program Files\Roomler\roomlerd.exe" enroll `
    --server https://roomler.ai `
    --token <FRESH-ENROLLMENT-TOKEN> `
    --name <HOST-NAME> `
    --machine-global
```

A non-elevated prompt will fail with a clear message; the command
refuses to silently fall back to `%APPDATA%`.

### 2c. Manual copy (preserves machine_id, no new token needed)

If the host already has a perUser config and you don't want a fresh
enrollment, copy it across as SYSTEM (LocalSystem can write `%PROGRAMDATA%`,
your admin token can with elevation):

```powershell
$Src = "C:\Users\<the-enrolling-user>\AppData\Roaming\roomler\roomler\config\config.toml"
$Dst = "C:\ProgramData\roomler\roomler\config.toml"
New-Item -ItemType Directory -Force -Path (Split-Path $Dst) | Out-Null
Copy-Item $Src $Dst
```

`machine_id` is a stored field inside `config.toml` so a copy
preserves it; the server's `agents` row stays the same.

---

## 3. Confirm the ACL is locked down

The Agent JWT inside `config.toml` must not be world-readable.

```powershell
icacls "C:\ProgramData\roomler"
# expect ONLY entries for:
#   NT-AUTORITÄT\SYSTEM          (F)(OI)(CI)   — well-known SID S-1-5-18
#   VORDEFINIERT\Administratoren (F)(OI)(CI)   — well-known SID S-1-5-32-544
# (German Windows shows localised names; the SIDs are constant.)
# `Users` / `Authenticated Users` should NOT appear.
```

If `Users` appears, the wizard's `harden_machine_global_dir` step never
ran (typical for an auto-update path rather than a fresh wizard
install). Apply it manually, from an elevated shell, the owner FIRST:

```powershell
icacls "C:\ProgramData\roomler" /setowner "*S-1-5-32-544"
icacls "C:\ProgramData\roomler" /inheritance:r `
       /grant:r "*S-1-5-18:(OI)(CI)F" `
       /grant:r "*S-1-5-32-544:(OI)(CI)F"
```

> ⚠️ **Never `/inheritance:r` a directory you do not own, or before the
> grant** (#3073). Once inherited ACEs are gone, SYSTEM reaches an object
> only through an ACE of its own or by owning it:
> - A directory that belongs to someone else, for example the signed-in
>   user because the desktop companion created it, refuses a grant made
>   after that: `Access is denied`. It is left with an empty protected DACL
>   that SYSTEM itself cannot open.
> - A child someone else owns gets SYSTEM back only by inheriting it, and
>   inheritance cannot rewrite a child that has already lost its inherited
>   ACEs. Dropped before the grant, inheritance leaves each such child with
>   an empty DACL. Measured on 0.4.185, which took the directory first but
>   still granted after.
>
> The second command above drops inheritance and grants in one call, so
> every child inherits SYSTEM and Administrators while it can still be
> rewritten. The daemon's hardening (`crates/agent-core/src/config.rs:3501`,
> steps at `:3523`) takes the directory for Administrators, grants, and only
> then drops inheritance. The wizard's
> (`agents/roomler-setup/src/orchestrator_agent.rs:1501`) takes it, then
> runs that one call. Both stop if they cannot take the directory.

### If SYSTEM is locked out of `C:\ProgramData\roomler\roomler` (#3073)

Before the fix, a SystemContext host's first healthy run could seal this
directory. On 0.4.185 it could still seal each child in it that the user
owned. Nothing fails at that moment: the daemon keeps writing through the
handles it already has. From the next restart on (usually an update), it
can open nothing that was sealed.

| What you see | Why |
|---|---|
| The service and worker logs stop at the restart, and `roomler logs` answers `no log file for source "daemon"` | the daemon cannot create its log in `service-logs`. A daemon with the fix logs instead into SYSTEM's own profile, and its first line names the reason (`crates/agent-core/src/logging.rs:254`) |
| `icacls C:\ProgramData\roomler\roomler` lists **no** entries | the DACL is protected and empty (`icacls … /save` shows `D:PAI`) |
| a leftover `config.toml.tmp.<pid>` in that directory | the config save whose hardening sealed it could not rename its file into place |
| the directory lists SYSTEM and Administrators, but a child such as `crashes`, `service-logs` or `config.toml.prev` lists **no** entries; the log says `crash_recorder: read_dir failed … Access is denied`, or the daemon logs into SYSTEM's own profile because `service-logs` refused it | 0.4.185 hardened the directory for the first time, and dropped inheritance before it granted. Each child the user owned was left with an empty DACL. The same repair below heals it |

Repair it as SYSTEM (`roomler exec <device> -- "…"`) or from an elevated
shell. Own the top, give it back its DACL, then let each pass reach one
level deeper:

```powershell
$d = 'C:\ProgramData\roomler\roomler'
takeown /F $d /A
icacls $d /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F'
foreach ($i in 1..3) { takeown /F $d /A /R /D Y; icacls "$d\*" /reset /T /C /Q }
```

`takeown /R` stops at every directory it cannot list yet, which is why
the passes repeat. Its `/D` answer is localized: `/D J` on German Windows.
Then `roomler restart`: a running daemon only reopens its log when it
starts.

Verify a non-admin user truly cannot read it:

```powershell
# From a NON-elevated PowerShell signed in as a normal user:
Get-Content "C:\ProgramData\roomler\roomler\config.toml"
# expect: "Access to the path … is denied"
```

---

## 4. The decisive test — pre-logon control

This is what SystemContext exists for.

1. **Reboot the host.**
2. **Do NOT log in.** Leave it at the Windows lock / logon screen.
3. From your browser controller (`https://roomler.ai`), open a remote
   session to the host.
4. **You should see the Winlogon screen** and be able to type the
   password / drive the SAS sequence.

Pre-rc.52 this was impossible — the worker crash-looped because it
couldn't find a config before logon.

---

## 5. Confirm in the agent log

The service-supervisor log lives at
`C:\ProgramData\roomler\roomler\service-logs\roomlerd-service.log.<date>`
(or fetch via the browser's log viewer panel).

### Healthy rc.52 — these lines should be present:

```
config: resolved load path
    config_path=C:\ProgramData\roomler\roomler\config.toml
    is_system_context=true
    machine_global=C:\ProgramData\roomler\roomler\config.toml
supervisor: M3 A1 auto-swap (user-context -> SystemContext) is ENABLED
supervisor: spawned SYSTEM-context worker via winlogon-token
```

Then **steady** `VP9-444 DC pump heartbeat` lines once a controller
connects — no respawn churn.

### The bug is gone if you DO NOT see:

- `config: SystemContext worker but couldn't resolve active-user profile`
  (the rc.48 failure line — must be absent post-rc.52).
- `worker exited with non-zero code … backing off` repeating every
  ~2.5 s. A few during the first minute after a reboot are acceptable
  (rc.51's normal escalating backoff); a sustained stream is not.
- `supervisor: worker has failed N times in a row` (rc.51's alarm).
  If this fires, **rc.52's config fix did not take** — go back to
  step 2.

---

## 6. Quick fail triage

| Symptom in the log | Likely cause | Fix |
|---|---|---|
| `config_path = …\systemprofile\AppData\…` (NOT `%PROGRAMDATA%`) | `%PROGRAMDATA%\roomler\roomler\config.toml` is missing | redo step 2 (self-heal didn't fire, or no machine-global write yet) |
| `supervisor: worker has failed N times in a row` after a reboot, never recovering | machine-global config missing | step 2 |
| `enroll --machine-global` → "requires elevated terminal" | not running as admin | re-open PowerShell as Administrator |
| Worker starts post-logon but crash-loops pre-logon | self-heal hasn't run yet (needs one ≥5 min healthy logged-in run) OR use step 2b directly | step 2a wait, or step 2b explicit |
| `config: machine-global self-heal withheld: the device keys (peer-auth) could not be copied` | the machine-global `peer-auth\` folder is not writable by the worker | fix the DACL (step 3); the self-heal retries after the next healthy run |
| `config: the machine-global config cannot be read, so this SystemContext worker falls back` | the machine-global folder shuts SYSTEM out (#3073) | fix the DACL (step 3). Until then the worker runs on the person's config and signs with its device keys |
| Browsers say `key-changed` after the move, and the log has `peer-auth: made this enrollment's device key` | the config was promoted without its `peer-auth\` (before #3096), and nobody was signed in at its first start, so there was no person's config to take the key from | the device's identity did not change. Its owner either trusts the new key in the viewer, or puts the old key file back from the person's `peer-auth\` |
| `Users` appears in `icacls` output | rc.52 dir-DACL wasn't applied (typical for auto-update path) | run the manual `icacls` from step 3 |
| `crash_recorder: malformed sidecar moved aside` (once) | a crash sidecar torn by a power loss or OS crash, usually NUL-filled (#2955). It now sits in `crashes\malformed\`; the crash it described is lost | nothing to fix; read the file there if the crash matters |
| `crash_recorder: suppressed sidecar write … reason=HardCapReached` flooding the log | `%PROGRAMDATA%\roomler\roomler\crashes\` filled up (100 file cap) while crash-looping; uploader can't drain because worker never starts | step 2 first (fixes the loop), then `Remove-Item C:\ProgramData\roomler\roomler\crashes\*.json` to clear the backlog |

---

## 7. Auto-fail conditions

Any of these means rc.52 hasn't actually fixed this host:

- After completing step 2, the `config: resolved load path` log line
  still names a path other than `C:\ProgramData\…\config.toml`.
- After a reboot **without** logging in, the controller cannot reach
  the Winlogon screen and the supervisor log shows continued
  non-zero exits.
- A non-admin local user can read `config.toml` (token leak — step 3
  ACL is missing).
- `machine_id` differs from the server's stored row for this agent
  (run `roomlerd --config C:\ProgramData\roomler\roomler\config.toml`
  through any inspect path, OR compare the server-side `agents`
  document `machine_id` field).
