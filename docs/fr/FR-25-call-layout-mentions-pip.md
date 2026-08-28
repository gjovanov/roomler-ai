# FR-25 — Call UX: layout controls that honour their labels, mentions, PiP, spotlight

**Issue:** [#839](https://github.com/gjovanov/roomler-ai/issues/839)
**Status:** implemented — awaiting field verification on prod

## Goal

Four operator reports against the call page, all of the shape *"the control is
there and nothing happens"*. Three had root causes that are provable without a
call — a bundler chunking fault, a missing DOM attribute, and a prop that was
only passed to one of three layouts. This FR fixes those and adds the two
missing affordances (double-click to spotlight, per-tile fullscreen).

## Root causes

### 1. Mentions crash the editor — prosemirror-model shipped twice

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

**Fix:** function-form `manualChunks` matching the whole family by path
(`@tiptap/*`, `prosemirror-*`, `tiptap-markdown`, `y-prosemirror`), so a future
import of any tiptap package cannot split prosemirror again.

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

- [x] The prosemirror duplicate-guard appears in exactly ONE built chunk
      (verified: 2 chunks before, 1 after)
- [ ] `@` → Enter inserts a mention with no console error, in call chat and room chat
- [ ] PiP opens from the toolbar and from a tile; solo call falls back to self-view;
      refusals say why
- [ ] Self-view cropped/uncropped visibly changes all three layouts
- [ ] Hide-non-video converges when a camera toggles, without a reload
- [ ] Sidebar never spotlights you while a remote is present
- [ ] Double-click spotlights; again restores the previous layout; fullscreen works
- [x] Unit tests for the layout rules (13 new; there were none)

## Field-verification log

- (pending prod deploy)
