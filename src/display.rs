//! Current monitor resolution detection for the OBS canvas-match feature.
//!
//! Shells out to `kscreen-doctor --json` (the right tool for the current
//! display mode on Wayland/KDE) and returns the *physical* pixel resolution
//! of a chosen display's current mode.
//!
//! We deliberately read the current mode's `size`, not the output geometry:
//! with KDE fractional scaling the geometry is the scaled logical size (e.g.
//! 2560x1440 at 1.5x), whereas OBS captures physical pixels (3840x2160). The
//! mode size is the physical resolution the OBS canvas must match.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::process::Command;

/// Hard cap on how long we wait for `kscreen-doctor`. It normally returns in
/// tens of ms; if it ever wedges we must not hang the OBS thread (which would
/// freeze event handling and the reconnect loop with no recovery), so we kill
/// it and fail the record-start instead.
const KSCREEN_TIMEOUT: Duration = Duration::from_secs(3);

/// The slice of `kscreen-doctor --json` we care about. Unlisted fields are
/// ignored by serde.
#[derive(Debug, Deserialize)]
struct KscreenConfig {
    outputs: Vec<Output>,
}

#[derive(Debug, Deserialize)]
struct Output {
    name: String,
    enabled: bool,
    connected: bool,
    /// 1 is the primary display. Optional so an older kscreen that omits it
    /// doesn't fail the whole parse — it just can't be used to disambiguate
    /// multiple displays (we then require an explicit `capture_display`).
    #[serde(default)]
    priority: Option<i64>,
    #[serde(rename = "currentModeId")]
    current_mode_id: String,
    modes: Vec<Mode>,
}

#[derive(Debug, Deserialize)]
struct Mode {
    id: String,
    size: Size,
}

#[derive(Debug, Deserialize)]
struct Size {
    width: u32,
    height: u32,
}

/// Resolve the current physical resolution of the display to match. `target`
/// is the configured `capture_display` connector name (e.g. "DP-1"); `None`
/// selects the primary display, or the sole enabled+connected one.
pub async fn current_display_resolution(target: Option<&str>) -> Result<(u32, u32)> {
    let json = run_kscreen().await?;
    let config = parse_kscreen(&json)?;
    let output = select_output(&config.outputs, target)?;
    resolution_of(output)
}

/// Run `kscreen-doctor --json` with a timeout, returning its stdout. A hung
/// subprocess is killed (`kill_on_drop`) rather than left to freeze the OBS
/// thread.
async fn run_kscreen() -> Result<String> {
    let output = tokio::time::timeout(
        KSCREEN_TIMEOUT,
        Command::new("kscreen-doctor")
            .arg("--json")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("kscreen-doctor timed out")?
    .context("failed to run kscreen-doctor (is it installed and on PATH?)")?;

    if !output.status.success() {
        bail!(
            "kscreen-doctor exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("kscreen-doctor output was not valid UTF-8")
}

/// Parse the first JSON value out of kscreen-doctor's output. We use a
/// streaming deserializer rather than `from_str` so a trailing newline — or
/// the human-readable dump that `-o --json` appends after the object (we run
/// `--json` alone, but stay robust) — doesn't trip a strict whole-string
/// parse that rejects trailing bytes.
fn parse_kscreen(json: &str) -> Result<KscreenConfig> {
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<KscreenConfig>();
    match stream.next() {
        Some(Ok(config)) => Ok(config),
        Some(Err(e)) => Err(e).context("failed to parse kscreen-doctor JSON"),
        None => bail!("kscreen-doctor produced no JSON output"),
    }
}

/// Pick the output whose resolution we'll match. With a `target` name, the
/// match must be an enabled+connected display or we error (fail-closed — we
/// never silently fall back to the wrong screen). Without one, a single
/// usable display is unambiguous; otherwise we require a primary.
fn select_output<'a>(outputs: &'a [Output], target: Option<&str>) -> Result<&'a Output> {
    let is_usable = |o: &&Output| o.enabled && o.connected;

    if let Some(name) = target {
        return outputs
            .iter()
            .filter(is_usable)
            .find(|o| o.name.eq_ignore_ascii_case(name))
            .with_context(|| {
                format!(
                    "capture_display \"{name}\" not found among enabled, connected displays"
                )
            });
    }

    let mut usable = outputs.iter().filter(is_usable);
    let first = usable
        .next()
        .context("no enabled, connected display found")?;
    if usable.next().is_none() {
        // Exactly one usable display — unambiguous, no primary needed.
        return Ok(first);
    }

    // Multiple usable displays — require a primary (priority == 1) to choose.
    outputs
        .iter()
        .filter(is_usable)
        .find(|o| o.priority == Some(1))
        .context(
            "multiple displays connected and no primary found; \
             set [obs] capture_display to choose one",
        )
}

/// The physical pixel size of the output's *current* mode.
fn resolution_of(output: &Output) -> Result<(u32, u32)> {
    let mode = output
        .modes
        .iter()
        .find(|m| m.id == output.current_mode_id)
        .with_context(|| {
            format!(
                "display \"{}\" current mode {} not found in its mode list",
                output.name, output.current_mode_id
            )
        })?;
    Ok((mode.size.width, mode.size.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-output config: DP-1 (primary, 4K current) and HDMI-A-1
    /// (secondary, 1080p current). Trimmed to the fields we read.
    const TWO_OUTPUTS: &str = r#"
    {
      "outputs": [
        {
          "name": "DP-1",
          "enabled": true,
          "connected": true,
          "priority": 1,
          "currentModeId": "2",
          "modes": [
            { "id": "1", "size": { "width": 1920, "height": 1080 } },
            { "id": "2", "size": { "width": 3840, "height": 2160 } }
          ]
        },
        {
          "name": "HDMI-A-1",
          "enabled": true,
          "connected": true,
          "priority": 2,
          "currentModeId": "9",
          "modes": [
            { "id": "9", "size": { "width": 1920, "height": 1080 } }
          ]
        }
      ]
    }
    "#;

    #[test]
    fn selects_named_display_case_insensitively() {
        let cfg = parse_kscreen(TWO_OUTPUTS).unwrap();
        let out = select_output(&cfg.outputs, Some("dp-1")).unwrap();
        assert_eq!(resolution_of(out).unwrap(), (3840, 2160));

        let out = select_output(&cfg.outputs, Some("HDMI-A-1")).unwrap();
        assert_eq!(resolution_of(out).unwrap(), (1920, 1080));
    }

    #[test]
    fn unknown_named_display_errors() {
        let cfg = parse_kscreen(TWO_OUTPUTS).unwrap();
        let err = select_output(&cfg.outputs, Some("DP-9")).unwrap_err();
        assert!(err.to_string().contains("DP-9"));
    }

    #[test]
    fn no_target_picks_primary_when_multiple() {
        let cfg = parse_kscreen(TWO_OUTPUTS).unwrap();
        let out = select_output(&cfg.outputs, None).unwrap();
        assert_eq!(out.name, "DP-1");
        assert_eq!(resolution_of(out).unwrap(), (3840, 2160));
    }

    #[test]
    fn no_target_uses_sole_usable_display_without_primary() {
        // Single connected display with no priority field at all — the
        // unambiguous case must not require a primary.
        let json = r#"
        {
          "outputs": [
            {
              "name": "DP-1",
              "enabled": true,
              "connected": true,
              "currentModeId": "2",
              "modes": [
                { "id": "2", "size": { "width": 2560, "height": 1440 } }
              ]
            },
            {
              "name": "HDMI-A-1",
              "enabled": false,
              "connected": false,
              "currentModeId": "1",
              "modes": [ { "id": "1", "size": { "width": 1920, "height": 1080 } } ]
            }
          ]
        }
        "#;
        let cfg = parse_kscreen(json).unwrap();
        let out = select_output(&cfg.outputs, None).unwrap();
        assert_eq!(out.name, "DP-1");
        assert_eq!(resolution_of(out).unwrap(), (2560, 1440));
    }

    #[test]
    fn no_target_multiple_without_primary_errors() {
        let json = r#"
        {
          "outputs": [
            { "name": "DP-1", "enabled": true, "connected": true,
              "currentModeId": "1", "modes": [ { "id": "1", "size": { "width": 3840, "height": 2160 } } ] },
            { "name": "DP-2", "enabled": true, "connected": true,
              "currentModeId": "1", "modes": [ { "id": "1", "size": { "width": 1920, "height": 1080 } } ] }
          ]
        }
        "#;
        let cfg = parse_kscreen(json).unwrap();
        let err = select_output(&cfg.outputs, None).unwrap_err();
        assert!(err.to_string().contains("capture_display"));
    }

    #[test]
    fn parse_tolerates_trailing_bytes() {
        // `-o --json` appends a human-readable dump after the JSON object.
        // We run `--json` alone, but the parser must not choke if trailing
        // bytes ever appear.
        let with_trailer = format!("{TWO_OUTPUTS}\nOutput: 1 DP-1 some-uuid\n\tenabled\n");
        let cfg = parse_kscreen(&with_trailer).unwrap();
        assert_eq!(cfg.outputs.len(), 2);
    }

    #[test]
    fn missing_current_mode_errors() {
        let json = r#"
        {
          "outputs": [
            { "name": "DP-1", "enabled": true, "connected": true, "priority": 1,
              "currentModeId": "99",
              "modes": [ { "id": "1", "size": { "width": 3840, "height": 2160 } } ] }
          ]
        }
        "#;
        let cfg = parse_kscreen(json).unwrap();
        let out = select_output(&cfg.outputs, None).unwrap();
        let err = resolution_of(out).unwrap_err();
        assert!(err.to_string().contains("current mode 99"));
    }

    #[test]
    fn empty_json_errors() {
        assert!(parse_kscreen("").unwrap_err().to_string().contains("no JSON"));
    }
}
