//! KDE Plasma OSD popups via the org.kde.osdService D-Bus interface.
//!
//! Holds a cached session-bus connection — the previous implementation
//! forked `gdbus` per call, which cost ~5-10ms per knob/slider tick. The
//! cache is reset on call failure so we automatically recover if the
//! session bus daemon restarts or drops our connection. Failures are
//! intentionally swallowed: the OSD is a nice-to-have and shouldn't
//! block volume/mute operations.

use std::sync::{LazyLock, Mutex};

use log::debug;
use zbus::blocking::Connection;
use zbus::zvariant::DynamicType;

const DEST: &str = "org.kde.plasmashell";
const PATH: &str = "/org/kde/osdService";
const IFACE: &str = "org.kde.osdService";

static SESSION: LazyLock<Mutex<Option<Connection>>> = LazyLock::new(|| Mutex::new(None));

fn call<T>(method: &str, body: &T)
where
    T: serde::Serialize + DynamicType,
{
    // Lock poisoning recovery: if a previous holder panicked, the cached
    // connection is still usable (or `None`), so treat the inner value as
    // authoritative rather than propagating a panic.
    let mut session = SESSION.lock().unwrap_or_else(|e| e.into_inner());

    // Connect lazily on first call, or after a previous failure invalidated
    // the cached connection.
    if session.is_none() {
        match Connection::session() {
            Ok(c) => *session = Some(c),
            Err(e) => {
                debug!("OSD: session bus unavailable: {e}");
                return;
            }
        }
    }

    let conn = session.as_ref().expect("set above");
    if let Err(e) = conn.call_method(Some(DEST), PATH, Some(IFACE), method, body) {
        debug!("OSD: {method} call failed: {e}; dropping cached connection");
        // Drop the cached connection so the next call reconnects. Helps if
        // the bus daemon restarted or dropped the connection mid-session.
        *session = None;
    }
}

/// Show the system volume OSD (same as pressing volume keys).
pub fn volume_changed(percent: i32) {
    call("volumeChanged", &(percent,));
}

/// Show volume OSD for a named media player/app.
pub fn media_player_volume_changed(percent: i32, player_name: &str, icon: &str) {
    call("mediaPlayerVolumeChanged", &(percent, player_name, icon));
}

/// Show the microphone volume OSD.
pub fn microphone_volume_changed(percent: i32) {
    call("microphoneVolumeChanged", &(percent,));
}

/// Show a text OSD with an icon (for mute toggles, etc.).
pub fn show_text(icon: &str, text: &str) {
    call("showText", &(icon, text));
}

/// Show a `"<label>: Muted/Unmuted"` OSD with a muted/unmuted icon pair.
/// Shared by the system and mic mute popups, which differ only in the label
/// and which icon set fits the device. Named-app mute does NOT use this — it
/// resolves a per-app icon (see `icons::resolve_mute`) rather than a fixed
/// pair.
fn show_mute_status(label: &str, muted: bool, muted_icon: &str, unmuted_icon: &str) {
    let icon = if muted { muted_icon } else { unmuted_icon };
    let status = if muted { "Muted" } else { "Unmuted" };
    show_text(icon, &format!("{label}: {status}"));
}

/// Show mute status via OSD.
pub fn show_mute(name: &str, muted: bool) {
    show_mute_status(name, muted, "audio-volume-muted", "audio-volume-high");
}

/// Show mic mute status via OSD.
pub fn show_mic_mute(muted: bool) {
    show_mute_status(
        "Microphone",
        muted,
        "microphone-sensitivity-muted",
        "microphone-sensitivity-high",
    );
}
