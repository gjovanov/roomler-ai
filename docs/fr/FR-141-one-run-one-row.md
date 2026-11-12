# FR-141: The Setup wizard installs remote desktop and the private network in one run, on one device row, as a choice the person sees (macOS and Linux)

**Issue:** [#3679](https://github.com/gjovanov/roomler-ai/issues/3679) · **Status:** claimed 2026-11-11. P0 done the same day: the field read, on Linux and on macOS, is recorded in the [field log](#field-log). D1 to D3 decided the same day. P1 onwards not started, and nothing is built
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
  (`agents/roomlerd/packaging/macos/postinstall:188-380`). Inferred, never run.

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
- The system card's text says what it does not give yet: the clipboard, the keyboard layout, display match,
  and desktops other than GNOME and KDE. FR-142's first phase, under a key that is off by default, adds the
  clipboard on GNOME and KDE (Wayland and X11) and the layout and display match on X11 only; the other X11
  desktops wait for a way to see their lock, and Wayland's layout and display match are not planned. Decided
  as D3.
- Done names every row it made (`app.js:1862-1865`, `:1907-1913` already carry a root half).

### 2. One row from the wizard, on a Mac (P2)

What each layer has to do. The daemon needs nothing new: FR-43 P3a, P3b and P3d are built.

| Layer | Today | For one row |
|---|---|---|
| Pages (`app.js`, `index.html`) | "both" = two rows; a second token field | "Screen and private network" = one row while `wizard_macos_one_row` is on; one token field; Done names one row |
| Roles and tokens (`role.rs`, `commands.rs`, `pairing.rs`) | `wants_root_half` → `halves: 2`, a root token and a person's token | a third shape beside "screen only" and "two rows": one token, `halves: 1`, which enrolls the ROOT half |
| Enrollment (`orchestrator_unix.rs:867-937`) | the root half as `<name>-daemon`, overlay on | the root half under the person's own name, overlay on, with `macos_supervise_gui_worker` and `macos_one_row` set as `enroll --one-row` sets them (`agents/roomlerd/src/app.rs:8778-8784`); no person's enrollment, no `--user-steps` |
| Privileged half (`install.sh --privileged-steps --config-from`) | marker, package, config placed, daemon started | a one-row form, lifting `install.sh:296-299`: package, the binary's `enroll --help` lists `--one-row` (as `install.sh:1043-1048`), marker, config placed, the package again so that its guard removes the LaunchAgent and starts the root half (`install.sh:1072-1076`, `postinstall:347-380`); on a Mac that is one row already, the package only (`install.sh:1013-1031`) |
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
(`crates/localapi/src/lib.rs:482`, `:515-527`). So a one-row or supervised Mac is told apart, and the wizard enrolls no
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
| P0 | Field read on production's released wizard and script, recorded RED: a Mac by pairing, by paste with two tokens, "Screen only", a re-install over a two-row Mac (row ids, SSH policy, names), the wizard over a one-row Mac (`install.sh --one-row` first); Linux on the system card on GNOME Wayland, GNOME on Xorg and KDE; `install.sh --system` | read only | done 2026-11-11: Linux and macOS (cells M1 to M5), in the [field log](#field-log) |
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
  P0 read, cell M2 ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6803951312)): the
  paste path with the default choice still asks for two tokens and leaves two rows, so AC2 is not held yet.
- [ ] **AC3**: macOS, **"Screen only"**: one row, no LaunchDaemon, no marker, no overlay address.
  P0 read, cell M3 ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6803951312)):
  today's "Screen only", the Advanced box unticked, already leaves one row and no system daemon, but it still asks
  for the administrator password, and the daemon card still says the Mac joins as two devices.
- [ ] **AC4**: macOS, **a re-install** over a Mac this wizard made (one row, and, with the switch off, two rows):
  the same row id(s), names and SSH policy as before, and no new row. Shown RED or already green by P0 first.
  P0 read, cell M5 ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6804483259)): a
  wizard run over a Mac that the script installed as one row does not recognise it and leaves three rows, because
  the wizard and the script derive the machine's identity differently, so AC4 and AC5 are not held for a one-row
  Mac; that is P3's subject.
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

None is open. The operator decided all three on 2026-11-11, each as recommended.

- **D1. Macs that already have two rows: stay, or move to one row, and how? Decided 2026-11-11, as recommended:**
  they stay two rows until a person runs `sudo roomlerd one-row` (FR-43 P3c, field-verified on 0.4.200), and the
  wizard over such a Mac re-installs onto the same two rows and names that command on Done. Cost: those Macs keep
  two rows and two plan slots until someone acts. The other way, the wizard offering the migration when it finds
  two rows, costs a daemon restart inside the wizard that ends live sessions, and the retired row still needs a
  person's delete.
- **D2. Does `macos_one_row`'s default flip for script installs too (#971), or only the wizard's? Decided
  2026-11-11, as recommended:** the wizard's first (P2's switch), and the script's after the same field read, as
  #971's own decision. Cost: for a while the wizard and the one-liner give a Mac different row counts, and the
  dialog has to say which.
- **D3. What the Linux system card says until FR-142 ships. Decided 2026-11-11, as recommended:** name the three
  missing features and the two desktops, point at the per-user card for them, with its outbound-only mesh named,
  and promise nothing on Wayland beyond the clipboard: a per-user install does not read or switch the layout or
  match the display under Wayland either. Cost: a longer card. The other way, saying nothing, keeps today's
  surprise.

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
| 2026-11-11 | P0 RED, AC9: the released wizard (setup-v0.4.210) on an Ubuntu 24.04 GNOME Wayland lab guest ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6796598555)) | Welcome: three cards, the system card preselected; no question between screen with private network and screen only. The choice is the box "Join the overlay network (private mesh between your devices)" under the collapsed "Advanced options (optional)" on the Server page, ticked by default. Done names one Device ID and does not mention the private network. Not held, as the spec says for today. |
| 2026-11-11 | P0, AC7 today: the default system card, paired, on the same guest (agent 0.4.210) ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6796598555)) | One administrator prompt; one row, online, `wizard/system`. The Devices page row shows its overlay IPv4, a ULA, a MagicDNS name and Connect (read on the page's screenshot and the Devices API). The viewer's frame matched the real screen (RMSE 0.0035, 0.0002 after input); a click and a typed line landed (DRM scanout). `roomler0` up; 5/5 pings each way with the lab's anchor peer. `roomler ssh` gives the organization's default refusal, "roomler SSH is disabled for this organization (an admin must enable it)"; the setting was not turned on. |
| 2026-11-11 | P0 RED, AC10: the served `install.sh` (sha256 `e6ee3d13…`) on a fresh Ubuntu 24.04 GNOME Wayland lab guest ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6796598555)) | `--system --overlay`: "unknown flag: --overlay", then the usage, exit 1 (RED). `--role daemon --system --token …`: exit 0; one row `script-sh/system`, no overlay address or MagicDNS (Devices API), no `roomler0`, `overlay_enabled = false`; a session shows the screen. |
| 2026-11-11 | P0, AC8 today, GNOME on Xorg: the released wizard's preselected system card, paired, on an Ubuntu 24.04 lab guest switched to GNOME on Xorg before the install (agent 0.4.210) ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6797644030)) | One administrator prompt. Done names one Device ID ("System service (root)", 0.4.210) and says nothing about the private network. One row, online, `wizard/system`, reporting `desktop {session: user, type: x11, capture_arm: drm, input_reach: full}`. `roomler0` up (100.65.x.x/22 and a ULA /96); 5/5 pings to the anchor peer (1-2 ms) and 5/5 back (1.9-2.3 ms). The viewer's frame matched the real screen (RMSE 0.0040; "DRM scanout · asks nobody", VP9 4:4:4 SW). A click and a typed line landed in a second session (0.0002 after input); the first session's click missed through a harness fault. `roomler ssh`: the organization's default refusal. Held on GNOME on Xorg, except "its overlay address shown": the Devices page row was not read on this cell. |
| 2026-11-11 | P0, AC8 today, KDE Plasma on X11: the released wizard (setup-v0.4.210) on an Ubuntu 24.04 lab guest switched to Plasma 5.27's X11 session (SDDM) before the install. The wizard's install step got agent 0.4.211: `agent-v0.4.211` was published at 13:41:40Z, seconds before it resolved the latest release (the GNOME on Xorg guest at 13:22 got 0.4.210) ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6797644030)) | The wizard starts on Plasma: the same three cards, the system card preselected and taken; one administrator prompt (KDE's polkit dialog); Done names one Device ID ("System service (root)", 0.4.211) and says nothing about the private network. One row, online, `wizard/system`, reporting `desktop {session: user, type: x11, desktop: KDE, capture_arm: drm, input_reach: full}`. The Devices page row shows its overlay IPv4, a ULA, a MagicDNS name and Connect. The viewer's frame matched the real screen (RMSE 0.040: a transient "Being viewed by …" notice; 0.0031 after input); a click and a typed line landed (DRM scanout). `roomler0` up; 5/5 pings each way with the anchor peer; `roomler ssh`: the organization's default refusal. Held on KDE Plasma X11 (agent 0.4.211); Plasma on Wayland not read. |
| 2026-11-11 | P0 RED, AC9 on GNOME on Xorg and KDE Plasma X11: the same two guests ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6797644030)) | As on GNOME Wayland: the system card preselected, no question on the first page, the private network is the Advanced box "Join the overlay network …", ticked by default under the collapsed "Advanced options (optional)". Not held, as the spec says for today. |
| 2026-11-11 | P0 RED, AC10 again with agent 0.4.211 served: the served `install.sh` (sha256 `e6ee3d13…`, unchanged since 12:29) on an Ubuntu 24.04 lab guest ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6798060335)) | `--system --overlay`: "unknown flag: --overlay", exit 1 (RED). `--role daemon --system`: exit 0; one row `script-sh/system`, 0.4.211; no `roomler0`, `overlay_enabled = false`. After a reboot into XFCE on X11, the same row reports `desktop {session: none, …}` and a session is refused (`x11_not_enabled`). |
| 2026-11-11 | P0 M1, macOS 27.2 lab guest, setup 0.4.211, pairing, defaults, read 15:11 to 15:34Z ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6800327603)) | Two rows (`<name>` wizard/per-user, `<name>-daemon` wizard/system), both online; the question is not on the first page (the overlay box under Advanced, ticked); Done names both rows; mesh both ways on the daemon row; the screen row has no overlay address. AC1 RED, AC9 RED (Done half held), AC6 held. Install's end: two TCC alerts and System Settings cover Done. |
| 2026-11-11 | P0 M4, the same guest, the wizard again, read 15:37 to 15:44Z ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6800327603)) | Same two row ids, no new row, the renamed name and the daemon row's SSH policy kept; the wizard's words before acting are a first install's (nothing about reuse), it pre-fills the host name, Done still says "One manual step remains". AC4 held today (two-row case); AC5 waits on D1. |
| 2026-11-11 | P0 M2, macOS 27.2 lab guest, setup 0.4.211, two pasted tokens, defaults, read 18:04 to 18:16Z ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6803951312)) | The paste section asks for two tokens ("Token" + "Second token, for the private network"); one administrator prompt; two rows (`<name>` wizard/per-user, `<name>-daemon` wizard/system) online; LaunchDaemon + marker; mesh both ways on the daemon row; Done names both rows. AC2 RED, AC6 held, AC9 Done held. |
| 2026-11-11 | P0 M3, a fresh lab guest, setup 0.4.211, pairing, "Join the overlay network" unticked, read 18:16 to 18:25Z ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6803951312)) | Pairing asks `halves: 1`; still one administrator prompt (the package); one row `<name>` wizard/per-user online; no LaunchDaemon, no `enable-daemon` marker, no root config, no overlay address; Done names the one row. AC3 held. |
| 2026-11-11 | P0 M5, a fresh macOS 27.2 lab guest: served `install.sh --one-row` under sudo (one token), then the released wizard (setup 0.4.211), pairing, defaults, read 18:31 to 18:48Z ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6804483259)) | The script gives one row `<name>` (`script-sh/system`, online, one-row switches on, no LaunchAgent). The wizard's Welcome says "enrolled both per-user and as a system daemon … two daemons"; pairing asks `halves: 2`; nothing says the Mac is one row; one administrator prompt. After: three rows — a new `<name>-daemon` (`wizard/system`, online; the root half took the hardware fingerprint, the script had used a derived one), the old one row offline, a new person's row (`wizard/per-user`) offline; one LaunchDaemon in one-row mode, no LaunchAgent; mesh both ways. RED for AC4/AC5 (new rows, the old row left offline, nothing said before acting). |
