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
    find_icon_in_dirs(app_name, DESKTOP_DIRS.iter().map(Path::new))
}

/// Inner search loop parameterized over the directories to scan. Extracted
/// so tests can point it at a tempdir.
fn find_icon_in_dirs<'a, I>(app_name: &str, dirs: I) -> Option<String>
where
    I: IntoIterator<Item = &'a Path>,
{
    let target = app_name.to_lowercase();
    for dir_path in dirs {
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

            if let Ok(content) = fs::read_to_string(&path)
                && let Some(icon) = parse_desktop_entry_icon(&content)
            {
                return Some(icon);
            }
        }
    }
    None
}

/// Parse the `Icon=` value from a `.desktop` file's `[Desktop Entry]`
/// section. Other sections (`[Desktop Action ...]`, `[X-Foo]`) can carry
/// their own `Icon=` for context-menu items, which are not the primary
/// app icon — so we deliberately ignore them.
fn parse_desktop_entry_icon(content: &str) -> Option<String> {
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
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_desktop(dir: &Path, name: &str, content: &str) {
        let path = dir.join(format!("{name}.desktop"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn parse_icon_from_desktop_entry() {
        let content = "\
[Desktop Entry]
Name=Foo
Icon=foo-app
Exec=/usr/bin/foo
";
        assert_eq!(parse_desktop_entry_icon(content), Some("foo-app".into()));
    }

    #[test]
    fn parse_icon_ignores_other_sections() {
        // The [Desktop Action] icon should NOT win — it's for a context-menu
        // entry, not the app icon.
        let content = "\
[Desktop Action New]
Name=New Window
Icon=action-new

[Desktop Entry]
Name=Foo
Icon=foo-app
";
        assert_eq!(parse_desktop_entry_icon(content), Some("foo-app".into()));
    }

    #[test]
    fn parse_icon_skips_when_only_other_section_has_icon() {
        let content = "\
[Desktop Action New]
Icon=action-only
";
        assert_eq!(parse_desktop_entry_icon(content), None);
    }

    #[test]
    fn parse_icon_empty_value_returns_none() {
        let content = "\
[Desktop Entry]
Icon=
";
        assert_eq!(parse_desktop_entry_icon(content), None);
    }

    #[test]
    fn find_icon_in_dirs_matches_filename_substring() {
        let tmp = tempfile::tempdir().unwrap();
        write_desktop(
            tmp.path(),
            "org.example.firefox",
            "[Desktop Entry]\nIcon=firefox-icon\n",
        );
        let dirs = [tmp.path()];
        assert_eq!(
            find_icon_in_dirs("firefox", dirs.iter().copied()),
            Some("firefox-icon".into())
        );
    }

    #[test]
    fn find_icon_in_dirs_returns_none_for_unrelated_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_desktop(tmp.path(), "vlc", "[Desktop Entry]\nIcon=vlc\n");
        let dirs = [tmp.path()];
        assert_eq!(find_icon_in_dirs("firefox", dirs.iter().copied()), None);
    }
}
