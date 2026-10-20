# FR-117: One version on every device — an update moves roomlerd, roomler and roomler-desktop together

**Issue:** [#2625](https://github.com/gjovanov/roomler-ai/issues/2625) · **Status:** proposed 2026-10-20, spec and
claim, on the operator's report (*"also noted that asahi has diff daemon and desktop version. we need to fix the
update … so that all are on the same version: roomlerd, roomler, roomler-desktop across all os"*) · **Owner:** the
updater (agent) · **Anchors:** master `0c1bb97f2` · **Builds on:** [FR-27](FR-27-consent-surfaces-and-desktop-companion.md)
(the companion, `companion_version` on the wire), [FR-84](FR-84-companion-and-installer-polish.md) (the companion's
launch), [FR-96](FR-96-per-user-linux-installs-update-themselves.md) (the root update helper),
[FR-67](FR-67-update-outcome-observability.md) (update outcomes)

## 1. Goal

After an update, a device runs **one version**: the daemon (`roomlerd`), the CLI (`roomler`) and the desktop companion
(`roomler-desktop`), on every OS. A device where they still differ says why.

## 2. Evidence (2026-10-20, after **Update all** moved the fleet to 0.4.147)

The operator's dashboard: `asahi-m2pro · v0.4.147 · desktop v0.4.139`. The census below covers every agent row that
reports a companion (`agents.companion_version`, read on mars, 18:20Z):

| Platform | Skew | Why |
|---|---|---|
| **Linux**, `.rpm` | `asahi-m2pro` (Fedora Asahi Remix 42, aarch64, online): daemon 0.4.147, companion 0.4.139. `rpm -q` → `roomler-desktop-0.4.139-1.aarch64`; `/usr/bin/roomlerd` and `/usr/bin/roomler` (0.4.147) are owned by no package: a tarball install | nothing upgrades the companion package |
| **Linux**, `.deb` | a3's Ubuntu cell (online): 0.4.147 / 0.4.146; offline lab hosts 0.4.142 / 0.4.129, 0.4.141 / 0.4.140 | the same |
| **Windows** | none online (9 of 9 at 0.4.147 / 0.4.147, the companion refreshed by the service host, e.g. `17:58:53Z installer signature verified asset=roomler-desktop-0.4.147-…exe`); the corp laptop CORPLAP-1 (offline, 2 rows): 0.4.146 / 0.4.145, with no uploaded log line saying why | the refresh runs only at the service's start; a failure waits for the next restart and says nothing |
| **macOS** | none: both Macs 0.4.147 / 0.4.147; the MacBook's `roomler`, `roomlerd` and `Roomler.app` all 0.4.147 from one package (`com.roomler.agent`) | the `.pkg` carries `Roomler.app` |

**The CLI is in sync by construction.** It ships inside every daemon package: the MSI's `roomler.exe` shim, the
`.pkg`, the `.deb`'s asset map, and the tarball, where `usr/bin/roomler` is a required member (`TARBALL_REQUIRED`,
`agents/roomlerd/src/updater.rs:2320`). Measured 2026-10-20: `roomler 0.4.147` on Windows, on the MacBook and on the
Asahi tarball install.

**Root cause.** `companion::refresh_if_stale` (`agents/roomlerd/src/companion.rs:881`) is Windows-only "by design".
Its premise (`companion.rs:888-899`) is that on Linux "the companion is its own `roomler-desktop` .deb that apt
upgrades". That is false: there is no apt or dnf repository. `install.sh` installs the companion once, from a release
asset (`desktop_install`, `scripts/install.sh:804`), and the daemon's updater picks only the daemon's own asset
(`is_daemon_asset`, `updater.rs:978`, on purpose: FR-27's companion-as-daemon freeze). So the companion package moves
only when a person re-runs the installer. The dashboard has shown the skew since FR-27 (`companion_version`), and
until today the fleet had none to show.

## 3. Key design

```mermaid
flowchart TD
    S([the update owner runs<br/>at its start, then every 4 h]) --> K{ROOMLERD_COMPANION_SYNC=0?}
    K -->|yes| N0[nothing: today's behaviour]
    K -->|no| Q{a roomler-desktop package<br/>installed? dpkg / rpm}
    Q -->|no| N1[nothing: a headless host stays headless]
    Q -->|yes| V{its version vs<br/>this daemon's own}
    V -->|equal| N2[nothing]
    V -->|newer| N3[log it, never downgrade]
    V -->|older| F[fetch agent-v OWN version<br/>pick roomler-desktop for this arch,<br/>.deb for dpkg / .rpm for rpm]
    F --> D[download_asset: digest, the pinned<br/>release key, the version binding]
    D -->|refused| R1[WARN, keep the old one,<br/>retry on the next tick]
    D -->|verified| I[install with the package manager<br/>needrestart suspended, a lock timeout]
    I -->|failed| R1
    I -->|ok| P[restart the companions it replaced,<br/>each in its person's session]
```

- **Linux: the companion tracks the daemon**, the invariant Windows has had since S1a (`companion.rs:3-29`). The one
  process that installs updates as root on the host also moves an installed `roomler-desktop` package to its **own**
  version (`CARGO_PKG_VERSION`), never "latest": the companion follows the daemon, not the feed.
  - **Who:** the owner `decide_update_owner` already names (`updater.rs:3518`, FR-96). That is a system install's root
    daemon, or `roomlerd update-helper` (`updater.rs:3723`) for a per-user install. A per-user daemon that sees the
    skew touches the helper's wake file (`/var/tmp/roomler-update-check`): a wake, never an instruction. A non-root
    daemon with no helper skips, with one log line, never a password prompt (D3).
  - **What:** the package database that owns the companion picks the format (`installed_version_inner`,
    `companion.rs:299`, reads dpkg then rpm). The asset is `roomler-desktop-<own>-<arch>-unknown-linux-gnu.<deb|rpm>`
    from release `agent-v<own>` (`fetch_release_by_tag`, `updater.rs:1745`). The 0.4.147 release carries all four, each
    with `.asc` and `.sha256`.
  - **Trust:** the same `download_asset` (`updater.rs:1545`) as the daemon's own update: the digest, the `.asc` against
    the key pinned in the binary, and the version binding. FR-97 P6a reads a `.deb`'s version, and P1 adds the `.rpm`
    reader (`rpm -qp --qf %{VERSION}`); `artifact_version.rs:139` has none today. A fresh private staging dir, as for
    the daemon (#2055).
  - **Install:** the host's own package manager, so it stays the source of truth. `apt-get install -y` (then
    `dpkg --install`) for a `.deb`; `dnf`, `zypper` or `yum` for an `.rpm`, in `install.sh`'s order
    (`desktop_rpm_tool`, `scripts/install.sh:793`). `NEEDRESTART_SUSPEND=1`: Ubuntu's needrestart hook otherwise
    restarts services, roomlerd among them, after an apt run (a vmtest trap, seen 2026-10-19). Then `DEBIAN_FRONTEND=noninteractive`
    and a dpkg lock timeout, so unattended-upgrades holding the lock costs one tick, not a hang.
  - **Restart:** a companion that was running is stopped and started again in its person's graphical session, through
    the path FR-27 already uses (`systemd_run_command`, `companion.rs:622`). One that was not running stays stopped.
  - **Never** installs a companion where none is installed, and **never** downgrades one (D1).
  - **The heartbeat:** `installed_version`'s 10-minute cache (`companion.rs:249`, `VERSION_TTL` `:264`) is dropped
    after a sync, so the dashboard's skew clears at the next heartbeat.
- **When:** at the owner's start, and again on a 4 h tick, so a failed sync no longer waits for the next restart.
- **Windows: the same retry.** `refresh_if_stale` runs once, at the start of whichever process owns the refresh (#1686:
  the SCM host, `win_service/mod.rs:471`, or the worker, `main.rs:3817`). It now runs on the same 4 h tick in that
  process. It costs one file read when the version marker matches.
- **Kill switch:** `ROOMLERD_COMPANION_SYNC=0` on both platforms gives back today's behaviour.

## 4. Phases

| Phase | What | Kill switch | Proven on | Size |
|---|---|---|---|---|
| **P0** | spec, claim, the census in §2 | — | ✅ 2026-10-20 | S |
| **P1** | Linux: the companion package tracks the daemon (root daemon + update helper; the per-user wake), the `.rpm` version binding | `ROOMLERD_COMPANION_SYNC=0` | | M |
| **P2** | Windows: `refresh_if_stale` on the 4 h tick in the process that owns it | `ROOMLERD_COMPANION_SYNC=0` | | S |
| **P3** | the sync outcome on the heartbeat, and the grid's skew says why (D4) | — | | M |
| **P4** | docs: `docs/linux-self-update.md`, `docs/desktop-companion.md`, the `docs/README.md` index row | — | | S |

## 5. Acceptance criteria

- [ ] **AC1** — On a Debian-family system install, a companion older than the daemon is at the daemon's version after
  the update, and one that was running runs the new binary. Red first, on the release before.
- [ ] **AC2** — The same on an RPM host: the Asahi box (Fedora, aarch64). Red: 0.4.147 / 0.4.139 today.
- [ ] **AC3** — A per-user install with its root helper: the helper moves the companion.
- [ ] **AC4** — A failed sync is retried on the next tick, not at the next restart, on Linux and on Windows.
- [ ] **AC5** — A host with no companion gets none. A newer companion is never downgraded. `ROOMLERD_COMPANION_SYNC=0`
  gives back today's behaviour.
- [ ] **AC6** — Only a verified package is installed: a bad digest, a missing or foreign `.asc`, or a version other
  than the tag is refused, and the companion keeps its version.
- [ ] **AC7** — After the release that carries it, the fleet census (§2's query) shows zero skew among online devices.
- [ ] **AC8** — Docs: `docs/linux-self-update.md` and `docs/desktop-companion.md` updated in the house style, and the
  `docs/README.md` index row.

## 6. Open decisions

| | Decision | Status |
|---|---|---|
| **D1** | A companion newer than the daemon | *(Recommended: leave it and log it. That is a person's deliberate install, or a lab, and the next daemon update catches up. Lockstep both ways would fight whoever installed it.)* |
| **D2** | A running companion after its package is upgraded | *(Recommended: restart it in its person's session, as the Windows swap does (#1686). Otherwise the person runs the old binary until they log out, and FR-27's consent prompt talks to it.)* |
| **D3** | A non-root daemon with no root helper | *(Recommended: skip, with one log line. Never a `pkexec` prompt for a background sync.)* |
| **D4** | The sync outcome on the wire | *(Recommended: P3, after P1/P2 ship: `companion_sync` on the heartbeat, so the grid's skew carries its reason. FR-67's point: an outcome nobody reads is not an outcome.)* |

## 7. Out of scope

- A tunnel-only host's standalone `roomler`: a different role with its own `self-update` (the LOW known issue in
  `CLAUDE.md`).
- An apt or dnf repository.
- Installing a companion on a host that never had one: `install.sh` and the wizard decide that.

## 8. Field-verification log

| Date | Host | Build | What | Result |
|---|---|---|---|---|
| 2026-10-20 18:18Z | `asahi-m2pro` (Fedora Asahi Remix 42, aarch64) | the released 0.4.147 | the red | 🔴 `roomlerd 0.4.147`, `roomler 0.4.147`, `roomler-desktop` 0.4.139 (`rpm -q` → `roomler-desktop-0.4.139-1.aarch64`) |
