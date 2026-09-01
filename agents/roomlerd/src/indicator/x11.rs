// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-27 — the native consent panel on X11.
//!
//! Linux had no on-screen consent surface of any kind: `indicator/mod.rs`
//! stubs the whole module out there, so a device set to "Prompt on host" could
//! only be answered from a terminal. Phase 3.2 gave it the desktop companion;
//! this is the daemon drawing the prompt itself, which needs no second process
//! running, no login-session plumbing and no IPC.
//!
//! ## What this is NOT
//!
//! Only the PROMPT. The "Being viewed by …" banner on Linux stays with the
//! companion — it already works there, and a second implementation of a
//! long-lived always-on-top strip buys nothing that the fallback chain does
//! not already provide. A prompt is different: it is the thing standing
//! between a request and a session, so it is worth being able to raise with
//! nothing else installed.
//!
//! ## Where it does and does not appear
//!
//! The probe is a real `x11rb::connect`, not a `DISPLAY` sniff, because the
//! question is "can this process actually put a window up", and the two
//! diverge constantly:
//!
//! - **A per-user daemon under a graphical login** — works. `DISPLAY` and
//!   `XAUTHORITY` are inherited from the session.
//! - **A ROOT systemd daemon** (the `--system` flavour, and every headless
//!   node) — no `DISPLAY`, so `connect` fails and the caller falls through to
//!   the companion, which is started INTO the user's session. Correct: a root
//!   process reaching into someone's X session is not something to arrange by
//!   accident.
//! - **Wayland** — XWayland gives us a connection, and an override-redirect
//!   window on it maps as a normal XWayland surface. It shows; it just cannot
//!   guarantee stacking above native Wayland clients, because no compositor
//!   outside wlroots exposes `wlr-layer-shell` to arbitrary clients. Better
//!   than nothing, and honest about it in the log.
//!
//! ⚠️ **X11 has no capture exclusion.** The Windows panel is invisible in the
//! outgoing video (`WDA_EXCLUDEFROMCAPTURE`); this one is not, and there is no
//! X protocol that would make it so. It is drawn ONLY while a prompt is
//! outstanding — before any media flows for that session — so the exposure is
//! bounded to a concurrent second session already in progress.

#![cfg(all(target_os = "linux", feature = "viewer-indicator-x11"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Gcontext,
    PropMode, Rectangle, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::{ConsentSender, PromptView};

/// Panel geometry, in pixels. X11 core fonts are bitmap and DPI-naive, so
/// these are literal — matching how `indicator/win.rs` already works.
const W: u16 = 480;
const H: u16 = 236;
const PAD: i16 = 14;
const BTN_W: i16 = 108;
const BTN_H: i16 = 30;

// 0xRRGGBB, the X pixel format for a TrueColor visual.
const BG: u32 = 0x001E1E1E;
const FRAME: u32 = 0x00FF3333;
const TEXT: u32 = 0x00FFFFFF;
const MUTED: u32 = 0x00B0B0B0;
const APPROVE: u32 = 0x002E9E4F;
const DENY: u32 = 0x00FF3333;

/// How often the event loop wakes: drives the countdown and picks up
/// show/hide requests. X has no timer, and blocking in `wait_for_event` would
/// mean a prompt could not be raised from another thread.
const TICK: Duration = Duration::from_millis(120);

#[derive(Default)]
struct Shared {
    prompt: Option<PromptView>,
    /// Set by [`Inner::dismiss`] / a click, read by the loop.
    dirty: bool,
}

#[derive(Clone)]
pub(super) struct Inner {
    shared: Arc<Mutex<Shared>>,
    /// Whether the X thread is up. A dead thread must report `false` from
    /// `prompt` so the caller falls through to the companion rather than
    /// waiting on a panel nobody will draw.
    alive: Arc<AtomicBool>,
}

// RETIRED-NAME-ANCHOR(24): `ROOMLER_AGENT_VIRTUAL_DESKTOP` is the RETIRED
// spelling. FR-21 P3 kept it working; FR-46 P2b stopped, after every host that
// set one had been migrated. It is still named here because the guard below is
// what a virtual-desktop host depends on, and the tests feed the retired name
// in to prove it now does nothing.
/// ⚠️ FR-27, field-measured on `mars` + `jupiter` 2026-08-29: a
/// virtual-desktop host is NOT an attended host, even though
/// it has a perfectly good X display.
///
/// The daemon starts that Xvfb itself so a headless server can be
/// remote-controlled, so the display's ONLY viewer is a remote controller —
/// which makes it the worst possible place to ask "may this remote controller
/// in?". Two concrete failures, both observed:
///
///   * With nobody attached, the panel is drawn where no human can see it, and
///     the agent reports `native=true have_surface=true`. The session then dies
///     `timeout` — "nobody answered" — when the truthful answer is
///     `no_prompt_surface`, "there was nobody to ask". Those have different
///     fixes, and telling them apart is the whole point of finding 3.
///   * With somebody attached, it is worse than useless: viewer A, already in
///     a session, sees and can click **Approve** on viewer B's prompt. The
///     party being asked for permission is the party asking.
///
/// So the virtual desktop is declined as a PROMPT surface. It stays a fine
/// place for the "being viewed" banner (the companion owns that on Linux
/// today), and a real X session on the same host is unaffected — this reads
/// the daemon's own configuration, not the display.
fn display_is_our_own_virtual_desktop() -> bool {
    // Must stay byte-identical to `main.rs::virtual_desktop_requested`: the two
    // answer the SAME question from different crates, and a host where they
    // disagree gets a virtual desktop that main.rs asked for and this refuses to
    // recognise as its own.
    //
    // They did disagree. This hand-rolled `ROOMLERD_` -> `ROOMLER_AGENT_` pair
    // skipped `node_env`'s middle arm (`ROOMLER_NODE_`) and, more importantly, its
    // config fallback — so the knob set through the S2 config surface
    // (`roomler config`, the way an operator is told to set it) was visible to
    // main.rs and invisible here. The comment claimed "same accessor shape as
    // main.rs" while being a different shape, which is the exact class of stale
    // assertion FR-21 exists to remove.
    tunnel_core::env::node_env("VIRTUAL_DESKTOP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

impl Inner {
    pub(super) fn new(consent_tx: ConsentSender) -> Result<Self> {
        if display_is_our_own_virtual_desktop() {
            anyhow::bail!(
                "this host's X display is the daemon's own virtual desktop — \
                 its only viewer is a remote controller, so it is not a place \
                 to ask for consent"
            );
        }
        // Probe by CONNECTING. `DISPLAY` being set proves nothing (a stale
        // value in a service environment is common) and its absence is the
        // normal state for the root daemon, where falling back is right.
        let (conn, screen_num) = x11rb::connect(None).context("connecting to the X display")?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let alive = Arc::new(AtomicBool::new(true));

        let shared_for_thread = shared.clone();
        let alive_for_thread = alive.clone();
        std::thread::Builder::new()
            .name("roomler-consent-x11".into())
            .spawn(move || {
                if let Err(e) = run(conn, screen_num, shared_for_thread, consent_tx) {
                    tracing::warn!(error = %format!("{e:#}"), "x11 consent panel exited");
                }
                // Say so, so the NEXT prompt takes the companion path instead
                // of a window that will never be drawn.
                alive_for_thread.store(false, Ordering::SeqCst);
            })
            .context("spawning the x11 consent thread")?;

        tracing::info!("x11 consent panel available");
        Ok(Self { shared, alive })
    }

    pub(super) fn prompt(&self, view: PromptView) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        let mut s = self.shared.lock().unwrap();
        // At most one on screen: two overlapping Approve/Deny panels is how
        // someone approves the wrong thing.
        if s.prompt.is_some() {
            return false;
        }
        s.prompt = Some(view);
        s.dirty = true;
        true
    }

    pub(super) fn dismiss(&self, session_hex: &str) {
        let mut s = self.shared.lock().unwrap();
        if s.prompt.as_ref().map(|p| p.session_hex.as_str()) != Some(session_hex) {
            return;
        }
        s.prompt = None;
        s.dirty = true;
    }
}

fn approve_rect() -> (i16, i16, u16, u16) {
    (
        W as i16 - PAD - BTN_W,
        H as i16 - PAD - BTN_H,
        BTN_W as u16,
        BTN_H as u16,
    )
}

fn deny_rect() -> (i16, i16, u16, u16) {
    let (ax, ay, aw, ah) = approve_rect();
    (ax - 8 - BTN_W, ay, aw, ah)
}

fn hit(r: (i16, i16, u16, u16), x: i16, y: i16) -> bool {
    x >= r.0 && x < r.0 + r.2 as i16 && y >= r.1 && y < r.1 + r.3 as i16
}

fn run<C: Connection + Send + 'static>(
    conn: C,
    screen_num: usize,
    shared: Arc<Mutex<Shared>>,
    consent_tx: ConsentSender,
) -> Result<()> {
    let screen = &conn.setup().roots[screen_num];
    let win: Window = conn.generate_id()?;

    // `override_redirect` keeps the window manager out of it entirely: no
    // decorations, no reparenting, no placement policy. That is what makes it
    // behave like an overlay rather than an app window — and on a bare X
    // session with no WM running at all, it is the only thing that works.
    let x = ((screen.width_in_pixels as i32 - W as i32) / 2) as i16;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        x,
        48,
        W,
        H,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(BG)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_RELEASE),
    )?;

    // Belt and braces for the compositors that DO honour hints on an
    // override-redirect window: say this is a notification and ask to be kept
    // above. Failures are ignored — the override-redirect above is the part
    // that actually carries.
    if let (Ok(wtype), Ok(notif), Ok(state), Ok(above)) = (
        conn.intern_atom(false, b"_NET_WM_WINDOW_TYPE"),
        conn.intern_atom(false, b"_NET_WM_WINDOW_TYPE_NOTIFICATION"),
        conn.intern_atom(false, b"_NET_WM_STATE"),
        conn.intern_atom(false, b"_NET_WM_STATE_ABOVE"),
    ) && let (Ok(wtype), Ok(notif), Ok(state), Ok(above)) =
        (wtype.reply(), notif.reply(), state.reply(), above.reply())
    {
        let _ = conn.change_property32(
            PropMode::REPLACE,
            win,
            wtype.atom,
            AtomEnum::ATOM,
            &[notif.atom],
        );
        let _ = conn.change_property32(
            PropMode::REPLACE,
            win,
            state.atom,
            AtomEnum::ATOM,
            &[above.atom],
        );
    }

    let font = open_font(&conn)?;
    let gc: Gcontext = conn.generate_id()?;
    conn.create_gc(
        gc,
        win,
        &CreateGCAux::new()
            .foreground(TEXT)
            .background(BG)
            .font(font),
    )?;
    conn.flush()?;

    let mut mapped = false;
    let mut last_secs = u64::MAX;
    loop {
        // Drain X events first — a click must not wait out a tick.
        while let Some(ev) = conn.poll_for_event()? {
            match ev {
                Event::Expose(_) => {
                    last_secs = u64::MAX; // force a repaint below
                }
                Event::ButtonRelease(e) => {
                    let allow = if hit(approve_rect(), e.event_x, e.event_y) {
                        true
                    } else if hit(deny_rect(), e.event_x, e.event_y) {
                        false
                    } else {
                        continue;
                    };
                    // Take the id and clear the prompt in ONE borrow, so a
                    // double-click cannot answer twice.
                    let session = {
                        let mut s = shared.lock().unwrap();
                        s.dirty = true;
                        match s.prompt.take() {
                            Some(v) => v.session_hex,
                            None => continue,
                        }
                    };
                    // Report, never resolve: the broker is the single decision
                    // point, and it applies the live-prompt gate.
                    let _ = consent_tx.try_send((session, allow));
                }
                _ => {}
            }
        }

        let (view, dirty) = {
            let mut s = shared.lock().unwrap();
            let d = std::mem::take(&mut s.dirty);
            (s.prompt.clone(), d)
        };

        match (&view, mapped) {
            (Some(_), false) => {
                conn.map_window(win)?;
                // Re-assert stacking on every raise: something else may have
                // come to the top since the last prompt.
                conn.configure_window(
                    win,
                    &x11rb::protocol::xproto::ConfigureWindowAux::new()
                        .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
                )?;
                mapped = true;
                last_secs = u64::MAX;
            }
            (None, true) => {
                conn.unmap_window(win)?;
                mapped = false;
            }
            _ => {}
        }

        if let Some(v) = &view {
            let secs = v
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_secs();
            // Repaint only when the displayed second changes: a full redraw at
            // 8 fps to show the same number is flicker for nothing.
            if secs != last_secs || dirty {
                last_secs = secs;
                paint(&conn, win, gc, v, secs)?;
                conn.flush()?;
            }
        }

        std::thread::sleep(TICK);
    }
}

/// Open a readable font, preferring something proportional and falling back to
/// the one font every X server is required to have.
///
/// Core X fonts, not Xft: `x11rb` has no Xft binding, and pulling a
/// client-side font stack into the daemon to draw six lines of text would cost
/// more than it returns. `fixed` is ugly and always present, which is the
/// right last resort for a prompt that must not fail to appear.
fn open_font<C: Connection>(conn: &C) -> Result<x11rb::protocol::xproto::Font> {
    const CANDIDATES: &[&[u8]] = &[
        b"-*-dejavu sans-medium-r-normal--14-*-*-*-*-*-iso10646-1",
        b"-*-helvetica-medium-r-normal--14-*-*-*-*-*-iso8859-1",
        b"-*-liberation sans-medium-r-normal--14-*-*-*-*-*-*-*",
        b"9x15",
        b"fixed",
    ];
    for name in CANDIDATES {
        let font = conn.generate_id()?;
        // `open_font` is asynchronous; only a round-trip proves the server
        // actually matched something.
        if conn.open_font(font, name).is_ok()
            && conn.query_font(font).is_ok_and(|c| c.reply().is_ok())
        {
            return Ok(font);
        }
    }
    Err(anyhow!("no usable X core font (not even `fixed`)"))
}

fn paint<C: Connection>(
    conn: &C,
    win: Window,
    gc: Gcontext,
    v: &PromptView,
    secs: u64,
) -> Result<()> {
    let fill = |color: u32, x: i16, y: i16, w: u16, h: u16| -> Result<()> {
        conn.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
        conn.poly_fill_rectangle(
            win,
            gc,
            &[Rectangle {
                x,
                y,
                width: w,
                height: h,
            }],
        )?;
        Ok(())
    };
    // `image_text8` draws the BASELINE at y, so every call below passes a
    // baseline, not a box top — the single most common way to lose a line of
    // text off the top of an X window.
    let text = |color: u32, x: i16, baseline: i16, s: &str| -> Result<()> {
        conn.change_gc(gc, &ChangeGCAux::new().foreground(color).background(BG))?;
        // Core fonts are 8-bit; a non-Latin-1 display name would otherwise
        // abort the whole paint. Substituting keeps the prompt on screen,
        // which matters more than rendering every glyph.
        let bytes: Vec<u8> = s
            .chars()
            .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
            .take(120)
            .collect();
        conn.image_text8(win, gc, x, baseline, &bytes)?;
        Ok(())
    };

    fill(BG, 0, 0, W, H)?;
    // 1px frame, as four rectangles — X has no "draw rect outline" that
    // respects a 1px line width consistently across servers.
    fill(FRAME, 0, 0, W, 1)?;
    fill(FRAME, 0, H as i16 - 1, W, 1)?;
    fill(FRAME, 0, 0, 1, H)?;
    fill(FRAME, W as i16 - 1, 0, 1, H)?;

    let mut y = PAD + 16;
    text(TEXT, PAD, y, &v.title)?;
    y += 24;
    text(TEXT, PAD, y, &v.lead)?;
    y += 22;
    if !v.org.is_empty() {
        text(MUTED, PAD, y, &format!("On behalf of {}", v.org))?;
        y += 20;
    }
    if !v.detail.is_empty() {
        text(TEXT, PAD, y, &v.detail)?;
        y += 20;
    }
    if !v.permissions.is_empty() {
        text(MUTED, PAD, y, &format!("Permissions: {}", v.permissions))?;
    }

    let (dx, dy, dw, dh) = deny_rect();
    let (ax, ay, aw, ah) = approve_rect();
    text(
        MUTED,
        PAD,
        ay + 20,
        &format!("Expires in {}", super::format_countdown(secs)),
    )?;
    fill(DENY, dx, dy, dw, dh)?;
    text(TEXT, dx + 34, dy + 20, "Deny")?;
    fill(APPROVE, ax, ay, aw, ah)?;
    text(TEXT, ax + 24, ay + 20, "Approve")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::display_is_our_own_virtual_desktop;

    // RETIRED-NAME-ANCHOR(16): the legacy env spelling under test, on purpose
    // — see the anchor on `display_is_our_own_virtual_desktop`.
    /// The FR-27 field finding, locked. `mars` and `jupiter` both carry
    /// `ROOMLER_AGENT_VIRTUAL_DESKTOP=1` in an operator-authored systemd
    /// drop-in (FR-21 P3 kept the legacy spelling working), and both reported
    /// `native=true have_surface=true` for a prompt drawn where nobody could
    /// see it. Both spellings must decline.
    ///
    /// ⚠️ Serial and self-restoring: these are process-wide env vars.
    #[test]
    fn a_virtual_desktop_host_is_not_a_consent_surface() {
        // RETIRED-NAME-ANCHOR(9): both arms of `node_env`'s chain plus the
        // retired one, so the accessor can neither silently drop a live arm nor
        // silently start honouring the dead one. The MIDDLE arm is the
        // regression this array exists to catch: the hand-rolled version this
        // replaced read only the first and last, so `ROOMLER_NODE_*` — and the
        // config fallback behind it — were invisible here while `main.rs` saw
        // them. docs/fr/FR-46
        const KEYS: [&str; 2] = ["ROOMLERD_VIRTUAL_DESKTOP", "ROOMLER_NODE_VIRTUAL_DESKTOP"];
        const RETIRED: &str = "ROOMLER_AGENT_VIRTUAL_DESKTOP";
        let saved: Vec<_> = KEYS
            .iter()
            .chain([RETIRED].iter())
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        // SAFETY (edition 2024): no other test in this crate touches these.
        unsafe {
            for k in KEYS {
                std::env::remove_var(k);
            }
            assert!(
                !display_is_our_own_virtual_desktop(),
                "unset must not suppress the panel — an ordinary Linux desktop"
            );
            for k in KEYS {
                std::env::set_var(k, "1");
                assert!(display_is_our_own_virtual_desktop(), "{k}=1 must decline");
                std::env::set_var(k, "true");
                assert!(
                    display_is_our_own_virtual_desktop(),
                    "{k}=true must decline"
                );
                std::env::set_var(k, "0");
                assert!(
                    !display_is_our_own_virtual_desktop(),
                    "{k}=0 is off, not on"
                );
                std::env::remove_var(k);
            }

            // FR-46 P2b: the retired arm must now DECLINE to suppress the
            // panel, because nothing reads it any more. Asserted rather than
            // dropped from the loop — a virtual-desktop host that stopped
            // being recognised would put a consent prompt back on a display
            // only the remote controller can see, which is the exact FR-27
            // finding this test was written for.
            std::env::set_var(RETIRED, "1");
            assert!(
                !display_is_our_own_virtual_desktop(),
                "{RETIRED}=1 must be IGNORED — it is retired, not an alias"
            );
            std::env::remove_var(RETIRED);
            for (k, v) in saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}
