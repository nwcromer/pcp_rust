use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

// Passing an empty icon name to KDE's mediaPlayerVolumeChanged renders the
// OSD with no icon, which is what we want when no app icon resolves —
// the standard speaker icon would imply the system volume is changing.
const DEFAULT_ICON: &str = "";

/// Priority: config icon > `.desktop` file lookup > lowercased app name
/// (only if it resolves in an installed icon theme) > None.
///
/// The last fallback verifies via `freedesktop-icons` that the name actually
/// resolves under the XDG icon spec (themes, inheritance, all sizes). If KDE
/// wouldn't find it either, we return None so the OSD renders no icon rather
/// than KDE's unknown-file (paper-fold) default.
fn find_app_icon(config_icon: Option<&str>, app_names: &[String]) -> Option<String> {
    if let Some(icon) = config_icon {
        return Some(icon.to_string());
    }

    // Cache per app-name set — the slider hot path re-resolves the same names
    // every tick. Process-lifetime, like the sibling caches.
    static CACHE: LazyLock<Mutex<HashMap<Vec<String>, Option<String>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    if let Some(cached) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(app_names) {
        return cached.clone();
    }
    let resolved = resolve_app_icon(app_names);
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(app_names.to_vec(), resolved.clone());
    resolved
}

/// Uncached icon resolution for a set of app names: `.desktop` lookup, then
/// the lowercased app name if it resolves in an installed icon theme.
fn resolve_app_icon(app_names: &[String]) -> Option<String> {
    for app in app_names {
        if let Some(icon) = find_icon_in_desktop_files(app) {
            return Some(icon);
        }
    }
    let candidate = app_names.first()?.to_lowercase();
    freedesktop_icon_resolves(&candidate).then_some(candidate)
}

/// `freedesktop_icons::lookup(name).find()` exhaustively scans every installed
/// theme/size/category when the name doesn't resolve — hundreds of stat() calls.
/// Fine once, but slider events fire ~10 Hz and each tick hit this for icon-less
/// apps, backing up the HID event loop. Cache so each name resolves at most once.
fn freedesktop_icon_resolves(name: &str) -> bool {
    static CACHE: LazyLock<Mutex<HashMap<String, bool>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    // Recover from lock poisoning: the cache is a plain memo, so treating a
    // poisoned lock's contents as authoritative is fine.
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

/// `.desktop` search dirs in XDG Base Directory precedence order:
/// `$XDG_DATA_HOME` (or `~/.local/share`), then `$XDG_DATA_DIRS` (or the spec
/// default `/usr/local/share:/usr/share`), each with `applications` appended.
/// The user dir surfaces integrated AppImages/user apps; system dirs cover packaged ones.
///
/// Intentionally untested: this reads process env (`XDG_DATA_*`), and a
/// deterministic test would have to mutate global env vars — `unsafe` in
/// edition 2024 and racy under parallel tests. The search logic over a given
/// dir set is what's worth covering, and `find_icon_in_dirs` already is.
fn desktop_dirs() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut push = |base: PathBuf| {
        let dir = base.join("applications");
        // Skip relative/empty entries (an empty XDG_DATA_DIRS component would
        // otherwise scan a cwd-relative "applications") and de-dup.
        if dir.is_absolute() && !paths.contains(&dir) {
            paths.push(dir);
        }
    };

    if let Some(home) = dirs::data_dir() {
        push(home);
    }
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    for base in std::env::split_paths(&system) {
        push(base);
    }
    paths
}

/// Search .desktop files for an app and return its Icon= value.
fn find_icon_in_desktop_files(app_name: &str) -> Option<String> {
    find_icon_in_dirs(app_name, desktop_dirs().iter().map(PathBuf::as_path))
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

/// Parse the `Icon=` value from a `.desktop` file's `[Desktop Entry]` section.
/// Other sections (`[Desktop Action ...]`, `[X-Foo]`) can carry their own
/// `Icon=` for context-menu items, not the app icon — so we ignore them.
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
