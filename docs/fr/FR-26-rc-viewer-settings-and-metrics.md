# FR-26 — Remote-desktop viewer: display name, quality-metric toggles, FSR default, settings reorg

**Issue:** [#840](https://github.com/gjovanov/roomler-ai/issues/840)
**Status:** shipped in #845, live on prod — awaiting the operator's own read

> **Renumbered from FR-24.** #838 (licensing split) claimed FR-24 in the same
> hours from a parallel session, and its ledger row lived on an unmerged branch
> while this one merged first — so the table could not arbitrate. Repaired by
> the standing rule: the **lower issue id keeps the number**, so #838 keeps
> FR-24 and this one moved to the next free N. FR-25 is #839, hence FR-26.
> Numbers already settled stay settled.

## Goal

Four operator items against the viewer, all about *what the operator sees while
a session runs*: the wrong name in the title, six metric pills with no way to
choose between them, a sharpening default tuned for video rather than text, and
a settings dialog that had grown into one long scroll.

## Changes

### 1. The display name wins in the title

`RemoteControl.vue` rendered `agent?.name` — the raw machine name — while
/devices, the sidebar and the dashboard mesh graph have all preferred the
admin-set `display_name` since FR-11. The viewer was the last surface that
disagreed, which is jarring precisely when you are looking at the machine.

Title is now `display_name || name`, and when they differ the machine name
moves into the existing subtitle (`CORP-LAPTOP-1` · `PC50045 · windows · 0.4.11`),
so the mapping stays visible rather than hidden.

### 2. Quality metrics are per-pill checkboxes

The toolbar readout grew one pill at a time until it was six wide:

| pill | example |
|---|---|
| codec / transport / decoder | `AV1 4:2:0 HW (av1_qsv) · direct · dec HW · FSR` |
| bitrate | `1.8 Mbps` |
| frame rate | `13 fps` |
| resolution | `2880×1800` |
| frame age (end to end) | `~4 ms` |
| pipeline diagnostics | `paint 0.3/0.4 · fwd 0.1/0.1 · dec …` |

Each now has a checkbox in **Viewer settings → Metrics**, stored as one object
under `roomler-rc-metrics`. **All default ON except `paint`** — the per-hop
numbers answer a question ("is this fps ceiling paint-, decode- or
main-thread-bound?") that only matters while you are chasing it.

Two details worth keeping:

- the fallback is **per key**, so an object written by an older build (fewer
  pills) keeps working and a newly added pill appears rather than silently
  reading `false`;
- `paint` **inherits the legacy `roomler-rc-diag-hud=1` flag** on first read, so
  anyone who set that undiscoverable knob by hand keeps their HUD; once the
  checkbox is used the stored object wins.

### 3. FSR sharpening defaults to ON

`normalizeSharpenMode` defaulted to `'auto'` — sharpen only when the stream is
smaller than the window. The viewer is used for **text** far more than for
video, and always-on RCAS is what makes remote text crisp at 1:1 too. Default is
now `'on'`; `auto` and `off` remain selectable, and an explicit stored choice is
untouched.

⚠️ Two test sites asserted the old default (`normalizeSharpenMode` and
`storedSharpenMode`) — both updated rather than deleted, so the round-trip of
the other two modes is still locked.

### 4. Settings dialog: four tabs

`Video · Display · Metrics · Session`, replacing one scrolling card with three
flat sections and ~8 stacked block buttons — on a phone (where the dialog is
fullscreen) the Session tools were below two screenfuls. Nothing moved between
sections except the new Metrics pane; FSR stays in **Display**, where it belongs
(it is viewer-side rendering, not a metric).

## Files

- `ui/src/views/remote/RemoteControl.vue` — title, pill gating, tabs, Metrics pane
- `ui/src/composables/useRemoteControl.ts` — `storedMetricToggles`,
  `persistMetricToggles`, `DEFAULT_RC_METRICS`, `storedSharpenMode` default
- `ui/src/workers/rc-fsr-render.ts` — `normalizeSharpenMode` default
- `ui/src/__tests__/composables/useRemoteControl.spec.ts` — +5 tests, 2 updated

## Acceptance criteria

- [x] Title prefers the display name; machine name kept visible in the subtitle
- [x] One checkbox per pill; defaults all-on except `paint`; per-key fallback;
      legacy flag honoured once (unit-locked)
- [x] FSR reads ON for a fresh profile
- [x] Settings dialog is tabbed with no control lost
- [ ] Field: a renamed device shows its display name; unchecking a metric hides
      exactly that pill and survives reload; sharpening is on by default

## Field-verification log

- **2026-08-28 — deployed and verified in the shipped bundle.** Prod
  `v20260828-f98ca7c0b356`, `/health` 200. The live `RemoteControl` chunk
  (`RemoteControl-AWwCL_GH.js`, reached from the current `index-peS0x4jI.js`)
  carries `roomler-rc-metrics`, `roomler-rc-sharpen` and `display_name` — i.e.
  all four items are in the code the browser actually runs, not merely on
  master. ⚠️ This is a **deployment** check, not a field result: it proves the
  build reached prod, and says nothing about whether a renamed device reads
  correctly or a checkbox survives a reload. Those three remain open below and
  need a real session.
