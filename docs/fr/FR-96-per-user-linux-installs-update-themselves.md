# FR-96: Per-user Linux installs update themselves — a root update half beside the daemon, as macOS has

**Issue:** [#2052](https://github.com/gjovanov/roomler-ai/issues/2052) · **Status:** proposed 2026-10-12, spec and claim
(#2046, on the operator's "do per recommendation") · **Owner:** the agent updater and the Linux installers ·
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
| the release check, with no config (the feed is `ROOMLERD_UPDATE_URL`, else the proxy) | reused | `check_once`, `updater.rs:1283-1324`; `DEFAULT_PROXY_URL` `:37` |
| download and verify: size floor, digest, the pinned release key | reused | `download_asset` `:1020-1184`, PGP `:1110-1145` |
| the install-storm cooldown (root's own marker) | reused | `recent_update_attempt` / `record_update_attempt` `:424-457`, `STARTUP_UPDATE_COOLDOWN` `:71` |
| the install, as root | reused | `linux_install_candidates(0, …)` `:1761-1775`; `install_tarball_linux(0, …)` `:1823-1946` |
| the opt-out, `/etc/roomler/disable-auto-update` | reused | the macOS helper's `OPT_OUT` `:2866-2873` |
| the wake file: content ignored, never followed through a symlink | generalised | `MACOS_UPDATE_TRIGGER` `:2705`, `macos_queue_update_check` `:2713-2726` |
| `roomlerd update-helper` on Linux | new | the `Command::UpdateHelper` arm, macOS-only today (`main.rs:1456-1468`) |
| `roomler-update.{service,timer,path}` | new | shipped in the `.deb` (`agents/roomlerd/Cargo.toml` assets `:840-873`) and the tarball (`scripts/stage-linux-tarball.sh:53-55`, `TARBALL_INSTALL_IF_ABSENT` `updater.rs:1804-1810`) |
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

### 3.2 The per-user daemon wakes the helper instead of installing

`run_periodic` forks for macOS at `updater.rs:2512-2514` (`macos_forward_triggers`). It gains a Linux twin: a
non-root daemon on a host with the helper forwards `rc:agent.update` to the wake file and installs nothing
itself. As on macOS, a pinned version does not cross the boundary; the helper installs the latest.

A host without the helper (an install from before this FR) keeps today's path unchanged. Kill switch:
`ROOMLERD_UPDATE_HELPER=0` gives back today's in-process path exactly.

### 3.3 Onto the new binary

After a successful install, the helper asks every per-user daemon on the host to restart.

- **Find them:** each `/run/user/<uid>/roomler.sock` (`crates/localapi/src/lib.rs:3272`, `user_socket_path`
  `:3291-3299`). Root reaches the 0600 socket, and the console-user gate accepts uid 0 (`lib.rs:2046-2047`).
- **Ask:** the existing `Request::RestartDaemon { reason }` (`lib.rs:1334-1338`). It exits 9 (`watchdog.rs:172`),
  and the unit's `Restart=always` relaunches the new binary (`packaging/linux/roomler.service:27-29`).
- ⚠️ **Not under a live session (D1).** The restart waits while a remote session, a transfer or a recording is
  active, the in-process updater's own gate (`active_work`, `updater.rs:385-392`), up to a ceiling.

### 3.4 What it must never do

- **Read the wake file's content.** A local user who could pin a version could make root install a genuine but
  old release, the downgrade class `artifact_version` closed on Windows. The macOS rule carries over as-is.
- **Install for a system install.** That host has one owner already.
- **Restart a daemon mid-session.**
- **Run as non-root.** It refuses, as macOS's helper does (`updater.rs:2839-2846`).
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
| **P2** | the per-user daemon wakes the helper (§3.2), and the helper restarts it onto the new binary (§3.3, D1) | `ROOMLERD_UPDATE_HELPER=0` | the lab, with a live session across the install | M |
| **P3** | the installers: `install.sh` enables the units for a per-user install; a non-root `roomler self-update` wakes the helper (it bails today, `main.rs:4834-4864`) | — | the lab, a fresh install of each format | S |
| **P4** | docs: `docs/linux-self-update.md` (status, the ladder claim, the helper), `docs/installation.md` (a Linux row beside the macOS three-unit table, `:297-301`, and `:497`), `README.Debian`, the `docs/README.md` index row | — | — | S |

Each phase ships in an agent release (the `ship-it` skill owns the mechanics) and is field-verified before the
next depends on it.

## 5. Acceptance criteria

- [ ] **AC1**: a per-user **tarball** install (the lab's Fedora KDE guest) goes from release N to N+1 with no
  human action: on its timer, or within a minute of a pushed `rc:agent.update`. Red first on 0.4.130 (§2).
- [ ] **AC2**: a per-user **`.deb`** install (the lab's GNOME guest) does the same, with no `pkexec` or `sudo` and
  no password prompt on its screen.
- [ ] **AC3**: after the install, the per-user daemon runs the new version (the server's row reports it). A remote
  session open across the install is not cut by the restart, which waits for it to end (D1).
- [ ] **AC4**: a system install is unchanged. Its root daemon updates itself in-process, and the helper's units
  are not enabled.
- [ ] **AC5**: `/etc/roomler/disable-auto-update` stops the helper, and removing it resumes updates (a round
  trip, like `installer-smoke.yml`'s macOS one at `:1203-1252`).
- [ ] **AC6**: the wake file is advisory only. Its content is never read; a symlink planted at its path is
  unlinked, not followed; a directory planted there is moved aside and removed; and a burst of touches (or a
  planted entry) leaves `roomler-update.path` `active (waiting)`, never `failed` (unit tests, plus one lab check).
- [ ] **AC7**: `ROOMLERD_UPDATE_HELPER=0` gives back today's behaviour exactly.
- [ ] **AC8**: Docs: `docs/linux-self-update.md`, `docs/installation.md` and `README.Debian` updated in the house
  style (a mermaid diagram, tables, `file:line` anchors), and the `docs/README.md` index row.
- [ ] **AC9** (security review, #2055): the download is staged in a fresh private directory — a new 0755
  caller-owned dir per download, the write refuses a planted symlink (not followed) or file (not truncated), and
  the dir is removed once the installer has read it (unit tests, plus one lab check: during an install
  `/tmp/roomlerd-update.*` is `root root` 0755 and a pre-existing `/tmp/roomlerd-update/` is untouched).

## 6. Open decisions (operator)

| | decision | recommendation |
|---|---|---|
| **D1** | the restart onto the new binary: at once, or when the daemon is idle | when idle, with a ceiling (say 24 h): a restart under a live session cuts it, and the update is not urgent enough for that |
| **D2** | a backstop for every other replacement (an admin's `apt upgrade`, a hand install): the daemon notices its executable was replaced and restarts when idle | yes, in P2: it is the same restart, and without it a replaced binary waits for a reboot |
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
| 2026-10-12 | same run | same guest | ⚠️ **Three checks in the same run were inconclusive — test-script bugs, not code bugs, and not yet re-run clean.** (a) The opt-out check touched `/etc/roomler/disable-auto-update` without `mkdir -p /etc/roomler` first (the directory doesn't exist on a per-user install) — the touch failed, and the script's own 8 s wait (carried over from `fr96-lab.sh`, written before this review's 60 s gate existed) wasn't long enough to reach the opt-out check anyway, since the one real invocation was still mid-gate. (b) The symlink-not-followed scenario (AC6) planted its symlink *after* that same invocation had already started (and already consumed a different, earlier wake); `systemctl start` on an already-active oneshot unit is a no-op, so nothing ever processed this symlink — the observed "canary intact" is real but doesn't test what it was meant to. (c) The planted-directory scenario (F2) couldn't even set up: the leftover symlink from (b) was still sitting at the wake path, and `mkdir -p` failed `Permission denied` trying to `stat` through it (`fs.protected_symlinks` blocking root's *traversal* of a lower-uid symlink in sticky `/var/tmp` — plain `unlink()` is unaffected and wasn't tried here). All three behaviours are already covered in isolation by the unit tests (`consume_wake_unlinks_a_symlink_without_following_it`, `consume_wake_moves_a_planted_directory_aside_and_removes_it`); what's owed is the same clean, uncontended field confirmation F1/F2 just got — gate each step on `systemctl is-active roomler-update.service` returning `inactive` before touching the wake path again. |
