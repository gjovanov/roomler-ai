# FR-141: The Setup wizard installs remote desktop and the private network in one run, on one device row, as a choice the person sees (macOS and Linux)

**Issue:** [#3679](https://github.com/gjovanov/roomler-ai/issues/3679) · **Status:** claimed 2026-11-11. Nothing built. P0 (the field read that records RED) comes first
**Builds on:** FR-129 ([#3022](https://github.com/gjovanov/roomler-ai/issues/3022), B2: the wizard's macOS root half), FR-43 ([#971](https://github.com/gjovanov/roomler-ai/issues/971), P3: one macOS row behind `macos_one_row`)
**Related:** [FR-142](FR-142-linux-system-install-full-desktop.md) (the Linux system install's full desktop, which this FR does not wait on), #3247, #3240, #2892, #3660, #3535

## Goal

On macOS and Linux the first page of Roomler Setup asks one question: **"Screen and private network"** (the
default) or **"Screen only"**. One run with the default leaves a machine that:

- can be remote-controlled in the person's own desktop session, from the web app's Devices page;
- is a member of the overlay mesh, with its Roomler address(es) shown on that same row, peers and `roomler ssh`;
- is **one device row**, on a Mac too, wherever one row holds. Where it does not, the Mac keeps today's two rows.

A re-install lands on the same row(s), with their settings.

## Evidence

Anchors are master `1e29eeae6` (2026-11-11). "Code" means read in the tree; "field" means read on a device;
"inferred" means neither.

### What the operator decided (2026-11-11)

- **A Mac ends as ONE row, if possible**: the row a person opens for remote desktop from the Devices page,
  showing the machine's overlay address(es). That is FR-43 P3's shape. "If possible" is the kill switch.
- **Linux: both, on one row if possible.** Today's system card stays the one-run path. A system install that
  also gives the person's full desktop is [FR-142](FR-142-linux-system-install-full-desktop.md).
- **The choice moves to the first page**, the Advanced box mirroring it.
- **An opt-in `install.sh --overlay` on Linux.**

Today the operator's own Mac shows two rows: one for the screen and one named `<host>-daemon` for the mesh.

### macOS today (code; the two rows also field-read in a lab on 2026-11-10)

- One daemon card, "Be accessed — this Mac (recommended)", whose text says the Mac "joins as two devices" and
  that screen-only means unticking a box under Advanced (`agents/roomler-setup/src/front/app.js:75-83`).
- The "both" control is that box, "Join the overlay network", inside the collapsed "Advanced options" on the
  Server page (`agents/roomler-setup/src/front/index.html:166-175`), ticked by default for the macOS daemon
  (`app.js:490-494`). It drives `role::wants_root_half` (`agents/roomler-setup/src/role.rs:124-132`), mirrored in
  the SPA (`app.js:496-503`).
- Both token paths add the root half: pairing asks `halves: 2` (`role.rs:134-140`) and refuses an install whose
  halves differ from the approval's (`agents/roomler-setup/src/pairing.rs:262-305`); the paste path shows a
  second token field (`index.html:282-292`) and refuses a missing or repeated second token
  (`agents/roomler-setup/src/commands.rs:798-838`).
- The server accepts `halves: 2` only for a macOS daemon (`crates/modules/fleet/src/pairing.rs:326-334`) and mints
  that many single-use tokens (`:985-1004`). Each half counts against the device cap
  (`docs/fr/FR-129-guided-cross-platform-installer.md:580-584`).
- One run: the root half enrolls in the wizard's process as `<name>-daemon` (`role.rs:142-147`) under a machine id
  from the hardware (`agents/roomler-setup/src/orchestrator_unix.rs:867-916`); one administrator prompt runs the
  privileged half (marker, package, config placed, daemon started: `scripts/install.sh:884-891`, `:949-983`); the
  person's half enrolls with the overlay keys stripped (`orchestrator_unix.rs:143-185`); `--user-steps` starts the
  LaunchAgent (`install.sh:938-947`). Two rows.
- **One row exists, but only in the terminal.** FR-43 P3a to P3d are built and field-verified on released 0.4.200
  and 0.4.201 behind `macos_one_row`, default off (`docs/installation.md:2016-2053`). `install.sh --one-row` is
  the one-token path (`install.sh:991-1087`); it is refused with the halves the wizard runs: "have no one-row
  form yet" (`install.sh:296-299`). The docs say the wizard has no one-row control
  (`docs/installation.md:2146-2150`).
- **The wizard cannot see a Mac's root daemon before it enrolls.** `root_identity::query` answers on Linux only
  (`agents/roomler-setup/src/root_identity.rs:67-79`), so on a Mac that is already one row or supervised the
  wizard would enroll a new person's half and start a LaunchAgent that the package's guard removes
  (`agents/roomlerd/packaging/macos/postinstall:186-239`). Inferred, never run.

### Linux today (code)

- The system card `daemon-system` is preselected on a clean machine (`role.rs:101-108`, `:287-305`), with the
  overlay box on by default for both scopes (`orchestrator_unix.rs:125-135`, `app.js:490-491`). It gives one row:
  the root unit with the TUN mesh, exit node and subnet router (`docs/platform-support.md:126-129`).
- Its desktop is reduced: GNOME or KDE, Wayland or X11, only while unlocked and on screen; no clipboard
  (`no-session`), no keyboard layout, no display match (`docs/platform-support.md:97`, `:106`, `:108-109`). That
  is FR-142's work.
- The per-user card has the full desktop and a mesh that is outbound only, through a loopback SOCKS5 front
  (`docs/platform-support.md:79-83`).
- **The script never turns the overlay on.** `enroll_daemon` passes no `--overlay` (`scripts/install.sh:1130-1186`),
  and the daemon's own system hint keeps it off on purpose: "a Linux system node is not forced onto the mesh"
  (`agents/roomlerd/src/service.rs:2324-2329`).
- The web app's add-device dialog says, for a Linux machine-wide install, "Screen capture needs a logged-in
  session — pick 'This user only' for desktops" (`ui/src/utils/enrollCommands.ts:239-241`). That predates FR-95
  and FR-113 and contradicts the wizard's recommended system card. Its macOS block gives a one-token one-liner,
  screen only, and says to mint a second token by hand (`enrollCommands.ts:254-258`).

### A re-install (code, plus a lab record that is NOT a read)

The 2026-11-10 lab record suggests that a wizard re-install on a Mac gave the root half a NEW row (the old one
stayed offline) without its SSH policy, while the screen row kept its name. Nobody read that directly. The code
says when a new root row is expected: the root half's id comes from the platform UUID, and on a virtual Mac its
`en0` address too (`crates/agent-core/src/machine.rs:130-156`); if those cannot be read it is a one-time id, and a
later re-install is a new row, said on the page (`machine.rs:169-196`); a root half first enrolled from a terminal
(`--daemon-token`, the host-name id) gets one new row at its first wizard re-install
(`docs/fr/FR-129-guided-cross-platform-installer.md:619-627`). A row's settings live on the server row, so a new
row starts with defaults (inferred). P0 shows which case the lab hit.

## Design

### 1. The question on the first page (P1)

- Under the daemon card on the Welcome page (`index.html:53-88`), on macOS and Linux, two options: **"Screen and
  private network"** (default) and **"Screen only"**. They set the state the Advanced box sets today
  (`state.overlayChoice`, `app.js:510-515`), and the box stays and mirrors them.
- Each option says what it installs on this OS and card: on a Mac one row, or two rows and two plan slots while
  P2's switch is off (today's hint, `app.js:525-527`); on Linux the TUN for the system card, the SOCKS front for the
  per-user card (`app.js:522-524`).
- The system card's text says what it does not give yet (clipboard, layout, display match, desktops other than
  GNOME and KDE) and that FR-142 adds it: open decision D3.
- Done names every row it made (`app.js:1862-1865`, `:1907-1913` already carry a root half).

### 2. One row from the wizard, on a Mac (P2)

What each layer has to do. The daemon needs nothing new: FR-43 P3a, P3b and P3d are built.

| Layer | Today | For one row |
|---|---|---|
| Pages (`app.js`, `index.html`) | "both" = two rows; a second token field | "Screen and private network" = one row while `wizard_macos_one_row` is on; one token field; Done names one row |
| Roles and tokens (`role.rs`, `commands.rs`, `pairing.rs`) | `wants_root_half` → `halves: 2`, a root token and a person's token | a third shape beside "screen only" and "two rows": one token, `halves: 1`, which enrolls the ROOT half |
| Enrollment (`orchestrator_unix.rs:867-937`) | the root half as `<name>-daemon`, overlay on | the root half under the person's own name, overlay on, with `macos_supervise_gui_worker` and `macos_one_row` set as `enroll --one-row` sets them (`agents/roomlerd/src/app.rs:8774-8780`); no person's enrollment, no `--user-steps` |
| Privileged half (`install.sh --privileged-steps --config-from`) | marker, package, config placed, daemon started | a one-row form, lifting `install.sh:296-299`: package, the binary's `enroll --help` lists `--one-row` (as `install.sh:1043-1048`), marker, config placed, the package again so that its guard removes the LaunchAgent and starts the root half (`install.sh:1072-1076`, `postinstall:221-239`); on a Mac that is one row already, the package only (`install.sh:1013-1031`) |
| Server pairing (`crates/modules/fleet/src/pairing.rs:326-334`) | `halves: 2` for a macOS daemon only | no change: `halves: 1` is accepted for every OS. `halves: 2` stays for two-row installs and older wizards. The `/pair` page shows its two-entries note only for `halves === 2` (`ui/src/views/pair/PairView.vue:110-111`) |
| Server features (`/api/setup/features`) | `wizard_macos_daemon` | a new key, `wizard_macos_one_row`, default off |
| Devices page (`ui/src/components/admin/AgentsSection.vue`) | the per-user row has no overlay address; the `-daemon` row has one | expected no change: the Overlay column reads the row's own address (`:325-333`) and Connect is the row's (`:898-901`), and a one-row Mac's row holds the node and reports the worker's capabilities (FR-43 P2c). Inferred: the field read confirms it |

The switch's default flips to on once P2's field read holds (AC1, AC2, AC4): that flip is the operator's. Where
one row does not hold, the switch stays off and the wizard installs today's two rows.

### 3. A re-install lands on the same row(s) (P3)

P0 decides its shape. What the code already gives: the root half's hardware id and `adopt-enrollment`'s fold
(`orchestrator_unix.rs:867-875`), and the warning a Linux re-install gets from `root_identity`
(`orchestrator_unix.rs:832-862`). What it lacks on a Mac: any look at the root daemon before the enroll
(`root_identity.rs:67-79`). The privileged half runs as root and can read the root's config as
`root_config_says` does (`install.sh:985-989`), and the root daemon's `Status` already reports `gui_worker.one_row`
(`crates/localapi/src/lib.rs:482-497`). So a one-row or supervised Mac is told apart, and the wizard enrolls no
person's half on it and says why.

### 4. The terminal and the web dialog (P4)

- `install.sh --overlay` on Linux, opt-in: with `--system` the TUN, per-user the netstack, as the wizard does. The
  default stays off, as the daemon's hint says (`service.rs:2324-2329`).
- The dialog's Linux note says what a system install shows today (`enrollCommands.ts:239-241`). Its macOS block
  puts the wizard first where the server offers it (`enrollCommands.ts:198-203`) and says what the one-liner
  installs.

## Phases

| phase | what | kill switch | status |
|---|---|---|---|
| P0 | Field read on production's released wizard and script, recorded RED: a Mac by pairing, by paste with two tokens, "Screen only", a re-install over a two-row Mac (row ids, SSH policy, names), the wizard over a one-row Mac (`install.sh --one-row` first); Linux on the system card on GNOME Wayland, GNOME on Xorg and KDE; `install.sh --system` | read only | not started |
| P1 | The question on the first page (design 1), with `front_tests` | server key `wizard_mesh_choice` in `/api/setup/features`, default on: off restores today's layout | not started |
| P2 | One row from the wizard on a Mac (design 2) | server key `wizard_macos_one_row`, default off: off = today's two rows | not started |
| P3 | A re-install lands on the same row(s); a one-row or supervised Mac gets no person's half (design 3) | server key `wizard_reinstall_guard`, default on: off = today's enroll | not started |
| P4 | `install.sh --overlay` on Linux; the dialog's Linux note and macOS block | the flag is opt-in; the dialog rolls back with the image | not started |
| P5 | Docs: `docs/installation.md` (the wizard's macOS and Linux sections, the dialog, `--overlay`), `docs/platform-support.md` §2 and §4, `docs/device-pairing.md` | none | not started |
| P6 | Field verification on released builds (the criteria below) | none | not started |

Each wizard phase ships in a `setup-v*` release; a bad one is rolled back by the release.

## Acceptance criteria

All are field reads on released builds, on throwaway lab guests, unless they say otherwise. "Online" means read
on the Devices page.

- [ ] **AC1**: macOS 15 or later, the released wizard, **pairing**, the default choice, one-row switch on: **one**
  row, online. From that row on the Devices page: its overlay address(es) shown, a remote-desktop session that
  streams and takes input; `sudo roomler peers` lists the mesh; a ping over the overlay both ways; `roomler ssh`
  once the device's `ssh_enabled` is on (`docs/installation.md:2156-2157`).
- [ ] **AC2**: the same with a **pasted** token: one field, one token, one row.
- [ ] **AC3**: macOS, **"Screen only"**: one row, no LaunchDaemon, no marker, no overlay address.
- [ ] **AC4**: macOS, **a re-install** over a Mac this wizard made (one row, and, with the switch off, two rows):
  the same row id(s), names and SSH policy as before, and no new row. Shown RED or already green by P0 first.
- [ ] **AC5**: macOS, **a Mac that already has two rows**: the wizard does what D1 decides, says so before it acts,
  and leaves no third row.
- [ ] **AC6**: macOS with the one-row switch **off**: today's two rows, `<name>` and `<name>-daemon`, both online,
  the mesh on the second.
- [ ] **AC7**: Linux, the default system card on Ubuntu **GNOME Wayland**: one row, online, its overlay address
  shown; a session shows the person's desktop with input; the TUN up, a ping both ways, `roomler ssh` answers.
- [ ] **AC8**: the same on **GNOME on Xorg** and on **KDE Plasma**.
- [ ] **AC9**: the question sits on the first page on macOS and Linux, defaults to both, the Advanced box follows
  it, and Done names every row made. `front_tests` cover it (CI); the page is read in the field.
- [ ] **AC10**: the **script**: `install.sh --system --overlay` on Linux gives AC7's row from a terminal, and
  `install.sh --system` alone still leaves the overlay off.
- [ ] **AC11**: the web dialog's Linux and macOS blocks say what each path installs, read on the live page.
- [ ] **AC12**: docs updated in house style (tables, mermaid where a flow is drawn, `file:line` anchors), linked
  from a row in `docs/README.md`.

## Open decisions

- **D1. Macs that already have two rows: stay, or move to one row, and how?** Recommendation: they stay two rows
  until a person runs `sudo roomlerd one-row` (FR-43 P3c, field-verified on 0.4.200), and the wizard over such a
  Mac re-installs onto the same two rows and names that command on Done. Cost: those Macs keep two rows and two
  plan slots until someone acts. The other way, the wizard offering the migration when it finds two rows, costs a
  daemon restart inside the wizard that ends live sessions, and the retired row still needs a person's delete.
- **D2. Does `macos_one_row`'s default flip for script installs too (#971), or only the wizard's?**
  Recommendation: the wizard's first (P2's switch), and the script's after the same field read, as #971's own
  decision. Cost: for a while the wizard and the one-liner give a Mac different row counts, and the dialog has to
  say which.
- **D3. What the Linux system card says until FR-142 ships.** Recommendation: name the three missing features and
  the two desktops, and point at the per-user card for them, with its outbound-only mesh named. Cost: a longer
  card. The other way, saying nothing, keeps today's surprise.

## Out of scope

- Windows: `daemon-system` is already one service and one row with the desktop and the TUN.
- The Linux system install's full desktop: [FR-142](FR-142-linux-system-install-full-desktop.md). This FR's Linux
  criteria are met by today's system card.
- A per-user Linux install with a TUN mesh: it needs a root daemon beside it, which the one-daemon rule forbids
  (`docs/multi-org.md:35-51`).
- The macOS privacy grants themselves, which a person must give.
- Any change to `POST /api/agent/enroll` or to token audiences.

## Field log

| date | what | result |
|---|---|---|
