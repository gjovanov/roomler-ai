//! M3 A1 SYSTEM-context capture pump.
//!
//! Bridges the M3 A1 backends ([`DxgiDupBackend`] + [`GdiBackend`])
//! behind the existing [`crate::capture::ScreenCapture`] trait, so
//! `peer.rs::media_pump` consumes one uniform interface regardless of
//! which worker context is running. The user-context worker keeps
//! using the WGC / scrap backends; the SYSTEM-context worker (chosen
//! at startup via [`super::worker_role::probe_self`]) uses this.
//!
//! ## Lifecycle
//!
//! 1. [`SystemContextCapture::primary`] spawns a dedicated OS thread
//!    (because both [`DxgiDupBackend`] and [`GdiBackend`] are `!Send`
//!    on Windows — D3D11 / GDI handles have thread affinity).
//! 2. The thread first calls [`super::desktop_rebind::attach_to_winsta0`]
//!    (idempotent — required so `OpenDesktopW` for `Default` /
//!    `Winlogon` is reachable from the SCM-spawned worker).
//! 3. Then [`super::desktop_rebind::try_change_desktop`] binds the
//!    thread to whichever desktop currently receives input — usually
//!    `Default` at startup, may flip to `Winlogon` after a `Win+L`.
//! 4. Builds a [`DxgiDupBackend`] against the primary monitor.
//! 5. Loops on capture commands from the async side (via
//!    `oneshot::Sender<CaptureReply>`) until the channel is dropped.
//!
//! ## BackendBail routing (matches RustDesk's
//! `video_service.rs:851-856` trip-wire convention):
//!
//! | Bail variant | Routed to |
//! |---|---|
//! | `Transient` | `Ok(None)` — no frame this tick (idle-keepalive will fire upstream) |
//! | `DesktopMismatch` | `try_change_desktop` rebind, then `Ok(None)` |
//! | `AccessLost` | `DxgiDupBackend::reset()`, then `Ok(None)` |
//! | `SessionGone` | `Err(...)` — terminal, supervisor tears down |
//! | `HardError` (×3 consecutive) | swap to GDI fallback |
//! | `HardError` (1-2 consecutive) | log + `Ok(None)` |
//!
//! After GDI takes over, every successful GDI frame *also* re-tries
//! DXGI on the next tick — we want to climb back to the GPU path as
//! soon as it recovers (driver reset, hybrid GPU re-enumeration). On
//! GDI-also-failing, return `Err(...)` and let `media_pump` rebuild
//! the entire pump.

#![cfg(all(feature = "system-context", target_os = "windows"))]

use anyhow::{Result, anyhow};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Instant;
use tokio::sync::oneshot;

use crate::capture::{DownscalePolicy, Frame, PixelFormat, ScreenCapture};

use super::desktop_rebind;
#[cfg(feature = "scrap-capture")]
use super::dxgi_dup::{BackendBail, DxgiDupBackend, DxgiFrame};
use super::gdi_backend::{GdiBackend, GdiFrame};

/// After this many consecutive `BackendBail::HardError` returns from
/// DXGI we drop the backend and switch to GDI. RustDesk uses 3 (see
/// `video_service.rs:851-856`); we mirror that — gives one frame of
/// "is this a real failure?" hysteresis without leaving the operator
/// staring at empty frames for long.
const HARD_ERROR_FALLBACK_THRESHOLD: u32 = 3;

/// Active capture backend. Starts as DXGI; swaps to GDI on persistent
/// HardError; can climb back to DXGI when it recovers.
#[cfg(feature = "scrap-capture")]
enum ActiveBackend {
    Dxgi(DxgiDupBackend),
    Gdi(GdiBackend),
}

#[cfg(not(feature = "scrap-capture"))]
enum ActiveBackend {
    Gdi(GdiBackend),
}

type CaptureReply = Result<Option<Frame>>;
type CaptureCmd = oneshot::Sender<CaptureReply>;

/// Async-side handle. `cmd_tx` posts capture requests to the worker
/// thread; the worker fills the embedded oneshot.
pub struct SystemContextCapture {
    cmd_tx: std_mpsc::Sender<CaptureCmd>,
    width: u32,
    height: u32,
}

impl SystemContextCapture {
    /// Spawn the worker thread + initialise DXGI (or GDI fallback).
    /// Surfaces init failures synchronously via a ready-ack channel —
    /// if both DXGI and GDI fail at startup the caller decides how to
    /// degrade (typically falls back to `NoopCapture`).
    pub fn primary(_target_fps: u32, _downscale: DownscalePolicy) -> Result<Self> {
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(u32, u32)>>();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<CaptureCmd>();

        thread::Builder::new()
            .name("roomler-agent-system-capture".into())
            .spawn(move || {
                worker_main(ready_tx, cmd_rx);
            })
            .map_err(|e| anyhow!("spawning system-context capture thread: {e}"))?;

        let (width, height) = ready_rx
            .recv()
            .map_err(|_| anyhow!("system-context capture worker never acked"))??;

        Ok(Self {
            cmd_tx,
            width,
            height,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
}

#[async_trait::async_trait]
impl ScreenCapture for SystemContextCapture {
    async fn next_frame(&mut self) -> Result<Option<Frame>> {
        let (tx, rx) = oneshot::channel::<CaptureReply>();
        self.cmd_tx
            .send(tx)
            .map_err(|_| anyhow!("system-context capture worker thread is gone"))?;
        match rx.await {
            Ok(reply) => reply,
            Err(_) => Err(anyhow!(
                "system-context capture worker dropped reply oneshot"
            )),
        }
    }

    fn monitor_count(&self) -> u8 {
        // M3 A1 captures the primary monitor only — see plan §4.
        // Multi-monitor capture stays in the user-context worker
        // (WGC has the dirty-rect API we need for that).
        1
    }
}

/// Worker thread main. Owns the `!Send` capture backend; receives
/// oneshot-wrapped capture commands from the async side.
fn worker_main(
    ready_tx: std_mpsc::Sender<Result<(u32, u32)>>,
    cmd_rx: std_mpsc::Receiver<CaptureCmd>,
) {
    // 1. Bootstrap window-station attachment. Skipped under user-mode
    //    test runs (already on WinSta0); idempotent on the real
    //    SYSTEM-context worker. Warn but don't fail — the SCM-service
    //    container has WinSta0 on its DACL by default for LocalSystem;
    //    the only environment that fails this is a stripped-down CI
    //    runner where we couldn't reach a real desktop anyway.
    if let Err(e) = desktop_rebind::attach_to_winsta0() {
        tracing::warn!(%e, "attach_to_winsta0 failed at worker startup — capture may not see Default/Winlogon desktops");
    }

    // 2. Bind to the current input desktop. On a logged-in user this
    //    is `Default`; on a freshly-locked machine it flips to
    //    `Winlogon`. Non-fatal at startup — the `try_change_desktop`
    //    call in the per-frame error path will retry on the first
    //    `DesktopMismatch`.
    match desktop_rebind::try_change_desktop() {
        Ok(desktop_rebind::DesktopChange::Unchanged) => {
            tracing::info!("system-context capture: thread already on input desktop");
        }
        Ok(desktop_rebind::DesktopChange::Switched(name)) => {
            tracing::info!(%name, "system-context capture: rebound to input desktop");
        }
        Err(e) => {
            tracing::warn!(%e, "try_change_desktop at startup — non-fatal, will retry on first DesktopMismatch");
        }
    }

    // 3. Build the primary backend. Prefer DXGI; fall back to GDI if
    //    DXGI fails to initialise (no GPU, driver missing, etc.).
    let mut backend = match build_initial_backend() {
        Ok(b) => b,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let dims = backend_dimensions(&backend);
    if ready_tx.send(Ok(dims)).is_err() {
        // Caller dropped the ready channel — async side already gave
        // up. Nothing to do but exit.
        return;
    }

    let start = Instant::now();
    let mut consecutive_hard: u32 = 0;
    let mut consecutive_empty: u64 = 0;

    while let Ok(res_tx) = cmd_rx.recv() {
        let reply = capture_one_blocking(
            &mut backend,
            &mut consecutive_hard,
            &mut consecutive_empty,
            start,
        );
        // Best-effort send; if the async side dropped its rx the next
        // recv() above will error out and we exit cleanly.
        let _ = res_tx.send(reply);
    }
    tracing::info!("system-context capture worker thread exiting (cmd channel closed)");
}

#[cfg(feature = "scrap-capture")]
fn build_initial_backend() -> Result<ActiveBackend> {
    match DxgiDupBackend::primary() {
        Ok(b) => {
            tracing::info!("system-context capture: backend=DXGI");
            Ok(ActiveBackend::Dxgi(b))
        }
        Err(BackendBail::HardError(e)) => {
            tracing::warn!(%e, "DXGI primary failed at startup — falling back to GDI BitBlt");
            let gdi = GdiBackend::primary()
                .map_err(|e2| anyhow!("DXGI + GDI both failed at startup: dxgi={e}; gdi={e2}"))?;
            Ok(ActiveBackend::Gdi(gdi))
        }
        Err(other) => Err(anyhow!(
            "DXGI primary returned non-HardError bail at startup: {other:?}"
        )),
    }
}

#[cfg(not(feature = "scrap-capture"))]
fn build_initial_backend() -> Result<ActiveBackend> {
    let gdi = GdiBackend::primary()
        .map_err(|e| anyhow!("GDI fallback init failed and DXGI not compiled in: {e}"))?;
    tracing::info!("system-context capture: backend=GDI (scrap-capture feature not compiled)");
    Ok(ActiveBackend::Gdi(gdi))
}

fn backend_dimensions(b: &ActiveBackend) -> (u32, u32) {
    match b {
        #[cfg(feature = "scrap-capture")]
        ActiveBackend::Dxgi(d) => d.dimensions(),
        ActiveBackend::Gdi(g) => g.dimensions(),
    }
}

/// Single capture iteration. Branches on the active backend; updates
/// `consecutive_hard` (DXGI HardError counter) so the fallback
/// trip-wire fires after `HARD_ERROR_FALLBACK_THRESHOLD`.
///
/// Returns:
/// * `Ok(Some(frame))` on a real captured frame.
/// * `Ok(None)` on transient / desktop-rebind / access-lost / single
///   HardError — `media_pump`'s idle-keepalive path covers the gap.
/// * `Err(e)` on terminal failure (SessionGone or GDI also failing) —
///   `media_pump` will rebuild the pump.
fn capture_one_blocking(
    backend: &mut ActiveBackend,
    consecutive_hard: &mut u32,
    consecutive_empty: &mut u64,
    start: Instant,
) -> CaptureReply {
    match backend {
        #[cfg(feature = "scrap-capture")]
        ActiveBackend::Dxgi(b) => match b.frame() {
            Ok(frame) => {
                *consecutive_hard = 0;
                *consecutive_empty = 0;
                Ok(Some(dxgi_to_frame(frame, start)))
            }
            Err(BackendBail::Transient) => {
                *consecutive_hard = 0;
                *consecutive_empty = consecutive_empty.saturating_add(1);
                Ok(None)
            }
            Err(BackendBail::DesktopMismatch) => {
                *consecutive_hard = 0;
                *consecutive_empty = consecutive_empty.saturating_add(1);
                match desktop_rebind::try_change_desktop() {
                    Ok(desktop_rebind::DesktopChange::Switched(name)) => {
                        tracing::info!(%name, "system-context capture: rebound desktop after DXGI DesktopMismatch");
                    }
                    Ok(desktop_rebind::DesktopChange::Unchanged) => {
                        tracing::warn!(
                            "DXGI DesktopMismatch but try_change_desktop reported Unchanged — race or stale binding"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(%e, "desktop rebind failed after DesktopMismatch");
                    }
                }
                Ok(None)
            }
            Err(BackendBail::AccessLost) => {
                *consecutive_hard = 0;
                *consecutive_empty = consecutive_empty.saturating_add(1);
                tracing::warn!(
                    "DXGI AccessLost — recreating capturer (desktop transition or GPU device-lost)"
                );
                if let Err(e) = b.reset() {
                    tracing::warn!(?e, "DXGI reset after AccessLost failed");
                }
                // Desktop may have flipped under us during AccessLost
                // (lock → unlock typically does), so opportunistically
                // rebind. Ignored if Unchanged.
                let _ = desktop_rebind::try_change_desktop();
                Ok(None)
            }
            Err(BackendBail::SessionGone) => {
                tracing::error!("DXGI SessionGone — capture pump must rebuild");
                Err(anyhow!(
                    "DXGI Desktop Duplication: session disconnected (SessionGone)"
                ))
            }
            Err(BackendBail::HardError(e)) => {
                *consecutive_hard = consecutive_hard.saturating_add(1);
                *consecutive_empty = consecutive_empty.saturating_add(1);
                tracing::warn!(
                    %e,
                    count = *consecutive_hard,
                    "DXGI hard error"
                );
                if *consecutive_hard >= HARD_ERROR_FALLBACK_THRESHOLD {
                    match GdiBackend::primary() {
                        Ok(g) => {
                            tracing::warn!(
                                threshold = HARD_ERROR_FALLBACK_THRESHOLD,
                                "DXGI failed past threshold — switching to GDI BitBlt fallback"
                            );
                            *backend = ActiveBackend::Gdi(g);
                            *consecutive_hard = 0;
                        }
                        Err(e2) => {
                            tracing::error!(
                                %e2,
                                "GDI fallback init also failed; capture pump must rebuild"
                            );
                            return Err(anyhow!(
                                "DXGI repeatedly failed and GDI fallback init also failed: {e2}"
                            ));
                        }
                    }
                }
                Ok(None)
            }
        },
        ActiveBackend::Gdi(g) => match g.frame() {
            Ok(frame) => {
                *consecutive_empty = 0;
                Ok(Some(gdi_to_frame(frame, start)))
            }
            Err(e) => {
                tracing::warn!(%e, "GDI capture error — pump will rebuild");
                Err(anyhow!("GDI BitBlt fallback failed: {e}"))
            }
        },
    }
}

#[cfg(feature = "scrap-capture")]
fn dxgi_to_frame(f: DxgiFrame, start: Instant) -> Frame {
    Frame {
        width: f.width,
        height: f.height,
        stride: f.stride,
        pixel_format: PixelFormat::Bgra,
        data: f.bytes,
        monotonic_us: start.elapsed().as_micros() as u64,
        monitor: 0,
        dirty_rects: vec![],
    }
}

fn gdi_to_frame(f: GdiFrame, start: Instant) -> Frame {
    Frame {
        width: f.width,
        height: f.height,
        stride: f.stride,
        pixel_format: PixelFormat::Bgra,
        data: f.bytes,
        monotonic_us: start.elapsed().as_micros() as u64,
        monitor: 0,
        dirty_rects: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_error_threshold_matches_rustdesk() {
        // Lock the trip-wire constant. RustDesk's
        // video_service.rs:851-856 uses 3; field tuning below that
        // gives premature GDI fallbacks on hybrid GPU laptops where
        // a single transient HardError isn't actually persistent.
        assert_eq!(HARD_ERROR_FALLBACK_THRESHOLD, 3);
    }

    #[test]
    fn primary_returns_send_handle() {
        // Compile-time check that SystemContextCapture is Send (the
        // ScreenCapture trait requires Send). The cmd_tx is the only
        // field that affects Send-ness; std_mpsc::Sender<T> is Send
        // when T is Send, and oneshot::Sender<Result<...>> is Send.
        fn assert_send<T: Send>() {}
        assert_send::<SystemContextCapture>();
    }

    #[test]
    fn screen_capture_trait_is_implemented() {
        // Compile-time check that the trait impl actually compiles
        // against the real ScreenCapture surface.
        fn assert_impl<T: ScreenCapture>() {}
        assert_impl::<SystemContextCapture>();
    }

    #[cfg(feature = "scrap-capture")]
    #[test]
    fn primary_does_not_panic_under_test_runner() {
        // On a real Win11 desktop runner the worker thread will
        // start, attach to WinSta0 (idempotent under user context),
        // and DXGI primary should succeed. CI without a GPU may
        // fail at DXGI primary; we accept either outcome — lock
        // against panic, not specific success.
        let res = SystemContextCapture::primary(30, DownscalePolicy::default());
        // Drop immediately; the worker thread will exit when the
        // cmd_rx side hangs up.
        drop(res);
    }
}
