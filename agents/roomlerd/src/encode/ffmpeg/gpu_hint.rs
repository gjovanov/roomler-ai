// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-118 P2-2 — which capture device an encoder open may take GPU frames from.
//!
//! An FFmpeg encoder decides its input form when it opens: a QSV encoder that
//! takes GPU frames is opened on a frame pool of the capture's own D3D11 device
//! (`gpu_input::GpuInput`). The device comes from the frame the pump is about
//! to encode ([`device_of`]), and reaches the open through [`opening_on`], a
//! scope around the open, rather than through the twenty-odd constructors
//! between the pump's one call and the place that opens the codec.
//!
//! ⚠️ The scope is a thread-local, so it holds only while the open runs on the
//! thread that set it: the pump sets it inside its `spawn_blocking` closure,
//! around a synchronous open with no `.await` in it, and a background rebuild
//! sets it again from the device its spec carries. A gate still applies at the
//! open itself: the switch and the denylist are read there, never trusted from
//! the capture.

use std::cell::RefCell;

use crate::capture::Frame;

/// A capture device an encoder can take GPU frames from.
#[cfg(all(
    target_os = "windows",
    any(feature = "mf-encoder", feature = "wgc-capture")
))]
pub(crate) type GpuDevice = windows::Win32::Graphics::Direct3D11::ID3D11Device;

/// No GPU frames on this build: a type with no values, so every
/// `Option<GpuDevice>` is `None`.
#[cfg(not(all(
    target_os = "windows",
    any(feature = "mf-encoder", feature = "wgc-capture")
)))]
#[derive(Clone, Debug)]
pub(crate) enum GpuDevice {}

/// The device `frame` lives on, when it is a GPU frame.
pub(crate) fn device_of(frame: &Frame) -> Option<GpuDevice> {
    #[cfg(all(
        target_os = "windows",
        any(feature = "mf-encoder", feature = "wgc-capture")
    ))]
    if let crate::capture::FrameData::Gpu(gpu) = &frame.data {
        return Some(gpu.device().clone());
    }
    let _ = frame;
    None
}

thread_local! {
    static OPENING_ON: RefCell<Option<GpuDevice>> = const { RefCell::new(None) };
}

/// Run `open` with `device` named as the capture device a QSV encoder it opens
/// may take GPU frames from. The previous value comes back afterwards, also
/// when `open` panics.
pub(crate) fn opening_on<T>(device: Option<GpuDevice>, open: impl FnOnce() -> T) -> T {
    struct Restore(Option<GpuDevice>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            OPENING_ON.with(|d| *d.borrow_mut() = previous);
        }
    }
    let _restore = Restore(OPENING_ON.with(|d| d.replace(device)));
    open()
}

/// The device the open running on this thread may take GPU frames from.
// Read only where a GPU input exists (`gpu_input::plan`).
#[cfg_attr(
    not(all(
        target_os = "windows",
        any(feature = "mf-encoder", feature = "wgc-capture")
    )),
    allow(dead_code)
)]
pub(crate) fn current() -> Option<GpuDevice> {
    OPENING_ON.with(|d| d.borrow().clone())
}

/// FR-118 §4.6 — the input form an encoder opened in: GPU frames, or CPU
/// frames and why. The reasons: `switch_off` (`ROOMLERD_ZERO_COPY` is off),
/// `cpu_frames` (the capture delivered frames in memory), `no_gpu_cell` (this
/// encoder or chroma takes no GPU frames yet), `denied` (`zero_copy_deny`),
/// `unproven` (the capability probe's child did not open this form),
/// `adapter_mismatch` (the capture's adapter has no QSV), `open_failed`. The
/// heartbeat carries both: without the reason, "zero-copy did nothing" cannot
/// be told apart from any of these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputForm {
    pub gpu: bool,
    pub reason: &'static str,
}

impl InputForm {
    #[cfg_attr(
        not(all(
            target_os = "windows",
            any(feature = "mf-encoder", feature = "wgc-capture")
        )),
        allow(dead_code)
    )]
    pub(crate) const GPU: Self = Self {
        gpu: true,
        reason: "",
    };

    pub(crate) const fn cpu(reason: &'static str) -> Self {
        Self { gpu: false, reason }
    }

    /// `gpu` or `cpu`, the heartbeat's `input_path`.
    pub fn path(&self) -> &'static str {
        if self.gpu { "gpu" } else { "cpu" }
    }
}

/// FR-118 #3071 — the input form an encoder reports once it adopts an encoder
/// opened in `opened`. A replacement is opened from the encoder's own device,
/// which is none while it runs CPU frames, so it reads `cpu_frames` even when
/// the encoder runs CPU frames because the capture's device was refused: FR-119's
/// frame-rate replacement, spawned at session start before the device follow,
/// and a rebuild-bound encoder's rate rebuild. The follow never retries a
/// refused device, so the refusal's reason has to outlive the replacement, or
/// the heartbeat names the wrong cause for the rest of the session.
pub(crate) fn adopted_input(
    current: InputForm,
    device_refused: bool,
    opened: InputForm,
) -> InputForm {
    if device_refused && !current.gpu && !opened.gpu && opened.reason == "cpu_frames" {
        current
    } else {
        opened
    }
}

/// FR-118 — the GPU-input cells the operator denied: `ROOMLERD_ZERO_COPY_DENY`
/// (config `zero_copy_deny`), entries `name:gpu` (the encoder and the input
/// path), the `encoder_cells_deny` shape and parser (`none` = empty). Read at
/// each open, like the cell denylist.
#[cfg_attr(
    not(all(
        target_os = "windows",
        any(feature = "mf-encoder", feature = "wgc-capture")
    )),
    allow(dead_code)
)]
pub(crate) fn denied() -> Vec<String> {
    tunnel_core::env::node_env("ZERO_COPY_DENY")
        .map(|v| crate::encode::cells::parse_denylist(&v))
        .unwrap_or_default()
}

#[cfg_attr(
    not(all(
        target_os = "windows",
        any(feature = "mf-encoder", feature = "wgc-capture")
    )),
    allow(dead_code)
)]
pub(crate) fn is_denied(deny: &[String], name: &str) -> bool {
    let key = format!("{name}:gpu");
    deny.iter().any(|d| d == &key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scope names a device only inside it, and restores the outer value.
    /// (Off Windows no device exists, so the scope is checked with `None`.)
    #[test]
    fn the_scope_restores_what_was_there() {
        assert!(current().is_none());
        let inner = opening_on(None, || current().is_none());
        assert!(inner);
        assert!(current().is_none());
    }

    /// A denylist entry is `name:gpu`; a bare name or another path denies
    /// nothing, so a typo cannot quietly switch a cell off by a near miss.
    #[test]
    fn a_gpu_cell_is_denied_by_its_exact_entry() {
        let deny = crate::encode::cells::parse_denylist("hevc_qsv:gpu, av1_qsv");
        assert!(is_denied(&deny, "hevc_qsv"));
        assert!(
            !is_denied(&deny, "av1_qsv"),
            "a bare name is not a gpu entry"
        );
        assert!(!is_denied(&deny, "h264_qsv"));
        assert!(crate::encode::cells::parse_denylist("none").is_empty());
    }

    /// #3071, neo16 on 0.4.183: NVENC refused the Radeon's frames
    /// (`adapter_mismatch`), then FR-119's replacement, opened before the
    /// refusal and with no device, was adopted. The refusal's reason stays.
    #[test]
    fn a_replacement_opened_without_the_device_keeps_the_refusal_s_reason() {
        let refused = InputForm::cpu("adapter_mismatch");
        let replacement = InputForm::cpu("cpu_frames");
        assert_eq!(adopted_input(refused, true, replacement), refused);
        // And a rate rebuild that fails its own open the same way.
        let failed = InputForm::cpu("open_failed");
        assert_eq!(adopted_input(failed, true, replacement), failed);
    }

    /// Everything else takes the opened form: no device was refused (the frames
    /// really are in memory), the opened encoder names its own refusal, or it
    /// takes GPU frames.
    #[test]
    fn an_opened_encoder_s_own_form_wins_otherwise() {
        let cpu_frames = InputForm::cpu("cpu_frames");
        let refused = InputForm::cpu("adapter_mismatch");
        assert_eq!(adopted_input(cpu_frames, false, cpu_frames), cpu_frames);
        assert_eq!(adopted_input(refused, false, cpu_frames), cpu_frames);
        assert_eq!(
            adopted_input(refused, true, InputForm::cpu("denied")),
            InputForm::cpu("denied")
        );
        assert_eq!(adopted_input(refused, true, InputForm::GPU), InputForm::GPU);
        assert_eq!(
            adopted_input(InputForm::GPU, true, cpu_frames),
            cpu_frames,
            "an encoder on GPU frames has no refusal to keep"
        );
    }
}
