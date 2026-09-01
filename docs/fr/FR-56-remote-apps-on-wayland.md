# FR-56 — Remote Apps on Wayland: per-app streaming, and the RAIL circle

**Issue:** [#1157](https://github.com/gjovanov/roomler-ai/issues/1157) · **Status:** proposed · **Owner:** agent / remote-control

> ⚠️ **Renumbered from FR-55**, which [#1155](https://github.com/gjovanov/roomler-ai/pull/1155)
> landed on master while this claim was in flight. The **ledger arbitrated**: a claim that
> never reached master is the one that moves, and the lower-issue-id repair applies only
> when two claims BOTH landed — invoking it here would have forced an already-merged FR to
> renumber, which is exactly the churn that rule exists to prevent. Renumbered to the next
> free N, never into a vacated one.

## Goal

Make **Viewer settings → Session → Remote Apps** work on a **Wayland** host —
list windows, focus one, launch an allowlisted app — and let a viewer stream a
**single application window** instead of the whole desktop.

That last part is the circle: [FR-45](FR-45-portal-capture.md) made a Wayland
desktop capturable (portal, then `org.gnome.Mutter.ScreenCast` direct), and
WSLg has been doing per-window remoting on the same machine the whole time —
Weston with `rdp-backend.so` + **`rdprail-shell.so`**, so each Linux window
arrives as its own native Windows window. We capture whole desktops; RAIL
capture is what turns "a screen" into "an app".

## Why now, and what is actually broken

Remote Apps exists and is good (`agents/roomlerd/src/apps/`, three verbs over
the control DC, allowlisted launch, tmux-backed sessions that survive a
restart). It is also, on Linux, **X11-only and gated on the daemon's own
`DISPLAY`** — so on every Wayland host it does not fail, it never engages.

Measured 2026-09-01 on the WSL2 dev box (headless `mutter` + Xwayland) and
cross-checked against the code:

| # | Measurement | Consequence |
|---|---|---|
| 1 | `apps_supported()` is literally `std::env::var_os("DISPLAY").is_some()` (`apps/mod.rs:152`) | The daemon runs as **root under systemd with no `DISPLAY`**, so a Wayland host always answers `supported:false` |
| 2 | `handle_control_message` reads the **daemon's** `DISPLAY` (`apps/mod.rs:~202`), set process-global only by `virtual_desktop` | The feature is structurally bound to Xvfb mode |
| 3 | `linux.rs:58` sets `DISPLAY` on the child but **never `XAUTHORITY`** | With `DISPLAY` alone `wmctrl` fails `Authorization required, but no authorization protocol specified` |
| 4 | mutter starts **Xwayland** on a headless Wayland desktop (`Using public X11 display :0`) | X11 tooling *does* work there — the feature is far closer than it looks |
| 5 | With `DISPLAY=:0` **and** `XAUTHORITY=/run/user/<uid>/.mutter-Xwaylandauth.*`: `wmctrl -l` listed a launched `roomler:app:test` window and `wmctrl -i -a` **focused it (rc=0)** | P1 is plumbing, not new capability |
| 6 | Native Wayland clients (`foot`, `gnome-text-editor`) were **alive and absent** from `wmctrl -l` | X11 tooling cannot see native Wayland windows — ever |
| 7 | Full protocol list of that mutter: `gtk_shell1 wl_compositor(6) wl_shm wl_seat wl_output wp_* xdg_activation_v1 xdg_wm_base(6) zwp_* zxdg_*` — **no `zwlr_foreign_toplevel_management_v1`, no `ext_foreign_toplevel_list_v1`** | There is no way to enumerate other apps' windows on mutter |
| 8 | No `gnome-shell` on the bus (bare mutter) ⇒ no `org.gnome.Shell.Introspect` | The one GNOME enumeration API needs a full shell, not just a compositor |
| 9 | `xdg_activation_v1` **is** present | It is token-based *self*-activation; it does not let us focus a third party's window |

🔑 **The honest headline:** the circle closes cheaply for **Xwayland** apps, and
for **native Wayland** apps only on compositors that expose a foreign-toplevel
protocol. **GNOME deliberately does not** — window management is the
compositor's private business there, and no amount of code on our side changes
that. This FR must ship that asymmetry as a *reported capability*, not discover
it per host.

## Design

| Phase | What | Kill switch |
|---|---|---|
| **P1** | **Make the existing feature engage on Wayland.** Replace the daemon-`DISPLAY` gate with **session discovery** — FR-45 already built `companion::graphical_session()` (uid, `DISPLAY`, `WAYLAND_DISPLAY`) for exactly this — and pass **`XAUTHORITY`** with `DISPLAY` into every `wmctrl`/`xterm` call. Result: list/focus/launch for Xwayland windows on GNOME/KDE/wlroots. ⚠️ Must be a **byte-for-byte no-op for the Xvfb path**, which is the only population using this today. | existing `[virtual_desktop_apps] enabled` |
| **P2** | **Say what is actually visible.** `supported: bool` cannot express "X11 windows only" — the exact `Some([])` vs `None` mistake this project has now made on three surfaces (overlay ACL, `ssh_activity`, FR-49's dark org). Add a `sources` field (`x11` / `wayland` / both) plus a human reason, so the panel can say *"showing X11 (Xwayland) windows; this compositor does not let us enumerate native Wayland windows"* instead of showing a short list that looks like the truth. | n/a (wire additive) |
| **P3** | **Native enumeration where the compositor allows it.** `zwlr_foreign_toplevel_management_v1` on wlroots (list + **activate** + close — full parity, and the only tier where focus works) and `org.gnome.Shell.Introspect.GetWindows` on a full GNOME (list **only**; there is no activate). Detected **at session time, never cached** — FR-45's rule, learned from a host that had every package and still offered nothing depending on start order. | `apps_wayland_enum` (default off until field-proven) |
| **P4** | **Per-window capture — the RAIL payoff.** Portal `SelectSources(types = WINDOW)` (the *host* picks; no enumeration needed, works wherever a portal backend runs) and `org.gnome.Mutter.ScreenCast.RecordWindow` (needs a window id from P3). Reuses the whole FR-45 P3 pipeline — POD negotiation, buffers, wire format, `ScreenCapture` — with only the source selection changed, exactly as P5 did. | `ROOMLERD_WINDOW_CAPTURE` (default off) |
| **P5** | **Wayland-native launch.** The tmux/xterm session model assumes X11. Keep tmux (surviving an agent restart is the whole point) but pick a **Wayland** terminal where there is no Xwayland. | same as P1 |

### What this does NOT try to be

Window *management* — move, resize, close, tile — is out of scope; focus is the
one operation the panel needs. Windows RAIL parity (drawing each remote window
as a separate local window in the browser) is out of scope: the viewer renders
one video surface, and changing that is a much larger UI program than this.

## Acceptance criteria

- [ ] On a **Wayland** host with no Xvfb, the Remote Apps panel lists windows
      instead of reporting the feature unsupported — field-verified, with the
      **before** state (`supported:false`) recorded beside the after
- [ ] `wmctrl` is invoked with **both** `DISPLAY` and `XAUTHORITY`; a host where
      only `DISPLAY` is set is shown to fail (`Authorization required`) so the
      fix is proven to be the thing that fixed it
- [ ] The **Xvfb** path is byte-for-byte unchanged — same replies on a
      virtual-desktop host before and after P1
- [ ] The panel **names what it cannot see**: on GNOME it says native Wayland
      windows are not enumerable, rather than silently listing only Xwayland
      ones. An empty list and an unsupported compositor are distinguishable
- [ ] On a wlroots host, a **native** Wayland window is listed **and focused**
      through `zwlr_foreign_toplevel_management_v1`
- [ ] A single application window is streamed to the browser, and switching
      between two windows is shown to change what the viewer sees
- [ ] Launch works on a Wayland host with no Xwayland at all
- [ ] Every tier degrades honestly: no host reports a capability it does not
      have, and the reason is in the reply rather than only in the daemon log

## Open decisions

- **Does P4 reuse the FR-45 helper process or spawn its own?** Reuse is
  tempting (one session, one consent) but the helper currently owns exactly one
  stream; per-window capture may want a second concurrent one.
- **Is GNOME enumeration worth it at list-only?** Without activate, the panel
  can show native windows but not focus them — arguably worse than saying
  nothing. Decide against a real GNOME session, not in the abstract.
- **Portal WINDOW capture shows a host-side picker.** That is a *second* consent
  surface after FR-45's, and on an unattended host nobody answers it. It may be
  wlroots/mutter-direct only in practice.

## Field-verification log

| Date | What | Result |
|---|---|---|
| 2026-09-01 | Feasibility measured before any code (see the table above) | The circle closes for Xwayland cheaply (P1 is plumbing: `DISPLAY`+`XAUTHORITY` and session discovery, both already built by FR-45); native Wayland enumeration is **compositor-dependent and impossible on GNOME**, which is a capability to report, not a bug to fix |
