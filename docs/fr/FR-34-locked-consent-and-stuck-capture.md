# FR-34 — A locked host: consent you can't see, and a stream that comes up black

**Issue:** [#917](https://github.com/gjovanov/roomler-ai/issues/917)
**Status:** design

## Goal

Controlling a **locked** Windows host must work: the operator at that host can
see and answer the consent prompt, and once they approve, the controller gets a
live screen — without having to refresh. Today neither holds on a perMachine /
SYSTEM host that is locked when the session starts.

## Root cause / field evidence

Field, 2026-08-29, `neo16` (controller) → **CORPLAP-1** (CORPLAP-1, Windows corp
laptop, perMachine SCM / SYSTEM, hybrid Intel Iris Xe, 0.4.17). The operator's
own account:

> it was locked and awaiting consent approval — on the lock screen the consent
> window isn't visible. I logged in directly on CORPLAP-1, saw the consent
> window, approved, and it disappeared — but on neo16's browser it was a black
> screen. Only after refreshing neo16's page did CORPLAP-1's screen come back.

Two independent defects, plus the trigger that chains them.

### 1. The consent panel is invisible on the lock screen — `indicator/win.rs`

The FR-27 native consent panel (`RoomlerConsentWClass`) is a window on the
**interactive** desktop. When the machine is locked, the input desktop is the
secure `Winlogon` desktop, where an ordinary window cannot appear — so a locked
host cannot show the prompt at all. The operator had to unlock first, which is
what set up defect 2. (The M3 lock-screen work put the video *capture* on the
secure desktop; this newer window is not there.)

### 2. A DXGI duplication bound during the lock→unlock transition returns empty frames forever — `system_context/capture_pump.rs:562`

The black session's own log (session `6a930b60`) is unambiguous:

```
16:40:16.5  PC state change … Connected;  all data channels opened
16:40:16.5  input: SystemContext worker — Locked-state suppression disabled (remote unlock enabled)
16:40:17.8  DXGI-direct: bound Desktop Duplication to the primary-output adapter (Intel Iris Xe)
16:40:17.8  capture: backend=system-context (DXGI + GDI fallback) 1920x1200
…            (13 s of:)
16:40:30.9  capture produced no frame (idle screen)  frames_empty=200000+  frames_unchanged=0
16:40:31.3  session terminated by server  reason=ControllerHangup
```

`frames_empty` (the duplication delivered **nothing**) climbed past 200 000
while `frames_unchanged` (we have a frame identical to the last — the real
idle-screen case) stayed **0**. The duplication was bound at 16:40:17.8, the
instant the desktop switched secure→default on unlock, and then
`AcquireNextFrame` returned `DXGI_ERROR_WAIT_TIMEOUT` for the rest of the
session — bound to a desktop/output that would never change.

The pump handles the two *error* transitions — `AccessLost` → `reset()`,
`DesktopMismatch` → `try_change_desktop()` — but `WAIT_TIMEOUT` maps to
`BackendBail::Transient`, whose arm is:

```rust
Err(BackendBail::Transient) => {
    *consecutive_hard = 0;
    *consecutive_access_lost = 0;
    *consecutive_empty = consecutive_empty.saturating_add(1);
    Ok(None)                       // ← no recovery, ever
}
```

The routing doc even reads `Transient → Ok(None) — no frame this tick (idle-
keepalive will fire upstream)`, i.e. it assumes Transient means "idle screen,
nothing changed." That is true for a *working* session that goes idle; it is
false for a duplication that came up stuck and has **never delivered a frame**.
The two are trivially separable: a stuck duplication has `frames_delivered == 0`
for its whole life.

Only a fresh session recovered it — session `6a930b80`, created 1 s after the
hangup, rebuilt the duplication on the now-settled desktop and ran at 18 fps
(HEVC). A reconnect should not be the only recovery.

### 3. The idle/empty optimisation must never suppress the FIRST frame

Compounding 2: even a genuinely idle screen must send an initial (key)frame, or
the controller is black until something moves. The output-suppression on
"unchanged/empty" is only sound *after* one frame has reached the controller.

## Key design

| # | Phase | Fix | Kill switch |
|---|---|---|---|
| 1 | **Stuck-duplication recovery** | In `capture_pump`, track `frames_delivered` (reset on every backend (re)build). When `frames_delivered == 0` and `consecutive_empty` crosses `STUCK_EMPTY_THRESHOLD`, `try_change_desktop()` + rebuild DXGI; if a second streak follows the rebuild, fall to the always-delivers GDI BitBlt path (as the `AccessLost` arm already does at its threshold). **Gated on `frames_delivered == 0`, so a session that has ever shown a frame is byte-for-byte unaffected** — the worst case for the change is a came-up-black session that stays black, i.e. no regression over today. | env `ROOMLER_AGENT_STUCK_CAPTURE_RECOVERY=0` |
| 2 | **First-frame guarantee** | Never let the empty/unchanged path suppress output before the controller has received one frame — force an initial keyframe. Largely subsumed by phase 1 (recovery delivers the frame) + the GDI backstop; this is the belt-and-suspenders for a truly static screen. | — |
| 3 | **Consent panel on the secure desktop** | Render the FR-27 consent panel on the `Winlogon` secure desktop when the host is locked, via the SystemContext worker that already owns the secure-desktop video overlay (M3), so a locked host can show and answer the prompt without unlocking. Larger; sequenced last. | feature-gated; absent = today's behaviour (unlock to answer) |

Phase 1 is the functional fix (black → live without a refresh) and is safe to
ship on its own by the `frames_delivered == 0` gate. Phase 3 removes the trigger
but is the hardest (secure-desktop window ownership) and does not block phase 1.

## Acceptance criteria

- [ ] A duplication that comes up delivering no frames recovers **in-session**
      (rebind → rebuild → GDI backstop), so the controller gets a live screen
      without a reconnect.
- [ ] A session that has ever delivered a frame is unaffected (no extra
      rebuilds, idle optimisation intact).
- [ ] Connecting to a genuinely static screen shows an initial frame, not black.
- [ ] On a **locked** perMachine host, the operator can see and answer the
      consent prompt without unlocking (phase 3).
- [ ] Field-verified on **CORPLAP-1**: lock → start a session → the prompt is
      visible → approve → the screen comes up live, no refresh.

## Out of scope

- The lock→unlock capture behaviour on GDI-only / non-SystemContext hosts (they
  BitBlt the current screen every frame and don't exhibit this).
- FR-22 time-to-first-frame (a different, non-locked path).

## Field-verification log

*(empty — phase 1 needs a lock/unlock repro on CORPLAP-1; the controller side is
observable from neo16's browser + the agent's `capture produced no frame`
counters, which is exactly how this was root-caused.)*
