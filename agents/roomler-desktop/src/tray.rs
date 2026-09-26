// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! System-tray icon + right-click menu. Built atop Tauri 2's
//! `TrayIcon` API (which wraps the `tray-icon` crate on the OS
//! layer).
//!
//! Menu items:
//!   - Open Roomler       — show the main window (Overview view)
//!   - Start/Stop recording — FR-85: record this screen through the device
//!                           service; the label and the tray tooltip follow
//!                           the recorder (a light poll, every 3 s), and
//!                           the ICON turns red while any recording runs,
//!                           this device's own or a controller's
//!   - Onboarding…        — show the main window on the Onboarding view
//!   - Welcome tour…      — FR-84 D6: the first-run tour, again
//!   - Check for Updates  — invoke `cmd_check_update` and surface
//!                           the result in the Overview's update panel
//!   - Open Logs Folder   — invoke `cmd_open_log_dir`
//!   - Quit                — exit the desktop app; the device service
//!                           keeps running unaffected.
//!
//! Navigation: the SPA is one page with a hash router (`app.js`), so
//! `show_window` navigates by evaluating `location.hash = '#/<view>'`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Runtime};

pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    // Build the menu. IDs are inspected in `on_menu_event` below.
    let open_status = MenuItem::with_id(app, "open_status", "Device status", true, None::<&str>)?;
    // S7 — the embedded web-app window (WebView2 on Windows; browser
    // elsewhere). Distinct from the LOCAL status window above.
    let open_web = MenuItem::with_id(app, "open_web", "Open Roomler…", true, None::<&str>)?;
    // FR-85 — disabled until the service says it has a recorder.
    let record = MenuItem::with_id(app, RECORD_ITEM_ID, "Start recording", false, None::<&str>)?;
    let onboarding = MenuItem::with_id(app, "onboarding", "Onboarding…", true, None::<&str>)?;
    let welcome = MenuItem::with_id(app, "welcome", "Welcome tour…", true, None::<&str>)?;
    let check_updates_item = MenuItem::with_id(
        app,
        "check_updates",
        "Check for Updates",
        true,
        None::<&str>,
    )?;
    let open_logs = MenuItem::with_id(app, "open_logs", "Open Logs Folder", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open_status,
            &open_web,
            &record,
            &onboarding,
            &welcome,
            &check_updates_item,
            &open_logs,
            &quit,
        ],
    )?;

    let on_menu = |app: &AppHandle<R>, event: tauri::menu::MenuEvent| match event.id.as_ref() {
        "open_status" => show_window(app, "/overview"),
        "open_web" => open_roomler_web(app),
        RECORD_ITEM_ID => toggle_recording(app),
        "onboarding" => show_window(app, "/onboarding"),
        "welcome" => show_window(app, "/welcome"),
        "check_updates" => check_updates(app),
        "open_logs" => {
            // The resolve probes the service flavour (CLI spawns) — keep
            // it off the menu/UI thread.
            tauri::async_runtime::spawn_blocking(|| {
                let _ = crate::commands::open_log_dir_blocking();
            });
        }
        "quit" => {
            app.exit(0);
        }
        _ => {}
    };
    let on_icon = |tray: &tauri::tray::TrayIcon<R>, event: tauri::tray::TrayIconEvent| {
        // Left-click on the tray icon opens the status window for parity with
        // operators who expect the icon itself to do something useful.
        if let tauri::tray::TrayIconEvent::Click {
            button: tauri::tray::MouseButton::Left,
            button_state: tauri::tray::MouseButtonState::Up,
            ..
        } = event
        {
            show_window(tray.app_handle(), "/overview");
        }
    };

    spawn_recording_watch(app.clone(), record);

    // FR-27 — ADOPT the tray Tauri already created from `app.trayIcon` in
    // tauri.conf.json instead of building a second one.
    //
    // Tauri 2 auto-creates a tray from that config block during `build()`,
    // carrying the icon and NO menu. This function then built its own with
    // `TrayIconBuilder` — menu, handlers, and (because nothing ever called
    // `.icon()`) no image at all. The result was two tray entries: one that
    // looked right and did nothing, and one that was blank and held the whole
    // menu. That blank one is what the operator was clicking.
    //
    // Adopting also keeps ONE source of truth for the image: `iconPath` in the
    // config, embedded at build time by `tauri-build`.
    if let Some(tray) = app.tray_by_id(CONFIG_TRAY_ID) {
        tray.set_menu(Some(menu))?;
        tray.set_show_menu_on_left_click(false)?;
        tray.on_menu_event(on_menu);
        tray.on_tray_icon_event(on_icon);
        return Ok(());
    }

    // Fallback: the config block is missing or its id changed. Build one, but
    // give it an EXPLICIT icon — a tray with no image is indistinguishable
    // from a broken install, and that is precisely the bug above.
    tracing::warn!(
        id = CONFIG_TRAY_ID,
        "no config-declared tray icon found — building one with the embedded fallback image"
    );
    let mut builder = TrayIconBuilder::with_id(FALLBACK_TRAY_ID)
        .tooltip("Roomler")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu)
        .on_tray_icon_event(on_icon);
    // The app icon is already embedded (`bundle.icon`) and needs no PNG
    // decoder at runtime, unlike `Image::from_bytes`, which is behind tauri's
    // `image-png` feature. Wrong shape for a macOS menu bar — it is not a
    // template — but VISIBLE, which is the entire job of a fallback here.
    match app.default_window_icon() {
        Some(icon) => builder = builder.icon(icon.clone()),
        // Never silently ship a blank tray. The symptom reads to a user as
        // "the app did not start", which is how this went unnoticed.
        None => tracing::error!(
            "no embedded app icon either — the tray will be BLANK; \
             check `app.trayIcon` and `bundle.icon` in tauri.conf.json"
        ),
    }
    builder.build(app)?;
    Ok(())
}

/// The id Tauri gives the tray it creates from `app.trayIcon`. Set explicitly
/// in `tauri.conf.json` so this lookup does not depend on Tauri's default.
const CONFIG_TRAY_ID: &str = "roomler";
/// The fallback tray's id (see `install`).
const FALLBACK_TRAY_ID: &str = "roomler-desktop-tray";

/// FR-85 — the Start/Stop recording menu item.
const RECORD_ITEM_ID: &str = "record_toggle";

/// FR-85 — the configured tray's icon while nothing records: the menu-bar
/// template `app.trayIcon` names.
const TRAY_IDLE: tauri::image::Image<'static> = tauri::include_image!("icons/tray.png");
/// FR-85 — the tray while a recording runs: the mark in full colour with a
/// red dot (`scripts/gen-tray-icons.py`).
const TRAY_RECORDING: tauri::image::Image<'static> =
    tauri::include_image!("icons/tray-recording.png");

/// FR-85 — the configured tray's icon for the recorder's state, and whether
/// it is a macOS template. ⚠️ The red one is never a template: the system
/// tints a template to the menu bar's colour, and a recording must read as
/// red (docs/remote-control.md §11.3).
fn tray_look(recording: bool) -> (tauri::image::Image<'static>, bool) {
    if recording {
        (TRAY_RECORDING, false)
    } else {
        (TRAY_IDLE, true)
    }
}

/// FR-85 — the two things that turn the tray red, each learned by a poll:
/// - a controller's recording MARKED on a session (the session watch in
///   `main.rs`, every 750 ms, the poll the "being viewed" banner rides). The
///   daemon marks a remote recording before its first frame is written, so
///   the red comes up with the banner (AC5: "the host's banner and red dot");
/// - the recorder RUNNING (`spawn_recording_watch`, every 3 s), which also
///   covers a recording the person started here.
///
/// ⚠️ Each changes only when its poll is ANSWERED. A poll that failed (the
/// pipe busy for a moment) keeps what the last answer said, so the red never
/// drops off a recording that is still running; a recording that ended
/// answers so. (The FR-84 D1 rule: a failed refresh keeps the last good
/// data. For an indicator of recording, the wrong way to fail is to say
/// nothing is.)
static REMOTE_MARKED: AtomicBool = AtomicBool::new(false);
static RECORDER_RUNNING: AtomicBool = AtomicBool::new(false);
/// Whether the tray is red now. Both watches apply it; the lock makes one
/// change at a time.
static SHOWN_RED: Mutex<bool> = Mutex::new(false);

/// FR-85 — the recorder's answer, or what the last one said when the poll
/// failed (`None`).
fn recorder_running(answer: Option<&roomler_localapi::RecordingState>, was: bool) -> bool {
    answer.map_or(was, |s| s.active)
}

/// FR-85 — whether any session carries a recording mark, or what the last
/// answer said when the poll failed (`None`).
fn remote_marked(answer: Option<&[roomler_localapi::RcSessionInfo]>, was: bool) -> bool {
    answer.map_or(was, |sessions| sessions.iter().any(|s| s.recording))
}

/// FR-85 — the session watch's answer (`main.rs`): `None` when the poll
/// failed.
pub fn sessions_answered<R: Runtime>(
    app: &AppHandle<R>,
    answer: Option<&[roomler_localapi::RcSessionInfo]>,
) {
    let was = REMOTE_MARKED.load(Ordering::Acquire);
    REMOTE_MARKED.store(remote_marked(answer, was), Ordering::Release);
    apply_red(app);
}

/// Red while either says a recording runs; back after.
fn apply_red<R: Runtime>(app: &AppHandle<R>) {
    let want = REMOTE_MARKED.load(Ordering::Acquire) || RECORDER_RUNNING.load(Ordering::Acquire);
    let Ok(mut shown) = SHOWN_RED.lock() else {
        return;
    };
    if *shown == want {
        return;
    }
    for id in [CONFIG_TRAY_ID, FALLBACK_TRAY_ID] {
        if let Some(tray) = app.tray_by_id(id) {
            set_tray_look(app, &tray, id == CONFIG_TRAY_ID, want);
        }
    }
    *shown = want;
}

/// What the tray shows for the recorder's state. Pure, so the wording is
/// tested: a service without a recorder answers `available: false` and the
/// item is disabled; `None` (nothing has answered yet) is the tray as
/// `install` builds it, the item disabled too. A controller's recording
/// names them: the person at the device sees who is recording their screen.
fn record_labels(state: Option<&roomler_localapi::RecordingState>) -> (String, bool, String) {
    match state {
        Some(s) if s.active => {
            let secs = s.duration_ms / 1000;
            let t = format!("{}:{:02}", secs / 60, secs % 60);
            let tooltip = match s.remote_controller.as_deref() {
                Some(who) => format!("Roomler — {who} is recording, {t}"),
                None => format!("Roomler — recording {t}"),
            };
            (format!("Stop recording ({t})"), true, tooltip)
        }
        // Start only where the service says a recording can start at all.
        Some(s) => ("Start recording".into(), s.available, "Roomler".into()),
        None => ("Start recording".into(), false, "Roomler".into()),
    }
}

/// Keep the menu item, the tooltip and the icon in step with the recorder,
/// started by this item, the Recordings view, `roomler record` or a
/// controller alike. One LocalAPI request every 3 s — the listener keeps a
/// pool of instances (FR-84 D1).
fn spawn_recording_watch<R: Runtime>(app: AppHandle<R>, item: MenuItem<R>) {
    tauri::async_runtime::spawn(async move {
        let mut shown: Option<(String, bool, String)> = None;
        loop {
            let state = match roomler_localapi::connect().await {
                Ok(mut c) => c.record_status().await.ok(),
                Err(_) => None,
            };
            // Only an answer changes the tray, the words as well as the icon
            // (`recorder_running`), so the two never disagree. Until the
            // first answer it stays as `install` built it: the item disabled.
            if let Some(s) = state.as_ref() {
                let labels = record_labels(Some(s));
                if shown.as_ref() != Some(&labels) {
                    let _ = item.set_text(&labels.0);
                    let _ = item.set_enabled(labels.1);
                    for id in [CONFIG_TRAY_ID, FALLBACK_TRAY_ID] {
                        if let Some(tray) = app.tray_by_id(id) {
                            let _ = tray.set_tooltip(Some(&labels.2));
                        }
                    }
                    shown = Some(labels);
                }
            }
            let was = RECORDER_RUNNING.load(Ordering::Acquire);
            RECORDER_RUNNING.store(recorder_running(state.as_ref(), was), Ordering::Release);
            apply_red(&app);
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
}

/// FR-85 — turn the tray red while a recording runs, and back after. The
/// fallback tray (built when `app.trayIcon` names none) idles on the app
/// icon, as `install` built it.
fn set_tray_look<R: Runtime>(
    app: &AppHandle<R>,
    tray: &tauri::tray::TrayIcon<R>,
    configured: bool,
    recording: bool,
) {
    let (icon, template) = if configured || recording {
        tray_look(recording)
    } else {
        match app.default_window_icon() {
            Some(icon) => (icon.clone(), false),
            None => return,
        }
    };
    if let Err(e) = tray.set_icon(Some(icon)) {
        tracing::warn!(%e, recording, "tray: could not change the icon");
    }
    // Only macOS has templates; elsewhere this does nothing.
    let _ = tray.set_icon_as_template(template);
}

/// Start or stop a recording from the tray. A refusal (not the console
/// user, no encoder, a SYSTEM/root service) is shown in the Recordings view,
/// where it can be read — a tray has nowhere to put a sentence.
fn toggle_recording<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let outcome: Result<(), String> = async {
            let mut client = roomler_localapi::connect()
                .await
                .map_err(|e| format!("connecting to the device service: {e}"))?;
            let state = client.record_status().await.map_err(|e| e.to_string())?;
            if state.active {
                client.record_stop().await.map(|_| ())
            } else {
                client.record_start(Default::default()).await.map(|_| ())
            }
            .map_err(|e| {
                let m = e.to_string();
                m.strip_prefix("localapi error: ").unwrap_or(&m).to_string()
            })
        }
        .await;
        if let Err(message) = outcome {
            tracing::warn!(%message, "tray: recording toggle refused");
            show_window(&app, "/recordings");
            if let Some(window) = app.get_webview_window("main") {
                let text = serde_json::to_string(&message).unwrap_or_else(|_| "\"\"".into());
                let _ = window.eval(format!(
                    "window.RoomlerRecordings && window.RoomlerRecordings.showError({text})"
                ));
            }
        }
    });
}

/// Show + focus the main window and route the SPA to `path` (a hash-router
/// path like `/overview`). The router treats an unknown hash as `/overview`,
/// so a stale path can't strand the window on a blank page.
fn show_window<R: Runtime>(app: &AppHandle<R>, path: &str) {
    if let Some(window) = app.get_webview_window("main") {
        // Navigate (no-op when already on the same path).
        let _ = window.eval(format!("window.location.hash = '#{path}'"));
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// S7 — resolve the server origin OFF the menu thread (config load +
/// service-flavour probe spawn), then open the web window ON the main
/// thread (window creation is main-thread-only on Windows).
fn open_roomler_web<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || match crate::commands::server_origin_blocking() {
        Ok(url) => {
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Err(e) = crate::commands::open_web_or_browser(&app2, &url) {
                    tracing::warn!(%e, "open roomler web failed");
                }
            });
        }
        Err(e) => tracing::warn!(%e, "open roomler web: no server origin"),
    });
}

fn check_updates<R: Runtime>(app: &AppHandle<R>) {
    match crate::commands::cmd_check_update() {
        Ok(stdout) => {
            // Forward to the main window so the SPA can render the
            // update banner. Sets a global flag the SPA polls.
            if let Some(window) = app.get_webview_window("main") {
                let payload = serde_json::json!({ "check": stdout }).to_string();
                let _ = window.eval(format!(
                    "window.__roomler_check_update_result = {payload}; window.dispatchEvent(new Event('roomler-update-check'))"
                ));
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        Err(e) => {
            tracing::warn!(%e, "check-update failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-85 — the tray says what the recorder is doing, and a service
    /// without a recorder (or none at all) leaves the item disabled rather
    /// than offering a Start that can only fail.
    #[test]
    fn the_record_item_follows_the_recorder() {
        let idle = roomler_localapi::RecordingState {
            available: true,
            ..Default::default()
        };
        assert_eq!(
            record_labels(Some(&idle)),
            ("Start recording".into(), true, "Roomler".into())
        );
        // A service that cannot record here (no recorder, or SYSTEM/root
        // until P1e) answers idle — and the item stays greyed out.
        let cannot = roomler_localapi::RecordingState::unavailable(roomler_localapi::NO_RECORDER);
        assert!(!record_labels(Some(&cannot)).1);
        let live = roomler_localapi::RecordingState {
            available: true,
            active: true,
            duration_ms: 83_900,
            ..Default::default()
        };
        assert_eq!(
            record_labels(Some(&live)),
            (
                "Stop recording (1:23)".into(),
                true,
                "Roomler — recording 1:23".into()
            )
        );
        assert_eq!(
            record_labels(None),
            ("Start recording".into(), false, "Roomler".into())
        );
        // A controller's recording names them: the person at the device sees
        // who is recording, not just that something is.
        let remote = roomler_localapi::RecordingState {
            available: true,
            active: true,
            duration_ms: 5_000,
            remote_controller: Some("Tester".into()),
            ..Default::default()
        };
        assert_eq!(
            record_labels(Some(&remote)),
            (
                "Stop recording (0:05)".into(),
                true,
                "Roomler — Tester is recording, 0:05".into()
            )
        );
    }

    /// FR-85 — the red follows the service's answers, and a poll that fails
    /// changes nothing: a busy pipe mid-recording must not take the red off
    /// (nor put it on). Both inputs: the recorder, and a controller's mark on
    /// a session, which only a session RECORDING sets (a viewer's does not).
    #[test]
    fn only_an_answer_moves_the_red() {
        let idle = roomler_localapi::RecordingState {
            available: true,
            ..Default::default()
        };
        let live = roomler_localapi::RecordingState {
            available: true,
            active: true,
            ..Default::default()
        };
        assert!(recorder_running(Some(&live), false), "a recording started");
        assert!(!recorder_running(Some(&idle), true), "a recording ended");
        assert!(
            recorder_running(None, true),
            "a failed poll took the red off"
        );
        assert!(
            !recorder_running(None, false),
            "a failed poll turned it red"
        );

        let session = |recording| roomler_localapi::RcSessionInfo {
            session_id: "0".repeat(24),
            recording,
            ..Default::default()
        };
        let watched = [session(false)];
        let recorded = [session(false), session(true)];
        assert!(
            remote_marked(Some(&recorded), false),
            "a controller records"
        );
        assert!(!remote_marked(Some(&watched), true), "a viewer is not red");
        assert!(!remote_marked(Some(&[]), true), "no session is not red");
        assert!(remote_marked(None, true), "a failed poll took the red off");
        assert!(!remote_marked(None, false), "a failed poll turned it red");
    }

    /// The red, fully opaque pixels of `img`, anywhere or only in its
    /// bottom-right quarter.
    fn red_pixels(img: &tauri::image::Image<'_>, bottom_right_only: bool) -> usize {
        let (w, h) = (img.width() as usize, img.height() as usize);
        img.rgba()
            .chunks_exact(4)
            .enumerate()
            .filter(|(i, p)| {
                let (x, y) = (i % w, i / w);
                (!bottom_right_only || (x >= w / 2 && y >= h / 2))
                    && p[0] > 200
                    && p[1] < 100
                    && p[2] < 100
                    && p[3] > 200
            })
            .count()
    }

    /// FR-85 — a recording turns the tray red (docs/remote-control.md
    /// §11.3). The icon swapped in has a red dot, in its bottom-right corner
    /// and nowhere else; the idle one has no red at all; the red one is never
    /// a macOS template (the system would tint it) and is the idle one's
    /// size, so the menu bar does not jump.
    #[test]
    fn a_recording_turns_the_tray_red() {
        let (rec, rec_template) = tray_look(true);
        let (idle, idle_template) = tray_look(false);
        assert!(!rec_template, "a template would be tinted, never red");
        assert!(idle_template, "the idle tray is the menu-bar template");
        assert!(
            red_pixels(&rec, true) > 100,
            "the recording icon has no red dot"
        );
        assert_eq!(
            red_pixels(&rec, false),
            red_pixels(&rec, true),
            "red outside the dot's corner"
        );
        assert_eq!(red_pixels(&idle, false), 0, "the idle icon has red in it");
        assert_eq!((rec.width(), rec.height()), (idle.width(), idle.height()));
    }
}
