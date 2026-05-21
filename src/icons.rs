use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

const DESKTOP_DIRS: &[&str] = &[
    "/usr/share/applications",
    "/usr/local/share/applications",
];

// Passing an empty icon name to KDE's mediaPlayerVolumeChanged renders the
// OSD with no icon, which is what we want when no app icon resolves —
// the standard speaker icon would imply the system volume is changing.
const DEFAULT_ICON: &str = "";

/// Try to find an icon for the given apps.
/// Priority: config icon > .desktop file lookup > lowercased app name
/// (only if it exists in an installed icon theme) > None.
///
/// The last fallback uses `freedesktop-icons` to verify the candidate
/// actually resolves under the XDG icon spec (themes, inheritance, all
/// sizes/categories). If KDE wouldn't find an icon for the name either,
/// we return None — the OSD then renders no icon, which is what we want
/// instead of KDE's unknown-file (paper-fold) default.
fn find_app_icon(config_icon: Option<&str>, app_names: &[String]) -> Option<String> {
    if let Some(icon) = config_icon {
        return Some(icon.to_string());
    }

    for app in app_names {
        if let Some(icon) = find_icon_in_desktop_files(app) {
            return Some(icon);
        }
    }

    let candidate = app_names.first()?.to_lowercase();
    if freedesktop_icon_resolves(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

/// `freedesktop_icons::lookup(name).find()` exhaustively scans every
/// installed theme/size/category when the name doesn't resolve — hundreds
/// of stat() calls. That's fine once, but slider events fire ~10 Hz and
/// each tick was hitting this for apps with no icon, backing up the HID
/// event loop. Cache the answer so each name is resolved at most once
/// per process lifetime.
fn freedesktop_icon_resolves(name: &str) -> bool {
    static CACHE: LazyLock<Mutex<HashMap<String, bool>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    // `unwrap_or_else(... e.into_inner())` recovers from lock poisoning —
    // the cache's invariants are simple enough that even if a prior
    // panic-while-locked left it in an "inconsistent" state, treating the
    // current contents as authoritative is fine.
    if let Some(&cached) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
    {
        return cached;
    }
    let found = freedesktop_icons::lookup(name).find().is_some();
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), found);
    found
}

/// Resolve an icon name for volume display.
pub fn resolve(config_icon: Option<&str>, app_names: &[String]) -> String {
    find_app_icon(config_icon, app_names).unwrap_or_else(|| DEFAULT_ICON.to_string())
}

/// Resolve an icon for mute toggle. Falls back to mute/unmute icons.
pub fn resolve_mute(config_icon: Option<&str>, app_names: &[String], muted: bool) -> String {
    find_app_icon(config_icon, app_names).unwrap_or_else(|| {
        if muted {
            "audio-volume-muted".to_string()
        } else {
            "audio-volume-high".to_string()
        }
    })
}

/// Search .desktop files for an app and return its Icon= value.
fn find_icon_in_desktop_files(app_name: &str) -> Option<String> {
    let target = app_name.to_lowercase();
    for dir in DESKTOP_DIRS {
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }

            // Quick check: does the filename contain the app name?
            let filename = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !filename.contains(&target) {
                continue;
            }

            // Parse the .desktop file for Icon= in the [Desktop Entry]
            // section only. Other sections (`[Desktop Action ...]`,
            // `[X-Foo]`) can carry their own Icon= for context-menu items,
            // which are not the primary app icon.
            if let Ok(content) = fs::read_to_string(&path) {
                let mut in_desktop_entry = false;
                for raw in content.lines() {
                    let line = raw.trim();
                    if line.starts_with('[') && line.ends_with(']') {
                        in_desktop_entry = line == "[Desktop Entry]";
                        continue;
                    }
                    if !in_desktop_entry {
                        continue;
                    }
                    if let Some(icon) = line.strip_prefix("Icon=") {
                        let icon = icon.trim();
                        if !icon.is_empty() {
                            return Some(icon.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}
