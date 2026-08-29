# FR-30 — Camera/mic state is never signalled, so peers cannot see who turned their camera off

**Issue:** [#884](https://github.com/gjovanov/roomler-ai/issues/884)
**Status:** proposed — design anchored against master, no code yet

## Goal

A participant who turns their camera (or mic) off should look that way to
everyone else: their tile shows it, and "hide participants without video"
finally hides them.

Today nobody can tell. There is **no pause/resume signalling anywhere in the
call protocol** — not client→server, not server→peers.

## Field evidence (2026-08-29, prod `v20260829-6d432cca9520`)

Two browsers in one call. Tab A turned its camera off; measured **in tab B**,
on the received track:

```
enabled: true    readyState: "live"    muted: false    videoWidth: 640
```

The receiver is blind to it: the last frame simply freezes. With
`hideNonVideo: true` the tile stayed, which is how this was found while
field-verifying FR-25 (#839).

### Why the receiver cannot know

`stores/conference.ts::toggleVideo` does two local things and nothing else:

```ts
if (!isVideoOn.value) videoProducer.pause()          // mediasoup-client, LOCAL only
localStream.value.getVideoTracks().forEach(t => { t.enabled = isVideoOn.value })
```

`producer.pause()` in mediasoup-client is a client-side state change; the
server only learns if the app asks it to. `grep` for `paused`/`resumed` finds
**no handler in the store and no relay in the API**. A disabled track keeps its
RTP stream alive (black frames), so `muted` never flips on the receiving side
either.

⚠️ This is why FR-25 could not deliver "hide participants without video". That
FR fixed the **reactivity** — the filter re-runs on track transitions now,
which it never did — and the Auto rules. It cannot supply an input that is
never sent, and no predicate over track state can substitute for it.

## Key design — mirror the pair that already exists

The media protocol already has a close pair, and this is the same shape:

| existing | new |
|---|---|
| `media:producer_close` (client→server) `crates/api/src/ws/handler.rs:858` | `media:producer_pause` |
| `media:producer_closed` (server→peers) `crates/api/src/ws/handler.rs:1355` | `media:producer_paused` |
| `handle_media_producer_close` `crates/api/src/ws/handler.rs:1311` | `handle_media_producer_pause` |
| `RoomManager::close_producer` `crates/services/src/media/room_manager.rs:439` | `pause_producer` |
| client sends at `ui/src/stores/conference.ts:456` | alongside `toggleVideo` |
| client handles at `ui/src/stores/conference.ts:111` | `handleProducerPaused` |

Wire (kind-agnostic, so it covers the mic indicator too):

```
→ media:producer_pause   { room_id, producer_id, paused: bool }
← media:producer_paused  { producer_id, user_id, paused: bool }
```

⚠️ **Pausing server-side is the point, not just the notification.** Pausing the
mediasoup producer stops forwarding to every consumer, so a camera-off
participant stops costing bandwidth — today they keep shipping black frames to
everyone. It also makes the receiving track go `muted` natively, which is a
second, slower signal; the event stays authoritative because it is immediate
and unambiguous.

⚠️ `close_producer` is **sync**; `producer.pause()` is **async**, so
`pause_producer` cannot be a copy-paste sibling — it needs an async path or a
handle clone out of the DashMap before awaiting. Getting that wrong deadlocks
the map.

⚠️ **Additive and old-client-safe by construction.** An older client never
sends `producer_pause` (so it behaves exactly as today) and ignores an unknown
`producer_paused` (the store dispatches by name). No flag day.

## Phases

| # | phase | kill switch |
|---|---|---|
| P1 | Wire: `media:producer_pause` → server pauses the producer → `media:producer_paused` to other peers | client simply doesn't send; server handler is inert without it |
| P2 | Client honours it: per-participant `videoPaused`/`audioPaused`; `hasLiveVideoTrack` consults it so hide-non-video converges | `hideNonVideo` remains a user preference, default unchanged |
| P3 | Camera-off / mic-off indicator on remote tiles | purely additive UI |
| P4 | Field-verify with two browsers, and measure the bandwidth drop while paused | — |

## Acceptance criteria

- [ ] With "hide participants without video" on, turning the camera off in tab A
      removes that tile in tab B within ~1 s, **without a reload**; turning it
      back on restores it
- [ ] A remote tile shows a camera-off indicator (and a mic-off one) driven by
      the signal, not by local state
- [ ] Consumers stop receiving video while the producer is paused — measured,
      not assumed
- [ ] An old client in the same call is unaffected (no errors, no missing tiles)
- [ ] The FR-25 criterion this unblocks is re-ticked in
      `docs/fr/FR-25-call-layout-mentions-pip.md`

## Open decisions

- **Do we pause on tab-hidden too?** Tempting for bandwidth, and wrong by
  default: a participant who is listening while reading something else would
  vanish from the grid. Out of scope until asked for.
- **Screen-share pause** — same wire, but the UX of a paused share is unclear;
  P1 covers camera and mic only.

## Out of scope

- Any change to how the local user sees themselves (the local tile already
  knows its own state).
- Simulcast/layer control, which is a different lever on the same producer.

## Field-verification log

- (pending)
