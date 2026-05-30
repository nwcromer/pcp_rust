use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use log::warn;

const SYSTEM_APP: &str = "system";
const MIC_APP: &str = "mic";

/// What an audio control targets. Parsed once from the config string so
/// downstream code can dispatch by enum variant instead of repeated
/// case-insensitive string comparisons at every call site.
#[derive(Debug, Clone)]
pub enum AppTarget {
    /// Default audio output (sink).
    System,
    /// Default audio input (source).
    Mic,
    /// Substring match against running audio applications.
    Named(String),
}

impl AppTarget {
    fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case(SYSTEM_APP) {
            AppTarget::System
        } else if s.eq_ignore_ascii_case(MIC_APP) {
            AppTarget::Mic
        } else {
            AppTarget::Named(s.to_string())
        }
    }

    /// Human-readable label used in verbose logs and OSD captions.
    pub fn label(&self) -> &str {
        match self {
            AppTarget::System => "System",
            AppTarget::Mic => "Mic",
            AppTarget::Named(s) => s.as_str(),
        }
    }
}

/// Shared payload for volume/mute actions: which targets to drive plus an
/// optional explicit icon name for the OSD.
#[derive(Debug)]
pub struct AppAction {
    pub targets: Vec<AppTarget>,
    pub icon: Option<String>,
}

#[derive(Debug)]
pub enum Action {
    Volume(AppAction),
    ToggleMute(AppAction),
    ObsSaveReplay,
    ObsToggleRecording,
    ObsPauseRecording,
    ObsSplitRecording,
}

impl Action {
    pub fn is_obs(&self) -> bool {
        matches!(
            self,
            Action::ObsSaveReplay
                | Action::ObsToggleRecording
                | Action::ObsPauseRecording
                | Action::ObsSplitRecording
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlId {
    Knob(u8),
    Slider(u8),
    Button(u8),
}

impl ControlId {
    fn is_button(self) -> bool {
        matches!(self, ControlId::Button(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RainbowStyle {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Copy)]
pub enum RgbMode {
    Solid { r: u8, g: u8, b: u8 },
    Rainbow { style: RainbowStyle },
    Gradient { color1: RgbColor, color2: RgbColor },
    VolumeGradient { color1: RgbColor, color2: RgbColor },
    Wave { hue: u8, brightness: u8, speed: u8, reverse: bool, bounce: bool },
    Breath { hue: u8, brightness: u8, speed: u8 },
}

#[derive(Debug)]
pub struct Config {
    pub mappings: HashMap<ControlId, Action>,
    pub rgb: Option<RgbMode>,
    pub obs: Option<ObsConfig>,
    pub logo: LogoConfig,
}

/// Which (if any) state the logo LED should indicate. The logo is a single
/// LED, so it can show at most one thing at a time — the user picks which.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogoIndicator {
    /// Logo just matches the panel color — no separate indication.
    #[default]
    None,
    /// Logo shows the default microphone's mute state.
    Mic,
    /// Logo shows OBS's replay-buffer state (only meaningful when OBS is
    /// connected; matches the panel color when disconnected).
    Replay,
}

/// Logo configuration: which indicator (if any) drives the logo, plus the
/// color for each possible state. Unused colors are simply ignored.
/// Sensible defaults mean users can just pick an indicator and get
/// reasonable behavior without touching the colors.
///
/// Only effective in states where the logo is independently writable
/// (i.e. not global animations like rainbow/wave/breath, nor the global
/// breath used by `paused_use_breath`).
#[derive(Debug, Clone, Copy)]
pub struct LogoConfig {
    pub indicator: LogoIndicator,
    pub mic_muted: RgbColor,
    pub mic_unmuted: RgbColor,
    /// Color shown (blinking) when the mic indicator is configured but PA
    /// hasn't been able to confirm the mic's actual state recently. The
    /// blink + non-standard color signal "do not trust the cached state"
    /// — the user should treat the mic as possibly unmuted until the
    /// logo returns to red/green.
    pub mic_unknown: RgbColor,
    pub replay_active: RgbColor,
    pub replay_inactive: RgbColor,
}

impl Default for LogoConfig {
    fn default() -> Self {
        Self {
            indicator: LogoIndicator::None,
            mic_muted: RgbColor { r: 0xFF, g: 0x00, b: 0x00 },
            mic_unmuted: RgbColor { r: 0x00, g: 0xFF, b: 0x00 },
            // Burnt-orange / dark amber — visually distinct from red, green,
            // and amber-paused, with semantic weight of "warning, verify."
            mic_unknown: RgbColor { r: 0x80, g: 0x40, b: 0x00 },
            replay_active: RgbColor { r: 0x00, g: 0xFF, b: 0xFF },
            replay_inactive: RgbColor { r: 0x00, g: 0x00, b: 0x00 },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ObsConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    /// If true, pcp_rust will start OBS's replay buffer on every successful
    /// connection (skipping if it's already running). Off by default — the
    /// user normally manages replay buffer state in OBS directly.
    pub start_replay_buffer: bool,
    /// If true, the panel shows a global breath effect when recording is
    /// paused — every LED including the logo joins the breath, so any
    /// configured `[logo]` indicator is unavailable while paused. If false
    /// (default), paused renders as a solid color and the logo continues
    /// to show whatever the indicator says.
    pub paused_use_breath: bool,
    pub colors: ObsColors,
}

#[derive(Debug, Clone, Copy)]
pub struct ObsColors {
    pub recording: RgbColor,
    pub paused: RgbColor,
    pub success_flash: RgbColor,
    pub error_flash: RgbColor,
    pub flash_duration_ms: u64,
    /// Panel color when OBS is connected and idle (not recording). When OBS
    /// is disconnected, `[rgb]` is used instead.
    pub idle_panel: RgbColor,
}

impl Default for ObsColors {
    fn default() -> Self {
        Self {
            // PCPanel Pro red is gamma-compressed at the high end — values
            // from 0x80 to 0xFF all read as roughly "full red". 0x50 is
            // visibly dimmer while still clearly red.
            recording: RgbColor { r: 0x50, g: 0x00, b: 0x00 },
            paused: RgbColor { r: 0xFF, g: 0xC0, b: 0x00 },
            success_flash: RgbColor { r: 0x00, g: 0xFF, b: 0x00 },
            error_flash: RgbColor { r: 0xFF, g: 0x00, b: 0xFF },
            flash_duration_ms: 500,
            idle_panel: RgbColor { r: 0x20, g: 0x20, b: 0x20 },
        }
    }
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("pcpanel").join("config.toml"))
}

pub fn load_config(path: &Path) -> Result<Config> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    parse_config(&content)
}

/// Keys present in `table` that aren't in `known`. Pure so it can be
/// unit-tested; `warn_unknown_keys` is the logging wrapper.
fn unknown_keys(table: &toml::value::Table, known: &[&str]) -> Vec<String> {
    table
        .keys()
        .filter(|k| !known.contains(&k.as_str()))
        .cloned()
        .collect()
}

/// Warn (don't fail) on keys the parser doesn't recognize. The TOML parser
/// silently ignores unknown keys, so a typo like `brigthness` would
/// otherwise leave the user with a default value and no feedback. We warn
/// rather than bail so a config written for a newer pcp_rust doesn't
/// hard-fail an older binary — an unknown key and a future key are
/// indistinguishable here.
fn warn_unknown_keys(table: &toml::value::Table, section: &str, known: &[&str]) {
    for key in unknown_keys(table, known) {
        warn!("[{section}] unknown key \"{key}\" — ignored (possible typo?)");
    }
}

/// Recognized keys for an `[rgb]` table, which depend on the mode.
fn rgb_known_keys(mode: &str) -> &'static [&'static str] {
    match mode {
        "solid" => &["mode", "color"],
        "rainbow" => &["mode", "style"],
        "gradient" | "volume-gradient" => &["mode", "color1", "color2"],
        "wave" => &["mode", "hue", "brightness", "speed", "reverse", "bounce"],
        "breath" => &["mode", "hue", "brightness", "speed"],
        _ => &["mode"], // unknown mode bails below; nothing else to validate
    }
}

fn parse_control_id(key: &str) -> Result<ControlId> {
    if let Some(n) = key.strip_prefix("knob") {
        let i: u8 = n.parse().context("invalid knob number")?;
        if !(1..=5).contains(&i) {
            bail!("knob number must be 1-5, got {i}");
        }
        return Ok(ControlId::Knob(i - 1));
    }
    if let Some(n) = key.strip_prefix("slider") {
        let i: u8 = n.parse().context("invalid slider number")?;
        if !(1..=4).contains(&i) {
            bail!("slider number must be 1-4, got {i}");
        }
        return Ok(ControlId::Slider(i - 1));
    }
    if let Some(n) = key.strip_prefix("button") {
        let i: u8 = n.parse().context("invalid button number")?;
        if !(1..=5).contains(&i) {
            bail!("button number must be 1-5, got {i}");
        }
        return Ok(ControlId::Button(i - 1));
    }
    bail!("unknown control: {key} (expected knob1-5, slider1-4, or button1-5)");
}

fn parse_hex_color(s: &str) -> Result<(u8, u8, u8)> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 || !s.is_ascii() {
        bail!("invalid color: expected 6 hex digits, got \"{s}\"");
    }
    let r = u8::from_str_radix(&s[0..2], 16).context("invalid red component")?;
    let g = u8::from_str_radix(&s[2..4], 16).context("invalid green component")?;
    let b = u8::from_str_radix(&s[4..6], 16).context("invalid blue component")?;
    Ok((r, g, b))
}

fn parse_rgb_section(table: &toml::value::Table) -> Result<RgbMode> {
    let mode = table
        .get("mode")
        .and_then(|v| v.as_str())
        .context("[rgb] missing \"mode\" field")?;

    warn_unknown_keys(table, "rgb", rgb_known_keys(mode));

    match mode {
        "solid" => {
            let color_str = table
                .get("color")
                .and_then(|v| v.as_str())
                .context("[rgb] solid mode requires a \"color\" field")?;
            let (r, g, b) = parse_hex_color(color_str)?;
            Ok(RgbMode::Solid { r, g, b })
        }
        "rainbow" => {
            let style = match table.get("style").and_then(|v| v.as_str()) {
                Some("horizontal") | None => RainbowStyle::Horizontal,
                Some("vertical") => RainbowStyle::Vertical,
                Some(other) => bail!("[rgb] unknown rainbow style: \"{other}\""),
            };
            Ok(RgbMode::Rainbow { style })
        }
        "gradient" => {
            let (color1, color2) = parse_two_colors(table, "gradient")?;
            Ok(RgbMode::Gradient { color1, color2 })
        }
        "volume-gradient" => {
            let (color1, color2) = parse_two_colors(table, "volume-gradient")?;
            Ok(RgbMode::VolumeGradient { color1, color2 })
        }
        "wave" => {
            let hue = parse_u8_required(table, "hue", "wave")?;
            let brightness = parse_u8_with_default(table, "brightness", DEFAULT_BRIGHTNESS)?;
            let speed = parse_u8_with_default(table, "speed", DEFAULT_SPEED)?;
            let reverse = parse_bool_with_default(table, "reverse", false)?;
            let bounce = parse_bool_with_default(table, "bounce", false)?;
            Ok(RgbMode::Wave { hue, brightness, speed, reverse, bounce })
        }
        "breath" => {
            let hue = parse_u8_required(table, "hue", "breath")?;
            let brightness = parse_u8_with_default(table, "brightness", DEFAULT_BRIGHTNESS)?;
            let speed = parse_u8_with_default(table, "speed", DEFAULT_SPEED)?;
            Ok(RgbMode::Breath { hue, brightness, speed })
        }
        _ => bail!("[rgb] unknown mode: \"{mode}\""),
    }
}

pub const DEFAULT_BRIGHTNESS: u8 = 200;
pub const DEFAULT_SPEED: u8 = 64;

fn parse_two_colors(table: &toml::value::Table, mode_name: &str) -> Result<(RgbColor, RgbColor)> {
    let color1_str = table
        .get("color1")
        .and_then(|v| v.as_str())
        .with_context(|| format!("[rgb] {mode_name} mode requires \"color1\" field"))?;
    let color2_str = table
        .get("color2")
        .and_then(|v| v.as_str())
        .with_context(|| format!("[rgb] {mode_name} mode requires \"color2\" field"))?;
    let (r1, g1, b1) = parse_hex_color(color1_str)?;
    let (r2, g2, b2) = parse_hex_color(color2_str)?;
    Ok((
        RgbColor { r: r1, g: g1, b: b1 },
        RgbColor { r: r2, g: g2, b: b2 },
    ))
}

fn parse_u8_required(table: &toml::value::Table, field: &str, mode_name: &str) -> Result<u8> {
    let n = table
        .get(field)
        .and_then(|v| v.as_integer())
        .with_context(|| format!("[rgb] {mode_name} mode requires \"{field}\" field (0-255)"))?;
    range_check_u8(n, field)
}

fn parse_u8_with_default(
    table: &toml::value::Table,
    field: &str,
    default: u8,
) -> Result<u8> {
    match table.get(field) {
        None => Ok(default),
        Some(v) => {
            let n = v
                .as_integer()
                .with_context(|| format!("[rgb] \"{field}\" must be an integer (0-255)"))?;
            range_check_u8(n, field)
        }
    }
}

fn range_check_u8(n: i64, field: &str) -> Result<u8> {
    if !(0..=255).contains(&n) {
        bail!("[rgb] \"{field}\" must be in 0-255, got {n}");
    }
    Ok(n as u8)
}

fn parse_bool_with_default(
    table: &toml::value::Table,
    field: &str,
    default: bool,
) -> Result<bool> {
    match table.get(field) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .with_context(|| format!("[rgb] \"{field}\" must be a boolean")),
    }
}

fn parse_targets(key: &str, table: &toml::value::Table) -> Result<Vec<AppTarget>> {
    let strings = parse_app_strings(key, table)?;
    Ok(strings.into_iter().map(|s| AppTarget::parse(&s)).collect())
}

fn parse_app_strings(key: &str, table: &toml::value::Table) -> Result<Vec<String>> {
    let value = table
        .get("app")
        .with_context(|| format!("[{key}] missing \"app\" field"))?;

    match value {
        toml::Value::String(s) => Ok(vec![s.clone()]),
        toml::Value::Array(arr) => {
            let mut apps = Vec::new();
            for item in arr {
                let s = item
                    .as_str()
                    .with_context(|| format!("[{key}] app array entries must be strings"))?;
                apps.push(s.to_string());
            }
            if apps.is_empty() {
                bail!("[{key}] app list cannot be empty");
            }
            Ok(apps)
        }
        _ => bail!("[{key}] \"app\" must be a string or array of strings"),
    }
}

fn parse_action(key: &str, table: &toml::value::Table) -> Result<Action> {
    let action = table
        .get("action")
        .and_then(|v| v.as_str())
        .with_context(|| format!("[{key}] missing \"action\" field"))?;

    let app_action = |table: &toml::value::Table| -> Result<AppAction> {
        Ok(AppAction {
            targets: parse_targets(key, table)?,
            icon: table.get("icon").and_then(|v| v.as_str()).map(String::from),
        })
    };
    match action {
        "volume" => Ok(Action::Volume(app_action(table)?)),
        "toggle-mute" => Ok(Action::ToggleMute(app_action(table)?)),
        "obs-save-replay" => Ok(Action::ObsSaveReplay),
        "obs-toggle-recording" => Ok(Action::ObsToggleRecording),
        "obs-pause-recording" => Ok(Action::ObsPauseRecording),
        "obs-split-recording" => Ok(Action::ObsSplitRecording),
        _ => bail!("[{key}] unknown action: \"{action}\""),
    }
}

fn parse_obs_section(table: &toml::value::Table) -> Result<ObsConfig> {
    warn_unknown_keys(
        table,
        "obs",
        &["host", "port", "password", "start_replay_buffer", "paused_use_breath", "colors"],
    );
    let host = table
        .get("host")
        .and_then(|v| v.as_str())
        .unwrap_or("localhost")
        .to_string();
    let port = match table.get("port") {
        None => 4455,
        Some(v) => {
            let n = v
                .as_integer()
                .context("[obs] \"port\" must be an integer (1-65535)")?;
            if !(1..=65535).contains(&n) {
                bail!("[obs] \"port\" must be in 1-65535, got {n}");
            }
            n as u16
        }
    };
    // Env var takes precedence over the config file value so secrets can be
    // injected without leaving them in dotfile backups. Falls back to the
    // file value when the env var is unset or empty.
    let password = std::env::var("PCPANEL_OBS_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            table
                .get("password")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        });

    let start_replay_buffer = match table.get("start_replay_buffer") {
        None => false,
        Some(v) => v
            .as_bool()
            .context("[obs] \"start_replay_buffer\" must be a boolean")?,
    };

    let paused_use_breath = match table.get("paused_use_breath") {
        None => false,
        Some(v) => v
            .as_bool()
            .context("[obs] \"paused_use_breath\" must be a boolean")?,
    };

    let colors = match table.get("colors") {
        None => ObsColors::default(),
        Some(toml::Value::Table(t)) => parse_obs_colors(t)?,
        Some(_) => bail!("[obs.colors] must be a table"),
    };

    Ok(ObsConfig { host, port, password, start_replay_buffer, paused_use_breath, colors })
}

fn parse_obs_colors(table: &toml::value::Table) -> Result<ObsColors> {
    // Warn on keys that used to live under [obs.colors] before being moved
    // to [logo] — old configs would otherwise lose those customizations
    // silently since the TOML parser doesn't reject unknown fields.
    for moved_key in ["replay_active", "replay_inactive"] {
        if table.contains_key(moved_key) {
            warn!(
                "[obs.colors] \"{moved_key}\" was moved to [logo] in a recent update — set [logo] indicator = \"replay\" and put \"{moved_key}\" there to keep this customization (the value here is being ignored)"
            );
        }
    }
    // `replay_active`/`replay_inactive` are listed as known here so they
    // don't double-warn — the moved-key message above already covers them.
    warn_unknown_keys(
        table,
        "obs.colors",
        &[
            "recording", "recording_paused", "success_flash", "error_flash",
            "flash_duration_ms", "idle_panel", "replay_active", "replay_inactive",
        ],
    );
    let defaults = ObsColors::default();
    Ok(ObsColors {
        recording: parse_obs_color(table, "recording", defaults.recording)?,
        paused: parse_obs_color(table, "recording_paused", defaults.paused)?,
        success_flash: parse_obs_color(table, "success_flash", defaults.success_flash)?,
        error_flash: parse_obs_color(table, "error_flash", defaults.error_flash)?,
        flash_duration_ms: parse_flash_duration(table, defaults.flash_duration_ms)?,
        idle_panel: parse_obs_color(table, "idle_panel", defaults.idle_panel)?,
    })
}

fn parse_obs_color(
    table: &toml::value::Table,
    field: &str,
    default: RgbColor,
) -> Result<RgbColor> {
    parse_section_color(table, "obs.colors", field, default)
}

/// Read an optional `field = "#RRGGBB"` entry from a TOML table, or return
/// `default` if absent. `section` is the bracketed table name used in error
/// messages (e.g. `"obs.colors"`, `"logo"`).
fn parse_section_color(
    table: &toml::value::Table,
    section: &str,
    field: &str,
    default: RgbColor,
) -> Result<RgbColor> {
    match table.get(field) {
        None => Ok(default),
        Some(v) => {
            let s = v
                .as_str()
                .with_context(|| format!("[{section}] \"{field}\" must be a hex color string"))?;
            let (r, g, b) = parse_hex_color(s)?;
            Ok(RgbColor { r, g, b })
        }
    }
}

fn parse_flash_duration(table: &toml::value::Table, default: u64) -> Result<u64> {
    match table.get("flash_duration_ms") {
        None => Ok(default),
        Some(v) => {
            let n = v
                .as_integer()
                .context("[obs.colors] \"flash_duration_ms\" must be a non-negative integer")?;
            if n < 0 {
                bail!("[obs.colors] \"flash_duration_ms\" must be non-negative, got {n}");
            }
            Ok(n as u64)
        }
    }
}

fn parse_config(content: &str) -> Result<Config> {
    let top: toml::value::Table =
        toml::from_str(content).context("failed to parse config file")?;

    let mut mappings = HashMap::new();
    let mut rgb = None;
    let mut obs = None;
    let mut logo = LogoConfig::default();
    // Track which keys had OBS actions so we can produce a clear error
    // if [obs] is missing — order of TOML iteration isn't guaranteed.
    let mut obs_action_keys: Vec<String> = Vec::new();

    for (key, value) in &top {
        if key == "rgb" {
            let table = value.as_table().context("[rgb] must be a table")?;
            rgb = Some(parse_rgb_section(table)?);
            continue;
        }
        if key == "obs" {
            let table = value.as_table().context("[obs] must be a table")?;
            obs = Some(parse_obs_section(table)?);
            continue;
        }
        if key == "logo" {
            let table = value.as_table().context("[logo] must be a table")?;
            logo = parse_logo_section(table)?;
            continue;
        }

        let table = value
            .as_table()
            .with_context(|| format!("[{key}] expected a table"))?;
        warn_unknown_keys(table, key, &["action", "app", "icon"]);
        let action = parse_action(key, table)?;
        let control = parse_control_id(key)?;

        // Validate: toggle-mute only on buttons
        if matches!(action, Action::ToggleMute(_)) && !control.is_button() {
            bail!("[{key}] toggle-mute can only be assigned to buttons");
        }

        // Validate: volume controls not on buttons
        if matches!(action, Action::Volume(_)) && control.is_button() {
            bail!("[{key}] volume controls cannot be assigned to buttons");
        }

        // Validate: OBS actions only on buttons
        if action.is_obs() && !control.is_button() {
            bail!("[{key}] OBS actions can only be assigned to buttons");
        }

        if action.is_obs() {
            obs_action_keys.push(key.clone());
        }

        mappings.insert(control, action);
    }

    // Validate: any OBS action requires an [obs] section
    if !obs_action_keys.is_empty() && obs.is_none() {
        bail!(
            "OBS actions configured on [{}] require an [obs] section",
            obs_action_keys.join("], [")
        );
    }

    Ok(Config { mappings, rgb, obs, logo })
}

fn parse_logo_section(table: &toml::value::Table) -> Result<LogoConfig> {
    warn_unknown_keys(
        table,
        "logo",
        &["indicator", "mic_muted", "mic_unmuted", "mic_unknown", "replay_active", "replay_inactive"],
    );
    let defaults = LogoConfig::default();
    let indicator = match table.get("indicator") {
        None => LogoIndicator::None,
        Some(v) => {
            let s = v
                .as_str()
                .context("[logo] \"indicator\" must be one of \"none\", \"mic\", \"replay\"")?;
            if s.eq_ignore_ascii_case("none") {
                LogoIndicator::None
            } else if s.eq_ignore_ascii_case("mic") {
                LogoIndicator::Mic
            } else if s.eq_ignore_ascii_case("replay") {
                LogoIndicator::Replay
            } else {
                bail!("[logo] unknown indicator \"{s}\" (expected \"none\", \"mic\", or \"replay\")");
            }
        }
    };
    Ok(LogoConfig {
        indicator,
        mic_muted: parse_logo_color(table, "mic_muted", defaults.mic_muted)?,
        mic_unmuted: parse_logo_color(table, "mic_unmuted", defaults.mic_unmuted)?,
        mic_unknown: parse_logo_color(table, "mic_unknown", defaults.mic_unknown)?,
        replay_active: parse_logo_color(table, "replay_active", defaults.replay_active)?,
        replay_inactive: parse_logo_color(table, "replay_inactive", defaults.replay_inactive)?,
    })
}

fn parse_logo_color(
    table: &toml::value::Table,
    field: &str,
    default: RgbColor,
) -> Result<RgbColor> {
    parse_section_color(table, "logo", field, default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(toml_src: &str) -> toml::value::Table {
        toml::from_str(toml_src).unwrap()
    }

    #[test]
    fn unknown_keys_flags_only_unrecognized() {
        let t = table("action = \"volume\"\napp = \"x\"\nicn = \"firefox\"\n");
        let unknown = unknown_keys(&t, &["action", "app", "icon"]);
        assert_eq!(unknown, vec!["icn".to_string()]);
    }

    #[test]
    fn unknown_keys_empty_when_all_recognized() {
        let t = table("action = \"volume\"\napp = \"x\"\nicon = \"firefox\"\n");
        assert!(unknown_keys(&t, &["action", "app", "icon"]).is_empty());
    }

    #[test]
    fn rgb_known_keys_track_mode() {
        assert_eq!(rgb_known_keys("solid"), &["mode", "color"]);
        assert_eq!(rgb_known_keys("gradient"), rgb_known_keys("volume-gradient"));
        // Unknown mode → only "mode" is recognized (the mode itself bails later).
        assert_eq!(rgb_known_keys("bogus"), &["mode"]);
    }

    #[test]
    fn unknown_key_warns_but_does_not_fail_parse() {
        // A typo'd optional key must still parse (warn, not error) so a
        // config written for a newer pcp_rust doesn't hard-fail here.
        let config = parse_config(
            r#"
            [rgb]
            mode = "wave"
            hue = 100
            brigthness = 150
            "#,
        )
        .expect("typo'd optional key should warn, not fail");
        match config.rgb {
            Some(RgbMode::Wave { hue, brightness, .. }) => {
                assert_eq!(hue, 100);
                // The misspelled key was ignored, so brightness is the default.
                assert_eq!(brightness, DEFAULT_BRIGHTNESS);
            }
            other => panic!("expected Wave, got {other:?}"),
        }
    }

    #[test]
    fn test_valid_config() {
        let config = parse_config(
            r##"
            [slider1]
            action = "volume"
            app = "system"

            [slider2]
            action = "volume"
            app = "firefox"

            [button1]
            action = "toggle-mute"
            app = "system"

            [button3]
            action = "toggle-mute"
            app = "firefox"

            [rgb]
            mode = "solid"
            color = "#E0FFFF"
            "##,
        )
        .unwrap();

        assert_eq!(config.mappings.len(), 4);
        assert!(config.mappings.contains_key(&ControlId::Slider(0)));
        assert!(config.mappings.contains_key(&ControlId::Button(0)));
        match config.rgb {
            Some(RgbMode::Solid { r, g, b }) => {
                assert_eq!((r, g, b), (0xE0, 0xFF, 0xFF));
            }
            other => panic!("expected Solid, got {other:?}"),
        }
    }

    #[test]
    fn test_multi_app() {
        let config = parse_config(
            r#"
            [slider1]
            action = "volume"
            app = ["firefox", "Dota 2"]
            "#,
        )
        .unwrap();

        match config.mappings.get(&ControlId::Slider(0)) {
            Some(Action::Volume(action)) => {
                let labels: Vec<&str> = action.targets.iter().map(|t| t.label()).collect();
                assert_eq!(labels, vec!["firefox", "Dota 2"]);
            }
            other => panic!("expected Volume, got {other:?}"),
        }
    }

    #[test]
    fn test_toggle_mute_on_slider_rejected() {
        let result = parse_config(
            r#"
            [slider1]
            action = "toggle-mute"
            app = "system"
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("toggle-mute"));
    }

    #[test]
    fn test_volume_on_button_rejected() {
        let result = parse_config(
            r#"
            [button1]
            action = "volume"
            app = "system"
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("volume"));
    }

    #[test]
    fn test_gradient() {
        let config = parse_config(
            r##"
            [rgb]
            mode = "gradient"
            color1 = "#FF0000"
            color2 = "#0000FF"
            "##,
        )
        .unwrap();
        match config.rgb {
            Some(RgbMode::Gradient { color1, color2 }) => {
                assert_eq!((color1.r, color1.g, color1.b), (0xFF, 0x00, 0x00));
                assert_eq!((color2.r, color2.g, color2.b), (0x00, 0x00, 0xFF));
            }
            other => panic!("expected Gradient, got {other:?}"),
        }
    }

    #[test]
    fn test_volume_gradient() {
        let config = parse_config(
            r##"
            [rgb]
            mode = "volume-gradient"
            color1 = "#00FF00"
            color2 = "#FF0000"
            "##,
        )
        .unwrap();
        match config.rgb {
            Some(RgbMode::VolumeGradient { color1, color2 }) => {
                assert_eq!((color1.r, color1.g, color1.b), (0x00, 0xFF, 0x00));
                assert_eq!((color2.r, color2.g, color2.b), (0xFF, 0x00, 0x00));
            }
            other => panic!("expected VolumeGradient, got {other:?}"),
        }
    }

    #[test]
    fn test_wave_full() {
        let config = parse_config(
            r#"
            [rgb]
            mode = "wave"
            hue = 200
            brightness = 150
            speed = 32
            reverse = true
            bounce = true
            "#,
        )
        .unwrap();
        match config.rgb {
            Some(RgbMode::Wave { hue, brightness, speed, reverse, bounce }) => {
                assert_eq!(hue, 200);
                assert_eq!(brightness, 150);
                assert_eq!(speed, 32);
                assert!(reverse);
                assert!(bounce);
            }
            other => panic!("expected Wave, got {other:?}"),
        }
    }

    #[test]
    fn test_wave_defaults() {
        let config = parse_config(
            r#"
            [rgb]
            mode = "wave"
            hue = 100
            "#,
        )
        .unwrap();
        match config.rgb {
            Some(RgbMode::Wave { hue, brightness, speed, reverse, bounce }) => {
                assert_eq!(hue, 100);
                assert_eq!(brightness, DEFAULT_BRIGHTNESS);
                assert_eq!(speed, DEFAULT_SPEED);
                assert!(!reverse);
                assert!(!bounce);
            }
            other => panic!("expected Wave, got {other:?}"),
        }
    }

    #[test]
    fn test_breath() {
        let config = parse_config(
            r#"
            [rgb]
            mode = "breath"
            hue = 50
            "#,
        )
        .unwrap();
        match config.rgb {
            Some(RgbMode::Breath { hue, brightness, speed }) => {
                assert_eq!(hue, 50);
                assert_eq!(brightness, DEFAULT_BRIGHTNESS);
                assert_eq!(speed, DEFAULT_SPEED);
            }
            other => panic!("expected Breath, got {other:?}"),
        }
    }

    #[test]
    fn test_hue_out_of_range() {
        let result = parse_config(
            r#"
            [rgb]
            mode = "wave"
            hue = 300
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("0-255"));
    }

    #[test]
    fn test_wave_missing_hue() {
        let result = parse_config(
            r#"
            [rgb]
            mode = "wave"
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("hue"));
    }

    #[test]
    fn test_obs_defaults() {
        let config = parse_config(
            r#"
            [obs]
            "#,
        )
        .unwrap();
        let obs = config.obs.unwrap();
        assert_eq!(obs.host, "localhost");
        assert_eq!(obs.port, 4455);
        assert!(obs.password.is_none());
        let defaults = ObsColors::default();
        assert_eq!(obs.colors.recording.r, defaults.recording.r);
        assert_eq!(obs.colors.flash_duration_ms, defaults.flash_duration_ms);
    }

    #[test]
    fn test_obs_full() {
        let config = parse_config(
            r##"
            [obs]
            host = "192.168.1.10"
            port = 4456
            password = "secret"

            [obs.colors]
            recording = "#AA0000"
            recording_paused = "#FFAA00"
            success_flash = "#00AA00"
            error_flash = "#AA00AA"
            flash_duration_ms = 250
            "##,
        )
        .unwrap();
        let obs = config.obs.unwrap();
        assert_eq!(obs.host, "192.168.1.10");
        assert_eq!(obs.port, 4456);
        assert_eq!(obs.password.as_deref(), Some("secret"));
        assert_eq!(obs.colors.recording.r, 0xAA);
        assert_eq!(obs.colors.paused.g, 0xAA);
        assert_eq!(obs.colors.flash_duration_ms, 250);
    }

    #[test]
    fn test_obs_action_on_knob_rejected() {
        let result = parse_config(
            r#"
            [obs]

            [knob1]
            action = "obs-save-replay"
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("buttons"));
    }

    #[test]
    fn test_all_obs_actions_on_knob_rejected() {
        for action in [
            "obs-save-replay",
            "obs-toggle-recording",
            "obs-pause-recording",
            "obs-split-recording",
        ] {
            let cfg = format!(
                r#"
                [obs]

                [knob1]
                action = "{action}"
                "#
            );
            let result = parse_config(&cfg);
            assert!(result.is_err(), "{action} on knob should be rejected");
            assert!(
                result.unwrap_err().to_string().contains("buttons"),
                "{action} on knob error should mention buttons"
            );
        }
    }

    #[test]
    fn test_all_obs_actions_on_slider_rejected() {
        for action in [
            "obs-save-replay",
            "obs-toggle-recording",
            "obs-pause-recording",
            "obs-split-recording",
        ] {
            let cfg = format!(
                r#"
                [obs]

                [slider1]
                action = "{action}"
                "#
            );
            let result = parse_config(&cfg);
            assert!(result.is_err(), "{action} on slider should be rejected");
            assert!(
                result.unwrap_err().to_string().contains("buttons"),
                "{action} on slider error should mention buttons"
            );
        }
    }

    #[test]
    fn test_obs_action_without_obs_section_rejected() {
        let result = parse_config(
            r#"
            [button1]
            action = "obs-save-replay"
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("[obs]"));
    }

    #[test]
    fn test_obs_actions_on_buttons_ok() {
        let config = parse_config(
            r#"
            [obs]

            [button1]
            action = "obs-save-replay"

            [button2]
            action = "obs-toggle-recording"

            [button3]
            action = "obs-pause-recording"

            [button4]
            action = "obs-split-recording"
            "#,
        )
        .unwrap();
        assert_eq!(config.mappings.len(), 4);
        assert!(matches!(
            config.mappings.get(&ControlId::Button(0)),
            Some(Action::ObsSaveReplay)
        ));
        assert!(matches!(
            config.mappings.get(&ControlId::Button(1)),
            Some(Action::ObsToggleRecording)
        ));
    }

    #[test]
    fn test_obs_start_replay_buffer() {
        let config = parse_config(
            r#"
            [obs]
            start_replay_buffer = true
            "#,
        )
        .unwrap();
        assert!(config.obs.unwrap().start_replay_buffer);

        // Default is false when the field is absent.
        let config = parse_config(
            r#"
            [obs]
            "#,
        )
        .unwrap();
        assert!(!config.obs.unwrap().start_replay_buffer);
    }

    #[test]
    fn test_obs_paused_use_breath() {
        let config = parse_config(
            r#"
            [obs]
            paused_use_breath = true
            "#,
        )
        .unwrap();
        assert!(config.obs.unwrap().paused_use_breath);

        let config = parse_config(
            r#"
            [obs]
            "#,
        )
        .unwrap();
        assert!(!config.obs.unwrap().paused_use_breath);
    }

    #[test]
    fn test_obs_start_replay_buffer_must_be_bool() {
        let result = parse_config(
            r#"
            [obs]
            start_replay_buffer = "yes"
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("boolean"));
    }

    #[test]
    fn test_obs_idle_panel_color() {
        let config = parse_config(
            r##"
            [obs]

            [obs.colors]
            idle_panel = "#101020"
            "##,
        )
        .unwrap();
        let colors = config.obs.unwrap().colors;
        assert_eq!(colors.idle_panel, RgbColor { r: 0x10, g: 0x10, b: 0x20 });
    }

    #[test]
    fn test_logo_indicator_mic() {
        let config = parse_config(
            r##"
            [logo]
            indicator = "mic"
            mic_muted = "#FF2030"
            "##,
        )
        .unwrap();
        assert_eq!(config.logo.indicator, LogoIndicator::Mic);
        assert_eq!(config.logo.mic_muted, RgbColor { r: 0xFF, g: 0x20, b: 0x30 });
        // Defaults applied to unset colors.
        let defaults = LogoConfig::default();
        assert_eq!(config.logo.mic_unmuted, defaults.mic_unmuted);
    }

    #[test]
    fn test_logo_indicator_replay() {
        let config = parse_config(
            r##"
            [logo]
            indicator = "replay"
            replay_active = "#10FF10"
            replay_inactive = "#330000"
            "##,
        )
        .unwrap();
        assert_eq!(config.logo.indicator, LogoIndicator::Replay);
        assert_eq!(config.logo.replay_active, RgbColor { r: 0x10, g: 0xFF, b: 0x10 });
        assert_eq!(config.logo.replay_inactive, RgbColor { r: 0x33, g: 0x00, b: 0x00 });
    }

    #[test]
    fn test_logo_section_absent_defaults_to_none() {
        let config = parse_config("").unwrap();
        assert_eq!(config.logo.indicator, LogoIndicator::None);
    }

    #[test]
    fn test_logo_indicator_case_insensitive() {
        for variant in ["MIC", "Mic", "mIc"] {
            let config = parse_config(&format!(
                r#"
                [logo]
                indicator = "{variant}"
                "#
            ))
            .unwrap();
            assert_eq!(config.logo.indicator, LogoIndicator::Mic, "for {variant}");
        }
    }

    #[test]
    fn test_logo_indicator_unknown_rejected() {
        let result = parse_config(
            r#"
            [logo]
            indicator = "foo"
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("foo"));
    }

    #[test]
    fn test_logo_mic_muted_must_be_string() {
        let result = parse_config(
            r#"
            [logo]
            mic_muted = 42
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("hex color"));
    }

    #[test]
    fn test_logo_mic_muted_bad_hex() {
        let result = parse_config(
            r##"
            [logo]
            mic_muted = "#GGGGGG"
            "##,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_obs_port_out_of_range() {
        let result = parse_config(
            r#"
            [obs]
            port = 99999
            "#,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("port"));
    }
}
