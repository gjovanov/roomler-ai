<!-- SPDX-License-Identifier: MPL-2.0 -->
# FR-55: A device stays reachable instead of quietly sleeping

**Status:** proposed (2026-09-01). Tracking issue: `FR-55`. Anchors verified against
master `40da643d`.

## Goal

An enrolled device is reachable when the operator needs it — mesh, remote desktop, SSH
and exec — instead of dropping off the fleet because the OS decided to sleep, with
nothing anywhere saying why.

Pillar 2's acceptance bar is *"just works"* on an unmanaged laptop as well as a managed
desktop. A device that is unreachable half the day does not meet it, and today **nothing
in the product has an opinion about power at all.**

## Field evidence

Reported by the operator on 2026-09-01 ("roomler going offline after a while"), then
measured on the MacBook.

**The machine really does sleep, on AC, with the lid open and closed:**

```
10:33:22  Entering Sleep state due to 'Idle Sleep':TCPKeepAlive=active Using AC (Charge:100%)
15:00:56  Entering Sleep state due to 'Clamshell Sleep':TCPKeepAlive=active Using AC (Charge:100%)
```

**Its idle timer is one minute** (`pmset -g`): `sleep 1`, `hibernatemode 3`, `standby 1`,
`disksleep 10`. Also `tcpkeepalive 1` and `womp 1` — both relevant below.

**Nothing we do holds it awake.** `pmset -g assertions` shows only `powerd` and
`WindowServer`. The single `roomlerd` mention is

```
pid 399(WindowServer): UserIsActive named:
  "com.apple.iohideventsystem.queue.tickle.nxevent service:IOHIDSystem pid:36743 process:roomlerd"
```

which is a **byproduct of injecting input during an active remote session**, not a
deliberate assertion. ⚠️ It therefore protects a session where someone is *typing*, and
not a view-only one — the case where an operator is watching a long build is exactly the
case that sleeps.

**And the codebase confirms it**: `IOPMAssertion`, `SetThreadExecutionState`,
`PowerCreateRequest` and `systemd-inhibit` have **zero** occurrences across
`agents/` and `crates/`.

## What is actually forced, and what is ours

Worth separating before designing, because one of these cannot be fixed:

| | forced by the OS | ours |
|---|---|---|
| A sleeping machine cannot serve a WS | ✅ | |
| macOS **clamshell** sleep ignores idle-sleep assertions on a laptop with no external display | ✅ | |
| Nothing asks the OS to stay awake | | ✅ |
| An active session does not hold an assertion | | ✅ |
| "Offline" and "asleep by policy" look identical to an operator | | ✅ |

So the honest framing is **two families of answer**, and a complete feature needs both:

1. **Do not sleep** — hold a power assertion while it matters.
2. **Wake it** — a mesh peer on the same L2 sends a magic packet. `womp 1` is already on
   here, and the overlay already knows which peers share a LAN.

Neither alone is sufficient: (1) cannot beat clamshell sleep or an operator who closes
the lid, and (2) needs a peer on the same segment.

## Key design

### Per-OS mechanism

- **macOS** — `IOPMAssertionCreateWithName`. Two candidate types, and they are not
  interchangeable: `kIOPMAssertionTypePreventUserIdleSystemSleep` (idle only) and
  `kIOPMAssertionTypeNetworkClientActive`, which exists precisely for "this machine is
  serving the network". ⚠️ Neither defeats **clamshell** sleep — that is an OS limit, and
  the feature must SAY so rather than appear not to work.
- **Windows** — `PowerCreateRequest` + `PowerSetRequest(PowerRequestSystemRequired)`, not
  `SetThreadExecutionState`: the latter is per-thread and unreliable from a service, which
  is exactly how `roomlerd` runs on a fleet host.
- **Linux** — logind `Inhibit()` over D-Bus (`what="sleep"`, `mode="block"`), holding the
  returned fd for as long as the inhibition should last. ⚠️ Headless servers rarely sleep,
  so this matters for desktops; and `mode=block` is a request a policy can override.

### Default OFF, and honest about it

⚠️ **Preventing sleep on a battery-powered laptop is user-hostile**, and a remote-access
tool that silently drains a battery deserves the reputation it gets. So:

- a device-owned config key, default **off** — the same last-word rule as `exec_enabled`
  and `ssh_enabled` (`docs/remote-config.md`);
- an obvious refinement to decide, not assume: **on AC only** as the middle setting;
- an **active session** should hold an assertion regardless of the standing policy — a
  session must not be cut by an idle timer. Today that happens by accident on macOS, and
  only while input flows.

### Say why the device is gone

⚠️ Today "offline" is one word covering *crashed*, *network died*, *powered off* and
*asleep by policy*, and each has a different fix. The device should report its power
policy and whether an assertion is currently held, so the dashboard can distinguish them.
This is the `Some([])` vs `None` lesson from the overlay ACL and `ssh_activity`'s
"empty ≠ inactive", recurring on a third surface — see FR-49, where five surfaces all
reported normally while a feature was dark.

## Phases

| P | Scope | Kill switch |
|---|---|---|
| P0 | **Measure first.** How often do fleet devices go offline, and does it correlate with sleep rather than crashes or network? Build nothing until the answer is known. | n/a |
| P1 | An **active rc/ssh session** holds an assertion, on all three OSes. Narrow, always-correct, no policy needed. | per-OS: assertion failure is logged and non-fatal |
| P2 | Standing policy `power_policy = never \| on-ac \| always`, default `never`, device-owned. macOS first. | the key itself; absent ⇒ today's behaviour |
| P3 | Windows + Linux implementations of the same policy. | as P2 |
| P4 | Report it: `roomler status` + the dashboard distinguish "asleep by policy" from "offline". | additive, read-only |
| P5 | **Wake on LAN from a mesh peer** — an awake peer on the same L2 sends the magic packet on request. | server-side switch; off by default |

## Acceptance criteria

- [ ] P0 produces a number: what share of fleet offline-time is sleep, measured, not assumed.
- [ ] With a remote-desktop session open and **no input for 10 minutes**, the device does
      not sleep — on macOS, Windows and Linux.
- [ ] With `power_policy = never` (the default), behaviour is byte-for-byte today's: no
      assertion is taken, and `pmset -g assertions` / `powercfg /requests` show nothing
      from us.
- [ ] With `power_policy = on-ac`, a laptop on battery still sleeps and the same laptop on
      AC does not.
- [ ] An operator looking at a device that is asleep by policy can tell that from the UI,
      without reading a log.
- [ ] ⚠️ The macOS **clamshell** limitation is documented in the UI where the policy is
      set — not discovered by a user whose lid-closed Mac still vanishes.
- [ ] P5: a sleeping device on the same LAN as an awake peer can be woken from the
      dashboard, and the audit says who did it.

## Open decisions

- Does the standing policy belong in `DesiredConfig` (server-pushable) or stay strictly
  device-owned? Pushing it means an org admin can drain an employee's battery; not
  pushing it means the last gate is again the one nobody can reach — the exact tension
  `docs/remote-config.md` resolves with `remote_config_enabled`.
- Is `NetworkClientActive` or `PreventUserIdleSystemSleep` the right macOS type? The
  former is semantically exact and may behave better with Power Nap; needs measuring.
- Should an **exec** run hold an assertion? A 40-minute build kicked off by `roomler exec`
  has the same problem as a session, and the same answer is not obvious.

## Out of scope

- Waking a device that is off (as opposed to asleep) — that is BIOS/AMT territory.
- Battery-life optimisation generally.
- Scheduled wake (`pmset repeat`), which is an operator's own configuration.

## Field-verification log

_(empty — P0 has not run)_
