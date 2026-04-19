use std::process::Command;

use log::debug;

const DEST: &str = "org.kde.plasmashell";
const PATH: &str = "/org/kde/osdService";
const IFACE: &str = "org.kde.osdService";

/// Failures are intentionally swallowed — OSD is non-critical and should
/// never prevent volume/mute operations from completing.
fn call(method: &str, args: &[&str]) {
    let mut cmd = Command::new("gdbus");
    cmd.arg("call")
        .arg("--session")
        .arg("--dest")
        .arg(DEST)
        .arg("--object-path")
        .arg(PATH)
        .arg("--method")
        .arg(format!("{IFACE}.{method}"));
    for arg in args {
        cmd.arg(arg);
    }

    match cmd.output() {
        Ok(output) if !output.status.success() => {
            debug!(
                "OSD call failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => debug!("OSD call failed: {e}"),
        _ => {}
    }
}

/// Show the system volume OSD (same as pressing volume keys).
pub fn volume_changed(percent: i32) {
    call("volumeChanged", &[&percent.to_string()]);
}

/// Show volume OSD for a named media player/app.
pub fn media_player_volume_changed(percent: i32, player_name: &str, icon: &str) {
    call(
        "mediaPlayerVolumeChanged",
        &[&percent.to_string(), player_name, icon],
    );
}

/// Show the microphone volume OSD.
pub fn microphone_volume_changed(percent: i32) {
    call("microphoneVolumeChanged", &[&percent.to_string()]);
}

/// Show a text OSD with an icon (for mute toggles, etc.).
pub fn show_text(icon: &str, text: &str) {
    call("showText", &[icon, text]);
}

/// Show mute status via OSD.
pub fn show_mute(name: &str, muted: bool) {
    let icon = if muted {
        "audio-volume-muted"
    } else {
        "audio-volume-high"
    };
    let status = if muted { "Muted" } else { "Unmuted" };
    show_text(icon, &format!("{name}: {status}"));
}

/// Show mic mute status via OSD.
pub fn show_mic_mute(muted: bool) {
    let icon = if muted {
        "microphone-sensitivity-muted"
    } else {
        "microphone-sensitivity-high"
    };
    let status = if muted { "Muted" } else { "Unmuted" };
    show_text(icon, &format!("Microphone: {status}"));
}
