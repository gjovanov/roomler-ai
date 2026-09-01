# FR-56 — Remote Apps on Wayland: per-app streaming, and the RAIL circle

**Issue:** [#1157](https://github.com/gjovanov/roomler-ai/issues/1157) · **Status:** **P1 + P2 shipped and field-verified**; **P3 REFUTED — GNOME refuses it**; P4–P5 proposed · **Owner:** agent / remote-control

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
| **P1** ✅ | **Make the existing feature engage on Wayland.** Replace the daemon-`DISPLAY` gate with **session discovery** — FR-45 already built `companion::graphical_session()` (uid, `DISPLAY`, `WAYLAND_DISPLAY`) for exactly this — and pass **`XAUTHORITY`** with `DISPLAY` into every `wmctrl`/`xterm` call. Result: list/focus/launch for Xwayland windows on GNOME/KDE/wlroots. ⚠️ Must be a **byte-for-byte no-op for the Xvfb path**, which is the only population using this today. | existing `[virtual_desktop_apps] enabled` |
| **P2** ✅ | **Say what is actually visible.** `supported: bool` cannot express "X11 windows only" — the exact `Some([])` vs `None` mistake this project has now made on three surfaces (overlay ACL, `ssh_activity`, FR-49's dark org). Add a `sources` field (`x11` / `wayland` / both) plus a human reason, so the panel can say *"showing X11 (Xwayland) windows; this compositor does not let us enumerate native Wayland windows"* instead of showing a short list that looks like the truth. | n/a (wire additive) |
| **P3** ⛔ **REFUTED on GNOME (2026-09-01), not built** | **Native enumeration where the compositor allows it.** `zwlr_foreign_toplevel_management_v1` on wlroots (list + **activate** + close — full parity, and the only tier where focus works) and `org.gnome.Shell.Introspect.GetWindows` on a full GNOME (list **only**; there is no activate). Detected **at session time, never cached** — FR-45's rule, learned from a host that had every package and still offered nothing depending on start order. | `apps_wayland_enum` (default off until field-proven) |
| **P4** ✅ **portal-picker half shipped; `RecordWindow` half unreachable** | **Per-window capture — the RAIL payoff.** Portal `SelectSources(types = WINDOW)`: the *host* picks, so no enumeration is needed and it works wherever a portal backend runs. Reuses the whole FR-45 P3 pipeline — POD negotiation, buffers, wire format, `ScreenCapture` — with only the source mask changed, exactly as P5 did. ⛔ The `org.gnome.Mutter.ScreenCast.RecordWindow` route is **not buildable**: it takes a window id and P3 measured that GNOME refuses the only API that could supply one. 🔑 That leaves per-window capture **attended by construction** — the portal answers by showing a picker, so on a host with nobody at it the capture never starts, which is why the switch defaults off and says so in its own config description. | `ROOMLERD_WINDOW_CAPTURE` (default off) |
| **P5** | **Wayland-native launch.** The tmux/xterm session model assumes X11. Keep tmux (surviving an agent restart is the whole point) but pick a **Wayland** terminal where there is no Xwayland. | same as P1 |

### What this does NOT try to be

Window *management* — move, resize, close, tile — is out of scope; focus is the
one operation the panel needs. Windows RAIL parity (drawing each remote window
as a separate local window in the browser) is out of scope: the viewer renders
one video surface, and changing that is a much larger UI program than this.

## Acceptance criteria

- [x] On a **Wayland** host with no Xvfb, the backend lists windows instead of
      reporting the feature unsupported — verified on Asahi (GNOME Wayland) as
      **root with no `DISPLAY` and no `XAUTHORITY`**, i.e. exactly the daemon's
      own environment: `apps supported: true`, `windows: 1`, and a real window
      title. The **before** is master's gate itself — `apps_supported()` was
      literally `env::var_os("DISPLAY").is_some()` and the daemon has none, so
      it answered `false` by construction. ⚠️ Not yet driven from the browser
      panel end-to-end; that needs a live session and is the remaining half
- [x] `wmctrl` is invoked with **both** `DISPLAY` and `XAUTHORITY`, and the
      fix is proven to be the thing that fixed it: with `DISPLAY` alone — what
      the pre-FR-56 code passed — the same call dies `Authorization required,
      but no authorization protocol specified`; with the discovered cookie it
      lists the window
- [x] The **Xvfb** path is unchanged: a daemon that HAS a `DISPLAY` still
      takes that arm first and runs as the daemon (no discovery, no privilege
      drop). Shown by pointing it at `:99` — it used the daemon's display
      rather than discovering the live session beside it
- [x] The panel **names what it cannot see**: the reply carries a `coverage`
      object (`sources` + `unlisted`), and on a real GNOME Wayland session the
      agent reports `sources: x11` / `NOT listed: native Wayland windows: this
      compositor exposes no protocol to enumerate them`. An empty list and an
      unenumerable source are now distinguishable — including on the ERROR
      arm, which is where an empty list is most likely to be read as calm
- [~] ⛔ **Not attempted — refuted on GNOME and unfalsifiable elsewhere.**
      `org.gnome.Shell.Introspect.GetWindows` exists but answers **`Access
      denied` / "GetWindows is not allowed"** (GNOME Shell 48.8, two different
      D-Bus clients, running AS the session user), and the interface exposes no
      activate method at all — so GNOME is not "list-only", it is **refused**.
      wlroots' `zwlr_foreign_toplevel_management_v1` would give list + activate,
      but **no host in this fleet runs wlroots**, so building it now could only
      be verified in a synthetic sway-in-WSL2 rig — which is the kind of "CI
      green ≠ done" claim this project rejects. Revisit when a wlroots host
      exists
- [x] Asking for a window **reaches the portal as a window request**, and the
      grant is kept apart from the monitor grant. Verified on Asahi (GNOME
      Wayland): that portal advertises `AvailableSourceTypes = 7`
      (`MONITOR|WINDOW|VIRTUAL`), the helper announces *recording ONE WINDOW*
      and then **blocks on the picker** with nobody at the screen — the
      attended-by-construction property observed rather than assumed. Four
      token files now exist (`portal-restore-token{,-rd,-win,-rd-win}`) because
      a window grant and a monitor grant are different grants and reusing one
      file would burn whichever was stored first
- [ ] A single application window is streamed to the browser, and switching
      between two windows is shown to change what the viewer sees. ⚠️ **Needs a
      human at the host** to answer the picker — it is not something this
      agent can complete unattended, and a synthetic pass would prove nothing
- [ ] Launch works on a Wayland host with no Xwayland at all
- [ ] Every tier degrades honestly: no host reports a capability it does not
      have, and the reason is in the reply rather than only in the daemon log

## Open decisions

- **Does P4 reuse the FR-45 helper process or spawn its own?** Reuse is
  tempting (one session, one consent) but the helper currently owns exactly one
  stream; per-window capture may want a second concurrent one.
- ✅ ~~**Is GNOME enumeration worth it at list-only?**~~ **ANSWERED 2026-09-01,
  and the premise was wrong: it is not list-only, it is DENIED.** Measured
  against a real GNOME session (Shell 48.8) as the session user, via both
  `busctl` and `gdbus`: `GetWindows` → `Access denied: GetWindows is not
  allowed`; `GetRunningApplications` → likewise. The refusal is silent on the
  shell side too (nothing in its journal). GNOME gates Introspect to callers it
  trusts, and a fleet agent is not one. 🔑 So the question "is a list without
  focus worth shipping" never arises — there is no list.
- ~~**Portal WINDOW capture shows a host-side picker.**~~ **Settled by
  measurement (P4).** It is a second consent surface, nobody answers it on an
  unattended host, and the mutter-direct escape hatch turned out not to exist
  (P3: `RecordWindow` needs an id GNOME will not give). So per-window capture
  is attended-only on every host in this fleet — shipped behind a default-off
  switch that states this in its own description, rather than left unbuilt.

## Field-verification log

| Date | What | Result |
|---|---|---|
| 2026-09-01 | Feasibility measured before any code (see the table above) | The circle closes for Xwayland cheaply (P1 is plumbing: `DISPLAY`+`XAUTHORITY` and session discovery, both already built by FR-45); native Wayland enumeration is **compositor-dependent and impossible on GNOME**, which is a capability to report, not a bug to fix |
| 2026-09-01 | ✅ **P1 shipped and field-verified on Asahi (GNOME Wayland)** | As **root with no `DISPLAY` and no `XAUTHORITY`** — the daemon's actual environment — the backend now discovers the session (`display=:0 user=<session owner> xauthority=/run/user/<uid>/`.mutter-Xwaylandauth.*`) and reports `apps supported: true`, `windows: 1` with a real title. Before, `apps_supported()` was `env::var_os("DISPLAY").is_some()` and answered **false** by construction. Proof the cookie is the fix: `DISPLAY` alone (what the old code passed) dies `Authorization required, but no authorization protocol specified`; `DISPLAY`+`XAUTHORITY` lists the window. |
| 2026-09-01 | 🚨 **Found a PRE-EXISTING silent failure while field-testing P1** | `list()` parsed `wmctrl -l`'s stdout **without checking its exit status**, so a display it could not open — empty stdout, non-zero exit — returned `Ok(vec![])`: *no windows*, which is a different and far more reassuring claim than *I could not reach the desktop*. Measured by pointing the daemon at `:99` (no X server): it reported `windows: 0`. `focus` and `tmux new-session` already checked; only this one did not. 🔑 P1 makes it matter: the display is now **discovered** rather than owned, so it can go stale (a compositor restart invalidates the cookie) where a daemon-started Xvfb could not. Now: `list failed: wmctrl could not read the window list from :99: Cannot open display.` |
| 2026-09-01 | ⚠️ **rustc 1.95 ICEs while RENDERING a real error here** | A `tracing::info!(%display, …)` whose local was named `display` collides with tracing's own `field::display` helper, and rustc panicked (`slice/index.rs`, empty query stack) instead of printing the error — `cargo check` reported only *the compiler unexpectedly panicked*. 🔑 `--message-format=short` bypasses the renderer and showed both real errors immediately. A/B'd against clean master first (it compiles), per the standing rule. |
| 2026-09-01 | 🔧 **`roomlerd apps-probe` added** | Remote Apps was answerable only by driving it over a WebRTC data channel from a browser, which conflates the backend with signalling, transport and the UI — the same argument `capture-smoke` was built on. It prints whether a desktop was found, as whom, with which cookie, and what it sees; and it says explicitly that an EMPTY list is not the same as unsupported, and that native Wayland windows would not appear even if present. |
| 2026-09-01 | ✅ **P2 shipped and field-verified** | The list reply carries `coverage` (`sources` + `unlisted`) end to end: agent → wire → composable → panel. On the real GNOME Wayland session the daemon reports `sources: x11` and `NOT listed: native Wayland windows: this compositor exposes no protocol to enumerate them`, beside the one Xwayland window it CAN see. 🔑 The trait method (rather than a field set at construction) means **the compiler forces every backend to answer** — it caught the test fake immediately. ⚠️ Coverage rides the ERROR arm too: a failed listing is exactly where an empty list reads as a calm desktop. ⚠️ The UI parses it defensively and an absent `coverage` stays absent — inventing an empty one would claim the listing was complete, which is the bug this phase exists to fix. |
| 2026-09-01 | ⛔ **P3 REFUTED — GNOME does not merely lack window enumeration, it REFUSES it** | `org.gnome.Shell.Introspect.GetWindows` is present and correctly typed (`a{ta{sv}}`), and calling it as the session user returns **`Access denied` — "GetWindows is not allowed"**. Reproduced with **two independent clients** (`busctl` and `gdbus`) on **GNOME Shell 48.8**; `GetRunningApplications` is refused identically, and gnome-shell logs nothing about either. The interface also has **no activate/focus/raise method at all** (0 matches on introspection), so even a granted listing could never drive the panel's one action. 🔑 The spec's open question — *is a list without focus worth shipping?* — is therefore moot: there is no list. ⚠️ wlroots' `zwlr_foreign_toplevel_management_v1` WOULD give list+activate, but **no fleet host runs wlroots**, so building that tier now could only be "verified" in a synthetic rig. Not built; recorded instead. |
| 2026-09-01 | ✅ **P4 shipped (portal-picker route) and field-measured on Asahi** | `SelectSources(types=WINDOW)` behind `ROOMLERD_WINDOW_CAPTURE` (default off). That host's portal advertises `AvailableSourceTypes = 7` (`MONITOR\|WINDOW\|VIRTUAL`), so the picker route is available; the helper logged *recording ONE WINDOW (the portal will show a picker)* and then **blocked until the 20 s timeout** with nobody at the screen. 🔑 That timeout **is the result**, not a failure: it is the attended-by-construction property observed instead of assumed, and it is why the switch defaults off and says so in its own config-surface description. ⛔ The mutter-direct half (`RecordWindow`) is **unreachable, not unimplemented** — it takes a window id and P3 measured that GNOME refuses the only API that could supply one, so there is no unattended per-window path on GNOME at all. ⚠️ The restore token had to split **four** ways (`portal-restore-token{,-rd,-win,-rd-win}`): a window grant and a monitor grant are different grants, and sharing the file would burn whichever was stored first — the same reason the input grant already lived apart. The test asserts all four differ **as a set**, because asserting only that two differ would pass with three of them colliding. |
