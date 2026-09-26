// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 — HQ screen recording (P1: the recorder core).
//!
//! A recording is encoded **at the source**, in a pipeline of its own, and
//! written to a local file: `roomlerd record` ([`child`]) opens its own
//! capturer at native resolution ([`recorder`]), paces it to a constant frame
//! rate on its own clock ([`pacer`]), encodes with a recording profile, and
//! writes a fragmented MP4 that survives a crash, remuxed on stop to a
//! progressive, moov-first file ([`mp4`], [`annexb`]). Where the capture
//! backend leaves the mouse pointer out, the recorder draws it ([`pointer`]).
//! The folder rules live in [`folder`], the facts about a finished recording
//! in [`sidecar`].
//!
//! Not here yet (later FR-85 phases): the microphone and computer audio on
//! macOS (P1c-mac), delivery out of the recorder's data folder (P2c),
//! re-attaching after a dropped session (P3b-3), and FFmpeg's decoders and
//! AAC (P4).
//!
//! Design and gates: `docs/fr/FR-85-hq-screen-recording.md`.

pub mod annexb;
/// FR-85 P1c — computer audio and the microphone, mixed on the recorder's
/// clock (the `audio` feature: cpal + audiopus).
#[cfg(feature = "audio")]
pub mod audio;
pub mod child;
/// FR-85 P5 — the edit list and its exact time map.
pub mod edit;
/// FR-85 P5a — the export engine (it decodes with openh264, so it comes with
/// the software encoder's feature).
#[cfg(feature = "openh264-encoder")]
pub mod export;
/// FR-85 P5b — an export's sound: the recording's audio through the edit
/// list, and background music (Opus, like a recording's).
#[cfg(feature = "audio")]
pub mod export_audio;
pub mod folder;
/// FR-85 P1e — who the recorder runs as (the person signed in at the
/// device, at normal integrity), and launching it as that.
pub mod launch;
pub mod manager;
/// FR-85 P5a — `roomlerd media probe|export`, the engine as its own process.
#[cfg(feature = "openh264-encoder")]
pub mod media;
pub mod mp4;
pub mod pacer;
/// FR-85 P1d — drawing the pointer into a recording, where the backend does
/// not.
pub mod pointer;
pub mod recorder;
/// FR-85 P3b — the device's half of remote recording: the owner's gates,
/// what the device advertises, and the `record` DataChannel.
pub mod remote;
pub mod sidecar;
