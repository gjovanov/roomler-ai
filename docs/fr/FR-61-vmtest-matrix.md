# FR-61: Throwaway-OS install & verify matrix ("vmtest") — every method, every type, every OS, on demand

**Issue**: [#1199](https://github.com/gjovanov/roomler-ai/issues/1199)
**Status**: proposed
**Repos**: `roomler-ai` (this spec, one Playwright spec), `roomler-ai-deploy` (orchestrator +
lanes), `k8s-cluster-multi` (host capability)

## Goal

An on-demand harness that boots **lean throwaway VMs** on the fleet hosts, installs roomler in
each supported OS **via the served script AND via the release installer, per install type**, and
verifies three things per cell before destroying the VM:

1. **Remote desktop works** — a real browser session decodes frames from the freshly installed
   agent.
2. **The overlay works** — `roomler peers` sees an anchor node and `roomler ping` succeeds in
   both directions.
3. **`roomler-desktop` works across all its pages** — every view renders.

The product's acceptance bar is "it just works" on machines people actually have. Today that
bar has no harness: a fresh install on a clean OS is exercised only when a human happens to do
one.

## Evidence (why this exists)

- `installer-smoke.yml` covers exactly **2 cells** — Windows **perUser** MSI install/uninstall
  on a GitHub runner and a macOS `.pkg` install. The perMachine wxs is only *compiled* ("no
  install"). No Linux `.deb` install smoke, no perMachine/SystemContext install, no ARM, no
  Wayland, no enrollment, no remote desktop, no `roomler-desktop` — anywhere in CI.
- Every recent install-path defect shipped precisely because nothing installs the product the
  way users do, and was found by a **human** doing a field install:
  - FR-50 (#1083): the served installer could not know which server served it — found by
    FR-42's clean-box run.
  - FR-49 (#1084): a second org got no mesh while **five** surfaces reported healthy — found by
    enrolling demo devices by hand.
  - macOS "first-class" arc (2026-08-23): the install ran nothing, the pkg relocated itself
    into its own build directory, the companion could not start — five independent defects,
    none catchable by any lane that existed, found by a brand-new MacBook.
  - FR-53 (#1104): a recovered device warned about a crash loop for seven releases — found by
    pointing a camera at a screen.
- FR-51 (#1095) shipped ephemeral nodes and field-verified them against throwaway docker
  containers — the enrollment/reaping machinery this harness needs already exists and is live
  on prod (0.4.46).

## The matrix (v1)

| # | Lane (host) | Method | Types | RD check | Desktop check |
|---|---|---|---|---|---|
| 1 | **Win11 x86_64** — KVM on zeus, OVMF+swtpm, autounattend golden image, autologon + OpenSSH baked | `install.ps1` AND silent MSI | system (perMachine `ENABLE_SYSTEM_CONTEXT=1`) / attended (perMachine) / per-user (perUser MSI) | Playwright | CDP page-walk (5 views) + screenshots |
| 2 | Win11 **wizard smoke** | `roomler-setup` driven over CDP | one flavour (perUser) | – | wizard reaches Done; row deleted after |
| 3 | **Ubuntu 24.04 GNOME Wayland x86_64** — KVM on zeus/mars, cloud-init golden image, GDM autologin on Wayland | `install.sh` AND `.deb` via dpkg | system (`--system`; `drm_capture`+`uinput`) / per-user (user unit; `mutter_capture`) | Playwright | launch + `--view` walk + `virsh screendump` per view |
| 4 | **ARM Linux** — Ubuntu arm64 under QEMU **TCG** on zeus (the fleet is x86), AAVMF UEFI, headless + virtual-desktop mode | `install.sh` AND aarch64 `.deb` | system | Playwright, long timeouts | N/A — no aarch64 companion asset; assert the graceful skip |
| 5 | **macOS arm64** — tart VM on the operator's MacBook Pro (opt-in lane; auto-skipped when unreachable), TCC pre-granted in the golden image | `install.sh` (± `--daemon-token`) AND `installer -pkg` (± daemon marker) | per-user (LaunchAgent only) / system (marker ⇒ + root LaunchDaemon, two device rows) | Playwright | launch + screenshot walk |

Cells that cannot exist are recorded as N/A with the reason, not silently skipped:
`machine-attended` is a Windows-only concept (Linux = {system, per-user}; macOS = {agent-only,
agent+daemon}); ARM has no companion and no Windows; macOS has no Intel asset.

## Key design

### Per-cell flow

```
COW-clone golden image → boot → SSH in
→ install (method × type)            # script lanes use --no-enroll / -NoEnroll
→ roomlerd enroll --ephemeral        # reusable EphemeralEnrollment key, vmtest org
→ roomler config set auto_update false
→ service up
→ CHECKS: agent online (API) → peers/ping ↔ anchor → Playwright RD → desktop walk
→ TEARDOWN: graceful shutdown (ephemeral self-unenroll) → destroy
→ end of run: assert org device list back at baseline (reaper is the backstop)
```

### What each repo owns

- **k8s-cluster-multi** — `playbooks/15-vmtest.yml` (opt-in, NOT in `site.yml`, like 16) +
  `roles/vmtest-host`: qemu-system-aarch64 + AAVMF + OVMF + swtpm, a `vmtest-net` NAT network
  on its own bridge (the `-i virbrX -j ACCEPT` HOST_FW_INPUT rule ships with it — the
  documented DHCP-hang foot-gun), `/var/lib/libvirt/vmtest` storage with a disk preflight, and
  cached images (noble amd64+arm64 cloud images, virtio-win ISO, Win11 Enterprise Eval ISO).
- **roomler-ai-deploy** — `vmtest/`: `vmtest.sh` (`bake|run|destroy|report|clean`, filters
  `--lane --method --type --host --keep`), per-lane bake+run scripts, guest drivers, dockerized
  Playwright runner, report in the e2e-nightly shape (`~/vmtest/<stamp>/`, `LATEST`,
  `expected-failures.txt` diff, isolated re-run for unexpected failures, GitHub issue on
  regression).
- **roomler-ai** — `ui/e2e/vmtest-remote.spec.ts`, modeled on
  `remote-session-smoke.spec.ts` with exact-name agent selection (`E2E_AGENT_NAME`) so multiple
  live VMs cannot cross-match.

### The facts the design rests on (anchors verified against master)

- **Install scripts**: `scripts/install.sh` — `--role daemon|tunnel --server --token --name
  --system --no-enroll --download-only --desktop`; per-user default (`systemctl --user`,
  `roomler.service`) vs `--system` (`/etc/roomler/config.toml` + `roomlerd.service`); macOS
  `--daemon-token` writes the `/etc/roomler/enable-daemon` marker before the pkg (two rows by
  construction). `scripts/install.ps1` — `-Role daemon-user|daemon-machine|daemon-system
  -Token -Name -NoEnroll -AllowElevated`; `daemon-user` **throws in an elevated shell** without
  `-AllowElevated`; `daemon-system` adds `ENABLE_SYSTEM_CONTEXT=1` and enrolls
  `--machine-global`. Both carry the FR-50 serve-time server-URL rewrite, so the script lane
  must fetch from the server under test, never from the repo.
- **MSI**: two products (perUser task-autostart, `agents/roomlerd/wix/main.wxs`; perMachine SCM
  service, `agents/roomlerd/wix-perMachine/main.wxs`); the third type is the property flip
  `ENABLE_SYSTEM_CONTEXT=1` — the **only** public property (`wix-perMachine/main.wxs:395`).
  Enrollment is always post-MSI (`roomlerd enroll`, `--machine-global` for the machine
  flavours); the perMachine service auto-starts at install, so the ephemeral enroll may need a
  stop-enroll-start dance (`enroll --ephemeral` refuses if a config exists —
  `agents/roomlerd/src/main.rs:96-102`).
- **Consent**: `resolve_session_authz` (`crates/api/src/ws/remote_control.rs:1708`) resolves
  the device **owner** to `owner_consent_mode()` = Auto unless `prompt_owner`; the agent-local
  `auto_grant_session` defaults **true** at enroll. Controller = the enrolling admin ⇒ RD
  checks run with zero prompts, by design, not by weakening anything.
- **Release assets**: aarch64 Linux `.deb`/`.tar.gz` exist (no `ffmpeg-encoder` — SW encode
  only); there is **no aarch64 `roomler-desktop`** and **no Intel macOS** asset; the macOS
  `.pkg` is `aarch64-apple-darwin` only.
- **roomler-desktop**: 5 views (`overview|devices|tunnels|settings|onboarding`,
  `agents/roomler-desktop/src/front/app.js`), `--view=` deep-link only fires via the
  single-instance second launch; Tauri ⇒ on Windows the WebView2 runtime honours
  `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<p>` from the environment, so
  the page-walk is CDP-driven DOM assertion; Linux WebKitGTK has no CDP ⇒ `virsh screendump`
  per view + LocalAPI state instead.
- **Verification CLI**: `roomler peers --json`, `roomler ping <target> --json` (failure ⇒
  exit 1), `roomler status --json` (`agents/roomler-cli/src/cli.rs`).
- **Ephemeral (FR-51)**: reusable keys gated by org `ephemeral_keys_enabled` (re-checked per
  use ⇒ flipping it off is class-wide revocation); reaper
  `ROOMLER__RC__EPHEMERAL_REAPER_ENABLED` already on in prod; graceful shutdown self-unenrolls.
  The wizard smoke cannot use an ephemeral key (wrong JWT audience for plain `enroll`) — it
  mints a single-use standard enrollment token and deletes its row afterwards.

### Safety rails

- **jupiter carries prod storage** (mongo/minio/roomler2 PVCs) — vmtest schedules on zeus (and
  mars for overflow); jupiter only in an announced window.
- **Sequential by default** — one VM per host at a time (≤8 GB peak), so the k8s VMs should not
  need shrinking; a capacity audit prints before every run, and the shrink runbook
  (`virsh setmem` + the `group_vars/all.yml vms:` mirror edit) exists but executes only on a
  measured shortfall.
- **Prod isolation** — a dedicated vmtest org; ephemeral rows only (plus one permanent anchor
  with `auto_update=false`); nothing in the prod fleet org is touched; the reaper is already
  prod-verified (FR-51 AC5: permanent rows survive).
- All host capability is **opt-in** (playbook outside `site.yml`; nothing scheduled; no
  standing VMs between runs unless `--keep`).

## Phases

| Phase | What | Kill switch / rollback |
|---|---|---|
| P0 | This spec + ledger row + issue | — |
| P1 | k8s-cluster-multi: `vmtest-host` role + playbook 15, capacity audit, shrink runbook | playbook is opt-in; role removal reverts the host |
| P2 | roomler-ai-deploy: `vmtest.sh` orchestrator + Ubuntu GUI Wayland lane; vmtest org + anchor + ephemeral key | org `ephemeral_keys_enabled=false` revokes all keys; anchor container stop |
| P3 | roomler-ai: `vmtest-remote.spec.ts` (RD check, name-filtered) | spec is env-gated, skips without `E2E_AGENT_NAME` |
| P4 | Win11 lane: golden image bake + 3 types × 2 methods + wizard smoke + CDP desktop walk | delete image + overlays |
| P5 | ARM lane (TCG on zeus): system × 2 methods; RD best-effort behind expected-failures | delete image + overlays |
| P6 | macOS lane (tart on the MacBook, opt-in): 2 types × 2 methods | `tart delete`; lane auto-skips when host absent |
| P7 | **Run and tweak** — every supported cell to green on the live fleet; fail-first evidence per cell class; Result comment with the matrix | — |
| P8 | Repeatable skill (`vmtest`) — written after P7 proves the flow | — |

## Acceptance criteria

- [x] AC1 — Ubuntu 4/4 GREEN (script + `.deb` × system + per-user; Wayland session verified
  `seat0 ... wayland`; per-user via netstack). Field-verified prod 0.4.48, 2026-09-02.
- [x] AC2 — Win11 6/6 GREEN (`install.ps1` daemon-system/machine/user AND silent perMachine/
  perUser MSI incl. `ENABLE_SYSTEM_CONTEXT=1`; SystemContext overlay = two roomlerd, session-0
  supervisor + session-1 worker; per-user via netstack). Field-verified 0.4.48, 2026-09-02.
- [~] AC3 — the wizard smoke is coded and drives WebView2 CDP, but is BLOCKED externally: no
  `roomler-setup` (`setup-v*`) release exists for `/api/setup/windows` to serve. Recorded in
  `expected-failures.txt`; unblocks when a wizard release is cut.
- [x] AC4 — ARM Linux GREEN 2/2 (script + aarch64 `.deb`, system): install / enroll / overlay
  (`ping anchor ~4 ms`); RD even PASSES under TCG (virtual-desktop Xvfb + SW encode). Desktop is
  N/A (no aarch64 companion) and the graceful skip is asserted. Field-verified 0.4.48, 2026-09-02.

- [ ] AC5 — macOS: coded; needs an Apple-silicon `tart` host (`VMTEST_MACOS_SSH`); lane
  auto-skips cleanly when unset.
- [x] AC6 — RD asserts frames FLOW and ADVANCE (getStats `framesDecoded` / the composable's live
  fps hook, with a transport-agnostic canvas-pixel-change fallback for the DataChannel/VP9-444
  path that has no `<video>`). Verified on Ubuntu + Windows (VP9-444, direct, 29 fps).
- [x] AC7 — all 5 roomler-desktop views walked; Windows asserts real DOM per view over WebView2
  CDP; Linux uses `virsh screendump` per view (pairwise-distinct). Verified.
- [x] AC8 — teardown leaves the org at baseline (graceful self-unenroll + reaper backstop
  observed; `count ≤ baseline` accepts the reaper cleaning older leftovers); k8s untouched
  (`kubectl get nodes` + prod `/health` green across every run).
- [ ] AC9 — the regression-issue mechanism (isolated re-run + `gh issue create`) is coded; not
  yet observed firing (no green-then-red regression occurred).
- [~] AC10 — fail-first evidence recorded for Linux `.deb` install, perMachine SystemContext,
  Wayland RD, ARM install (ALSA runtime-lib symbol), wizard — all shown failing before their fix/expectation.
- [x] AC11 — invocable as one command from the dev box via mars (`vmtest.sh run --lane/--method/
  --type`, `--keep`); the `vmtest` skill documents it.

## Open decisions

1. Win11 image: swtpm-backed TPM2 vs LabConfig registry bypasses — ship both, prefer swtpm,
   keep the bypasses as belt-and-braces (decided in P4 by what the bake proves).
2. DXGI Desktop Duplication on virtio-gpu DOD vs the agent's GDI fallback — whichever the field
   shows; the check only requires frames, and the harness records which path carried them.
3. RD input round-trip assertion (inject via session, observe in-VM) — v2; v1 is frames-only.
4. Weekly cron — one line once the matrix is stable; v1 is on-demand only.

## Out of scope

- Windows-on-ARM (no asset), Intel macOS (no asset).
- Full wizard automation on Linux/macOS (WebKitGTK/WKWebView expose no CDP).
- Portal/consent-prompt UI automation on Wayland — the portal is attended **by design**; the
  Wayland cells use the `drm_capture`/`uinput` (system) and `mutter_capture` (per-user) paths.
- Corp-network topologies (VPN/symmetric NAT) — this FR verifies *install × type × OS*; network
  topology matrices remain the fleet's job (FR-33 et al.).
- Publishing the harness for third parties (it assumes the fleet's layout).

## Field-verification log

**2026-09-02, prod 0.4.48, zeus KVM — 12 cells GREEN (Ubuntu 4/4, Win11 6/6, ARM 2/2).** The harness was
built and driven to green on the live fleet; the run-and-tweak phase found and fixed **27 field
bugs**, each shown failing before its fix. Highlights (full detail in the memory + the issue's
step-log comments):

- **Ubuntu** (script + `.deb` × system + per-user): install / enroll / overlay (`roomler ping`
  anchor round-trip ~1–2 ms) / Wayland (`seat0 ... wayland`) / RD (VP9-444 direct 29 fps, real
  GNOME desktop — DRM capture in a virtio-gpu KVM guest) / roomler-desktop (5 views).
- **Win11** (install.ps1 + MSI × system/attended/user): install / enroll / SystemContext overlay
  (two roomlerd — session-0 SYSTEM supervisor + session-1 worker) / RD / desktop (5 views over
  WebView2 CDP). Golden image bakes unattended (autounattend + OVMF + swtpm + virtio).

Load-bearing bugs (each cost a real debug cycle): an ephemeral daemon **unenrolls itself on
SIGTERM** so a post-enroll restart deleted the device; the configless crash-loop trips systemd's
**start rate-limit**; libvirt's default IP source is unreliable (needs `--source arp`); a bash
`${2:-{}}` default **corrupted every JSON API body**; `roomler status` overlay address is
top-level `.overlay_ip`; a multi-cell run **ran only the first cell** (ssh ate the loop pipe);
sessions are **cookie-only** (RD spec landed on /login); a SW-encode agent streams **VP9-444 over
a DataChannel → a canvas, not `<video>`** (transport-agnostic frame oracle); on Windows,
`$ErrorActionPreference='Stop'` turns native stderr into a terminating error, `wait_guest_ssh`'s
`true` probe doesn't exist in PowerShell, and non-ASCII in a `.ps1` breaks the parse.

**Two product findings surfaced by the harness (each worth its own FR):**
1. **The test org was on the FREE plan — a 3-device cap** (`crates/db/src/models/tenant.rs`).
   Anchor + un-reaped orphans hit it and every new enroll got `403 "Device limit reached"` — a
   *silent* matrix-killer that reads like an overlay bug.
2. **Per-user overlay is broken out of the box on BOTH OSes** — an unprivileged per-user daemon
   can't create a WireGuard TUN, and a vanilla per-user install doesn't auto-configure the
   userspace netstack (`ROOMLERD_OVERLAY_NETSTACK_SOCKS`), so `overlay_mode=tun` never gets an
   address. The installer should set netstack up for a per-user role.

Remaining: macOS (needs an Apple-silicon `tart` host) and the wizard smoke (needs
a `roomler-setup` release).
