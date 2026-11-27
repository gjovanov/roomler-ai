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
  device runs as in step 2a until it is fixed (step 3). Setup's machine write,
  which an administrator's process always makes (an elevated Setup's own, or
  for one started without elevation the step that ran the installer behind its
  one prompt), and step 2b from an elevated prompt take the machine folder and
  `%PROGRAMDATA%\roomler` for Administrators before they write, and carry an
  existing machine config over only when it is private to the computer's
  administrators: one that is not is set aside, and a fresh one is written in
  its place.
- **#4176** — a machine `config.toml` that went missing is restored from its
  `config.toml.prev` when that copy passes every check the config itself
  would, as found and as it is, and holds an enrollment: the service starts the
  SYSTEM worker for it, and the worker's start restores the file (step 2d). A
  SYSTEM-profile config that opens runs ahead of the copy. A copy that fails is
  never restored: the device runs as in step 2a, and the service log says why
  once.
- **#4324** — a worker with no config to run (exit 78: no enrollment, nor a copy
  of one to restore) does not count toward rc.51's alarm and never raises it.
  The service log names the way forward instead, in one `WARN` at the first
  such exit, and again only when the exit changes (§6). Every other exit keeps
  the alarm as it was.
- **#4411** — while the service waits for a machine enrollment (step 2a), that
  line names the machine's own enrollment first and the signed-in person's as
  the other way. Before, it named only the person's.

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
enrollment, and 2b or 2c gives it one. If `config.toml.prev` is there, read 2d
first: the service restores the file from it by itself when it may.

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

When the person signed in has no enrollment either, their worker ends at once
(exit 78), and the service log names the way forward once: the machine's own
enrollment first (the setup wizard, or `roomlerd enroll --machine-global`), and
that person's own as the other way (#4411; the whole line is in §6).

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

From an elevated prompt it first takes the machine folder and its parent,
`C:\ProgramData\roomler`, for Administrators (#3609,
`crates/agent-core/src/config.rs:4907`): the folder's owner and nothing else,
and for the parent its owner and what it inherits derived again for that
owner (`crates/agent-core/src/machine_tree.rs:998`). A person's daemon may have
made both to stage received files before the service ever ran, and the
service never takes over an entry another account owns, so without this the
device would keep waiting (2a). What that person made below them stays theirs.
A config already at the path is carried over (its `machine_id`, mesh and
settings) only when it is private to the computer's administrators once the
service's repair has run (`crates/agent-core/src/machine_tree.rs:1292`), and
(#3666) only when it was the machine's **as found**, read before that repair
changes anything: `C:\ProgramData\roomler` gave no account other than an
administrator a right to replace the machine folder, and the folder and the
config gave none a right to add to or change them, their owners included. One
that is not is set aside: the command says so, writes a fresh config in its
place with no `.prev` and no key this build does not know kept from the old
files, and the device's earlier settings do not carry over. Setup's
background-service role does the same, from the step that runs the installer
when Setup itself was started without elevation
(`agents/roomler-setup/src/machine_write.rs:477`), and reads the config before
it seals `C:\ProgramData\roomler`. `roomlerd re-enroll` and `roomlerd org`
refuse such a config, with nothing read from it or sent.

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

A copy does not take the machine folder itself, as Setup and 2b do. If another
account owns it (a person's daemon made it to stage received files, and the
service log says `C:\ProgramData\roomler\roomler is owned by an account other
than SYSTEM or Administrators`), take it first, the folder alone:
`takeown /F C:\ProgramData\roomler\roomler /A`.

### 2d. A machine config that went missing beside its copy (#4176)

Every save of the machine config keeps the outgoing one as `config.toml.prev`
in the same folder. When `config.toml` is missing and that copy is there, the
service restores the file from it by itself, with no command and no restart,
when the copy passes everything `config.toml` itself would (step 3):

- as found, before the repair changes anything (#3666): `C:\ProgramData\roomler`
  gave no account other than an administrator a right to replace the machine
  folder, and the machine folder, the copy, `peer-auth\` and each key file the
  daemon loads gave none a right to add to or change them, inherited entries
  counted. Until all of that holds, the repair leaves the tree as it found it
  while only the copy is there (step 3), and the check refuses the copy;
- the check: `C:\ProgramData\roomler`, the machine folder, the copy,
  `peer-auth\` and each key file the daemon loads are SYSTEM's and
  Administrators' alone;
- it loads, and it holds an enrollment.

The order is the worker's ladder: the machine config, then the SYSTEM
profile's own config, then the copy. A host with a SYSTEM-profile config that
opens runs that config, and nothing is restored. The service host, the SYSTEM
worker's start and the restore read the copy through one function
(`crates/agent-core/src/machine_tree.rs:1175`), asked by the host after the two
configs (`agents/roomlerd/src/win_service/supervisor.rs:990`), so they cannot
disagree. Within a second of finding such a copy the service starts the SYSTEM
worker, and its log has `config missing — RESTORED from the previous good copy;
the live file was not there`: an ERROR, because the device now runs on an
older config and that must never look like a normal boot. The restored file is
written from the bytes the check judged, sealed as any save seals it, and put
in place only where no `config.toml` is: a config an elevated save writes in
the meantime is loaded instead. The next start counts no crash (#4157).

A copy that fails any of them is never restored, and
`ROOMLERD_MACHINE_TREE_CHECK=0` does not change that. The device runs as in 2a,
and the service log says why once: `config: the machine config is missing, and
its copy is not restored: …`, with `reason=` naming the entry, or the copy's own
case (it does not load, it holds no enrollment). Fix that entry as step 3 says,
or write the machine's enrollment again with 2b. To make a device forget its
machine enrollment, remove `config.toml.prev` with `config.toml`; the desktop's
"Two configurations found" clean-up sets both aside.

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
(`crates/agent-core/src/machine_tree.rs:640`), before every check: its own
start, the worker's start, every save of the config, and `keep-system-context`
at every per-machine upgrade:

| Entry | The repair leaves it | It never |
|---|---|---|
| `%ProgramData%\roomler` | an entry of its own for another account that lets that account change anything, cut to read and execute (`machine_tree.rs:329`). Explorer adds full control for the account that confirms its "You don't currently have permission to access this folder" prompt | turns its inheritance off: on an attended host the person's daemon stages received files below it. #3666: while the machine folder holds a config (#4176: or, with none, its `.prev`) and no save is writing one, it never cuts an entry that lets an account other than an administrator replace the folder (delete what the parent holds, delete the parent, change its permissions or owner); the check refuses the parent until an elevated save cuts it |
| the machine folder, once it holds a config that was the machine's as found with the folder, `peer-auth\` and its key files (#3666; #4176: with no config, a `.prev` that was), or a save writes one | owned by Administrators, not inheriting, SYSTEM and Administrators `(OI)(CI)` full control only. What is below it inherits that | touches a folder that holds no machine config (a waiting or attended host's), or one whose config was not the machine's as found: sealed, the check would pass whatever another account put in it |
| `config.toml`; `.prev`, and outside a save `peer-auth\` and the key files the daemon loads, only when that entry itself gave no account other than an administrator a right to change it as found | not inheriting, SYSTEM and Administrators full control only, so an entry added to a folder above later does not reach them | seals a `.prev` or a key file another account could have written (#3666): the check, or the recovery that reads `.prev`, refuses it instead |
| `%ProgramData%\roomler`, the machine folder and what is below it, while the folder holds a `config.toml` (#3666) | as found, unless `%ProgramData%\roomler` gave no account other than an administrator a right to replace the folder, and the `config.toml`, the folder, `peer-auth\` and each key file the daemon loads none a right to add to or change them, inherited entries counted, and a `.prev` beside the config inherited none (`machine_tree.rs:705`); only then do the rows above apply. The attempt record is repaired either way | cuts the parent or seals a folder for a config that was not the machine's as found: the check refuses that config at this tick and every one after, until an elevated save writes a closed tree. Beside a `.prev` that inherited such a right, that `.prev` is refused where it is read, and the config runs only as the check finds it. An elevated save whose config does not load does not take that `.prev` either (#4453, `crates/agent-core/src/config.rs:3853`): it writes a fresh config, and says `config: the machine config does not load, and the copy beside it is not taken …`. Whatever the config's state, an elevated save sets that `.prev` aside as `config.toml.prev.set-aside` and says `machine tree: a config.toml.prev that a start would not take is set aside` (`crates/agent-core/src/machine_tree.rs:3224`) |
| `%ProgramData%\roomler`, the machine folder and what is below it, while the folder holds no `config.toml` and holds a `.prev` (#4176) | as found, unless `%ProgramData%\roomler` gave no account other than an administrator a right to replace the folder, and the `.prev`, the folder, `peer-auth\` and each key file the daemon loads none a right to add to or change them, inherited entries counted (`machine_tree.rs:705`); only then do the rows above apply | cuts the parent or seals a folder for a `.prev` that was not the machine's as found: the check refuses that copy at this tick and every one after |
| `%ProgramData%\roomler`, the machine folder and what is below it, outside a save, while the folder holds neither a `config.toml` nor its copy (#4485) | as found, until an elevated save of the machine config: the repair changes nothing but the attempt record. When `%ProgramData%\roomler` holds an entry the first row cuts, or an administrator's own account owns it, the service log says so once: `machine tree: not repaired` with `line=… is left as it is until an elevated save of the machine config: the machine folder holds neither a config nor its copy` | changes an owner or an entry of `%ProgramData%\roomler` or of the machine folder |
| the attempt record | the same, and read for Users: Setup reads it as the person who started it, to say which stage of a failed background-service install failed | gives Users anything more than read |
| the attempt record's read at a per-machine upgrade, on a host whose machine folder holds a config, or only a `.prev` (#4176), that was not the machine's as found (the `config.toml` and `.prev` rows above), while `%ProgramData%\roomler` gives an account other than SYSTEM or Administrators a right to replace what it holds, an administrator's own account included | that right, left as it was found, so `keep-system-context` refuses the parent as it reads the record (`machine_tree.rs:1406`) and reads it as no record: the upgrade sets SystemContext off (`agents/roomlerd/src/app.rs:6441-6462`). That fails closed | cuts that right for the record's read |
| any of them owned by a member of the computer's Administrators group | given to the Administrators group. The computer's own account database decides, read locally: an account of this computer directly or through another group, a domain account only when the group names it directly | takes over one owned by any other account, or by one that database does not name (a domain account that is an administrator only through a domain group, an Entra account) |

Then it checks the same entries (`machine_tree.rs:1048`), all but `.prev`: each is a plain folder or file, owned
by SYSTEM or Administrators, and no other account may change it (for
`%ProgramData%\roomler`: may rename or replace what it holds). `.prev` is checked
where it is read, to recover a config that does not parse; an old one another
account owns stops nothing, and the next save replaces it. With no
`config.toml`, `.prev` is checked as `config.toml` itself would be, as found and
as it is, before a start restores it (step 2d). A machine config
that fails is not run: the device runs as in step 2a, and the service log says
once which entry fails and why (step 6). The service host and the worker read
it through one function (`agents/roomlerd/src/win_service/supervisor.rs:1006`),
so they cannot disagree, and a SYSTEM worker never loads a config the check
refused (`agents/roomlerd/src/app.rs:1910`). `keep-system-context` reads a
record that fails as no record, so SystemContext off (`machine_tree.rs:1380`).

The writers make every file they write (`machine_tree.rs:429`): the config's temp file, its
`.prev` and the record are new files, given their owner and DACL through their
handle before they take the name (`machine_tree.rs:1444`). A file of that name made by anyone
before them is never written into; at worst the save fails and says so. The daemon's own key
files in `peer-auth\` (a new enrollment's device key, a browser key it has not seen before),
and a `peer-auth\` it makes for them, are made sealed in the call that creates them and renamed
in by handle (`agents/roomlerd/src/peer_auth.rs:1193`, #3956): the repair finds nothing to
change on them.

**Reading the logs:** use `roomler logs` or an elevated shell. Opening these
folders in Explorer and confirming its prompt adds an entry for your account
again; the next check cuts it, and the files' own permissions keep it off the
config meanwhile. That holds for an account the computer's own account
database names an administrator. An account that is an administrator only
through a domain group (Domain Admins, an IT group) is not one here: its entry
on the machine folder, or a replace right of its on `C:\ProgramData\roomler`,
makes the config not the machine's as found (#3666), and the device runs as in
step 2a until the entry is removed and an elevated Setup or step 2b writes a
fresh config.

**When the check refuses an entry**, fix that entry from an elevated shell; the
service's next check (within a second while it waits) repairs the rest and
starts the SYSTEM worker:

| The log's reason | Fix |
|---|---|
| `… is owned by an account other than SYSTEM or Administrators`, or `… is left as it is: it is owned by an account that is not an administrator of this computer` | `takeown /F "<the entry>" /A`, for a folder with `/R /D Y` (`/D J` on German Windows) |
| the same for the machine folder, `C:\ProgramData\roomler\roomler`, or for `C:\ProgramData\roomler`: a person's daemon made it to stage received files | Setup run again (its machine write is an administrator's either way), or step 2b from an elevated shell: the save of the machine config takes both for Administrators before it writes (`crates/agent-core/src/machine_tree.rs:962`, `:998`), and its repair then seals the machine folder. What that person made below them keeps its owner. By hand: `takeown /F C:\ProgramData\roomler\roomler /A`, without `/R`, and for `C:\ProgramData\roomler` the row two below |
| `… is a link (a junction or a symbolic link), not a plain folder` or `file` | move it away and put a plain folder or file in its place (step 2b or 2c for the config) |
| `C:\ProgramData\roomler and C:\ProgramData\roomler\roomler are left as they are: the config the folder holds, with the folder, its keys and any copy beside it, was not the machine's as found (…)` (#4176: or `C:\ProgramData\roomler and C:\ProgramData\roomler\roomler are left as they are: the copy of a missing config the folder holds, with the folder and its keys, was not the machine's as found (…)`), or `C:\ProgramData\roomler lets an account other than SYSTEM or Administrators rename or replace what it holds` on a host whose folder holds a config (#3666) | another account could have put that config there, or chosen what it holds, so it is never run, folded into, re-enrolled or joined from, and the repair does not seal it. Remove that account's entry (`icacls "<the entry>" /remove:g *<SID>`), then run Setup again or step 2b from an elevated shell: the save writes a fresh config and keeps no `.prev`. `… lets an account that is not an administrator of this computer …` on `.prev` or a key file is the same entry-level case: the save replaces `.prev`, and a key file another account could write is sealed only by that elevated save. When the reason in brackets names the `.prev` beside a config, that `.prev` is refused where it is read, and the config runs only as the check finds it, until that save. A save whose config does not load does not recover from that `.prev` either (#4453): it says `config: the machine config does not load, and the copy beside it is not taken …` and writes a fresh config. Whatever the config's state, an elevated save sets that `.prev` aside as `config.toml.prev.set-aside` and says `machine tree: a config.toml.prev that a start would not take is set aside`; when it says `… is neither moved nor kept as found; the save stops`, move that `.prev` out of `C:\ProgramData\roomler\roomler` from an elevated shell, then run the save again |
| `… lets an account other than SYSTEM or Administrators change it` | the repair is off (`ROOMLERD_MACHINE_TREE_REPAIR=0`), or this process may not change the entry; `icacls "<the entry>" /reset` from an elevated shell, or switch the repair back on |
| `C:\ProgramData\roomler lets an account other than SYSTEM or Administrators rename or replace what it holds` | an entry the folder inherits, which the repair leaves (a person's daemon made the folder, so the creator's entry `%ProgramData%` hands down names that person): Setup run again, or step 2b, takes the folder and derives that entry again for Administrators (`machine_tree.rs:998`). By hand: once the folder is Administrators' (`takeown /F C:\ProgramData\roomler /A`), `icacls C:\ProgramData\roomler /reset` |

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
> (measured on #3609's tests). A file gets both in one call. Setup's machine
> write (the wizard, or the step that runs the installer for a wizard started
> without elevation, `agents/roomler-setup/src/machine_write.rs:449`) first gives
> the folder to Administrators by handle, refusing a link
> (`crates/agent-core/src/machine_tree.rs:998`). Its `icacls` step
> (`agents/roomler-setup/src/orchestrator_agent.rs:1753`)
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
| The service and worker logs stop at the restart, and `roomler logs` answers `no log file for source "daemon"` | the daemon cannot create its log in `service-logs`. A daemon with the fix logs instead into SYSTEM's own profile, and its first line names the reason (`crates/agent-core/src/logging.rs:346`) |
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
  (rc.51's normal escalating backoff); a sustained stream is not. With
  `code=78` it is a signed-in person with no enrollment, not this bug (§6).
- `supervisor: worker has failed N times in a row` (rc.51's alarm): the
  worker fails at every start, and its own last line says why.
- For a missing machine config, which no longer raises that alarm (#4324):
  step 2a's `supervisor: SystemContext is on, but SYSTEM can open no machine
  enrollment`, and, when the person signed in has no enrollment either, one
  `supervisor: the worker had no config to run (exit 78): SystemContext is on, …`,
  which names the machine's way first (§6). Either line means
  **rc.52's config fix did not take** — go back to step 2.

---

## 6. Quick fail triage

| Symptom in the log | Likely cause | Fix |
|---|---|---|
| `config_path = …\systemprofile\AppData\…` (NOT `%PROGRAMDATA%`) | `%PROGRAMDATA%\roomler\roomler\config.toml` does not open, and the SYSTEM profile holds an enrollment of its own (`roomlerd enroll` run as SYSTEM without `--machine-global`) | step 2, if the machine's enrollment belongs at the machine location |
| `supervisor: worker has failed N times in a row` after a reboot, never recovering | the worker fails at every start with a code other than 78, and its own last line says why. Since #4324 a missing machine config is not one: the service runs the signed-in person's own enrollment (step 2a), and a worker with no config to run raises no alarm (the `code=78` rows below) | the worker's last line; step 2 when it names the machine config |
| `enroll --machine-global` → "requires elevated terminal" | not running as admin | re-open PowerShell as Administrator |
| `supervisor: SystemContext is on, but SYSTEM can open no machine enrollment`; the device is reachable only while someone is signed in | no machine config that SYSTEM can open, so the service runs the signed-in person's own enrollment as that person (step 2a) | step 2b or 2c; when the file is there, the #3073 repair under step 3 |
| `config: the machine config is missing, and its copy is not restored: …`, with `reason=`, once; the device is reachable only while someone is signed in | `config.toml` is missing, and its `config.toml.prev` fails a check `config.toml` itself would pass, does not load, or holds no enrollment (#4176, step 2d) | the reason names the entry: step 3, "When the check refuses an entry", or write the machine's enrollment again with step 2b |
| `config: a machine config was written while its copy was being restored; the restore is dropped, and the config that was written is loaded` | an elevated save (Setup, step 2b) wrote `config.toml` while the SYSTEM worker restored it from its copy (#4176, step 2d): the restore never replaces a config, and the one written runs | nothing |
| `config missing — RESTORED from the previous good copy; the live file was not there` in a SYSTEM worker's log | `config.toml` was missing and its copy passed every check (#4176, step 2d): the service restored it by itself | nothing for the device; find what removed the file |
| `config: the machine-global config cannot be read, so this SystemContext worker cannot run it` | the machine-global folder shuts SYSTEM out (#3073) | the repair under step 3. Until then the service runs the signed-in person's own enrollment as that person (step 2a) |
| Browsers say `key-changed` after a move to the machine location, and the log has `peer-auth: made this enrollment's device key` | the enrollment arrived without its `peer-auth\` folder (step 2c), or with keys this build cannot read | the device's identity did not change. Its owner either trusts the new key in the viewer, or puts the old key files back from the person's `peer-auth\` and restarts the service |
| `Users` appears in `icacls` output for the machine folder or the config | the folder holds no machine config yet (a waiting or attended host, step 2a), the config in it, with the folder and its keys, was not the machine's as found (#3666: a config moved or written into a folder Users may add to is never sealed), or the repair is switched off (`ROOMLERD_MACHINE_TREE_REPAIR=0`). On `C:\ProgramData\roomler` itself, what `%ProgramData%` hands down is expected | step 2: Setup or step 2b writes the config, and its save seals the folder; a config that was there already and was not the machine's is replaced, never sealed in place (step 3) |
| `machine tree: the machine enrollment is not private, so the service does not run it as SYSTEM …`, with `reason=` naming an entry; the device is reachable only while someone is signed in | that entry fails the check (#3609): owned by an account other than SYSTEM or Administrators, not a plain folder or file, or changeable by another account | step 3, "When the check refuses an entry". The service starts the SYSTEM worker within a second of the check passing |
| `machine tree: not repaired` with `line=… is left as it is: it is owned by an account that is not an administrator of this computer` | the repair never takes over such an entry; the check then refuses it | the same |
| `machine tree: not repaired` with `line=… is left as it is until an elevated save of the machine config: the machine folder holds neither a config nor its copy` | a host with no machine enrollment yet (step 2a) whose `C:\ProgramData\roomler` holds an entry for another account (Explorer's folder prompt adds one), or belongs to an administrator's own account: outside a save the repair changes nothing but the attempt record (#4485, step 3) | nothing for the device; step 2b or 2c when it should run a machine enrollment |
| `machine tree: repaired` WARN lines, once each, at the first start after an update, or in `service-logs\roomlerd-installer.log` when one of the per-Machine MSI's own steps repaired first ([installation.md](installation.md#where-the-per-machine-msis-own-steps-log-3230-3640)) | the entries had drifted: an entry for another account (Explorer's folder prompt adds one), or permissions inherited from above. A build without #3956 also logs one at the start after its daemon wrote a key file itself (`peer-auth\<tenant>.controllers.json` after a new browser, `<tenant>.json` after a new enrollment), with nothing drifted | nothing to do; the device keeps running as before |
| `the machine config at … is not run as SYSTEM: …` as a worker's last line | the tree changed between the host's check and the worker's start | the host runs the device as in step 2a; step 3 |
| `worker exit: no config to run, so this account has no enrollment on this device yet. That is the device's state, not a crash: no crash record is written`, with `config=`, as a worker's last line, again at each start | the person signed in has no enrollment of their own (no config, and no last good copy of one that holds an enrollment, #4157), and the service starts their worker all the same (an attended per-Machine device, or step 2a): it ends at once with `no config found at …`. That is the device's state, not a fault, so since #4023 the worker records no crash for it (`agents/roomlerd/src/daemon_error.rs`, `record_worker_exit`), and since #4028 neither does the service: that worker exits `78`, not 1 (next row). Every other worker error is recorded as before, and so are two more of the device's state: the line in the row above (`agents/roomlerd/src/app.rs:1911`) and `loading config` for a config that cannot be read or parsed (`agents/roomlerd/src/app.rs:4152`) | enroll that person (Setup, or `roomlerd enroll`), or nothing: the device runs the enrollments it has. In step 2a, the machine's own enrollment first (2b or 2c), as the service's line in the next row says (#4411) |
| `supervisor: worker exited with non-zero code; backing off before respawn` with `code=78` in the service log, again at each start, and at the first of them only, `supervisor: the worker had no config to run (exit 78): …` (`WARN`) | the row above, as the service sees it (#4028): the worker it started for the signed-in person found no config, and exited with its own code, `NO_ENROLLMENT_EXIT_CODE` (`agents/roomlerd/src/daemon_error.rs:249`). The service tells that exit apart by the code alone, never by a text, and writes no `worker exit code` record for it (`should_record_supervisor_crash`, `agents/roomlerd/src/win_service/supervisor.rs:1173`). It respawns the worker exactly as after an exit 1: 2, 4, 8, 16 and 32 s, then every 60 s. Since #4324 that exit does not count toward the crash-loop alarm and never raises it (`ExitTally`, `agents/roomlerd/src/win_service/supervisor.rs:1269`). One `WARN` names the way forward at the first such exit (`:1900`), for the person's worker ``the account signed in at the console has no enrollment on this device, nor a copy of one to restore. Enroll the device as that account (the setup wizard, or `roomlerd enroll`), or put the account's config back``, and comes again only when the exit changes: another code, or a worker that ran past its start (30 s). For a worker the service started while it waits for a machine enrollment (step 2a), the line names the machine's way first (#4411): ``SystemContext is on, but SYSTEM can open no machine enrollment, so the worker runs as the account signed in at the console, and that account has no enrollment on this device either, nor a copy of one to restore. Enroll the device for the machine (the setup wizard, or `roomlerd enroll --machine-global`), or else as that account (`roomlerd enroll`), or put the account's config back``. Which of the two it says follows the state the service started that worker under, kept from the spawn (`way_forward`, `:1391`), never the state at the exit. A build before #4324 logs `supervisor: worker has failed N times in a row` with `last_exit_code=78` from the eighth such exit, then once a minute. The service's worker (`run --supervisor scm`), since #4071 the GUI worker FR-43's macOS supervisor starts on a two-row Mac (`run --supervised`), and since FR-150 P2 the per-user task's daemon (`run --supervisor task`), whose launcher then leaves, exit 78; a bare `run`, Linux, a Mac's LaunchAgent and a container still exit 1. The macOS supervisor's worker exits 78 even beside an enrolled `config.toml.prev`, because it never restores at its start (#4157): on a converted one-row Mac the person's copy can still hold the enrollment that moved to root (#4184). The other two restore a missing config from such a copy, and exit 78 only without one. A build before #4028 exits 1 here, and the service writes a `worker exit code 1` record into `crashes` for it: one per 30 s at most, up to the 100 cap | as in the row above |
| Setup or `enroll --machine-global` says `…\config.toml is set aside, because …` | the machine config that was there was not private to the computer's administrators once repaired (#3609): another account owns it, or that account may change it or the folders above it | nothing to undo: the enrollment was written as a fresh config in its place. Set again what the device had (its mesh, routes, lock policy), and fix what the reason names (step 3) |
| Setup warns `could not take the machine-global config folder for Administrators (…)` or `could not take the machine-global config directory's parent for Administrators (…)`, or `enroll` prints a `WARNING:` ending `Until the folder belongs to Administrators the service may not run this enrollment as SYSTEM.` | the save could not give the machine folder or `C:\ProgramData\roomler` to Administrators: it is not a plain folder (a link), or, for `enroll`, the prompt was not elevated | step 3's row for the machine folder |
| Setup stops with `Roomler was installed, but its administrator step could not write this device's enrollment` | Setup was started without elevation, and the step that ran the installer could not take the enrollment from it (Setup was closed, or its channel was not reached) | run Setup again: nothing was enrolled, and the token is still unused. Until then the service runs as in step 2a |
| `keep-system-context: cannot read …last-system-context-attempt.json (…); treating it as off` after an update, and SystemContext is off | the record fails the check (owned by another account) | step 3 for the record, then `roomlerd enable-system-context` from an elevated shell |
| `crash_recorder: malformed sidecar moved aside` (once) | a crash sidecar torn by a power loss or OS crash, usually NUL-filled (#2955). It now sits in `crashes\malformed\`; the crash it described is lost | nothing to fix; read the file there if the crash matters |
| `crash_recorder: suppressed sidecar write … reason=HardCapReached` flooding the log | `%PROGRAMDATA%\roomler\roomler\crashes\` filled up (100 file cap) while crash-looping; uploader can't drain because worker never starts. Before #3799 an attended host also filled it with records its person's worker had already sent but may not delete, and then recorded no crash at all | step 2 first (fixes the loop), then `Remove-Item C:\ProgramData\roomler\roomler\crashes\*.json` to clear the backlog. On a build with #3799 the service removes the records a worker sent (next row) |
| `crash_uploader: uploaded`, then `crash_uploader: uploaded; this process may not delete the sidecar in the machine's crashes folder, so its verdict is recorded and it is not sent again. The service removes it once this ledger names it`, with a `file=` in `crashes`, in a person's worker's log | expected (#3799): the server took a record of the service's, and the person's worker may not delete it there. Every record a drain sends gets two lines, in this order (#4022): what the server did, `uploaded`, or `server rejected payload (4xx is permanent)` with the status and the body; then what became of the file, once the delete was tried: `…; sidecar deleted`, `…; the sidecar was gone already` (another drainer or the service took it first), or this line. A build before #4022 said `uploaded + deleted` (`deleting` for a 4xx) before it tried, so such a record read as deleted and then as kept | nothing: the service removes it (next row) |
| `crash sweep: removed the records a drainer had sent removed=N records=[…]` in the service log | expected (#3799): a person's worker uploaded those records from `crashes` but may not delete them there, so it wrote each one's signature in its own `crash-verdicts.txt`, and the service, as SYSTEM, removed them. It sweeps before every worker start (every profile's ledger and `service-logs`') and once a minute while a person's worker runs (that person's). Only records whose exact signature, `<len>:<mtime> <digits>-<digits>.json` as the service listed them itself, is a line of a ledger are removed, by handle; nothing is sent | nothing. `crashes` holds no record a worker has sent, so a SystemContext worker or another person never sends it again |
| `crash sweep: still running; the worker starts without waiting for it` | the sweep before a worker start took longer than its bound (3 s), usually a profile on a disk that did not answer. The worker started anyway; a record another account sent may then be sent once more | read the disk the profiles are on; nothing to fix in roomler |
| `crash sweep: off (ROOMLERD_CRASH_SWEEP); a record a person's worker sent stays in crashes`, once per service start | the sweep's kill switch is set in the service's environment | remove `ROOMLERD_CRASH_SWEEP` from the service's `Environment` value and restart the service, unless it was set on purpose |
| `icacls C:\ProgramData\roomler\roomler\crashes` shows `NT AUTHORITY\SYSTEM:(OI)(CI)(F)`, `BUILTIN\Administrators:(OI)(CI)(F)` and `BUILTIN\Users:(OI)(CI)(RX)`, and no `(I)` entry | expected since FR-145: SYSTEM makes `crashes`, or seals the one an earlier build made, by handle. SYSTEM and Administrators write; Users read and list, so a worker running as the signed-in person drains the supervisor's records | nothing |
| `machine tree: is a link; moved aside whole, nothing in it deleted, and a sealed folder made in its place` (or `is not a folder`, or `is owned by another account (<SID>)`), with `path=` and `moved_to=`; a `crashes.moved-<unix>` entry beside `crashes` | something the service did not make stood where it writes: a junction or a symbolic link, a file, or a folder another account owns. The service moved that entry aside whole, by its own handle (a link is never followed), changed nothing in it, and made a sealed `crashes` in its place (FR-145) | nothing for the device. Look at what the entry holds before removing it; for a junction or a symbolic link, `cmd /c rmdir "<path>"` removes the link alone, never what it leads to (not `Remove-Item -Recurse`) |
| `crash_recorder: crash records left alone: <path> is a link (…)`, or `… ends elsewhere (…)`, once per start | a link, or a folder whose final path is elsewhere, on the way to `crashes`: the machine folder, its parent, or `ProgramData` itself. The crash is not recorded, and is never written anywhere else | step 3 for the machine folder and its parent. A `ProgramData` reached through a link is refused by design |
| `crash_recorder: crashes sealed; removed what was not a record`, with `gone=[…]`, once | the first start on a build with FR-145 sealed the `crashes` an earlier build made, and deleted by handle a record name in it that was not a plain file of SYSTEM's or Administrators' | nothing |
| `crash_recorder: ROOMLERD_MACHINE_TREE_HELD=0: crash records in the machine folder are written and read by path, not by handle` | FR-145's kill switch is set in the service's environment | remove it from the service's `Environment` value and restart the service, unless it was set on purpose |

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
