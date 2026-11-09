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
- **#3387** — the SystemContext worker runs the machine's own enrollment and
  nothing else: the machine-global config, or the SYSTEM profile's own. Until
  the machine has one that SYSTEM can open, the service runs the signed-in
  person's own enrollment, if they have one, **as that person**, the way the
  attended role does, and starts the SYSTEM worker at the next tick after one
  appears (step 2a). Nothing copies a person's `%APPDATA%` config to
  `%PROGRAMDATA%` any more; until 0.4.206 the first healthy post-logon run of an
  rc.50-or-earlier host did.
- **#3609** — the service runs that config as SYSTEM only while the files it
  loads it from are SYSTEM's and Administrators' alone: `%PROGRAMDATA%\roomler`,
  the machine folder, `config.toml` and its `.prev`, the device keys in
  `peer-auth\`, and the attempt record. Before every check it repairs what
  drifted, in-process: the service's start, the worker's start, every save of
  the config, and `keep-system-context` at every per-machine upgrade. What it
  cannot repair, an entry owned by an account that is not an administrator of
  the computer or one that is not a plain folder or file, it refuses, and the
  device runs as in step 2a until it is fixed (step 3).

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

If **False**, 2a says what the service runs until the machine has an
enrollment, and 2b or 2c gives it one.

### 2a. Until then: the person's own enrollment, as that person (#3387)

With no machine config that SYSTEM can open, the service does not start the
SYSTEM worker. It runs the signed-in person's own enrollment, if they have
one, as that person, with their standard rights. The device stays reachable
while someone is signed in, but not at the lock or sign-in screen, and not
before anyone signs in. It says so in three places:

- the service log, once: `supervisor: SystemContext is on, but SYSTEM can open
  no machine enrollment: the signed-in person's own enrollment runs as that
  person until one is written …`;
- `roomler status` on the device: a line `machine     no machine enrollment yet: …`;
- the device's row: the `sas` entry of its capabilities' `unavailable` map has the
  code `no-machine-enrollment`, and the viewer's Ctrl+Alt+Del button says why.

Nothing is copied to the machine location by itself. Write the machine's own
enrollment with 2b or 2c: the service starts the SYSTEM worker within a second
of it appearing, with no restart.

### 2b. Direct write via `enroll --machine-global` (no logon required)

The usual way, and the only one for a kiosk that is never logged into: enroll
directly to the machine-global path. **Get a fresh enrollment token from the
admin UI first** — this counts as a new enrollment for that host:

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

If the host already has a person's own config and the service should run that
same enrollment, copy it across from an elevated shell, **with its `peer-auth`
folder**. That folder holds the device keys browsers have pinned: an enrollment
that arrives without them mints new ones, and every such browser reads the
device as `key-changed`. On a machine with no machine config yet:

```powershell
$Src = "C:\Users\<the-enrolling-user>\AppData\Roaming\roomler\roomler\config"
$Dst = "C:\ProgramData\roomler\roomler"
New-Item -ItemType Directory -Force -Path "$Dst\peer-auth" | Out-Null
Copy-Item "$Src\config.toml" "$Dst\config.toml"
if (Test-Path "$Src\peer-auth") { Copy-Item "$Src\peer-auth\*" "$Dst\peer-auth\" }
```

`machine_id` is a stored field inside `config.toml` so a copy
preserves it; the server's `agents` row stays the same. Copied from an elevated
shell, the files belong to you or to Administrators; the service's next check
(within a second) gives them to Administrators and makes them readable by
SYSTEM and Administrators only (#3609). Step 3 says what to expect.

---

## 3. Confirm the ACL is locked down

The Agent JWT inside `config.toml` must not be world-readable, and nobody but
SYSTEM and Administrators may change what the service runs as SYSTEM. Since
#3609 the service keeps it so itself; this step reads what it left.

```powershell
icacls "C:\ProgramData\roomler\roomler"
# expect ONLY these two, with no (I) (inherited) entry:
#   NT-AUTORITÄT\SYSTEM          (OI)(CI)(F)   — well-known SID S-1-5-18
#   VORDEFINIERT\Administratoren (OI)(CI)(F)   — well-known SID S-1-5-32-544
icacls "C:\ProgramData\roomler\roomler\config.toml"
# expect ONLY SYSTEM (F) and Administrators (F), not inherited. The same for
# config.toml.prev, peer-auth and each <tenant id>.json in it, and
# C:\ProgramData\roomler\last-system-context-attempt.json.
icacls "C:\ProgramData\roomler"
# SYSTEM and Administrators (F), and either nothing else (Setup's seal) or what
# %ProgramData% hands down: CREATOR OWNER, and Users' read and their right to
# add files and folders. An entry for another account, if any, is (OI)(CI)(RX).
# (German Windows shows localised names; the SIDs are constant.)
```

The service repairs the tree in-process, by handle, with no `icacls` child
(`crates/agent-core/src/machine_tree.rs:429`), before every check: its own
start, the worker's start, every save of the config, and `keep-system-context`
at every per-machine upgrade:

| Entry | The repair leaves it | It never |
|---|---|---|
| `%ProgramData%\roomler` | an entry of its own for another account that lets that account change anything, cut to read and execute (`machine_tree.rs:191`). Explorer adds full control for the account that confirms its "You don't currently have permission to access this folder" prompt | turns its inheritance off: on an attended host the person's daemon stages received files below it |
| the machine folder, once it holds the machine config or a save writes one | owned by Administrators, not inheriting, SYSTEM and Administrators `(OI)(CI)` full control only. What is below it inherits that | touches a folder that holds no machine config (a waiting or attended host's) |
| `config.toml`, `.prev`, `peer-auth\` and the key files the daemon loads, the attempt record | not inheriting, SYSTEM and Administrators full control only, so an entry added to a folder above later does not reach them | — |
| any of them owned by a member of the computer's Administrators group | given to the Administrators group. The computer's own account database decides, read locally: an account of this computer directly or through another group, a domain account only when the group names it directly | takes over one owned by any other account, or by one that database does not name (a domain account that is an administrator only through a domain group, an Entra account) |

Then it checks the same entries (`machine_tree.rs:594`), all but `.prev`: each is a plain folder or file, owned
by SYSTEM or Administrators, and no other account may change it (for
`%ProgramData%\roomler`: may rename or replace what it holds). `.prev` is checked
where it is read, to recover a config that does not parse; an old one another
account owns stops nothing, and the next save replaces it. A machine config
that fails is not run: the device runs as in step 2a, and the service log says
once which entry fails and why (step 6). The service host and the worker read
it through one function (`agents/roomlerd/src/win_service/supervisor.rs:972`),
so they cannot disagree, and a SYSTEM worker never loads a config the check
refused (`agents/roomlerd/src/app.rs:1727`). `keep-system-context` reads a
record that fails as no record, so SystemContext off (`machine_tree.rs:732`).

The writers make every file they write (`machine_tree.rs:278`): the config's temp file, its
`.prev` and the record are new files, given their owner and DACL through their
handle before they take the name (`machine_tree.rs:794`). A file of that name made by anyone
before them is never written into; at worst the save fails and says so.

**Reading the logs:** use `roomler logs` or an elevated shell. Opening these
folders in Explorer and confirming its prompt adds an entry for your account
again; the next check cuts it, and the files' own permissions keep it off the
config meanwhile.

**When the check refuses an entry**, fix that entry from an elevated shell; the
service's next check (within a second while it waits) repairs the rest and
starts the SYSTEM worker:

| The log's reason | Fix |
|---|---|
| `… is owned by an account other than SYSTEM or Administrators`, or `… is left as it is: it is owned by an account that is not an administrator of this computer` | `takeown /F "<the entry>" /A`, for a folder with `/R /D Y` (`/D J` on German Windows) |
| `… is a link (a junction or a symbolic link), not a plain folder` or `file` | move it away and put a plain folder or file in its place (step 2b or 2c for the config) |
| `… lets an account other than SYSTEM or Administrators change it` | the repair is off (`ROOMLERD_MACHINE_TREE_REPAIR=0`), or this process may not change the entry; `icacls "<the entry>" /reset` from an elevated shell, or switch the repair back on |
| `C:\ProgramData\roomler lets an account other than SYSTEM or Administrators rename or replace what it holds` | an entry the folder inherits, which the repair leaves: a person's daemon made the folder, so the creator's entry `%ProgramData%` hands down names that person | once the folder is Administrators' (`takeown /F C:\ProgramData\roomler /A`), `icacls C:\ProgramData\roomler /reset` |

**Switches**, in the `Roomler` service's `Environment`
(`roomlerd set-service-env-var`): `ROOMLERD_MACHINE_TREE_REPAIR=0` stops the
repair; `ROOMLERD_MACHINE_TREE_CHECK=0` makes the check log only, and the
config runs as before #3609. With the repair off and the check on, a device
whose permissions drifted runs as in step 2a. `keep-system-context` runs from
the installer and does not read the service's `Environment`: there, both are on.

> ⚠️ **Repairing by hand: never `/inheritance:r` a directory you do not own,
> or before the grant** (#3073). Once inherited ACEs are gone, SYSTEM reaches
> an object only through an ACE of its own or by owning it:
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
> The service's repair gives each folder its new DACL in one write, before its
> owner, so SYSTEM keeps every entry it reached before; written in one call with
> the owner, the new DACL did not reach the children another account owns
> (measured on #3609's tests). A file gets both in one call. The wizard
> (`agents/roomler-setup/src/orchestrator_agent.rs:1728`)
> takes `%ProgramData%\roomler` for Administrators, then drops its inheritance
> and grants in one `icacls` call, and stops if it cannot take it.

Until #3609 each save of this config ran four `icacls` steps, which removed
only `Users` and `Authenticated Users` and so kept any other account's entry
for good, and ran them on every save of a folder that held one (#3291: on a
host kept busy by higher-priority work each waited for the load to end, 18.6 s
for `/setowner` on a 4-vCPU guest). The repair replaced them: in-process, and
on a tree already as it should be it changes nothing.

### If SYSTEM is locked out of `C:\ProgramData\roomler\roomler` (#3073)

Before the fix, a SystemContext host's first healthy run could seal this
directory. On 0.4.185 it could still seal each child in it that the user
owned. Nothing fails at that moment: the daemon keeps writing through the
handles it already has. From the next restart on (usually an update), it
can open nothing that was sealed.

| What you see | Why |
|---|---|
| The service and worker logs stop at the restart, and `roomler logs` answers `no log file for source "daemon"` | the daemon cannot create its log in `service-logs`. A daemon with the fix logs instead into SYSTEM's own profile, and its first line names the reason (`crates/agent-core/src/logging.rs:305`) |
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
| `config_path = …\systemprofile\AppData\…` (NOT `%PROGRAMDATA%`) | `%PROGRAMDATA%\roomler\roomler\config.toml` does not open, and the SYSTEM profile holds an enrollment of its own (`roomlerd enroll` run as SYSTEM without `--machine-global`) | step 2, if the machine's enrollment belongs at the machine location |
| `supervisor: worker has failed N times in a row` after a reboot, never recovering | machine-global config missing | step 2 |
| `enroll --machine-global` → "requires elevated terminal" | not running as admin | re-open PowerShell as Administrator |
| `supervisor: SystemContext is on, but SYSTEM can open no machine enrollment`; the device is reachable only while someone is signed in | no machine config that SYSTEM can open, so the service runs the signed-in person's own enrollment as that person (step 2a) | step 2b or 2c; when the file is there, the #3073 repair under step 3 |
| `config: the machine-global config cannot be read, so this SystemContext worker cannot run it` | the machine-global folder shuts SYSTEM out (#3073) | the repair under step 3. Until then the service runs the signed-in person's own enrollment as that person (step 2a) |
| Browsers say `key-changed` after a move to the machine location, and the log has `peer-auth: made this enrollment's device key` | the enrollment arrived without its `peer-auth\` folder (step 2c), or with keys this build cannot read | the device's identity did not change. Its owner either trusts the new key in the viewer, or puts the old key files back from the person's `peer-auth\` and restarts the service |
| `Users` appears in `icacls` output for the machine folder or the config | the folder holds no machine config yet (a waiting or attended host, step 2a), or the repair is switched off (`ROOMLERD_MACHINE_TREE_REPAIR=0`). On `C:\ProgramData\roomler` itself, what `%ProgramData%` hands down is expected | step 2; once the config is there, the service's next check repairs the rest (step 3) |
| `machine tree: the machine enrollment is not private, so the service does not run it as SYSTEM …`, with `reason=` naming an entry; the device is reachable only while someone is signed in | that entry fails the check (#3609): owned by an account other than SYSTEM or Administrators, not a plain folder or file, or changeable by another account | step 3, "When the check refuses an entry". The service starts the SYSTEM worker within a second of the check passing |
| `machine tree: not repaired` with `line=… is left as it is: it is owned by an account that is not an administrator of this computer` | the repair never takes over such an entry; the check then refuses it | the same |
| `machine tree: repaired` lines, once each, at the first start after an update | the entries had drifted: an entry for another account (Explorer's folder prompt adds one), or permissions inherited from above | nothing to do; the device keeps running as before |
| `the machine config at … is not run as SYSTEM: …` as a worker's last line | the tree changed between the host's check and the worker's start | the host runs the device as in step 2a; step 3 |
| `keep-system-context: cannot read …last-system-context-attempt.json (…); treating it as off` after an update, and SystemContext is off | the record fails the check (owned by another account) | step 3 for the record, then `roomlerd enable-system-context` from an elevated shell |
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
