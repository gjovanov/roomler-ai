---
paths:
  - "agents/roomlerd/src/**"
  - "agents/roomler-desktop/**"
  - "crates/remote_control/**"
  - "crates/modules/remote/**"
  - "crates/modules/fleet/**"
  - "ui/src/components/remote/**"
  - "ui/src/views/remote/**"
  - "ui/src/composables/useRemote*"
---

# Remote desktop — pillar 1 invariants

<!-- Moved verbatim from CLAUDE.md on 2026-10-28, when that file had grown to 58 KB and was
     loaded on every prompt and into every subagent. CLAUDE.md keeps the encoder preference order and a pointer here. This file loads only when Claude reads
     or edits a file matching `paths:` (Claude Code path-scoped rules). Keep it under 40,000
     characters: from Claude Code 2.1.309 an on-demand file is cut there. Block HTML comments like
     this one are stripped before the model sees the file. -->

Design: `docs/remote-control.md` · encoders: `docs/encoders.md` + the `encoder-cells` skill · rate control: `docs/rate-control.md`.

### Remote desktop

Design: `docs/remote-control.md` · encoders: `docs/encoders.md` + the
`encoder-cells` skill · rate control: `docs/rate-control.md`.

One native agent per controlled host, the API as a **signalling-only** relay, the
browser as controller. All media + input flow over direct WebRTC P2P (TURN-relayed
if needed) — the server never sees raw pixels or keystrokes.

**Wire**: `rc:*` JSON over the existing `/ws`. `ClientMsg`/`ServerMsg` in
`crates/remote_control/src/signaling.rs`. ObjectIds are raw hex strings and
`Permissions` serialises pipe-separated (bitflags 2.x convention) — both locked by
tests.

**Agent capability verbs** (`AgentCaps.rpc`): the wire stays `Vec<String>` because
fleet agents span many releases and a typed wire format would strand them, but both
sides go through **`models::RpcCap`**. Adding a verb: variant → `wire()` arm (the
match is exhaustive, so the compiler makes you) → `ALL` entry (a test makes you).
⚠️ Unrecognised verbs are **IGNORED, never an error** — a newer agent may advertise
something this server has never heard of, and an additive list is only additive if
old readers skip what they don't know.
⚠️ **`ssh` is a PREFIX of `ssh-consent`** ("runs a server" vs "honours
`consent_mode`"), and `config` is a prefix of `config-report`. Matching must stay
**equality** — `starts_with`/`contains` would silently mark every ssh-capable agent
consent-capable, fleet-wide. Locked by `ssh_does_not_imply_ssh_consent`.
⚠️ The wire spellings are a compatibility surface: renaming one doesn't fail
loudly, it makes every deployed device look like it lacks the feature.

### Encoders — load the `encoder-cells` skill

Preference resolution: **CLI `--encoder` > env `ROOMLERD_ENCODER` >
`encoder_preference` in config TOML > `Auto`**.

Everything else about the encoder surface — the child-process capability probe and
its two phases, the cache, the `encoder_cells_deny` kill switch, the codec ×
backend × chroma cell matrix, and the per-backend traps (NVENC / QSV / AMF /
VAAPI / D3D12 / Vulkan / VideoToolbox) — lives in the **`encoder-cells` skill** and
`docs/encoders.md`. Three that must not be re-learned the hard way:

- ⚠️ The probe runs in a **child process**, and a failure costs the cell it died in
  or, with no cell to blame, "no hardware" (#3267). A capability probe is untrusted
  third-party code by definition (vendor drivers, GPU firmware) and does not belong
  in the daemon's address space — before this, a fault inside one took `roomlerd`
  down and the service manager restarted it straight back into the same probe.
- ⚠️ **A gate applies at every entry point, or it is a courtesy**: the cell
  denylist was probe-only until 0.4.90, and a session happily opened a denied cell
  the hello never advertised.
- ⚠️ **A probe proves an OPEN; only a SESSION proves a CELL.**

### Host consent (FR-27, closed — the macOS native panel is still unverified)

Moved from CLAUDE.md's Known Issues, verbatim; the full field log is in `docs/fr/FR-27-consent-surfaces-and-desktop-companion.md`.

- **[MEDIUM]** [2026-04-17, rewritten 2026-08-29 by **FR-27**, #854] Host consent now
  has a **`PromptSurface` chain** — native (`win`·`mac`·`x11`) → companion
  (`roomler-desktop` over LocalAPI) → notification (Linux, the desktop's own, with
  Approve/Deny: FR-93 P6f) → cli (`roomlerd consent`) → none, which reports
  `no_prompt_surface` instead of a silent deny, and the chosen surface is logged per
  prompt (before it, "the prompt didn't appear" was unattributable). ⚠️ On
  **Wayland** the native panel is the X11 one, through Xwayland: a per-user daemon
  shows it, with no guarantee of stacking above native windows (mutter offers no
  `wlr-layer-shell` at all; KWin offers `zwlr_layer_shell_v1`, but only to a native
  Wayland client, #2286), and a root daemon has no `DISPLAY` and falls through to the
  companion **by design, not by omission**. ⚠️ An agent-side prompt **timeout** must not come back as a bare
  `granted:false` — "nobody was at the machine" reaching the controller as "a human
  refused you"; the hub waits 5 s **longer** than the window it announces, because
  with equal timers its own fallback fired ~130 ms before the agent's reasoned
  verdict. ⚠️ A server directive must never override a device's
  `auto_grant_session=false` — `consent::strictest_of` makes the local setting a
  floor.
<!-- RETIRED-NAME-ANCHOR: the env-var spelling on the line below carries a
     retired product name deliberately — it is what the daemon actually reads,
     so renaming it would silently disable every virtual-desktop host. FR-27. -->
  ⚠️ A `ROOMLER_AGENT_VIRTUAL_DESKTOP=1` host is **not** a consent surface even
  though its X display connects: the only viewer of that Xvfb is a remote
  controller, so an unattended host reports `timeout` where the truth is
  `no_prompt_surface`, and an attended one lets viewer A approve viewer B.
  Field-verified: `no_prompt_surface` (2026-08-29, 2026-09-25), `prompt_owner` on
  GNOME Wayland (2026-10-23), all five consent modes, two concurrent viewers
  through `free` ↔ `exclusive`, and the `exec` and `ssh` prompts on the same
  surface as an RC one (2026-10-24; two BROWSERS on one PC are enough).
  `push` reaches the owner in-app AND by web push. Prod has had a push service
  since 2026-10-24. Each browser opts in through the notification panel's
  *Turn on* (#2817); a page load never prompts. The controller is told who is asked
  (`consent_asks` on `rc:session.created`, #2810) and words the wait and the
  outcome for that person. The X11 panel was field-verified through Xwayland in
  FR-93 AC10 (0.4.141). Still unverified: the macOS native panel.
  Detail: `docs/fr/FR-27-*.md`.
