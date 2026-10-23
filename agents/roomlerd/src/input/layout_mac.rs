// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P2d-2a — a Mac's keyboard layout: which one is in use, and a switch to
//! another, for the viewer's layout chip and picker. The same two messages the
//! Windows and X11 modules answer (`rc:layout`, `rc:layout.set`), through the
//! same snapshot and watch, so the peer's emitter serves all three;
//! `input::layout` is this module on macOS.
//!
//! - **What a layout is here.** An enabled keyboard input source the person
//!   can select, as the menu bar's input menu lists them: a keyboard layout
//!   (`com.apple.keylayout.German`) or an input method's mode. Text Input
//!   Sources lists them (`TISCreateInputSourceList`) and names the one in use
//!   (`TISCopyCurrentKeyboardInputSource`).
//! - **The tag.** A layout the table knows goes out as its BCP-47 tag
//!   (`com.apple.keylayout.German` → `de-DE`), so the chip reads as a Windows
//!   or X11 host's does. Any other source goes out under its own localized
//!   name, never under a guess.
//! - **The token.** The picker echoes back an opaque hex string, here the
//!   source ID's hash: a click on a source that was disabled meanwhile is
//!   refused instead of selecting something else.
//! - **The main thread.** Text Input Sources is called on the main thread only
//!   (recent macOS asserts it), and a daemon built with
//!   `viewer-indicator-macos` gives that thread to AppKit for its whole life.
//!   Every read and switch is queued there
//!   ([`crate::indicator::mac::on_main`]) and answered on the pump's next pass.
//! - **Reading** polls twice a second while a session watches, on a thread of
//!   its own; with no session watching it reads nothing. **Switching** selects
//!   the source, checks it took, and wakes the reader, whose next snapshot is
//!   the answer.
//! - **Offered** wherever the switch is on and the build runs that main loop.
//!   Not asked of this process's own session: on a supervised Mac the root
//!   daemon says the hello, and a delegated session runs in the GUI worker,
//!   inside the console session (as for copied images, P2e-1).
//!
//! Kill switch `ROOMLERD_MAC_LAYOUT=0`: no `layout` in the hello, no chip.

use std::sync::{Condvar, LazyLock, Mutex, Once};
use std::time::Duration;

/// How often a watched layout is read.
const POLL: Duration = Duration::from_millis(500);
/// How long a read or a switch waits for the main thread's answer.
#[cfg(feature = "viewer-indicator-macos")]
const MAIN_WAIT: Duration = Duration::from_secs(3);

/// FR-97 P2d-2a — kill switch `ROOMLERD_MAC_LAYOUT=0`.
pub fn enabled() -> bool {
    tunnel_core::env::flag("MAC_LAYOUT", true)
}

/// This build can read and switch the layout: the switch is on, and it hands
/// the main thread to AppKit ([`crate::indicator::mac`]), where Text Input
/// Sources has to be called.
pub fn offered() -> bool {
    enabled() && cfg!(feature = "viewer-indicator-macos")
}

/// A switch here stays switched: the selected input source is the system's,
/// and nothing on a Mac puts another one back (measured, FR-97 §8).
pub fn switchable() -> bool {
    true
}

/// Snapshot of the host's keyboard layouts, published on change. The same
/// shape as the Windows and X11 modules'.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutSnapshot {
    /// The source in use, as the token [`request_set_layout`] takes back.
    pub active_hkl: String,
    /// Its tag ([`tag_for`]).
    pub active_tag: String,
    /// Every enabled source as `(token, tag)`, in the input menu's order.
    pub installed: Vec<(String, String)>,
}

type LayoutWatchPair = (
    tokio::sync::watch::Sender<Option<LayoutSnapshot>>,
    tokio::sync::watch::Receiver<Option<LayoutSnapshot>>,
);

static LAYOUT_WATCH: LazyLock<LayoutWatchPair> = LazyLock::new(|| {
    let (tx, rx) = tokio::sync::watch::channel(None);
    (tx, rx)
});

/// Set when a session subscribes or a switch was made: the reader reads now.
static WAKE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static READER: Once = Once::new();

/// Subscribe to layout changes, and start the reader the first time. The
/// receiver starts at the last snapshot (`None` before the first read).
pub fn subscribe() -> tokio::sync::watch::Receiver<Option<LayoutSnapshot>> {
    let rx = LAYOUT_WATCH.1.clone();
    if offered() {
        READER.call_once(|| {
            if let Err(e) = std::thread::Builder::new()
                .name("roomlerd-layout-mac".into())
                .spawn(reader)
            {
                tracing::warn!(%e, "layout: could not start the macOS layout reader");
            }
        });
        wake();
    }
    rx
}

/// The last snapshot, for a control channel that just opened.
pub fn current() -> Option<LayoutSnapshot> {
    LAYOUT_WATCH.1.borrow().clone()
}

fn wake() {
    let (lock, cvar) = &WAKE;
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cvar.notify_all();
}

/// Waits up to `timeout` for a [`wake`]; `true` when one came.
fn wait_for_wake(timeout: Duration) -> bool {
    let (lock, cvar) = &WAKE;
    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let (mut woken, _) = cvar
        .wait_timeout_while(guard, timeout, |w| !*w)
        .unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *woken)
}

/// A session is watching: a receiver beyond the channel's own.
fn watched() -> bool {
    LAYOUT_WATCH.0.receiver_count() > 1
}

/// The reader thread: while a session watches, read every [`POLL`] (or at
/// once when woken) and publish what changed. Without a session it sleeps
/// until one subscribes.
fn reader() {
    loop {
        if !watched() {
            wait_for_wake(Duration::from_secs(60));
            continue;
        }
        match read() {
            Some(sources) => publish(sources.snapshot()),
            None => tracing::debug!("layout: no answer from the main thread for a layout read"),
        }
        wait_for_wake(POLL);
    }
}

fn publish(snap: Option<LayoutSnapshot>) {
    let Some(snap) = snap else { return };
    LAYOUT_WATCH.0.send_if_modified(|cur| {
        if cur.as_ref() == Some(&snap) {
            return false;
        }
        *cur = Some(snap);
        true
    });
}

/// Handle `rc:layout.set {hkl}`, the viewer's picker. A fresh thread per
/// command (rare, a person's click). The token must name a source enabled
/// right now; anything else is refused. The reader's next snapshot is the
/// answer, so a refused switch snaps the picker back.
pub fn request_set_layout(token: String) {
    if !offered() {
        tracing::info!(%token, "layout: rc:layout.set on a host that offers no layout — refusing");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("roomlerd-layout-set".into())
        .spawn(move || {
            match select(token.clone()) {
                Some(Ok(id)) => tracing::info!(%token, source = %id, "layout: switched"),
                Some(Err(why)) => tracing::info!(%token, %why, "layout: switch refused"),
                None => {
                    tracing::warn!(%token, "layout: no answer from the main thread for the switch")
                }
            }
            wake();
        });
    if let Err(e) = spawned {
        tracing::warn!(%e, "layout: could not start the switch");
    }
}

/// One enabled input source.
#[derive(Debug, Clone, PartialEq)]
struct Source {
    /// `com.apple.keylayout.German`.
    id: String,
    /// The system's name for it in the person's language (`German`).
    name: String,
}

/// The enabled sources and the one in use: pure, so it is tested without a
/// Mac.
#[derive(Debug, Clone, PartialEq)]
struct Sources {
    list: Vec<Source>,
    /// The ID of the source in use.
    active: String,
}

impl Sources {
    fn snapshot(&self) -> Option<LayoutSnapshot> {
        let active = self.list.iter().find(|s| s.id == self.active)?;
        Some(LayoutSnapshot {
            active_hkl: token(&active.id),
            active_tag: tag_for(active),
            installed: self
                .list
                .iter()
                .map(|s| (token(&s.id), tag_for(s)))
                .collect(),
        })
    }

    /// The source `token` names, when it names one enabled now.
    #[cfg_attr(not(feature = "viewer-indicator-macos"), allow(dead_code))]
    fn by_token(&self, token: &str) -> Option<&Source> {
        if token.len() != 8 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        self.list
            .iter()
            .find(|s| self::token(&s.id).eq_ignore_ascii_case(token))
    }
}

/// The source ID's hash, 8 hex.
fn token(id: &str) -> String {
    format!("{:08x}", fnv1a(id.as_bytes()))
}

/// What the chip shows for a source: the BCP-47 tag of a layout the table
/// knows, else the system's name for it, else its ID.
fn tag_for(s: &Source) -> String {
    if let Some(tag) = s.id.strip_prefix("com.apple.keylayout.").and_then(bcp47) {
        return tag.to_string();
    }
    if !s.name.is_empty() {
        return s.name.clone();
    }
    s.id.clone()
}

/// Apple keyboard layout → BCP-47, for the layouts whose language and region
/// the name settles. One left out goes by its own name, never by a guess:
/// `ABC` is a US-like layout, but it is not called U.S., and the chip should
/// not claim it is.
fn bcp47(layout: &str) -> Option<&'static str> {
    Some(match layout {
        "US" | "USExtended" | "USInternational-PC" => "en-US",
        "British" | "British-PC" => "en-GB",
        "Australian" => "en-AU",
        "Irish" | "IrishExtended" => "en-IE",
        "Canadian" => "en-CA",
        "German" => "de-DE",
        "Austrian" => "de-AT",
        "SwissGerman" => "de-CH",
        "French" | "French-PC" | "French-numerical" => "fr-FR",
        "Belgian" => "fr-BE",
        "Canadian-CSA" => "fr-CA",
        "SwissFrench" => "fr-CH",
        "Spanish" | "Spanish-ISO" => "es-ES",
        "LatinAmerican" => "es-419",
        "Portuguese" => "pt-PT",
        "Brazilian" | "Brazilian-ABNT2" | "Brazilian-Pro" => "pt-BR",
        "Italian" | "Italian-Pro" => "it-IT",
        "Dutch" => "nl-NL",
        "Swedish" | "Swedish-Pro" => "sv-SE",
        "Norwegian" => "nb-NO",
        "Danish" => "da-DK",
        "Finnish" => "fi-FI",
        "Icelandic" => "is-IS",
        "Estonian" => "et-EE",
        "Latvian" => "lv-LV",
        "Lithuanian" => "lt-LT",
        "Polish" | "PolishPro" => "pl-PL",
        "Czech" | "Czech-QWERTY" => "cs-CZ",
        "Slovak" | "Slovak-QWERTY" => "sk-SK",
        "Hungarian" | "Hungarian-QWERTY" => "hu-HU",
        "Slovenian" => "sl-SI",
        "Croatian" | "Croatian-PC" => "hr-HR",
        "Serbian" => "sr-RS",
        "Serbian-Latin" => "sr-Latn-RS",
        "Macedonian" => "mk-MK",
        "Bulgarian" | "Bulgarian-Phonetic" => "bg-BG",
        "Romanian" | "Romanian-Standard" => "ro-RO",
        "Greek" | "GreekPolytonic" => "el-GR",
        "Turkish" | "Turkish-QWERTY" | "Turkish-QWERTY-PC" => "tr-TR",
        "Russian" | "RussianWin" | "Russian-Phonetic" => "ru-RU",
        "Ukrainian" | "Ukrainian-PC" => "uk-UA",
        "Byelorussian" => "be-BY",
        "Kazakh" => "kk-KZ",
        "Georgian-QWERTY" => "ka-GE",
        "Armenian-HMCharacters" | "Armenian-WesternQWERTY" => "hy-AM",
        "Azeri" => "az-AZ",
        "Hebrew" | "Hebrew-QWERTY" | "Hebrew-PC" => "he-IL",
        "Arabic" | "Arabic-PC" | "Arabic-QWERTY" => "ar",
        "Persian" | "Persian-ISIRI2901" => "fa-IR",
        "Thai" | "Thai-PattaChote" => "th-TH",
        "Vietnamese" => "vi-VN",
        _ => return None,
    })
}

/// FNV-1a, 32 bits: a stable hash for the token, the same on every build.
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5u32, |h, &b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// The enabled sources and the one in use, read on the main thread. `None`
/// when the main thread did not answer.
#[cfg(feature = "viewer-indicator-macos")]
fn read() -> Option<Sources> {
    crate::indicator::mac::on_main(MAIN_WAIT, |_| tis::sources())
}

#[cfg(not(feature = "viewer-indicator-macos"))]
fn read() -> Option<Sources> {
    None
}

/// Select the source `token` names, on the main thread, and check it took.
/// `Ok` carries its ID; `Err` says why not.
#[cfg(feature = "viewer-indicator-macos")]
fn select(token: String) -> Option<Result<String, String>> {
    crate::indicator::mac::on_main(MAIN_WAIT, move |_| {
        let sources = tis::sources();
        let Some(source) = sources.by_token(&token) else {
            return Err("no enabled input source has that token".to_string());
        };
        let id = source.id.clone();
        tis::select(&id)?;
        let now = tis::sources().active;
        if now == id {
            Ok(id)
        } else {
            Err(format!("selected {id}, but {now} is in use"))
        }
    })
}

#[cfg(not(feature = "viewer-indicator-macos"))]
fn select(_token: String) -> Option<Result<String, String>> {
    None
}

/// Text Input Sources (Carbon's HIToolbox). Every function here runs on the
/// main thread only: [`read`] and [`select`] queue them there.
#[cfg(feature = "viewer-indicator-macos")]
mod tis {
    use std::ffi::c_void;

    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};

    use super::{Source, Sources};

    type TISInputSourceRef = *const c_void;

    // Carbon's own spellings.
    #[allow(non_snake_case, non_upper_case_globals)]
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
        fn TISCreateInputSourceList(
            properties: CFDictionaryRef,
            include_all_installed: u8,
        ) -> CFArrayRef;
        fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *const c_void;
        fn TISSelectInputSource(source: TISInputSourceRef) -> i32;
        static kTISPropertyInputSourceID: CFStringRef;
        static kTISPropertyLocalizedName: CFStringRef;
        static kTISPropertyInputSourceCategory: CFStringRef;
        static kTISCategoryKeyboardInputSource: CFStringRef;
        static kTISPropertyInputSourceIsSelectCapable: CFStringRef;
    }

    /// A string property of a source, `""` when it has none.
    fn string_property(source: TISInputSourceRef, key: CFStringRef) -> String {
        // SAFETY: `source` is a live input source (the caller holds the list or
        // the copy it came from); the property is returned under the Get rule,
        // so it is wrapped without taking ownership.
        unsafe {
            let p = TISGetInputSourceProperty(source, key);
            if p.is_null() {
                return String::new();
            }
            CFString::wrap_under_get_rule(p as CFStringRef).to_string()
        }
    }

    /// The enabled, selectable keyboard sources: the input menu's list.
    fn list() -> CFArray<CFType> {
        // SAFETY: the keys and values are the framework's own constant strings
        // and CFBoolean's singleton; the list comes back under the Create rule
        // and is owned by the returned CFArray.
        unsafe {
            let filter = CFDictionary::from_CFType_pairs(&[
                (
                    CFString::wrap_under_get_rule(kTISPropertyInputSourceCategory).as_CFType(),
                    CFString::wrap_under_get_rule(kTISCategoryKeyboardInputSource).as_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kTISPropertyInputSourceIsSelectCapable)
                        .as_CFType(),
                    CFBoolean::true_value().as_CFType(),
                ),
            ]);
            CFArray::wrap_under_create_rule(TISCreateInputSourceList(
                filter.as_concrete_TypeRef(),
                0,
            ))
        }
    }

    pub(super) fn sources() -> Sources {
        let arr = list();
        // SAFETY: the framework's constant keys, read on the main thread.
        let (id_key, name_key) = unsafe { (kTISPropertyInputSourceID, kTISPropertyLocalizedName) };
        let list = arr
            .get_all_values()
            .into_iter()
            .map(|s| Source {
                id: string_property(s as TISInputSourceRef, id_key),
                name: string_property(s as TISInputSourceRef, name_key),
            })
            .filter(|s| !s.id.is_empty())
            .collect();
        // SAFETY: the current source comes back under the Copy rule; wrapping
        // it in a CFType releases it when it goes out of scope.
        let active = unsafe {
            let cur = TISCopyCurrentKeyboardInputSource();
            if cur.is_null() {
                String::new()
            } else {
                let owned = CFType::wrap_under_create_rule(cur as CFTypeRef);
                string_property(owned.as_CFTypeRef() as TISInputSourceRef, id_key)
            }
        };
        Sources { list, active }
    }

    /// Select the enabled source with this ID.
    pub(super) fn select(id: &str) -> Result<(), String> {
        let arr = list();
        // SAFETY: the framework's constant key, read on the main thread.
        let id_key = unsafe { kTISPropertyInputSourceID };
        let Some(source) = arr
            .get_all_values()
            .into_iter()
            .find(|&s| string_property(s as TISInputSourceRef, id_key) == id)
        else {
            return Err(format!("{id} is not enabled"));
        };
        // SAFETY: `source` is owned by `arr`, alive for this call.
        let status = unsafe { TISSelectInputSource(source as TISInputSourceRef) };
        if status == 0 {
            Ok(())
        } else {
            Err(format!("TISSelectInputSource failed ({status})"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: &str, name: &str) -> Source {
        Source {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    fn two() -> Sources {
        Sources {
            list: vec![
                src("com.apple.keylayout.US", "U.S."),
                src("com.apple.keylayout.German", "German"),
            ],
            active: "com.apple.keylayout.German".to_string(),
        }
    }

    #[test]
    fn the_snapshot_names_the_source_in_use_and_lists_every_one() {
        let snap = two().snapshot().expect("a snapshot");
        assert_eq!(snap.active_hkl, token("com.apple.keylayout.German"));
        assert_eq!(snap.active_tag, "de-DE");
        assert_eq!(
            snap.installed,
            vec![
                (token("com.apple.keylayout.US"), "en-US".to_string()),
                (token("com.apple.keylayout.German"), "de-DE".to_string()),
            ]
        );
    }

    #[test]
    fn no_snapshot_when_the_source_in_use_is_not_listed() {
        // An input method in the middle of a mode switch, or a list read that
        // raced a change: say nothing rather than name the wrong source.
        let mut s = two();
        s.active = "com.apple.keylayout.French".to_string();
        assert_eq!(s.snapshot(), None);
    }

    #[test]
    fn a_token_names_only_an_enabled_source() {
        let s = two();
        let t = token("com.apple.keylayout.US");
        assert_eq!(t.len(), 8);
        assert_eq!(
            s.by_token(&t).map(|x| x.id.as_str()),
            Some("com.apple.keylayout.US")
        );
        assert_eq!(
            s.by_token(&t.to_uppercase()).map(|x| x.id.as_str()),
            Some("com.apple.keylayout.US")
        );
        // A source disabled since the picker was drawn, and garbage.
        assert_eq!(s.by_token(&token("com.apple.keylayout.French")), None);
        assert_eq!(s.by_token("xyz"), None);
        assert_eq!(s.by_token(""), None);
        // The X11 module's 9-hex shape is not a Mac token.
        assert_eq!(s.by_token(&format!("{t}0")), None);
    }

    #[test]
    fn the_tag_is_a_known_layouts_bcp47_else_its_name_else_its_id() {
        assert_eq!(
            tag_for(&src("com.apple.keylayout.British", "British")),
            "en-GB"
        );
        assert_eq!(
            tag_for(&src("com.apple.keylayout.SwissGerman", "Swiss German")),
            "de-CH"
        );
        assert_eq!(
            tag_for(&src("com.apple.keylayout.Serbian-Latin", "Serbian - Latin")),
            "sr-Latn-RS"
        );
        // Not in the table: its own name, never a guess.
        assert_eq!(tag_for(&src("com.apple.keylayout.ABC", "ABC")), "ABC");
        assert_eq!(
            tag_for(&src(
                "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese",
                "Hiragana"
            )),
            "Hiragana"
        );
        assert_eq!(
            tag_for(&src("org.example.layout.Mine", "")),
            "org.example.layout.Mine"
        );
    }

    #[test]
    fn the_hash_is_fnv1a() {
        assert_eq!(fnv1a(b""), 0x811c_9dc5);
        assert_eq!(fnv1a(b"a"), 0xe40c_292c);
        assert_eq!(token(""), "811c9dc5");
    }

    #[test]
    fn switching_is_never_reverted_on_a_mac() {
        assert!(switchable());
    }
}
