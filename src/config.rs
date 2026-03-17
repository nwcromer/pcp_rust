use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

const SYSTEM_APP: &str = "system";
const MIC_APP: &str = "mic";

#[derive(Debug)]
pub enum Action {
    Volume { apps: Vec<String> },
    ToggleMute { apps: Vec<String> },
}

impl Action {
    pub fn is_system(app: &str) -> bool {
        app.eq_ignore_ascii_case(SYSTEM_APP)
    }

    pub fn is_mic(app: &str) -> bool {
        app.eq_ignore_ascii_case(MIC_APP)
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

#[derive(Debug, Clone, Copy)]
pub enum RgbMode {
    Solid { r: u8, g: u8, b: u8 },
    Rainbow { style: RainbowStyle },
}

#[derive(Debug)]
pub struct Config {
    pub mappings: HashMap<ControlId, Action>,
    pub rgb: Option<RgbMode>,
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("pcpanel").join("config.toml"))
}

pub fn load_config(path: &Path) -> Result<Config> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    parse_config(&content)
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
    if s.len() != 6 {
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
        _ => bail!("[rgb] unknown mode: \"{mode}\""),
    }
}

fn parse_apps(key: &str, table: &toml::value::Table) -> Result<Vec<String>> {
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

    let apps = parse_apps(key, table)?;

    match action {
        "volume" => Ok(Action::Volume { apps }),
        "toggle-mute" => Ok(Action::ToggleMute { apps }),
        _ => bail!("[{key}] unknown action: \"{action}\""),
    }
}

fn parse_config(content: &str) -> Result<Config> {
    let top: toml::value::Table =
        toml::from_str(content).context("failed to parse config file")?;

    let mut mappings = HashMap::new();
    let mut rgb = None;

    for (key, value) in &top {
        if key == "rgb" {
            let table = value.as_table().context("[rgb] must be a table")?;
            rgb = Some(parse_rgb_section(table)?);
            continue;
        }

        let table = value
            .as_table()
            .with_context(|| format!("[{key}] expected a table"))?;
        let action = parse_action(key, table)?;
        let control = parse_control_id(key)?;

        // Validate: toggle-mute only on buttons
        if matches!(action, Action::ToggleMute { .. }) && !control.is_button() {
            bail!("[{key}] toggle-mute can only be assigned to buttons");
        }

        // Validate: volume controls not on buttons
        if matches!(action, Action::Volume { .. }) && control.is_button() {
            bail!("[{key}] volume controls cannot be assigned to buttons");
        }

        mappings.insert(control, action);
    }

    Ok(Config { mappings, rgb })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Some(Action::Volume { apps }) => {
                assert_eq!(apps, &["firefox", "Dota 2"]);
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
}
