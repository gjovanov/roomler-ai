# FR-124: A black call tile says why — a covered camera is named, a refused start recovers

**Issue:** [#2945](https://github.com/gjovanov/roomler-ai/issues/2945) · **Status:** in progress — P1–P3 (#2949, with the
image-build fix #2951) on production since 2026-10-26 12:42Z (`hosted-20261026-212b254`), and in every image since,
including the 2026-10-27 rollback target `hosted-20261026-21d7c18`. Field-verified red then green: AC1, AC2, AC4, AC5,
AC6. AC3 is open for one real-camera check in a dim room (§8)
· **Owner:** the web app (UI) · **Anchors:** master `a350082a8` · **Builds on:** [FR-30](FR-30-camera-state-signalling.md)
(a peer's own word that their camera is off), [FR-25](FR-25-call-layout-mentions-pip.md) (the call layouts)

## 1. Goal

A call tile that shows black says why, on the screen of the person who can fix it.

- **A covered camera is named.** When a camera sends a black picture (a covered lens, a closed privacy shutter, a
  camera-off key that blanks the sensor while the device stays attached), its tile says so in text, and the sender's
  call page warns: *"Your camera is sending a black picture. Is the lens covered or a privacy shutter closed?"* A
  viewer's tile says the black comes from that camera, not from their own network or browser.
- **A tile the browser refused to start recovers.** When the autoplay policy refuses `play()`, the picture comes back
  muted at once, the tile offers its sound, and the next click or key anywhere on the page restores it. Today the
  refusal is swallowed and the tile stays black for good.

## 2. Evidence (2026-10-26)

### 2.1 An hour on a covered lens

The report: a call between neo16 and a MacBook, and "the MacBook's camera doesn't work", in Chrome and in Safari.
Both devices were signed in to the same account. Every layer turned out to be healthy:

| layer | measured | verdict |
|---|---|---|
| the Mac's camera (macOS 27.0.1, Chrome 156, Safari 27.0.1) | `cameracaptured` ran a capture session for "MacBook Pro Camera" (1280×720 `420v`) for the whole of each join, in both browsers | healthy |
| the SFU | each join created a `kind=Video source=camera` producer, the other device consumed it, and no `media transport has no DTLS` warning fired | healthy |
| the media sampler (`stats_call`, 30 s buckets) | in every two-party sample one device sent 1.2–1.8 Mbps and the other 90–160 kbps; neo16 alone sent 141 kbps | neo16 the weak sender |
| a test call: a synthetic pattern from a third tab, the Mac in Chrome, neo16's own tab | the Mac's stream: 640×480 at 30 fps, mean luma ≈ 130, spread ≈ 36. neo16's: 640×480 at ~10 fps, mean luma 0, spread 0 | the black picture was neo16's |
| neo16's raw camera track (`getUserMedia` in the browser, before any encoding) | mean luma 0, peak 1–3 of 255, spread 0.2, 1–10 fps; brightness, contrast and exposure compensation at their defaults, exposure continuous | a covered lens, then confirmed by the reporter |

Why it took an hour:

1. **Nothing on screen said a picture was black, or whose it was.** A black tile and a tile that never started look
   the same.
2. **Both tiles carried the same name.** A second device of the same account is labelled like the first, so the black
   tile was pinned on the wrong device.
3. **The constant pointed the wrong way.** Both Mac browsers "failed", which pointed at the Mac, when the thing that
   never changed was the black sender.

⚠️ Two traps, recorded for the next triage: VP8's bitrate is low for a black picture **and** for a static scene, so
bitrate alone cannot say which camera is black. And `stats_call_user` is keyed by user, so two devices of one account
overwrite each other's row in every bucket.

### 2.2 Found on the way: a refused start is silent

`VideoTile.vue:264` and `:303` call `el.play().catch(() => {})`. A remote tile's `<video>` is not muted (`:26`),
because it carries the peer's audio. Measured in neo16's Edge 156 on 2026-10-26: an unmuted `play()` on a received
stream before any gesture was refused with `NotAllowedError: play() failed because the user didn't interact with the
document first`. The element drew black (mean luma 0) although 140 frames had decoded. After one real click the same
`play()` succeeded, muted and unmuted. Nothing in the tile retries, so a refused tile stays black and silent until its
stream changes. It was not the cause in §2.1, because the Join click gives the page its activation, but it is a black
tile with no reason given.

## 3. Design

### 3.1 The detector (`ui/src/utils/blackPicture.ts`, new)

- Once a second, draw the tile's own `<video>` into a 16×12 canvas and take the luma (`0.299 R + 0.587 G + 0.114 B`)
  of each of its 192 samples.
- **Black** = mean ≤ 4 and peak ≤ 16 (of 255). A covered lens measured mean 0 and peak 1–3. A dim room under
  auto-exposure reads far brighter, and a dark screen with text has bright peaks (D1).
- **Flagged** after 3 black samples in a row (about 3 s, which also rides out a camera's first dark frames while its
  exposure settles), and **cleared** by the first sample with a picture.
- **No judgement**, and the run starts again, while the tile isn't playing (`paused`, `readyState < 2`, no width), its
  track is disabled (your own camera turned off), ended or muted (no media arriving), the peer said their camera is off
  (FR-30's `videoPaused`), or the tile is a screen share. A camera that is off is not a covered camera, and a black
  slide is not one either. A hidden page skips its samples.
- The luma values never leave the tab, and nothing goes on the wire (D3).

### 3.2 Where it says so

- `VideoTile.vue`: a notice over the black picture, in text, never hover-only (FR-120). Your own tile reads *"Your
  camera is sending a black picture · Is the lens covered?"*; a peer's reads *"Black picture from this camera · Its
  lens may be covered"*. A compact tile reads *"Black picture"*.
- The tile emits `picture-black`. The three layouts forward it with the stream key, and `ConferenceView.vue` shows a
  closable warning while the **local** picture is black. Closing it hides it until the picture recovers and turns
  black again.

### 3.3 A refused start (`VideoTile.vue`)

```mermaid
flowchart TB
    P["play()"] -->|resolves| OK["picture and sound"]
    P -->|AbortError| IGN["ignored: a newer srcObject or play() took over"]
    P -->|other error| LOG["console.warn — the tile keeps what it has"]
    P -->|NotAllowedError| M{"was the element<br/>muted?"}
    M -->|"no (a peer's tile)"| MUTE["soundBlocked = true → muted<br/>play() again"]
    MUTE -->|resolves| SND["picture on screen<br/>'Tap for sound' on the tile"]
    MUTE -->|refused| TAP["'Tap to play' on the tile"]
    M -->|yes| TAP
    SND & TAP --> G["the next click / keydown anywhere<br/>(capture phase) or a tap on the tile's button"]
    G --> R["unmute → play() inside the gesture"]
```

The `:muted` binding reads `isLocal || soundBlocked`, so a re-render cannot unmute a tile behind the browser's back,
and the gesture listeners are removed once used and when the tile unmounts.

⚠️ **`click`, not `pointerdown`** (changed from the claim's design while building). On a touch screen the user
activation arrives with the pointer's release, so a `play()` at `pointerdown` would be refused again. `click` fires
after the release for both a mouse and a finger.

## 4. Phases

| Phase | What | Kill switch | Proven on | Size |
|---|---|---|---|---|
| **P1** | the detector, the tile's notice, the call page's warning | revert: UI only | a black camera stream in a production call, red on the current deploy then green; unit tests | S |
| **P2** | a refused start recovers | revert: UI only | a tab joined without a gesture in a production call, red then green; unit tests | S |
| **P3** | docs: `docs/ui.md` § Calls, with its diagram, and the `docs/README.md` index row | — | CI | S |

P1–P3 ship in one PR.

## 5. Acceptance criteria

- [x] **AC1**: a black camera is named on its sender's call page within 5 s: the warning shows, and the local tile
  reads "Black picture". A picture again clears both within 2 s. Shown to fail on the current deploy first. *(Red
  2026-10-26 10:55Z; green 2026-10-27 20:07Z: flagged 3.0 s after the picture turned black, cleared 0.24 s after a
  picture came back. §8)*
- [x] **AC2**: a peer's black picture is named on a viewer's tile within 5 s. A peer who turned their camera off gets
  FR-30's avatar, not the notice. *(Red 2026-10-26; green 2026-10-27: the viewer's notice 3.2 s after the sender turned
  black, timed by a watcher armed before the switch. After the sender turned its camera off: the avatar and the
  camera-off badge, no notice. §8)*
- [ ] **AC3**: no false flag: your own camera turned off shows no warning, and a real picture in a dim room shows none.
  *(2026-10-27 on production: the sender's camera turned off cleared its warning and notice within 4 s, and a synthetic
  dim picture (mean 16.4, peak 41) ran 9 s with no flag on either side. Owed: a real camera in a dim room.)*
- [x] **AC4**: a refused start recovers: a remote tile whose unmuted `play()` is refused shows its picture within 1 s,
  muted, with "Tap for sound", and the next click anywhere restores the sound. Shown to fail on the current deploy
  first. *(Red 2026-10-26: `paused: true`, black, no button. Green 2026-10-27: the tile appeared 0.76 s after the join
  and played at 1.05 s, muted, picture visible, "Tap for sound" shown. One real click on empty stage space unmuted it.
  §8)*
- [x] **AC5**: unit tests lock the thresholds and the hysteresis, the tile's notice (local, remote, camera off, screen
  share) and the refused-start path (`NotAllowedError` → a muted retry, `AbortError` ignored, a gesture restores the
  sound). *(#2949: 4 files, 50 cases. Each guard was removed in turn and its test failed: 11 negative controls.)*
- [x] **AC6**: Docs: `docs/ui.md` describes the notice and the recovery, with a diagram, and the `docs/README.md` index
  row for `ui.md` names them. *(#2949: `docs/ui.md` § "Calls: a black tile says why", a flowchart and a sequence
  diagram, plus the index row.)*

## 6. Decisions

Proposed with the claim; the operator may overrule any of them.

| | decision | proposal |
|---|---|---|
| **D1** | the thresholds | mean ≤ 4, peak ≤ 16, 3 samples a second apart, as measured in §2.1 |
| **D2** | where it says so | the tile and the call page. Not the mini call view (it shows at most four small tiles) and not a toast, which fades while the lens stays covered |
| **D3** | a wire signal | none. The sender judges its own picture and each viewer judges what it decodes, so an older client changes nothing for anyone |

## 7. Out of scope

- `MiniConference.vue`'s own `<video>` elements. They never call `play()`, and the mini view is reached only after a
  Join click, which gives the page its activation.
- **Found on the way, for a separate fix:** remote audio plays only through a rendered tile, so a participant whose
  tile is not rendered is not heard. That happens when "hide participants without video" hides them, past the tiled
  maximum (`useConferenceLayout.ts:306`), and for a fourth remote in the mini view (`MiniConference.vue:94`). The fix
  is an audio sink per remote, independent of the tiles. Read from the code, not yet measured.
- Telling a second device of the same account apart on its tile (§2.1's second reason): a product call.
- A camera preview before joining.

## 8. Field-verification log

The instrument: two tabs of neo16's Edge 156 in an operator-only room on production, each with `getUserMedia`
replaced by a canvas (black, or a bright moving pattern) and silent audio, so no real camera or microphone was used.
Tab A joined with a real click (user activation); tab B joined with a scripted `click()`, which gives none. Each read
is the page's own DOM, plus a 16×12 luma probe of every tile's `<video>`.

| date | build | host | result |
|---|---|---|---|
| 2026-10-26 10:55–10:57Z | production before FR-124 (entry `index-DbWa1WpV.js`) | neo16, Edge 156, tabs A and B as above | 🔴 **AC1**: tab A sent black for 8 s; its own tile played at 640 px with mean and peak luma 0, and there was **no notice and no warning**. 🔴 **AC2**: with tab B sending black, tab A played B's tile (`paused: false`, mean and peak 0) for 7 s with **no notice**. 🔴 **AC4**: in tab B (`userActivation.hasBeenActive: false`) the browser refused A's unmuted tile: `paused: true`, black, **no button**. It stayed black after A switched to a bright pattern (A's own tile read mean 83), so the black was the refusal and not the content |
| 2026-10-26 11:53Z | #2949's image (run 40317779897, `7a5ca9f89`) | the hosted-image lane | 🔴 **a wrong turn: the image did not build.** `VideoTileFr124.spec.ts(181,24): error TS2353 … 'videoPaused' does not exist in type 'Partial<{ style?: unknown; …`. The image runs `vue-tsc` in `oven/bun`, which has no node, so `.vue` resolves through `env.d.ts`'s shim: a `setProps({ realProp })` literal fails there and nowhere else (PR CI 24/24, local build green). This is the trap `KeepBusyMenu.spec.ts:85` documents, met for the fourth time. Every master image was blocked until #2951 (`212b254cb`, 12:38Z), proven red then green with a 6-line `oven/bun:1` Dockerfile over `ui/` |
| 2026-10-26 12:42–12:53Z | `hosted-20261026-212b254` promoted (run 40324468944) | prod | pods on it from 12:42Z, `/health` 0.4.175, 14 devices online before and 14 after. It carried #2938 (FR-90, server-only), with its owner's agreement |
| 2026-10-27 20:07–20:10Z | production `hosted-20261026-21d7c18` (entry `index-CQ0nDNH8.js`), the rollback target that afternoon, with FR-124 inside; announced to the session investigating the rollback, and kept to 2 tabs at its request | neo16, Edge 156, the same two-tab instrument | 🟢 **AC1**: tab A's own tile read *"Your camera is sending a black picture · Is the lens covered?"* and the warning showed at the first sample after the join. A picture again cleared both in 0.24 s, and black again flagged them in 3.0 s. 🟢 **AC4**: in tab B (`hasBeenActive: false`) A's tile appeared 0.76 s after the join and played at 1.05 s, muted, with the picture visible (mean 89–177) and "Tap for sound". One real click on empty stage space unmuted it (`muted: false`) and removed the button. 🟢 **AC2**: B's tile for A read *"Black picture from this camera · Its lens may be covered"* 3.2 s after A turned black, timed by a 100 ms watcher armed before the switch (a first, unarmed read said 5.3 s: that was the tool round trip, not the detector). When A turned its camera off, B showed the avatar and the camera-off badge with no notice, and A's warning and notice cleared within 4 s (AC3's first half). **AC3, synthetic**: a dim picture (mean 16.4, peak 41) ran 9 s with no flag in A or B. Both pods stayed ready with 0 restarts. **Owed**: a real camera in a dim room |

## 9. Related

- [FR-30](FR-30-camera-state-signalling.md) (#884): a peer's own camera-off signal, which the detector defers to.
- [FR-25](FR-25-call-layout-mentions-pip.md) (#839): the layouts that forward the tile's event.
- [FR-120](FR-120-phone-parity-part-2.md) (#2839): no fact is hover-only, so the notice is text.
- [FR-4](FR-4-conference-media-path-integrity.md) (#776): conference media-path integrity, the layer §2.1 proved
  healthy first.
