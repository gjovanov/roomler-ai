# FR-43: One macOS device row — a supervising daemon and an unenrolled GUI worker

**Status:** P0 + P1 + **P2 complete and field-verified** (0.4.33 → 0.4.43) — the daemon's row streams real pixels; P2c/P3 next. Tracking issue: `FR-43`.

⚠️ Anchors RE-VERIFIED against master `dfec6128` on 2026-09-06, when this record was
landed 27 releases after the field run. `remote_control`'s hub moved to
`crates/modules/fleet/src/hub.rs` in the interim and every citation to it was stale —
which is exactly why the convention asks for a master sha rather than a bare line number.

## Goal

A Mac appears in Devices **once**, like every other platform, while keeping both things
it can do today: remote desktop from the GUI session, and overlay mesh + SSH from boot.
One enrollment, one control WS, one row to `exec` and `ssh` against.

## Why there are two of them today, and what is actually forced

macOS is the only platform that needs **two processes**, and that half is not
negotiable:

| Plane | Needs | Where it must run |
|---|---|---|
| capture / input / clipboard | a WindowServer connection | the console user's GUI session |
| `utun` + route table + SSH | root | session 0 |

`docs/installation.md:104-113` states it; `agents/roomlerd/packaging/macos/com.roomler.agent.plist`
and `com.roomler.daemon.plist` are the two units, and both invoke the *same* binary with
the same `run` subcommand — the split is context, not code.

**Windows does not need two because a SYSTEM process can reach the interactive desktop.**
`agents/roomlerd/src/win_service/desktop.rs:1-16` documents the mechanism (a thread
attaches to `winsta0\Default` / `winsta0\Winlogon` and captures/injects there), and
`supervisor.rs:236` (`spawn_in_session`) + `:662` (`decide_spawn`) is the shape: the SCM
service supervises, the agent proper runs in a session. macOS has **no `SetThreadDesktop`
equivalent** — a session-0 daemon cannot borrow the console user's WindowServer, at all.
That is the whole asymmetry.

**What is NOT forced is two enrollments.** That comes from us: the hub keys a device's
control WS on `agent_id` and a second connection displaces the first
(`crates/modules/fleet/src/hub.rs:91`), so we gave each half its own identity and the
Mac became two rows. Cost, paid daily: every `roomler exec` / `roomler ssh` must target
the *right* row (the GUI row cannot reach root, the root row cannot see the screen), the
install needs two tokens, and Devices shows one machine twice.

## Key design — port our own Windows supervisor to launchd (P1; see the P2 correction below)

The root daemon becomes the **single enrolled identity** (control WS, overlay, SSH, from
boot); the GUI-session process becomes an **unenrolled worker** it spawns and drives.

- **Spawn**: root → `launchctl asuser <console-uid>` (or a daemon-managed LaunchAgent),
  the launchd analogue of `spawn_in_session`. Console-user changes (login, logout, fast
  user switch) drive respawn, mirroring `decide_spawn`'s session-change handling.
- **Transport**: the LocalAPI unix socket, which already solves exactly this addressing
  problem — `crates/localapi/src/lib.rs:1810-1820` documents `/var/run/roomler` as the
  root daemon's well-known socket precisely because "a per-user path cannot serve a root
  daemon … that is the trap the macOS LaunchDaemon split walks into".
- **Division**: signalling + consent + policy in the daemon; the rc session's WebRTC
  peer, capture, encode, input, clipboard and file transfer in the worker (media stays
  local to the session that can produce it — no pixels cross the socket).
- **TCC**: the worker is the same bundle at the same path, so Screen Recording and
  Accessibility grants carry. This is only safe *now*: before FR-5's stable signing
  identity, any change to how the capture process launches risked re-prompting on every
  update. `codesign -d -r-` equality is the pre-flight check (FR-5's method).

## Phases

| P | Scope | Kill switch |
|---|---|---|
| P0 | Spike: daemon spawns a GUI worker via `launchctl asuser`, worker answers a LocalAPI caps probe from the daemon. Measures the respawn latency and the TCC verdict. | n/a (spike, no ship) |
| P1 | Daemon-as-supervisor: spawn + babysit + session-change respawn. Worker still enrolled — nothing changes for the server yet. | `macos_supervise_gui_worker` (default **off** ⇒ today's two independent halves) |
| P2 | Session delegation: the daemon's WS accepts an rc session and drives the worker over LocalAPI. | per-session fallback — delegation failure re-serves from the worker's own enrollment |
| P3 | Collapse the enrollment: installer mints ONE token; existing two-row Macs migrate. | `--daemon-token` keeps working (two-row install stays reachable for a release) |

## P2 design — session delegation over LocalAPI

Anchors below verified against master `3980e79d`.

### ⚠️ First, a correction to "Key design" above: P2 has no Windows precedent

"Port our own Windows supervisor to launchd" is true of **P1 and only P1**. The Windows
SCM service is a *pure launcher*: `win_service/mod.rs:396` hands off to
`supervisor::run(worker_exe, vec!["run"], …)` and does nothing else, so the spawned worker
is the entire agent — control WS, overlay, capture, input, all in one process. Windows
never had to split the planes, because a SYSTEM process can attach to the interactive
desktop (`win_service/desktop.rs`). **There is no delegation protocol to port.** P2 is new
work, and the design below should be read as such rather than as a translation.

### Why not the Windows shape (worker enrolled, daemon only supervising)

Because it deletes the reason the macOS daemon half exists. With nobody logged in there
would be no agent at all: no overlay, no `roomler ssh`, no `roomler exec`, no presence —
exactly the "reachable before anyone logs in" property `win_service/mod.rs`'s own module
docs list as the point of service mode. So the **daemon must hold the enrollment**, and
the session has to reach the worker some other way.

### Why not "let the worker open its own WS for the session"

Tempting, and it would remove the IPC entirely. It cannot work as-is: `Hub::register_agent`
(`crates/modules/fleet/src/hub.rs:280`) is keyed on `agent_id`, and a second connection
**displaces** the first — the displaced socket is cancelled within milliseconds by design
(`hub.rs:91-96`). A worker dialling in as the same device would knock the daemon's control
WS off the air, which is precisely the login/displace/relaunch loop P1's stand-down exists
to prevent. Making it work would mean teaching the server about session-scoped secondary
connections: a server change, a new authenticated surface, and a new way for a compromised
device to hold two sockets. Not worth it to avoid a unix socket we already own.

### What actually has to cross the boundary

Of the 28 `ServerMsg` and 28 `ClientMsg` variants the agent handles today, the rc session
needs **nine**, and no media among them:

| direction | messages |
|---|---|
| daemon → worker | `SessionCreated`, `SdpOffer`, `SdpAnswer`, `Ice`, `Terminate` |
| worker → daemon | `SdpAnswer`, `Ice`, `SessionStats`, `Terminate` |

Everything else stays where it already is: `RpcExec*`, `Ssh*`, `Tunnel*`, `Config*`,
`Derp*`, `KeyRotate`, `JoinOrg` and the whole overlay are daemon-side concerns and do not
move. Input, clipboard and file transfer ride the session's **data channels**, so once the
worker owns the peer they never touch the socket either. Pixels never cross it. This is the
property that makes the whole phase affordable: the IPC carries a few hundred bytes at
session setup and then nothing.

### The transport problem, stated honestly

LocalAPI is **strictly request→response**: `serve_connection` reads newline-delimited JSON,
answers each line, and loops to EOF (`crates/localapi/src/lib.rs:1505`); `TailLog`'s
own doc calls itself "poll-based follow (no streaming)" (`lib.rs:1043`). There is no server
push, and delegation needs it in both directions — trickle ICE arrives whenever the network
decides, not when someone asks.

Three options, and the trade is latency versus a protocol exception:

1. **Long-poll** (`RcPoll` blocks until a frame is queued, `RcPush` for the reverse). Fits
   the existing protocol with zero changes. Costs a round-trip of latency on every ICE
   candidate — i.e. on the session-setup critical path — and still holds a connection open
   permanently, so it buys nothing operationally.
2. **One streaming verb** — `RcAttach`. The worker connects and sends it; from that point
   the daemon treats *that connection* as a bidirectional frame channel until EOF. A
   documented, single-verb exception to the request/response rule, confined to one match
   arm and invisible to every other caller.
3. **A second LocalAPI server in the worker**, so daemon→worker push is an ordinary
   request. No protocol change, but two dispatch surfaces to secure instead of one, a
   second socket path to agree on, and a startup ordering problem (the daemon cannot push
   before the worker's listener exists).

**Recommendation: (2).** The exception is real and should be written down rather than
disguised as polling. It keeps one socket, one ACL, one dispatch surface, and it is the
only option whose latency does not sit on the path a human perceives as "how long until my
screen appears".

⚠️ `RcAttach` must be **daemon-only**, not something any local process can call: it would
otherwise let any user-session process on the box volunteer to serve remote-control
sessions. The socket's own ACL is the trust boundary today
(`lib.rs:1810-1820` — `/var/run/roomler`, 0600), which is *not* sufficient on its own here,
because the worker is an ordinary user process and so is an attacker's. The attach must
carry the one-shot secret the daemon passed to the worker in its spawn environment, and the
daemon must accept exactly one attached worker at a time.

### Consent

Stays in the daemon, and this is not a compromise — FR-27 already built the mechanism.
`PromptSurface` (`agents/roomlerd/src/consent.rs:184`) is a chain, and its **companion**
rung is precisely "a per-user process the daemon starts on demand"
(`companion::ensure_running()`). A session-0 daemon has no native surface and correctly
falls through to it. So the daemon keeps the policy decision (`strictest_of`, the local
floor) and the prompt appears in the user's session, with no new mechanism.

### Kill switch and fallback

Per the phase table: delegation failure **re-serves from the worker's own enrollment**.
That is only meaningful while P2 and P3 are separate — during P2 the worker is still
enrolled, so a failed `RcAttach` means the daemon declines the session and the worker's own
WS serves it exactly as today. ⚠️ This makes P2 genuinely reversible and P3 genuinely not:
once the second enrollment is gone, the fallback is gone with it. P3 must therefore not
ship until P2 has field evidence, not merely CI.

### P2b-2 design — the worker serves a delegated session

Measured against master `4fa59b9c`, and the measurements are what make it small.

### The seam is the SENDER, not the message

The instinct is to route each reply as it is produced. That is not needed, and
the numbers say why:

| in the five rc arms | count |
|---|---|
| direct `send_msg(ws, …)` | **3**, all in the `SdpOffer` arm |
| `outbound_tx` uses | **1** — `AgentPeer::new(…, outbound_tx.clone(), …)` |

Everything an rc session later emits — ICE, `SessionStats`, its own
`Terminate` — leaves through the sender the **peer was constructed with**. So a
delegated session needs one decision, taken once, at peer construction: hand it
a sender whose drain wraps each `ClientMsg` in a `DelegateFrame::FromWorker`
instead of the worker's own WS queue. No per-message routing, and nothing
downstream needs to know it is delegated.

The three direct sends are the only remainder, and they take a small
`ReplySink` (`Ws(&mut …)` | `Delegate(&…)`) so the daemon path stays
byte-for-byte what it is today.

### Where the worker runs it

In the loop it already has. The worker keeps its own enrollment and WS through
P2 (that is the fallback), so `connect_once` simply gains the delegation
channel as another `select!` source; delegated messages go through the SAME
`handle_server_msg` with the SAME session state, differing only in the sender
their peer was built with. Session ids come from the daemon's row and cannot
collide with the worker's own.

### It is field-falsifiable, which P2b-1 was not

`AgentCaps.permissions` — including `no-gui-session` — is **reporting only**;
nothing server-side gates session creation on it (`models.rs:112-138` is
explicit that readers should treat it as "not a capture target"). So a
controller can open a session against the DAEMON's row today, and the test is
binary: pixels appear, or they do not.

### Order of work

1. `ReplySink` + the three `SdpOffer` sends (pure refactor, no behaviour
   change on the daemon path).
2. The delegated sender at `AgentPeer::new`.
3. The worker's `select!` arm.

⚠️ Step 1 touches the shared session-setup path, so it breaks remote desktop on
**every** platform if it is wrong — not just macOS. The integration lane
(`agent_e2e_tests` drives a full `rc:*` round-trip in-process) is the check that
actually covers it, and it does NOT run on a PR unless `crates/tests/**` is
touched: dispatch it on the branch before merging.

## Open questions for implementation

- **Session ownership on worker death.** If the worker dies mid-session the daemon holds a
  live server-side session with nothing behind it. It must `Terminate` with a reason the
  controller can act on, rather than let the controller watch a frozen screen — the same
  argument that made `rc:consent.reason` worth splitting in FR-27.
- **Does `SessionCreated` carry everything the worker needs** (permissions, TURN creds,
  relay region) or does the worker need daemon state as well? If the latter, the attach
  handshake should carry it once rather than per-session.
- **Ordering.** ICE candidates may arrive before the worker has attached. The daemon needs
  a small per-session queue, or an explicit "not ready" refusal — silently dropping them
  would produce a session that half-works, which is worse than one that fails.

## Acceptance criteria

- [ ] A fresh install with **one** token produces **one** device row, and that row serves
      both a remote-desktop session and `roomler ssh`.
- [ ] `roomler exec <mac>` reaches root-owned paths **and** the GUI session's log from the
      same row (today this needs two different rows).
- [ ] After a reboot with **nobody logged in**, the Mac is present in `roomler peers` and
      `roomler ssh` works — the capability the daemon half exists for.
- [ ] A remote-desktop session started while logged in streams real pixels and accepts
      input, with `caps` reporting `has_input_permission: true` and **no TCC re-prompt**.
- [ ] `codesign -d -r-` on the bundle is byte-identical before and after the migration.
- [ ] `kill -9` on the GUI worker respawns it within 10 s **without** the control WS
      dropping (measured: the device row never goes offline).
- [ ] An existing two-row Mac migrates to one row keeping its overlay address, and the
      retired row is tombstoned per `release_overlay_node`'s ordering (never re-issued).
- [ ] With the kill switch off, behaviour is byte-for-byte today's.

## Dead hypotheses — do not re-run these

1. **"The self-signed cert (FR-5) lets us merge the halves."** No. The cert fixed TCC
   *persistence* (a stable designated requirement across updates). The split is about
   *session access*. Orthogonal axes; merging was never gated on signing. What the cert
   *does* buy is that this refactor is now safe to attempt at all.
2. **"Windows merges them into one process, so macOS can."** Windows can only do it
   because of desktop attachment (`win_service/desktop.rs`). Absent that API, a session-0
   macOS daemon has no path to the screen — no entitlement, no TCC grant, nothing.
3. **"Run the daemon as the console user."** Loses root ⇒ no `utun`, no routes, no SSH
   server.
4. **"Run the agent as root inside the GUI session."** `/Library/LaunchAgents` jobs run
   as the console user; macOS has no root GUI agent.
5. **"Make the GUI half primary and give it a privileged helper."** Works while someone
   is logged in, and silently drops the mesh + SSH at the login window and on a headless
   reboot — a regression against what the daemon half exists for.
6. **"Pass the delegation channel as an inherited socketpair fd, so possession of the fd
   IS the authorisation."** Strictly better than a spawn-time secret — nothing to leak,
   no one-attached-worker bookkeeping, no new verb reachable by other processes, and the
   channel dies with the process. It does not survive P1's spawn chain: **`sudo` closes
   inherited descriptors**. Measured on the MacBook 2026-08-31 rather than read out of
   sudo's manual —

   | chain | fd 3 in the child |
   |---|---|
   | `launchctl asuser 501 <exe>` | **inherited** (`/etc/hosts`, 213 bytes) |
   | `launchctl asuser 501 sudo -u '#501' <exe>` | **gone** |

   Note the first row: `launchctl asuser` itself preserves fds, so this becomes available
   the day the chain drops `sudo` and drops privilege **in-process** instead — the
   machinery already exists (`RunAs::ConsoleUser`, `exec::apply_run_as`, P5). That is a
   real future simplification, but it would replace the identity mechanism that has only
   just been field-verified after costing an outage, so it is not a P2 dependency. Worth
   revisiting at P3, when the daemon owns the spawn unconditionally.

## Open decisions

- Spawn mechanism: `launchctl asuser` from the daemon vs a daemon-managed LaunchAgent
  (the postinstall already bootstraps into `gui/<uid>` — `packaging/macos/postinstall:108`).
- Migration: delete or tombstone the retired per-user row, and who initiates it.
- Whether the worker keeps a *dormant* enrollment as the P2 fallback, or is fully
  unenrolled from the start.

## Out of scope

Apple Developer ID / notarization (FR-7) · Windows and Linux, unchanged · the update half
`com.roomler.update` (FR-5), which stays a separate root unit by design · the desktop
companion.

## Field-verification log

All of it on the operator's MacBook (Apple Silicon), driven
remotely — `roomler exec` for P0, `roomler ssh` on the daemon row from P1 onwards.

### 2026-08-30 — P0 spike: the mechanism works, and one half was never measured

From the running root daemon, a capture in session 0 fails with *"could not create image
from display"*. The same call through `launchctl asuser <console-uid>` produced an 8.4 MB
screenshot in **233 ms**, and our own binary spawned that way reported both a live GUI
session and its TCC grants — so the grants survive, which is what made P1 worth building.

**What the spike got wrong, and why:** it never measured the worker's *identity*. The
probe was `roomlerd caps`, which reports the GUI session and the TCC grants — both of
which root-in-a-session genuinely has — and says nothing about *which config* the process
would load. **A probe that shares a blind spot with the design confirms the design.**
`id -u` was the missing measurement, and it cost the outage below.

### 2026-08-30 — P1 (0.4.33) shipped, then took the Mac's remote-desktop half offline

`launchctl asuser <uid> <cmd>` joins the user's Mach bootstrap namespace but does **not**
change credentials — `launchctl asuser 501 id -u` prints `0`. Every spawned worker
therefore ran as root, resolved *root's* profile config, died instantly with
`no config found at /var/root/Library/Application Support/…`, and was respawned for as
long as the LaunchAgent stayed unloaded. Nothing bounded the ladder.

Fixed in **#1026** (0.4.34): `sudo -u "#<uid>"` — numeric, so no `getpwuid` lookup and no
wrong account when a uid has several names — plus `MAX_FAST_EXITS`, because a worker that
can *never* start is a configuration fault and hammering it hides the cause.

### 2026-08-30 — 0.4.34 re-test: privilege drop verified

Worker pid 4526 at **uid 501 (the console user, not root)**, parent 4525 = `sudo -u '#501'` at uid 0;
LaunchAgent confirmed not loaded; `permissions: ["screen-capture", "input"]`.
`kill -9` → respawned in ~14 s, again uid 501. Stand-down verified separately: with the
LaunchAgent loaded, `action=LaunchdOwns` and the user half's pid never moved.

Hand-back left an **orphan**: `stop_worker` killed only the direct child, so the agent
survived re-parented to launchd, held the single-instance lock, and launchd's own worker
exited *cleanly* on it — `KeepAlive{SuccessfulExit=false}` never retried. Turning the
supervisor **off** left the Mac running an unsupervised orphan nothing would restart.
A switch whose off position leaves the machine worse than either steady state is not a
kill switch. Fixed in **#1029** (0.4.35): own process group + group signal.

### 2026-08-31 — 0.4.35 re-test: #1029 confirmed, and it uncovered the other half

Precondition asserted first (`git merge-base --is-ancestor <fix> agent-v0.4.35`), because
"merged" is not "released" and "released" is not "installed".

The group structure, measured under supervision:

<!-- RETIRED-NAME-ANCHOR(5): verbatim `ps` output from the MacBook. The macOS
     bundle is deliberately NOT renamed — its name keys the TCC grants, so
     renaming it would drop Screen Recording + Accessibility on every Mac
     (FR-21). Rewriting the capture to the current name would make it a
     transcription rather than evidence. -->
```
25894     1 25894     0  roomler-agent   <- root daemon
34322 25894 34322     0  sudo            <- our child, OWN group (process_group(0))
34323 34322 34322   501  roomler-agent   <- worker, SAME group, uid 501
```

`sudo` does not `setsid` here (a separate probe showed a shell under `sudo -u "#501"`
carrying a pgid inherited from its ancestor), so `kill(-pgid, …)` reaches the whole
`launchctl → sudo → agent` chain and cannot touch the daemon. **No orphan survived.**

But the hand-back then left **no user half at all**:

```
22:58:03.902  user half:  WARN single-instance lock held by another process; exiting   <- exit 0
22:58:08.725  daemon:     INFO macOS supervisor: stopping our GUI worker group … pgid=34322
```

launchd starts its worker the instant the plist is bootstrapped; it hits our worker's
lock and exits **0**; up to `POLL` (5 s) later we notice `LaunchdOwns` and kill ours.

**The orphan and this are the same root cause seen from opposite ends: whoever loses the
single-instance lock exits 0, and `KeepAlive{SuccessfulExit=false}` never retries a clean
exit.** #1029 fixed *which side* loses; it did not make the hand-back complete. Fixed in
**#1039**: handing back is two steps, so it is its own `Action::HandBack(uid)` — stop our
group, then `launchctl kickstart` the LaunchAgent (without `-k`, so it is idempotent).

Separate hazard seen in the same window and filed as **#1040**, not fixed here: while
losing the lock race during the update, the user half logged
`rollback installer downloaded — spawning + exiting target=agent-v0.4.33`. Lock-conflict
exits can read as a crash loop to the rollback machinery, which would *downgrade* a
healthy host. This is why #1039 deliberately did **not** take the otherwise-obvious route
of making that exit non-zero.

### 2026-08-31 — 0.4.36: hand-back complete, P1 done

Same sequence, on a build carrying #1039. Takeover clean (worker uid 501 in its own
process group); bootstrapping the LaunchAgent back left **no orphan** *and* launchd owning
exactly one user half.

The log is the result worth keeping, because the race still fires:

```
00:09:01.773  user half:  WARN single-instance lock held by another process; exiting
00:09:02.424  daemon:     INFO macOS supervisor state action=HandBack(501)
00:09:02.424  daemon:     INFO stopping our GUI worker group ... pgid=71668
00:09:12.672  daemon:     INFO handed the worker back to launchd uid=501
00:09:17.684  daemon:     INFO macOS supervisor state action=LaunchdOwns
```

launchd's worker still loses the lock race and still exits 0; the kickstart is what brings
it back. A failure condition that reproduced and was recovered from is better evidence
than one that failed to occur.

Hand-back takes ~10 s end to end (SIGTERM, grace, SIGKILL, reap, kickstart), during which
the session has no user half. Bounded and self-healing; noted, not tuned.

**P1 is complete.** The switch stays default-off until P2 gives the daemon something to
delegate to.

### 2026-09-01 — P2b: the daemon's row streams real pixels

The goal of this FR, demonstrated on `agent-v0.4.43`. A session opened against the
**root daemon's device row** — session 0, no WindowServer, never able to show a
screen — streams the real desktop and accepts input, served by the GUI worker:

```
6a96be11  delegated  hevc_videotoolbox  viewer_age_ms 26, 5, 6
6a96bed9  delegated  hevc_videotoolbox  viewer_age_ms 5, 13, 8
6a96bf00  delegated  hevc_videotoolbox  viewer_age_ms 6, 8, 10
```

5–13 ms, indistinguishable from a direct session on the user's own row.

**The negative arm came for free.** Before delegation was armed, the same row gave a
black screen — and reproduced this FR's premise exactly:

```
WARN scrap capture unavailable — falling back to NoopCapture
     error=creating scrap::Capturer: other error
```

Session established, consent auto-granted, encoder running, ICE direct: everything
worked except the picture.

**Two bugs the field found and CI could not.**

1. **The delegated session was missing everything `Request` resolved** (#1137). First
   attempt: input worked — the operator unlocked macOS and dragged a window from the
   browser — and the screen stayed black. `Request` is not delegable, but it is the arm
   that RESOLVES the session and stashes seven values `SdpOffer` CONSUMES. `transport`
   defaults to `None` = the legacy RTP track, so the worker wrote 13 MB to a pipe the
   browser was not reading. The whitelist was chosen by asking which messages a session
   HANDLES; the miss was that one of them carries state another consumes.

2. **A latency finding that was NOT delegation** (#1152). 230–600 ms frame age vs 5–9 ms
   direct. Delegation and codec were perfectly confounded — every delegated session
   H.264 and slow, every direct one HEVC and fast — and breaking that needed one
   controlled run: among DELEGATED sessions, HEVC gives 5–13 ms and H.264 gives 139 ms.
   Ruled out first, each by measurement: ICE relay (`relay=false`, Host↔Host), the media
   path (`send_wait_avg 0.04 ms`, zero drops/errors), the daemon negotiating on weaker
   caps (both halves report identical caps), B-frame reordering (`max_b_frames(0)` is
   applied to every encoder). ⚠️ The UI auto-selected H.264, so the slow path was the
   DEFAULT.

⚠️ **Instrumentation lesson.** The delegation trace shipped at `debug!`, so the field
logs could not answer *"did it cross?"* — the first diagnosis had to correlate session
ids across two log files. Now `info!`, and the serving line should also carry
`session_id`: establishing WHICH sessions were delegated still took three queries.
A feature whose whole question is "did this reach the other side" must answer it at the
level the field runs at.

**P2b is complete.** P2c is next: the daemon's row still advertises `no-gui-session`, so
the UI presents it as "not a capture target" even though it now is one.
