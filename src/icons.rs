use std::fs;
use std::path::Path;

const DESKTOP_DIRS: &[&str] = &[
    "/usr/share/applications",
    "/usr/local/share/applications",
];

const DEFAULT_ICON: &str = "audio-volume-high";

/// Resolve an icon name for an app.
/// Priority: config icon > .desktop file lookup > app name > default.
pub fn resolve(config_icon: Option<&str>, app_names: &[String]) -> String {
    // 1. Explicit config icon
    if let Some(icon) = config_icon {
        return icon.to_string();
    }

    // 2. Search .desktop files for a matching app
    for app in app_names {
        if let Some(icon) = find_icon_in_desktop_files(app) {
            return icon;
        }
    }

    // 3. Try the first app name directly as an icon name
    // (works for apps like firefox, chromium, etc.)
    if let Some(first) = app_names.first() {
        let candidate = first.to_lowercase();
        if icon_exists(&candidate) {
            return candidate;
        }
    }

    // 4. Fallback
    DEFAULT_ICON.to_string()
}

/// Resolve an icon for mute toggle. Uses app icon if found, otherwise mute/unmute icons.
pub fn resolve_mute(config_icon: Option<&str>, app_names: &[String], muted: bool) -> String {
    if let Some(icon) = config_icon {
        return icon.to_string();
    }

    // Try to find an app icon
    for app in app_names {
        if let Some(icon) = find_icon_in_desktop_files(app) {
            return icon;
        }
    }
    if let Some(first) = app_names.first() {
        let candidate = first.to_lowercase();
        if icon_exists(&candidate) {
            return candidate;
        }
    }

    // Fallback to standard mute/unmute icons
    if muted {
        "audio-volume-muted".to_string()
    } else {
        "audio-volume-high".to_string()
    }
}

/// Search .desktop files for an app and return its Icon= value.
fn find_icon_in_desktop_files(app_name: &str) -> Option<String> {
    let target = app_name.to_lowercase();
    for dir in DESKTOP_DIRS {
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            continue;
        }
        let entries = fs::read_dir(dir_path).ok()?;
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

            // Parse the .desktop file for Icon=
            if let Ok(content) = fs::read_to_string(&path) {
                for line in content.lines() {
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

/// Check if a freedesktop icon name likely exists.
/// This is a heuristic — we check common icon theme paths.
fn icon_exists(name: &str) -> bool {
    let theme_dirs = [
        "/usr/share/icons/hicolor",
        "/usr/share/icons/breeze",
        "/usr/share/pixmaps",
    ];

    for dir in &theme_dirs {
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            continue;
        }
        // Check pixmaps directly
        if *dir == "/usr/share/pixmaps" {
            for ext in &["png", "svg", "xpm"] {
                if dir_path.join(format!("{name}.{ext}")).exists() {
                    return true;
                }
            }
            continue;
        }
        // For icon themes, check common sizes
        for size in &["scalable", "48x48", "64x64", "128x128", "256x256"] {
            for category in &["apps", "mimetypes"] {
                for ext in &["svg", "png"] {
                    if dir_path
                        .join(size)
                        .join(category)
                        .join(format!("{name}.{ext}"))
                        .exists()
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}
