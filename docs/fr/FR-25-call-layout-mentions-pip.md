# FR-25 — Call UX: layout controls that honour their labels, mentions, PiP, spotlight

**Issue:** [#839](https://github.com/gjovanov/roomler-ai/issues/839)
**Status:** shipped in #842, live on prod — awaiting the operator's own read

## Goal

Four operator reports against the call page, all of the shape *"the control is
there and nothing happens"*. Three had root causes that are provable without a
call — a bundler chunking fault, a missing DOM attribute, and a prop that was
only passed to one of three layouts. This FR fixes those and adds the two
missing affordances (double-click to spotlight, per-tile fullscreen).

## Root causes

### 1. Mentions crash the editor — prosemirror-model installed five times

> ⚠️ **This section was wrong when first written, and the first fix did not
> work.** Both are kept below rather than rewritten away, because the mistake
> is the reusable lesson. Corrected 2026-08-29 (#859) after field-testing the
> deployed build.

`@` opened the popup, Enter threw:

```
RangeError: Can not convert <mention, " "> to a Fragment
(looks like multiple versions of prosemirror-model were loaded)
```

The error names its own cause. `ui/vite.config.ts` used the **object form** of
`manualChunks` and listed only two of the nine tiptap packages:

```ts
tiptap: ['@tiptap/starter-kit', '@tiptap/vue-3'],
```

`@tiptap/extension-mention`, `@tiptap/suggestion` and `tiptap-markdown` were
therefore chunked wherever they happened to be imported — the message-editor
chunk — and **each chunk got its own `prosemirror-model`**. A node built by one
instance is rejected by the other, which is exactly what picking a mention does.

Proven against the live bundle before changing anything: the duplicate-guard
string appeared in **both** `tiptap-BIJn1zU9.js` (4×) **and**
`MessageBubble-od6guLWD.js` (1×).

⚠️ This was never call-specific — `MessageEditor` is shared by `MessageBubble`,
`ChatView` and `ConferenceView`, so **room chat was equally broken**.

**First fix (#842), which did NOT work:** function-form `manualChunks`
matching the whole family by path. It grouped the packages into one chunk and
the crash survived it, because *grouping copies is not removing them*.

#### The measurement error

`grep -c "multiple versions of prosemirror-model" dist/assets/*.js` returning
"2 chunks before, 1 chunk after" was read as proof. That string occurs **once
per copy** of the library, so the honest reading of the same numbers is
**`4+1 = 5` copies before, `5+0 = 5` copies after** — the count that mattered
never moved. A proxy for placement was reported as a count of instances. ⚠️ The
only sound version of that check is the **TOTAL across every chunk**, and it
must be exactly 1.

#### The actual cause — the install tree, not the bundler

| package | `prosemirror-model` it vendors |
|---|---|
| (root) | **1.25.11** |
| `prosemirror-`{`commands`,`markdown`,`schema-list`,`state`,`tables`} | **1.25.4** each |

`prosemirror-transform` and `prosemirror-view` are nested the same way. Every
requirer's range — `^1.0.0`, `^1.25.0`, `^1.25.4` — accepts the root version,
so this is **installer duplication, not a version conflict**, and no bundler
arrangement can fix it.

**Real fix (#859):** `resolve.dedupe` over the whole prosemirror family in
`ui/vite.config.ts`, which forces one instance regardless of how the install
tree nests. Measured after: **1 copy total, was 5.**

#### The test that should have caught it

`ui/e2e/mention.spec.ts` stopped at *"the autocomplete list is visible"* — which
is precisely why it stayed green through a total outage: opening the popup was
never broken, **picking an item was**. #859 adds the case that presses Enter,
asserts a mention node lands, and asserts on the console **first** so a
regression reports its cause rather than a missing element. ⚠️ Only an e2e can
observe this class at all — the unit tests import a single module graph, where
the duplicate cannot exist by construction.

### 2. PiP did nothing — the attribute it looks up was never rendered

`ConferenceView.handlePiP` finds the element with
`document.querySelector('video[data-stream-key="…"]')`. `VideoTile` binds
`:data-stream-key="streamKey"` — but **no layout ever passed `stream-key`**, so
the attribute rendered as nothing and the selector never matched. Compounding
it, `requestPiP` returned silently when PiP was disabled, and `togglePiP` fell
off the end when a call had no remote stream yet (solo call).

**Fix:** pass `:stream-key` in all three layouts; `requestPiP` now *returns the
reason* it failed and the view surfaces it; `togglePiP` falls back
speaker → first remote → **local**.

### 3. Layout / self-view controls

| | Defect | Fix |
|---|---|---|
| a | `selfViewMode` was passed **only in the tiled branch** of `layoutProps`; Spotlight/Sidebar did not even declare the prop — so "in grid (cropped/uncropped)" was inert in the two layouts Auto picks most often (screen share → sidebar, ≤2 people → spotlight) | prop declared and applied in all three |
| b | "Hide participants without video" called `hasLiveVideoTrack` from inside a computed. **Native MediaStream/track state is invisible to Vue reactivity** (`VideoTile` documents this exact trap and keeps its own ref), so the filter was decided once and a camera switched on later never brought the tile back | a `videoStateVersion` counter bumped on `addtrack`/`removetrack`/`ended`/`mute`/`unmute`, read by `hasLiveVideoTrack`; listeners torn down on scope dispose |
| c | Sidebar with no screen-share and no pin took `sorted[0]` — **alphabetical**, so an operator whose name sorted first got a full-screen view of themselves | `pickPrimaryFallback`: active speaker (remote only) → first remote → self only when alone |
| d | Auto's rules were an inline chain nobody could point at | extracted as the pure `resolveEffectiveMode(mode, ctx)`: screen share → sidebar; pin → spotlight; ≤2 → spotlight; else tiled |

### 4. Spotlight + fullscreen (new)

Double-click a tile → `toggleSpotlight(streamKey)`: pins it as the **sole** pin
and switches to spotlight, remembering the mode it came from **in memory** (it
describes this call, not a durable preference) so the second double-click really
restores it. Per-tile fullscreen button uses element `requestFullscreen()` with
a `fullscreenchange` listener; ⚠️ hidden where `document.fullscreenEnabled` is
false (iOS Safari) rather than shipping a dead control.

## Files

- `ui/vite.config.ts` — the chunking fix
- `ui/src/composables/useConferenceLayout.ts` — `resolveEffectiveMode`,
  `pickPrimaryFallback`, `videoStateVersion`, `toggleSpotlight`
- `ui/src/components/conference/VideoTile.vue` — `spotlight` emit, fullscreen
- `ui/src/components/conference/layouts/{Tiled,Spotlight,Sidebar}Layout.vue` —
  `stream-key`, `selfViewMode`, `spotlight`
- `ui/src/views/conference/ConferenceView.vue` — pass `selfViewMode` everywhere,
  `handleSpotlight`, PiP fallback + error surfacing
- `ui/src/composables/usePictureInPicture.ts` — return the failure reason
- `ui/src/__tests__/composables/useConferenceLayout.spec.ts` — **new**

## Acceptance criteria

- [x] Exactly ONE copy of prosemirror-model in the served bundle — the TOTAL
      across all chunks, not the number of chunks holding it (prod: 1, was 5)
- [x] `@` → Enter inserts a mention with no console error — proven by e2e in
      room chat; the in-call chat mounts the same `MessageEditor`
- [ ] PiP opens from the toolbar and from a tile; solo call falls back to self-view;
      refusals say why
- [ ] Self-view cropped/uncropped visibly changes all three layouts
- [ ] Hide-non-video converges when a camera toggles, without a reload
- [ ] Sidebar never spotlights you while a remote is present
- [ ] Double-click spotlights; again restores the previous layout; fullscreen works
- [x] Unit tests for the layout rules (13 new; there were none)

## Field-verification log

- **2026-08-28 — deployed; the mentions fix is verified against the live
  bundle.** Prod `v20260828-f98ca7c0b356`, `/health` 200. The prosemirror
  duplicate-guard string now appears in **exactly one** served chunk —
  `tiptap-CKsT6A5v.js` (5 hits) with **0** in `MessageBubble-kza3Ixuv.js` —
  where before the fix it was in two (`tiptap-BIJn1zU9.js` ×4 **and**
  `MessageBubble-od6guLWD.js` ×1). Same measurement that identified the bug,
  re-run against what prod serves, so this criterion is genuinely closed.
  `ConferenceView-Doy6qIeS.js` carries `data-stream-key`, `spotlight` and
  `fullscreen`. ⚠️ Everything else below is a **behaviour** claim and needs a
  real call — a chunk containing the code is not the code working.
- ⚠️ #839 was auto-closed by the merge of #842 (a closing keyword in the PR
  body) one second after it landed, i.e. **before any field verification**. The
  workflow closes an FR on verified criteria, not on a merge; reopened — and
  the field test then showed the fix did not work, so the auto-close had been
  hiding a live defect.
- **2026-08-29 — mentions A/B on the standing e2e stack**, `ui/e2e/mention.spec.ts`
  unchanged between runs, one variable (the image):

  | stack image | new insert test | old popup test |
  |---|---|---|
  | `v20260825-18b9c16c429b` (before #842) | **FAIL** — `Can not convert <mention, " "> to a Fragment` | pass |
  | `v20260829-dbef86e67d8b` (with #842) | **FAIL** — identical | pass |
  | `v20260829-ae6bfa254495` (with #859) | **PASS** (3/3) | pass |

  The middle row is the point: the shipped "fix" was indistinguishable from no
  fix, and only a test that had been **shown to fail first** could reveal it.
  Prod is on `v20260829-ae6bfa254495`, `/health` 200, and its served bundle
  carries **1** copy of prosemirror-model (`tiptap-KeNVj9jG.js`).
