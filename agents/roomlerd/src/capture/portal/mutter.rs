// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-45 P5 — screencast through **`org.gnome.Mutter.ScreenCast`**, mutter's
//! own API, with no portal in the path.
//!
//! ## Why this exists
//!
//! The portal is the obstacle on the host FR-45 was opened for, and the
//! compositor is not. Measured on WSL2 (2026-09-01):
//! `xdg-desktop-portal-gnome` **exits immediately** without a real GNOME
//! session, so it never owns its bus name and the frontend's activation of it
//! times out — `CreateSession: … StartServiceByName … Timeout was reached`.
//! Meanwhile `mutter --headless --wayland --virtual-monitor 1920x1080` runs
//! there happily (`Created surfaceless renderer without GPU`, `Added virtual
//! monitor Meta-0`) and exposes **`org.gnome.Mutter.ScreenCast` v4**.
//!
//! So this module swaps out **only the session broker**. Everything downstream
//! — the SPA POD negotiation ([`super::pod`]), buffer handling and frame
//! delivery ([`super::pipewire`]), the wire format ([`super::wire`]) and the
//! `ScreenCapture` backend ([`super::backend`]) — is the P3 code, unchanged.
//!
//! ## ⚠️⚠️ This path does NOT ask. Never describe it as a portal variant.
//!
//! `org.gnome.Mutter.ScreenCast` is a privileged session API: anything on the
//! user's session bus may call it, and mutter shows **no consent dialog**. The
//! spec's emphatic "ATTENDED only" warning is about [`super::screencast`] and
//! must not be transferred onto this module by anyone skimming.
//!
//! That trade is acceptable, but only because of what it actually is — not
//! because it is convenient:
//!
//! - It is **not** a privilege escalation. Reaching this bus already means
//!   running as the session user, who can screenshot at will; and the daemon
//!   is root, which can read the framebuffer regardless (that is precisely
//!   what FR-36's DRM backend does).
//! - Its honest peer is therefore **FR-36's DRM/KMS backend** — the same
//!   unattended bargain by a different mechanism — so it is gated the same
//!   way: opt-in, default OFF, loud in the log when it engages.
//! - It is **GNOME-specific**. KDE and wlroots hosts keep the portal path;
//!   nothing here implies an equivalent for them.
//!
//! ## The two rules that are easy to get wrong
//!
//! 1. ⚠️⚠️ **The session dies with the D-Bus connection that created it** —
//!    the same rule [`super::screencast::Session`] holds `_conn` for. This is
//!    also why the API could not be prototyped with `busctl`: a fresh
//!    connection per call can never hold a session open.
//! 2. ⚠️ **Subscribe to `PipeWireStreamAdded` BEFORE calling `Start`.** The
//!    node id arrives as a *signal* on the stream object, and mutter emits it
//!    as soon as the stream is live — which can be before a subscription made
//!    afterwards exists. Same shape as the portal's Request/Response race, and
//!    the same fix.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const MUTTER_BUS: &str = "org.gnome.Mutter.ScreenCast";
const MUTTER_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const MUTTER_IFACE: &str = "org.gnome.Mutter.ScreenCast";
const SESSION_IFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const STREAM_IFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";

/// `org.gnome.Mutter.DisplayConfig`, used only to learn a connector name.
const DISPLAY_BUS: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const DISPLAY_IFACE: &str = "org.gnome.Mutter.DisplayConfig";

/// How long the whole handshake may take.
///
/// Generous but bounded, and — unlike the portal — a long wait here means
/// something is wrong rather than that a human is reading a dialog: nothing on
/// this path asks anyone anything.
///
/// ⚠️ Applied by [`open`] around the WHOLE blocking handshake, not inside it.
/// zbus's blocking signal iterator has no timed `next`, so a deadline checked
/// around it would be dead code that only looks like a bound — clippy caught
/// exactly that in the first draft ("this loop never actually loops"). The
/// thread is left running on a timeout, the same accepted trade
/// [`super::pipewire::negotiate`] documents, and acceptable for the same
/// reason: this is a short-lived helper, not the daemon.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// What we learned from mutter, in the shape the caller needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MutterReport {
    /// The PipeWire node to attach to — the whole point.
    pub node_id: u32,
    /// Which output is being recorded, for the log. A host with several
    /// monitors records exactly one, and saying which beats guessing.
    pub connector: String,
    /// `org.gnome.Mutter.ScreenCast`'s `Version` property, when readable.
    pub version: Option<i32>,
    pub elapsed_ms: u64,
}

/// Why a mutter screencast could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MutterError {
    /// Nothing owns `org.gnome.Mutter.ScreenCast` — not a GNOME session, or
    /// mutter is too old. Distinguished because it is the ONE arm that means
    /// "wrong host" rather than "something broke".
    Unavailable,
    /// mutter answered but no output could be recorded.
    NoMonitor,
    Failed(String),
}

impl std::fmt::Display for MutterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MutterError::Unavailable => write!(
                f,
                "org.gnome.Mutter.ScreenCast is not on the session bus — this needs a running \
                 mutter (GNOME, or `mutter --headless`)"
            ),
            MutterError::NoMonitor => {
                write!(f, "mutter reported no monitor that could be recorded")
            }
            MutterError::Failed(why) => write!(f, "{why}"),
        }
    }
}

impl MutterError {
    fn failed(why: impl std::fmt::Display) -> Self {
        MutterError::Failed(why.to_string())
    }
}

/// A live mutter screencast session.
pub struct MutterSession {
    pub report: MutterReport,
    /// ⚠️⚠️ **Holding this open is what keeps the session alive.** Mutter owns
    /// the session against the D-Bus connection that created it and tears it
    /// down when that connection goes — taking the PipeWire node with it. The
    /// portal has exactly this rule and P3b-ii was lost to it once already
    /// (`no target node available`, long after a handshake that looked
    /// perfect). The underscore is deliberate: nothing reads it, and deleting
    /// it "because it is unused" re-breaks capture.
    _conn: zbus::blocking::Connection,
}

/// Open a screencast on the first recordable monitor.
///
/// ⚠️ **Runs on its own thread, and that is not optional** — `zbus`'s blocking
/// API panics inside a tokio runtime, and this is reached from the
/// `#[tokio::main]` helper. The same guard [`super::screencast::open`] carries,
/// for the same reason, and P4's review caught exactly this hazard being
/// reintroduced one call site over.
pub fn open() -> Result<MutterSession, MutterError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The receiver may already have timed out; nothing to do about that.
        let _ = tx.send(open_blocking());
    });
    match rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(r) => r,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(MutterError::failed(format!(
            "mutter did not complete the screencast handshake within {}s",
            OPEN_TIMEOUT.as_secs()
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(MutterError::failed("the mutter screencast thread panicked"))
        }
    }
}

fn open_blocking() -> Result<MutterSession, MutterError> {
    let started = Instant::now();
    let conn = zbus::blocking::Connection::session()
        .map_err(|e| MutterError::failed(format!("session bus: {e}")))?;

    let screencast = zbus::blocking::Proxy::new(&conn, MUTTER_BUS, MUTTER_PATH, MUTTER_IFACE)
        .map_err(|_| MutterError::Unavailable)?;
    // Reading a property is the cheapest proof that something actually ANSWERS
    // on the name — a proxy alone constructs happily for a name nobody owns,
    // which is the same trap as "the portal is running" in P1.
    let version = match screencast.get_property::<i32>("Version") {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!(%e, "mutter: ScreenCast.Version unreadable");
            return Err(MutterError::Unavailable);
        }
    };

    let connector = first_connector(&conn)?;

    // ── 1. CreateSession ────────────────────────────────────────────────
    let opts: HashMap<&str, Value> = HashMap::new();
    let session_path: OwnedObjectPath = screencast
        .call_method("CreateSession", &(opts,))
        .map_err(|e| MutterError::failed(format!("CreateSession: {e}")))?
        .body()
        .deserialize()
        .map_err(|e| MutterError::failed(format!("CreateSession returned no session path: {e}")))?;

    let session = zbus::blocking::Proxy::new(&conn, MUTTER_BUS, &session_path, SESSION_IFACE)
        .map_err(|e| MutterError::failed(format!("session proxy: {e}")))?;

    // ── 2. RecordMonitor ────────────────────────────────────────────────
    //
    // `cursor-mode` 1 = embedded, matching what the portal path asks for so
    // both produce the same picture. Sent as a plain u32 in an a{sv}.
    let mut props: HashMap<&str, Value> = HashMap::new();
    props.insert("cursor-mode", Value::from(1u32));
    let stream_path: OwnedObjectPath = session
        .call_method("RecordMonitor", &(connector.as_str(), props))
        .map_err(|e| MutterError::failed(format!("RecordMonitor({connector}): {e}")))?
        .body()
        .deserialize()
        .map_err(|e| MutterError::failed(format!("RecordMonitor returned no stream path: {e}")))?;

    let stream = zbus::blocking::Proxy::new(&conn, MUTTER_BUS, &stream_path, STREAM_IFACE)
        .map_err(|e| MutterError::failed(format!("stream proxy: {e}")))?;

    // ── 3. Arm the signal BEFORE Start ──────────────────────────────────
    //
    // ⚠️ The node id arrives as a signal, and mutter emits it as soon as the
    // stream is live — which can be before a subscription made afterwards
    // exists. Exactly the portal's Request/Response race, same fix.
    let mut added = stream
        .receive_signal("PipeWireStreamAdded")
        .map_err(|e| MutterError::failed(format!("subscribing to PipeWireStreamAdded: {e}")))?;

    // ── 4. Start ────────────────────────────────────────────────────────
    session
        .call_method("Start", &())
        .map_err(|e| MutterError::failed(format!("Start: {e}")))?;

    // ── 5. The node id ──────────────────────────────────────────────────
    // ⚠️ This blocks with no bound of its own — [`open`] wraps the whole
    // handshake in `OPEN_TIMEOUT` instead, because zbus's blocking signal
    // iterator has no timed `next` and a deadline checked around this call
    // would be a bound that does not bind.
    let node_id = match added.next() {
        Some(msg) => msg
            .body()
            .deserialize::<u32>()
            .map_err(|e| MutterError::failed(format!("PipeWireStreamAdded body: {e}")))?,
        None => {
            return Err(MutterError::failed(
                "the PipeWireStreamAdded stream ended before a node arrived",
            ));
        }
    };

    Ok(MutterSession {
        report: MutterReport {
            node_id,
            connector,
            version,
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
        _conn: conn,
    })
}

/// The connector name of the first usable monitor, via
/// `org.gnome.Mutter.DisplayConfig.GetCurrentState`.
///
/// Needed because `RecordMonitor` is addressed by connector (`Meta-0`,
/// `eDP-1`, …) and there is no "just record the primary" call. Parsed
/// structurally rather than through a derive: the reply is a deeply nested
/// tuple whose later fields we do not care about, and a shape change in a part
/// we ignore must not cost the capture.
fn first_connector(conn: &zbus::blocking::Connection) -> Result<String, MutterError> {
    let display = zbus::blocking::Proxy::new(conn, DISPLAY_BUS, DISPLAY_PATH, DISPLAY_IFACE)
        .map_err(|e| MutterError::failed(format!("display-config proxy: {e}")))?;
    let msg = display
        .call_method("GetCurrentState", &())
        .map_err(|e| MutterError::failed(format!("GetCurrentState: {e}")))?;
    let body = msg.body();
    // ⚠️ The reply is `(u, a<monitor>, a<logical_monitor>, a{sv})` — FOUR
    // fields. Deserialising only the two we care about is a signature
    // mismatch, not a partial read: zvariant matches the WHOLE body. Measured
    // on GNOME 46, which answered
    // `(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})`.
    let (_serial, monitors, _logical, _props): CurrentState = body
        .deserialize()
        .map_err(|e| MutterError::failed(format!("GetCurrentState body: {e}")))?;

    monitors
        .into_iter()
        .map(|m| m.spec.connector)
        .next()
        .ok_or(MutterError::NoMonitor)
}

/// The whole `GetCurrentState` reply: serial, monitors, logical monitors,
/// properties. Only the monitors are read, but the arity must match exactly —
/// see the note at the call site.
type CurrentState = (
    u32,
    Vec<MonitorEntry>,
    Vec<LogicalMonitorEntry>,
    HashMap<String, OwnedValue>,
);

/// One entry of `GetCurrentState`'s LOGICAL monitor array — `(iiduba(ssss)a{sv})`.
///
/// Entirely unread; it exists so the reply's arity matches. Deserialising it
/// properly (rather than skipping it) is what makes a future shape change fail
/// loudly here instead of somewhere downstream.
#[derive(serde::Deserialize, zbus::zvariant::Type)]
struct LogicalMonitorEntry {
    #[allow(dead_code)]
    x: i32,
    #[allow(dead_code)]
    y: i32,
    #[allow(dead_code)]
    scale: f64,
    #[allow(dead_code)]
    transform: u32,
    #[allow(dead_code)]
    primary: bool,
    #[allow(dead_code)]
    monitors: Vec<MonitorSpec>,
    #[allow(dead_code)]
    properties: HashMap<String, OwnedValue>,
}

/// One entry of `GetCurrentState`'s monitor array.
///
/// Only the connector is read; the modes and properties are deserialised into
/// throwaway values so the tuple arity matches without this having an opinion
/// about their contents.
#[derive(serde::Deserialize, zbus::zvariant::Type)]
struct MonitorEntry {
    spec: MonitorSpec,
    #[allow(dead_code)]
    modes: Vec<MonitorMode>,
    #[allow(dead_code)]
    properties: HashMap<String, OwnedValue>,
}

#[derive(serde::Deserialize, zbus::zvariant::Type)]
struct MonitorSpec {
    connector: String,
    #[allow(dead_code)]
    vendor: String,
    #[allow(dead_code)]
    product: String,
    #[allow(dead_code)]
    serial: String,
}

#[derive(serde::Deserialize, zbus::zvariant::Type)]
struct MonitorMode {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    width: i32,
    #[allow(dead_code)]
    height: i32,
    #[allow(dead_code)]
    refresh: f64,
    #[allow(dead_code)]
    preferred_scale: f64,
    #[allow(dead_code)]
    supported_scales: Vec<f64>,
    #[allow(dead_code)]
    properties: HashMap<String, OwnedValue>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Unavailable` has to stay distinguishable from a fault: it means "this
    /// is not a mutter host", which the caller answers by falling through to
    /// another backend rather than by reporting a failure.
    #[test]
    fn unavailable_is_not_a_fault() {
        assert_ne!(MutterError::Unavailable, MutterError::NoMonitor);
        let msg = MutterError::Unavailable.to_string();
        assert!(msg.contains("mutter"), "{msg}");
        assert!(
            msg.contains("headless"),
            "the message should name the fix a headless host needs: {msg}"
        );
    }

    /// The report crosses the helper→daemon pipe, so it must round-trip.
    #[test]
    fn the_report_round_trips() {
        let r = MutterReport {
            node_id: 42,
            connector: "Meta-0".into(),
            version: Some(4),
            elapsed_ms: 12,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<MutterReport>(&json).unwrap(), r);
    }

    /// The error type crosses the same pipe, and `Unavailable` in particular
    /// must survive as itself — the caller branches on it.
    #[test]
    fn errors_round_trip_including_unavailable() {
        for e in [
            MutterError::Unavailable,
            MutterError::NoMonitor,
            MutterError::Failed("boom".into()),
        ] {
            let json = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<MutterError>(&json).unwrap(), e);
        }
    }

    /// ⚠️⚠️ Pin the **WHOLE reply**, not just the element.
    ///
    /// The first version of this test asserted only `MonitorEntry`'s
    /// signature. It passed — that part was right — while the code
    /// deserialised the reply as a 2-tuple and every live call failed with
    /// `Signature mismatch: got (ua(…)a(…)a{sv}), expected (ua(…))`. zvariant
    /// matches the ENTIRE body, so reading "just the fields we want" is not a
    /// partial read, it is a type error. A test at the wrong level is worse
    /// than no test: it says the shape is locked when the shape that actually
    /// broke was never looked at. Measured against GNOME 46.
    #[test]
    fn get_current_state_reply_signature_is_locked() {
        use zbus::zvariant::Type;
        assert_eq!(
            CurrentState::SIGNATURE.to_string(),
            "(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})"
        );
        assert_eq!(
            MonitorEntry::SIGNATURE.to_string(),
            "((ssss)a(siiddada{sv})a{sv})"
        );
    }
}
