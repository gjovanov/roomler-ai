# FR-96: Per-user Linux installs update themselves — a root update half beside the daemon, as macOS has

**Issue:** [#2052](https://github.com/gjovanov/roomler-ai/issues/2052) · **Status:** in progress — P1 shipped in
0.4.133; P2a (#2132) and P2b (#2184) in review, both field-verified on the lab's KDE guest; P3–P4 not started (spec and claim 2026-10-12, #2046, on the operator's
"do per recommendation") · **Owner:** the agent updater and the Linux installers ·
**Builds on:** [FR-5](FR-5-macos-unattended-update-chain.md) (the macOS update half this mirrors),
[FR-67](FR-67-update-outcome-observability.md) (what an install reports) · **Fixes:** #2046

## 1. Goal

A **per-user** Linux install takes every release by itself, the way a system install and a Mac already do. That
is `install.sh`'s default, with no `--system`. The update needs no password prompt, and no person at the machine.

- Every distro: Debian and Ubuntu (the `.deb`), and Fedora, Arch, openSUSE (the universal tarball).
- **One owner** of check → download → verify → install: a root, single-shot helper. The per-user daemon only
  wakes it, as the macOS agent wakes `com.roomler.update`.
- The per-user daemon ends up running the new binary, and is not restarted under a session someone is using.
- An operator can opt out, with the same marker as on macOS.

## 2. Evidence

The released **0.4.130**, on the FR-93 lab's four Linux guests, 2026-10-12:

| Guest | Install | Package | The guest's own updater |
|---|---|---|---|
| GNOME 50 | per-user | `.deb` | `SucceededVerified`, through `sudo -n` (the lab user has passwordless sudo) |
| GNOME 50 | system | `.deb` | `SucceededVerified` |
| Plasma 6.7 (Fedora 44) | system | tarball | `SucceededVerified` |
| **Plasma 6.7 (Fedora 44)** | **per-user** | **tarball** | 🔴 *"the tarball install needs root (euid 1000); run the agent as a system service, or install … manually"* |

The code agrees, line by line (master `b82001355`):

- **The tarball has no way up.** `install_tarball_linux` refuses any `euid != 0`
  (`agents/roomlerd/src/updater.rs:1826-1835`), and has since the tarball path landed (#462, 2026-08-15).
- **The `.deb` ladder asks a person.** A non-root daemon tries `pkexec apt-get install`, then `sudo -n`
  (`updater.rs:1761-1775`). On a desktop with a polkit agent that is a password prompt in front of whoever is
  at the screen, every 4 h. Without one, it is `sudo -n`, which works only where sudo needs no password.
- **The installer already admits it.** `install.sh:371` prints *"a per-user daemon cannot install its own
  updates (writing /usr needs root, whichever format). Re-run with --system for unattended self-update."*
  `README.Debian:19,46-48` says the same.
- **The docs promise otherwise:**
  - `docs/linux-self-update.md:88-89` promises the tarball *"the same privilege ladder (root direct; otherwise
    `sudo -n`, then `pkexec`)"*, and `:197` says *"the tarball makes self-update work everywhere"*.
  - `docs/installation.md:497` says the updater *"hands off to MSI / `dpkg` / `installer` and restarts"*.
- **Nothing restarts a per-user daemon onto a replaced binary.** There is no restart-on-replace anywhere
  (`strip_deleted_suffix`, `updater.rs:2208-2216`, only fixes up a path for the watcher). So even an admin's
  `apt upgrade` leaves the per-user daemon on the old, unlinked image until something restarts it.

**Who this serves.** Not yet measured, and partly unmeasurable from the server: FR-93 P2b's hello reports
`desktop.session = "user"` for a per-user daemon, but only from 0.4.130, and a frozen tarball host never reaches
0.4.130. P0 counts Linux devices by version, from the server's own rows.

## 3. Key design

### 3.1 The update half: `roomlerd update-helper` on Linux

```mermaid
sequenceDiagram
    autonumber
    participant D as per-user roomlerd (the user)
    participant W as /var/tmp/roomler-update-check
    participant U as roomler-update.service (root, one-shot)
    participant R as the release feed
    D->>W: touch (on rc:agent.update) — a WAKE, content never read
    Note over U: also woken by roomler-update.timer (boot + every 6 h)
    W-->>U: roomler-update.path fires
    U->>R: check_once: the latest agent-v*
    U->>U: download_asset: size floor, digest, PGP (the pinned key)
    U->>U: install as root: dpkg / apt-get, or the tarball
    U->>D: RestartDaemon, over /run/user/<uid>/roomler.sock (deferred while busy, D1)
    Note over D: exits 9; systemd --user relaunches the new binary
```

| piece | reused or new | where |
|---|---|---|
| the release check, with no config (the feed is `ROOMLERD_UPDATE_URL`, else the proxy) | reused | `check_once`, `updater.rs:1531-1572`; `DEFAULT_PROXY_URL` `:37` |
| download and verify: size floor, digest, the pinned release key | reused | `download_asset` `:1269-1432`, PGP `:1358-1393` |
| the install-storm cooldown (root's own marker) | reused | `recent_update_attempt` / `record_update_attempt` `:424-457`, `STARTUP_UPDATE_COOLDOWN` `:71` |
| the install, as root | reused | `linux_install_candidates(0, …)` `:2009-2023`; `install_tarball_linux(0, …)` `:2075-2198` |
| the opt-out, `/etc/roomler/disable-auto-update` | reused | the helper's `OPT_OUT` `:3380-3387` (shared by macOS and Linux) |
| the wake file: content ignored, never followed through a symlink | generalised | `MACOS_UPDATE_TRIGGER` `:3017`, `LINUX_UPDATE_TRIGGER` `:3022`, `queue_update_check` / `queue_update_check_at` `:3115-3134` (was `macos_queue_update_check`) |
| `roomlerd update-helper` on Linux | new (P1) | the `Command::UpdateHelper` arm, macOS and Linux (`main.rs:1604-1619`) |
| `roomler-update.{service,timer,path}` | new (P1) | shipped in the `.deb` (`agents/roomlerd/Cargo.toml` assets `:848-887`) and the tarball (`scripts/stage-linux-tarball.sh:55-58`, `TARBALL_INSTALL_IF_ABSENT` `updater.rs:2052-2062`) |
| the download's private staging dir (mkdtemp, 0755, `O_EXCL` write) — the security review's finding 1 | new | `stage_dir` / `write_installer` / `discard_staging` (`updater.rs`) |
| the run-spacing gate — launchd's `ThrottleInterval` twin, the security review's finding 2 | new | `UPDATE_HELPER_MIN_GAP` / `helper_gate_wait` / `stamp_helper_gate` (`updater.rs`) |
| consuming the wake: a symlink unlinked NOT followed, a planted directory moved aside into a fresh root-owned dir | new | `consume_wake` (`updater.rs`) |

The three units are the systemd twins of the macOS plist's wake sources
(`agents/roomlerd/packaging/macos/com.roomler.update.plist`):

| macOS (`com.roomler.update`) | Linux |
|---|---|
| `RunAtLoad` | `roomler-update.timer`: `OnBootSec=5min` |
| `StartInterval` 21600 | `roomler-update.timer`: `OnUnitActiveSec=6h`, `RandomizedDelaySec=30min` |
| `WatchPaths` `/private/var/tmp/roomler-update-check` | `roomler-update.path`: `PathExists=/var/tmp/roomler-update-check` |
| the program | `roomler-update.service`: `Type=oneshot`, `ExecStart=/usr/bin/roomlerd update-helper` |

⚠️ **One owner per host.** `install.sh` enables the timer and the path for a **per-user** install only. It
already runs `sudo` for the payload. A system install's root daemon keeps updating itself in-process, so two
updaters never race. The `.deb`'s `postinst` must never enable anything (`packaging/linux/debian/postinst:18-21`,
pinned by `watchdog.rs:932-960`), so the units ship disabled.

### 3.2 The per-user daemon wakes the helper instead of installing (P2a, as built)

`run_periodic` forks once, at the daemon's start (`updater.rs:2792-2826`). macOS takes `forward_triggers` (the
shared successor of `macos_forward_triggers`, `:3146-3228`) as before. Linux asks a pure
`decide_update_owner(euid, kill switch, enable link)` (`:3087-3105`):

| daemon | `ROOMLERD_UPDATE_HELPER` | helper enabled | owner |
|---|---|---|---|
| root | any | no | in-process |
| root | any | yes | in-process, and a WARN: two updaters race on the package manager (AC4: a system install is unchanged) |
| per-user | `0` | any | in-process — the kill switch, today's path exactly |
| per-user | unset / on | no | in-process |
| per-user | unset / on | **yes** | **the helper**: forward each push to the wake file, install nothing, run no periodic check |

- **"Enabled"** is the `.wants` link `systemctl enable` (or `enable --runtime`) leaves for either unit, under
  `/etc` or `/run` (`UPDATE_HELPER_ENABLE_LINKS`, `:3041-3046`), FOLLOWED: a link left dangling by a removed
  package is not an enabled helper. A file check, not `systemctl is-enabled` — no bus, no child, and it reads the
  same inside the per-user unit's sandbox.
- ⚠️ **On doubt, in-process.** A wrong "helper" answer freezes the host (it forwards wakes nobody hears — #2046
  again); a wrong "in-process" answer costs a failed `pkexec`, and the helper's own timer still runs if it exists.
- As on macOS, a pinned version does not cross the boundary; the helper installs the latest.
- ⚠️ **The per-user unit's sandbox had to be opened for one file.** `roomler.service` runs under
  `ProtectSystem=strict`, which leaves `/var/tmp` read-only wherever the user manager applies the sandbox —
  **measured** on the lab's Fedora 44 KDE guest (§8): `touch /var/tmp/…` in the daemon's own mount namespace
  answered `Read-only file system`, while its runtime dir answered writable. Every push there would have failed
  `EROFS`. P2a adds `ReadWritePaths=-/var/tmp` (only the directory can be granted: a `ReadWritePaths` entry binds
  what exists when the sandbox is built, and the helper deletes the wake file each run). A daemon whose unit
  predates P2a logs the failure and names the cure; the push then waits for the helper's 6 h timer. CI guards
  the grant in both sandbox cells. The system-unit cell (`ci.yml:804-829`) has the sandboxed unit write
  `/var/tmp`, and a negative control (the same sandbox without the grant) must come back read-only. The
  user-manager cell (Fedora's configuration, `ci.yml:883-887`) requires a read-write `/var/tmp` mount in the
  daemon's namespace. Both directions were measured on systemd 255 before landing (§8).
- **Decided at the start.** Enabling or disabling the helper takes effect at the daemon's next start — `install.sh`
  (P3) must refresh `roomler.service`, then `systemctl --user daemon-reload` and restart the per-user daemon after
  enabling the units (the user manager keeps a changed unit's old sandbox until it reloads; the updater's own
  tarball install writes units only where absent, `TARBALL_INSTALL_IF_ABSENT`, while `install.sh`'s plain
  `tar -xzf` overwrites them).
- **`auto_update = false`** in the per-user config switches the updater off entirely, forwarding included — a push
  is dropped, as before. The helper's own timer still installs: on a helper host the opt-out is
  `/etc/roomler/disable-auto-update`, as on macOS.

A host without the helper (an install from before this FR) keeps today's path unchanged.

### 3.3 Onto the new binary (P2b, as built)

The per-user daemon notices for itself (D2): `agents/roomlerd/src/replaced_binary.rs`, spawned beside the updater
(`main.rs:4523-4524`) on a Linux, non-root daemon whose kill switch is unset and whose supervisor will relaunch it
(`spawn`, `replaced_binary.rs:241-265`). ONE mechanism serves the helper's install, an admin's `apt upgrade` and a
hand install alike, and the helper never speaks to a daemon.

| step | what | where |
|---|---|---|
| baseline | at start, the exec path (`argv[0]` when absolute: systemd passes `ExecStart=`'s) and `/proc/self/exe` must be ONE file by `(st_dev, st_ino)`, or the watch stands down — this filesystem cannot tell a replacement | `baseline`, `replaced_binary.rs:270-287` |
| replaced | every minute, the path's identity against the running image's. Not a ` (deleted)` check: the tarball's install renames the old binary to `.prev`, which `/proc/self/exe` then names | `watch` `:342-503`, `CHECK_EVERY` `:41` |
| settled | the same new identity for 2 min: an install places the binary before its libraries and units | `SETTLE` `:45` |
| answers | the new binary's `--version` must be `roomlerd <another version>`. One that cannot load (a missing shared library) is never restarted onto, so the daemon stays online on the old image; the same version is not worth cutting anything for | `verdict_from` `:196-211` |
| idle (D1) | no remote-control session, no SSH connection, no transfer or recording — work a restart CUTS, cut after the 24 h ceiling. A network transition in the last 10 min, or an install of the daemon's own in flight — states a restart could WEDGE, never forced. Agent turns are waited for as an install waits for them | `Busy` `:82-107`, `decide` `:164-190`, `CEILING` `:48` |
| restart | FR-84 D3's door: a supervisor that relaunches, the persisted record that makes a loop impossible, systemd's own read-back of the unit's policy. These are "Apply now"'s gates, minus the owner's switch for LOCAL CLIENTS' requests: nobody is asking. A refusal is retried after 10 min | `RestartHandle::arm_from_within`, `localapi_state.rs:326-385`; `RETRY_AFTER`, `replaced_binary.rs:52` |

- ⚠️ **An SSH session counts as a session.** The likeliest replacement outside the helper is an admin's
  `apt upgrade` typed into a roomler SSH session on that very device. `ssh.rs` now counts its open connections
  (`live_connections`, `ssh.rs:2539-2545`), and the restart waits for them.
- **`auto_update = false` does not stop it.** Running what is already installed is not an update. The kill switch
  is the FR's, `ROOMLERD_UPDATE_HELPER=0` (AC7).
- **Root daemons are unchanged** (AC4). An admin's `apt upgrade` on a system install still leaves the root daemon
  on its old image until something restarts it, because the postinst never restarts anything. Taking D2 to root
  daemons is a separate decision, not made here.
- **Why not the helper asking each daemon** (this section's first sketch, over `Request::RestartDaemon` on
  `/run/user/<uid>/roomler.sock`): root would connect to a socket a user owns and have to distrust every answer,
  and it would miss every replacement the helper did not make.

### 3.4 What it must never do

- **Read the wake file's content.** A local user who could pin a version could make root install a genuine but
  old release, the downgrade class `artifact_version` closed on Windows. The macOS rule carries over as-is.
- **Install for a system install.** That host has one owner already.
- **Restart a daemon mid-session** before D1's 24 h ceiling (P2b's `decide`, `replaced_binary.rs:164-190`), nor
  ever beside a network transition.
- **Run as non-root.** It refuses, as macOS's helper does (`updater.rs:3296-3303`).
- **Stage a download at a fixed or shared name** (security review, finding 1). `/tmp` is sticky but a
  *subdirectory* of it is not, so the old fixed `/tmp/roomlerd-update/<asset>` let any local user own the
  directory, wait for root's verified `.deb` to land, and `rename` an attacker package over it in the window
  before `apt-get install <path>` / `tar -xzf <path>` re-read the path — root then installs it (dpkg maintainer
  scripts run as root). Each download now goes to a FRESH private directory (mkdtemp, 0755, owned by the caller;
  `O_EXCL` write), which a non-owner cannot create, rename or unlink inside. The verified bytes are the installed
  bytes. macOS (per-user `_CS_DARWIN_USER_TEMP_DIR`) and Windows (SYSTEM `GetTempPath2`) keep the shared dir;
  Windows 10's `C:\Windows\Temp` has the same shape and is a separate follow-up.
- **Let a wake storm or a planted entry FAIL the units** (security review, finding 2). `PathExists=` is
  level-triggered and the service had systemd's default start limit (5/10 s), so a local user could permanently
  fail `roomler-update.path` with a touch burst or one `mkdir` at the wake path — a pushed update then silently
  degrades to the 6 h timer. The helper now spaces its own runs ≥60 s (`UPDATE_HELPER_MIN_GAP`, launchd's
  `ThrottleInterval` twin), consumes a planted entry (a symlink unlinked unfollowed, a directory moved aside),
  and the unit carries `StartLimitIntervalSec=0` + `Restart=on-failure`/`RestartSec=60s` so neither a burst nor an
  exec-failing helper can fail the path unit. Never `PrivateTmp=` — it would hide `/var/tmp` and the wake could
  never be consumed.

## 4. Phases

| Phase | What | Kill switch | Proven on | Size |
|---|---|---|---|---|
| **P0** | the population: Linux devices by version, and per-user ones by P2b's `session`, from the server's rows | — | the server | S |
| **P1** | `roomlerd update-helper` on Linux, and the three units in the `.deb` and the tarball | the opt-out marker; the units disabled | the lab: the KDE per-user (tarball) and GNOME per-user (`.deb`) guests, red first on the release | M |
| **P2a** | the per-user daemon wakes the helper instead of installing (§3.2), and its unit may write the wake file | `ROOMLERD_UPDATE_HELPER=0` | the lab: the KDE per-user guest, a real push → the helper's install (§8) | S |
| **P2b** | the daemon onto the new binary once idle, after ANY replacement (§3.3, D1, D2) | `ROOMLERD_UPDATE_HELPER=0` | the lab: the KDE per-user guest, a viewer session held across a real push, which was not cut; the restart came once it ended (§8, 2026-10-14) | M |
| **P3** | the installers: `install.sh` enables the units for a per-user install; a non-root `roomler self-update` wakes the helper (it bails today, `main.rs:5083-5113`) | — | the lab, a fresh install of each format | S |
| **P4** | docs: `docs/linux-self-update.md` (status, the ladder claim, the helper), `docs/installation.md` (a Linux row beside the macOS three-unit table, `:297-301`, and `:497`), `README.Debian`, the `docs/README.md` index row | — | — | S |

Each phase ships in an agent release (the `ship-it` skill owns the mechanics) and is field-verified before the
next depends on it.

## 5. Acceptance criteria

- [ ] **AC1**: a per-user **tarball** install (the lab's Fedora KDE guest) goes from release N to N+1 with no
  human action: on its timer, or within a minute of a pushed `rc:agent.update`. Red first on 0.4.130 (§2).
  Progress (P1 + P2a + P2b, §8). A real push installed the release on the KDE guest about a minute later; the
  helper's first run waits the full 60 s gap. P2b then restarted the daemon onto it once idle, and the server's
  row reported the new version. Still owed: the units were enabled by hand, and P3's `install.sh` owes "no human
  action".
- [ ] **AC2**: a per-user **`.deb`** install (the lab's GNOME guest) does the same, with no `pkexec` or `sudo` and
  no password prompt on its screen.
- [x] **AC3**: after the install, the per-user daemon runs the new version (the server's row reports it). A remote
  session open across the install is not cut by the restart, which waits for it to end (D1). Field-verified
  2026-10-14 on `wlab-kde` (§8, the P2b candidate), red first on the stock build.
  - A viewer session was held across a real push and the helper's install of 0.4.134.
  - The daemon logged `restart deferred busy=1 remote-control session(s)`.
  - The viewer stayed connected at all 32 checks, with 0 cuts.
  - The restart came 44 s after the viewer let go, and the row read `0.4.134 online=true`.
- [ ] **AC4**: a system install is unchanged. Its root daemon updates itself in-process, and the helper's units
  are not enabled.
- [ ] **AC5**: `/etc/roomler/disable-auto-update` stops the helper, and removing it resumes updates (a round
  trip, like `installer-smoke.yml`'s macOS one at `:1203-1252`). Half done: the STOP half is field-verified
  (§8, direct journal line `auto-update opted out — exiting`); the round trip's RESUME half (remove the marker,
  confirm the next wake actually installs) was not re-exercised and stays owed.
- [x] **AC6**: the wake file is advisory only. Its content is never read; a symlink planted at its path is
  unlinked, not followed; a directory planted there is moved aside and removed; and a burst of touches (or a
  planted entry) leaves `roomler-update.path` `active (waiting)`, never `failed` (unit tests, plus one lab check).
  Field-verified 2026-10-12/13 on `wlab-kde` — see §8. "Content never read" rests on the unit tests and direct
  code inspection (`consume_wake` never opens the file for reading); every other clause has a direct field log
  line.
- [x] **AC7**: `ROOMLERD_UPDATE_HELPER=0` gives back today's behaviour exactly. Field-verified on `wlab-kde`, both
  halves (§8).
  - P2a (2026-10-13): with the helper enabled, the switch keeps the daemon on the in-process path, and the log
    says why.
  - P2b (2026-10-14): the switch logs `replaced-binary watch: off (ROOMLERD_UPDATE_HELPER=0)`. With the binary
    replaced underneath it, the daemon kept its pid and ran the deleted image for 4 min, as the stock build does.
- [ ] **AC8**: Docs: `docs/linux-self-update.md`, `docs/installation.md` and `README.Debian` updated in the house
  style (a mermaid diagram, tables, `file:line` anchors), and the `docs/README.md` index row.
- [x] **AC9** (security review, #2055): the download is staged in a fresh private directory — a new 0755
  caller-owned dir per download, the write refuses a planted symlink (not followed) or file (not truncated), and
  the dir is removed once the installer has read it (unit tests, plus one lab check: during an install
  `/tmp/roomlerd-update.*` is `root root` 0755 and a pre-existing `/tmp/roomlerd-update/` is untouched).
  Field-verified 2026-10-12/13 on `wlab-kde` — see §8: three independent real installs each staged at a fresh
  dotted mkdtemp path (never the fixed name) and left none behind afterward; a pre-existing bare
  `/tmp/roomlerd-update/` from an unrelated unfixed baseline was never touched. The `root:root 0755` ownership
  and the planted-symlink/file refusal **at the staging path itself** (as opposed to the wake path, which IS
  field-verified under AC6) are unit-test-confirmed (`staging_dir_is_fresh_private_and_ours`,
  `write_installer_refuses_a_planted_symlink_and_a_planted_file`) — polling never caught a live install in the
  narrow window to additionally stat it mid-flight.

## 6. Open decisions (operator)

| | decision | recommendation |
|---|---|---|
| **D1** | the restart onto the new binary: at once, or when the daemon is idle | **Decided 2026-10-13 (operator), as recommended: when idle, with a 24 h ceiling** (P2b, §3.3). A restart under a live session cuts it, and the update is not urgent enough for that |
| **D2** | a backstop for every other replacement (an admin's `apt upgrade`, a hand install): the daemon notices its executable was replaced and restarts when idle | **Decided 2026-10-13 (operator), as recommended: yes** — and it became the ONE mechanism (§3.3): without it a replaced binary waits for a reboot |
| **D3** | hosts installed before this FR cannot receive it by themselves | say so where the operator looks: a devices-page note for a Linux row more than N releases behind, pointing at re-running `install.sh` |
| **D4** | the `.deb` ladder's `pkexec`, once the helper ships | drop it on hosts that have the helper; keep `sudo -n` only on hosts that don't. A password prompt every 4 h is worse than either |

## 7. Out of scope

- An `.rpm` lane (`docs/linux-self-update.md`'s own out-of-scope).
- The tunnel CLI's separate `roomler self-update` on tunnel-only hosts (its own known issue in `CLAUDE.md`).
- Windows and macOS, which have their own update halves.

## 8. Field-verification log

| date | build | host | result |
|---|---|---|---|
| 2026-10-12 | 0.4.128 → 0.4.130 | the FR-93 lab's KDE guest (Fedora 44, per-user, tarball) | 🔴 refused: *"the tarball install needs root (euid 1000)"* (#2046). The other three lab guests took 0.4.130 through their own updaters |
| 2026-10-12 | P1 branch | systemd 255 (WSL), the units' exact directives | 🔴→🟢 security-review finding 2 reproduced and fixed. RED (default start limit): 6 touches of the wake file in 10 s, or one `mkdir` at it, left the path unit `failed (unit-start-limit-hit)`; a wake 15 s later was not consumed. GREEN (`StartLimitIntervalSec=0` + `Restart=on-failure`/`RestartSec`): an 8-touch burst left the path unit `active` and the service `Result=success`; an exec-failing helper (`/nonexistent`) left the path unit `active` and the service `auto-restart`, never `failed`. A bare `Type=oneshot` shows `TimeoutStartUSec=infinity`, so the 60 s gate + download is not a start timeout |
| 2026-10-12 | P1 branch | WSL, `cargo test -p roomlerd --lib updater::` | 🟢 90 tests (76 pre-existing + 14 new): the private staging dir (fresh/0755/owned, `O_EXCL` refuses a planted symlink and file, cleanup removes only our own dir), the gate arithmetic + its equality to the plist's `ThrottleInterval`, and `consume_wake` (file / nothing / symlink-unfollowed / planted-dir-moved-aside). `--features full,…` clippy (`ci.yml:341`) and default `--all-targets` clippy clean |
| 2026-10-12 | candidate 0.4.1 → released 0.4.131 | `wlab-kde` (Fedora 44 KDE, per-user tarball; zeus, `~/wwlab/fr96-lab-secfix.sh` on mars) | 🟢 **F1 and F2's two headline properties, confirmed on a real root systemd deployment, not just WSL.** Red first: a stock 0.4.131 refuses `update-helper` (`"this platform's updater runs inside the agent itself"`), reproducing #2046 unchanged. The security-fixed candidate's units carry both markers (`StartLimitIntervalSec=0`, `Restart=on-failure`, confirmed by `grep` on the installed files). A real wake → gate → install, **verbatim from the unit's own journal**: `spacing this run from the last (ThrottleInterval twin) wait_secs=60` at `22:05:26`, then at `22:06:27` (61 s later) `installing as root … package=/tmp/roomlerd-update.yjHckQ/roomlerd-0.4.131-x86_64-unknown-linux-gnu.tar.gz` — a **mkdtemp-style dotted name**, never the old fixed `/tmp/roomlerd-update/` — followed by `installed and verified`. A 6-touch burst in ~3 s left `roomler-update.path` `active`/`Result=success` and the service `Result=success, NRestarts=0` — no unit ever failed. |
| 2026-10-13 | same candidate, `fr96-lab-secfix-v2.sh` | same guest, re-run after fixing three bugs in the TEST script (not the code): `mkdir -p /etc/roomler` before the opt-out touch, and waiting for `roomler-update.service` to go `inactive` before each new wake instead of a blind `sleep 8` left over from before this review's 60 s gate existed | 🟢 **Opt-out and the symlink scenario now cleanly confirmed, unconfounded.** Opt-out: direct journal line `auto-update opted out — exiting marker=/etc/roomler/disable-auto-update`. The symlink scenario (AC6): a second real wake → gate → install, this time correctly isolated — `spacing this run … wait_secs=54` (the **remainder** since the prior stamp, not a flat 60 — the gate's arithmetic, not just its presence, confirmed live) → `installing as root … package=/tmp/roomlerd-update.ztDLkg/…` (a second, independent mkdtemp path) → `installed and verified` → **`link gone: yes, canary: canary`** — the planted symlink was unlinked, its target never touched. ⚠️ A side effect surfaced an edge case worth keeping: once the candidate self-upgraded to the real (unfixed) 0.4.131, every later invocation in this same run was that OLDER binary, which has no `update-helper` on Linux at all and fails immediately — `roomler-update.path` stayed `active`/`Result=success` throughout anyway, a harder version of F2's no-unit-ever-fails claim than planned, even though it meant the planted-DIRECTORY check that followed answered from the wrong binary (untouched, not a code finding) |
| 2026-10-13 | same candidate, `fr96-lab-secfix-v3.sh` (one focused re-run, ordered correctly: plant-and-check immediately after installing the candidate, before anything can upgrade past it) | same guest | 🟢 **The planted-directory case (F2, `consume_wake`), closed.** Exact journal lines, consume-before-gate in the same millisecond: `22:40:52.995185 WARN update-helper: a non-file was planted at the wake path; moved aside and removed` then `22:40:52.995216 INFO update-helper: spacing this run from the last (ThrottleInterval twin) wait_secs=60`. The directory was gone, no `.aside` leftover, `roomler-update.path` stayed `active`/`Result=success`. Every check originally planned for this review is now field-verified: F1 (three independent real installs, three different dotted mkdtemp paths, none left behind, the bare pre-fix name from an unrelated earlier baseline never touched), F2's gate (flat and remainder), F2's burst immunity, F2's planted-symlink and planted-directory handling, and the opt-out stop |
| 2026-10-13 | P2a candidate (0.4.1), run 1 | `wlab-kde`, `~/wwlab/fr96-p2a-lab.sh` on mars | ⚪ **Measured nothing past setup — a confound in the TEST, kept because it is a real behaviour.** The guest's `config.toml` carries `auto_update = false`, so `run_periodic` never ran and the push was dropped: the per-user switch gates forwarding too (§3.2). Re-run with the updater armed through the user manager's environment (`ROOMLERD_AUTO_UPDATE=1`, not a config edit — #2078: a clean shutdown erases keys the old binary does not know), and with each step asserting `auto-updater armed` before it measures anything |
| 2026-10-13 | P2a candidate (0.4.1) → released 0.4.133, run 2 | same guest, reverted to `stock-0.4.128` first | 🔴→🟢 **P2a end to end, on a real push.** RED on the stock 0.4.128 unit: `touch` in the daemon's own mount namespace → `cannot touch '/var/tmp/fr96-p2a-probe': Read-only file system`, while its runtime dir (the control) is writable — every forwarded push would have failed `EROFS`. The two controls on the candidate log their reason: `ROOMLERD_UPDATE_HELPER=0` → `this daemon installs its own updates why=ROOMLERD_UPDATE_HELPER=0 — the in-process path, as before FR-96`; the helper disabled → `why=the root update helper is not enabled on this host (roomler-update.path / .timer)`. GREEN with only `roomler-update.path` enabled (the timer left disabled — either link is enough): `the root update helper (roomler-update.service) owns updates on this host — forwarding rc:agent.update to its wake file, installing nothing here enabled_by=/etc/systemd/sys…`, and `/var/tmp` writable in the new unit's namespace. Then a **real push** from the server (`POST …/agent/{id}/update` → `delivered:true`): the daemon at `20:48:15.765` `rc:agent.update queued for the root update helper`; the helper 16 ms later `spacing this run … wait_secs=60` (a reverted guest has no stamp, so the full gap); at `20:49:16.691` `installing as root current=0.4.1 latest=agent-v0.4.133 package=/tmp/roomlerd-update.hePzZM/…tar.gz`; `installed and verified`; `/usr/bin/roomlerd` = 0.4.133, 61 s after the push. The candidate logged no install attempt of its own (0 lines). ⚠️ The contrast count was ALSO 0 — the stock daemon was not armed, and each control ran ~9 s before the next restart — so it proves nothing either way; the split rests on the decision lines above and the unit tests. The daemon itself stays on 0.4.1 until its next restart, which the helper's own line says: P2b's job |
| 2026-10-13 | the P2a CI guard | systemd 255 (WSL), the two sandbox cells' logic | 🔴→🟢 **The grant's CI guard can fail, measured before it landed.** PR #2132's first CI run went red in FR-85's system-unit sandbox cell, which requires every optional (`-`) path to be missing, and `/var/tmp` exists on every host. That cell now exempts `/var/tmp` and asserts it is WRITABLE instead. Under `roomler.service`'s sandbox as a system unit with `User=`, the probe answered `VARTMP_WRITABLE`. The same sandbox without `ReadWritePaths=-/var/tmp` answered `VARTMP_READ_ONLY`, which is the negative control. Under a user manager that applies the sandbox (the #1997 cell's configuration; the unit in its own mount namespace), the grant gave `/var/tmp rw,nosuid,relatime` as a mount of its own, while without it there was no `/var/tmp` entry and `/` was `ro` |
| 2026-10-14 | P2b candidate (0.4.1), run 1 | `wlab-kde` | ⚪ **Void: a lab collision, not a result.** Another session's FR-93 P5e run started on the same guest 4 s after this one, and both had passed a "is the guest off?" check. Its candidate swap restarted the stock daemon at 06:31:54, which read like a red that was not one: the stock daemon restarting after its binary was replaced. Every step after that ran on the other session's binary. This run was stopped at 06:33:55, and the other session was told exactly what had changed on the guest; both runs were voided. The lab now has a claim both sessions honour: `flock -n ~/wwlab/locks/<guest>.lock` around the whole run, plus a re-check for other users right after the revert |
| 2026-10-14 | P2b candidate (0.4.1) → released 0.4.134, run 2 | `wlab-kde` under the lab lock (`~/wwlab/fr96-p2b-lab.sh` on mars), reverted to `stock-0.4.128`, the automatic portal off for the run (`ROOMLERD_WAYLAND_AUTO=0`: a reverted guest has no portal grant, and nobody answers the dialog) | 🔴→🟢 **P2b end to end, D1 included.**<br>• **RED**, the stock 0.4.128 daemon: four minutes after `sudo install` replaced its binary, it was the same pid (1255), running `/usr/bin/roomlerd (deleted)`. Nothing restarts a per-user daemon (§2).<br>• **The candidate** came up `replaced-binary watch: armed … path=/usr/bin/roomlerd supervisor="systemd"`, in P2a's helper mode.<br>• **A viewer session**, held 8 min from 06:43:41. The push was delivered at 06:43:43 and the daemon logged `queued for the root update helper`. The helper installed **0.4.134**, which was released in the meantime, at 06:44:47.<br>• **The watch** logged `the binary was replaced — waiting for it to settle` at 06:45:26, then `the new binary answers — restarting onto it once idle new=0.4.134 running="0.4.1"` at 06:47:26, then `restart deferred busy=1 remote-control session(s)` at 06:48:26.<br>• **The viewer** was connected at all 32 of its 15-s checks, with **0 CUT**, and released at 06:51:42.<br>• **The restart**: at 06:52:26, the very next check, `restart decided by this daemon: armed reason=FR-96 P2b: /usr/bin/roomlerd was replaced (0.4.1 -> 0.4.134) supervisor="systemd" exit_code=9`, then `restart: committed`, then `marking graceful`. systemd logged `Main process exited, code=exited, status=9/n/a` and `Started` at 06:52:31. The new pid 4811 runs `/usr/bin/roomlerd` = 0.4.134, and the server row reads **`0.4.134 online=true`**.<br>• **AC7**: the candidate again under `ROOMLERD_UPDATE_HELPER=0` logged `replaced-binary watch: off (ROOMLERD_UPDATE_HELPER=0)`. Four minutes after the release replaced its binary, it kept its pid (5050) on the deleted image.<br>• The run ended with no `ROOMLERD_*` left in the user manager |
