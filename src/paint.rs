//! LED writes derived from runtime state. Every code path that touches a
//! panel/logo region goes through here; `paint_leds` is the single
//! entry point the main loop calls on every dirty repaint.

use anyhow::Result;
use log::info;

use crate::config::{self, RainbowStyle, RgbColor, RgbMode};
use crate::device::PcPanelPro;
use crate::led::{self, LedMode, Rgb};
use crate::runtime::{ObsRuntime, ObsState};

/// Log a human-readable description of the configured RGB mode. Called
/// once at startup — kept separate from `apply_rgb` because apply_rgb runs
/// on every disconnected-idle repaint and would otherwise spam the log on
/// every mic-mute toggle.
pub fn log_rgb_mode(mode: RgbMode) {
    match mode {
        RgbMode::Solid { r, g, b } => {
            info!("RGB mode: solid (#{:02X}{:02X}{:02X})", r, g, b);
        }
        RgbMode::Rainbow { style } => {
            let name = match style {
                RainbowStyle::Horizontal => "horizontal",
                RainbowStyle::Vertical => "vertical",
            };
            info!("RGB mode: rainbow ({name})");
        }
        RgbMode::Gradient { color1, color2 } => info!(
            "RGB mode: gradient (#{:02X}{:02X}{:02X} -> #{:02X}{:02X}{:02X})",
            color1.r, color1.g, color1.b, color2.r, color2.g, color2.b
        ),
        RgbMode::VolumeGradient { color1, color2 } => info!(
            "RGB mode: volume-gradient (#{:02X}{:02X}{:02X} -> #{:02X}{:02X}{:02X})",
            color1.r, color1.g, color1.b, color2.r, color2.g, color2.b
        ),
        RgbMode::Wave { hue, .. } => info!("RGB mode: wave (hue={hue})"),
        RgbMode::Breath { hue, .. } => info!("RGB mode: breath (hue={hue})"),
    }
}

/// Apply an `[rgb]` mode to the panel. If `logo_override` is `Some`, that
/// color is used for the logo instead of the mode's natural logo color —
/// callers that intend to overlay an indicator should pass it here so the
/// logo is written exactly once with the right color (rather than written
/// twice with two separate calls).
///
/// `logo_override` is ignored for global animation modes (rainbow / wave /
/// breath) because they drive every LED in one packet — there is no
/// separate logo write to redirect.
pub fn apply_rgb(panel: &PcPanelPro, mode: RgbMode, logo_override: Option<RgbColor>) -> Result<()> {
    match mode {
        RgbMode::Solid { r, g, b } => {
            let panel_color = RgbColor { r, g, b };
            let logo_color = logo_override.unwrap_or(panel_color);
            paint_panel_solid(panel, panel_color)?;
            paint_logo_solid(panel, logo_color)?;
        }
        RgbMode::Rainbow { style } => {
            let rainbow_type = match style {
                RainbowStyle::Horizontal => led::ANIM_RAINBOW_HORIZONTAL,
                RainbowStyle::Vertical => led::ANIM_RAINBOW_VERTICAL,
            };
            led::set_rainbow(panel, rainbow_type, config::DEFAULT_BRIGHTNESS, config::DEFAULT_SPEED)?;
        }
        RgbMode::Gradient { color1, color2 } => {
            let c1 = Rgb::new(color1.r, color1.g, color1.b);
            let c2 = Rgb::new(color2.r, color2.g, color2.b);
            let led = LedMode::Gradient(c1, c2);
            led::set_knob_colors(panel, &[led; 5])?;
            led::set_slider_colors(panel, &[led; 4])?;
            led::set_slider_label_colors(panel, &[led; 4])?;
            paint_logo_solid(panel, logo_override.unwrap_or(color1))?;
        }
        RgbMode::VolumeGradient { color1, color2 } => {
            let c1 = Rgb::new(color1.r, color1.g, color1.b);
            let c2 = Rgb::new(color2.r, color2.g, color2.b);
            let static_mode = LedMode::Static(c1);
            led::set_knob_colors(panel, &[static_mode; 5])?;
            led::set_slider_colors(panel, &[LedMode::VolumeGradient(c1, c2); 4])?;
            led::set_slider_label_colors(panel, &[static_mode; 4])?;
            paint_logo_solid(panel, logo_override.unwrap_or(color1))?;
        }
        RgbMode::Wave { hue, brightness, speed, reverse, bounce } => {
            led::set_wave(panel, hue, brightness, speed, reverse, bounce)?;
        }
        RgbMode::Breath { hue, brightness, speed } => {
            led::set_breath(panel, hue, brightness, speed)?;
        }
    }
    Ok(())
}

/// Repaint the LEDs based on the current OBS state and any active flash.
///
/// Priority on the logo:
///   1. Flash color (when a flash is active — command feedback wins over
///      the steady-state indicator for the brief flash duration).
///   2. Configured `[logo]` indicator (`LogoIndicator::Mic` or `Replay`),
///      when the active mode leaves the logo independently writable.
///   3. Panel color (so an unconfigured logo blends in).
///
/// The panel (knobs/sliders/labels) follows OBS state: `idle_panel` when
/// connected-idle, `recording` while recording, `paused` while paused, the
/// `[rgb]` mode when OBS is disconnected.
///
/// Indicators do not apply during global animation modes (rainbow, wave,
/// breath, and the paused breath effect from `paused_use_breath = true`),
/// which drive every LED in lockstep and don't expose the logo separately.
pub fn paint_leds(panel: &PcPanelPro, obs: &ObsRuntime, idle_rgb: Option<RgbMode>) -> Result<()> {
    let colors = obs.colors();
    if let Some(f) = obs.flash() {
        // Flash takes the whole panel including the logo — command feedback
        // (especially success on SaveReplay / SplitRecording, whose only ack
        // is the flash) wins over the steady-state indicator for the brief
        // flash duration. The indicator resumes as soon as the flash expires.
        let flash_color = f.current_color();
        paint_panel_solid(panel, flash_color)?;
        paint_logo_solid(panel, flash_color)?;
        return Ok(());
    }
    match obs.state() {
        ObsState::Idle if obs.connected() => {
            paint_panel_solid(panel, colors.idle_panel)?;
            paint_logo_with_indicator(panel, obs, colors.idle_panel)?;
        }
        ObsState::Idle => {
            // Only pass the indicator color through when the active mode
            // leaves the logo independently writable. Global animations
            // (rainbow / wave / breath) drive the logo as part of one
            // packet and would fight any override.
            let logo_override = obs
                .logo_indicator_color()
                .filter(|_| logo_is_independent(idle_rgb));
            match idle_rgb {
                Some(mode) => apply_rgb(panel, mode, logo_override)?,
                None => {
                    let off = RgbColor { r: 0, g: 0, b: 0 };
                    paint_panel_solid(panel, off)?;
                    paint_logo_solid(panel, logo_override.unwrap_or(off))?;
                }
            }
        }
        ObsState::Recording => {
            paint_panel_solid(panel, colors.recording)?;
            paint_logo_with_indicator(panel, obs, colors.recording)?;
        }
        ObsState::RecordingPaused => {
            if obs.paused_use_breath() {
                // Global breath animation — drives every LED including the
                // logo, so the replay-buffer indicator (and mic-mute
                // override) is unavailable during paused.
                let hue = led::rgb_to_hue(Rgb::new(
                    colors.paused.r,
                    colors.paused.g,
                    colors.paused.b,
                ));
                led::set_breath(panel, hue, config::DEFAULT_BRIGHTNESS, config::DEFAULT_SPEED)?;
            } else {
                paint_panel_solid(panel, colors.paused)?;
                paint_logo_with_indicator(panel, obs, colors.paused)?;
            }
        }
    }
    Ok(())
}

/// Whether the active `[rgb]` mode writes the logo separately. Static modes
/// (solid, gradient, volume-gradient) do; the global animation modes don't
/// — they drive every LED in lockstep.
fn logo_is_independent(idle_rgb: Option<RgbMode>) -> bool {
    matches!(
        idle_rgb,
        None | Some(
            RgbMode::Solid { .. } | RgbMode::Gradient { .. } | RgbMode::VolumeGradient { .. }
        )
    )
}

/// Whether the configured logo indicator is actually rendered on the logo
/// in the current runtime state — i.e. whether a repaint right now would
/// show it. Mirrors the logo routing in `paint_leds`.
///
/// The main loop uses this to decide whether to force continuous repaints
/// for a blinking (stale-mic) indicator. When the indicator can't be shown
/// — a flash owns the whole panel, or a global animation (rainbow / wave /
/// breath, including the paused-breath effect) owns the logo — forcing a
/// repaint every iteration would only restart the animation from phase 0
/// without ever displaying the blink, making the animation visibly stutter
/// for the duration of the outage.
pub fn logo_indicator_visible(obs: &ObsRuntime, idle_rgb: Option<RgbMode>) -> bool {
    if obs.flash().is_some() {
        // A flash takes the whole panel including the logo.
        return false;
    }
    match obs.state() {
        ObsState::Idle if obs.connected() => true,
        ObsState::Idle => logo_is_independent(idle_rgb),
        ObsState::Recording => true,
        ObsState::RecordingPaused => !obs.paused_use_breath(),
    }
}

/// Paint the logo with the selected indicator's color, falling back to the
/// panel `fallback` color when no indicator is configured (or the selected
/// one has nothing meaningful to show — e.g. replay with OBS disconnected).
/// The fallback is the panel color so an unconfigured logo blends in.
fn paint_logo_with_indicator(
    panel: &PcPanelPro,
    obs: &ObsRuntime,
    fallback: RgbColor,
) -> Result<()> {
    let color = obs.logo_indicator_color().unwrap_or(fallback);
    paint_logo_solid(panel, color)
}

/// Paint knobs/sliders/labels to one solid color. Leaves the logo alone.
///
/// The slider strips have no firmware mode that lights all LEDs uniformly.
/// Two options exist and we picked the second:
///   - `Static(c)` lights every LED but applies a bottom-bright/top-dim
///     brightness ramp — washed-out appearance.
///   - `Gradient(c, c)` renders the strip as a level meter: only LEDs
///     below the physical slider position are lit, with uniform color.
///
/// Gradient looks more deliberate (it reads as "your slider is at X")
/// than Static's brightness ramp, so we use it.
fn paint_panel_solid(panel: &PcPanelPro, c: RgbColor) -> Result<()> {
    let rgb = Rgb::new(c.r, c.g, c.b);
    let static_mode = LedMode::Static(rgb);
    let uniform_slider = LedMode::Gradient(rgb, rgb);
    led::set_knob_colors(panel, &[static_mode; 5])?;
    led::set_slider_colors(panel, &[uniform_slider; 4])?;
    led::set_slider_label_colors(panel, &[static_mode; 4])
}

fn paint_logo_solid(panel: &PcPanelPro, c: RgbColor) -> Result<()> {
    led::set_logo(panel, Rgb::new(c.r, c.g, c.b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_is_independent_classifies_modes() {
        assert!(logo_is_independent(None));
        assert!(logo_is_independent(Some(RgbMode::Solid { r: 0, g: 0, b: 0 })));
        let gc = RgbColor { r: 0, g: 0, b: 0 };
        assert!(logo_is_independent(Some(RgbMode::Gradient { color1: gc, color2: gc })));
        assert!(logo_is_independent(Some(RgbMode::VolumeGradient { color1: gc, color2: gc })));
        // Global animations own the logo too.
        assert!(!logo_is_independent(Some(RgbMode::Rainbow {
            style: RainbowStyle::Horizontal
        })));
        assert!(!logo_is_independent(Some(RgbMode::Wave {
            hue: 0, brightness: 0, speed: 0, reverse: false, bounce: false
        })));
        assert!(!logo_is_independent(Some(RgbMode::Breath {
            hue: 0, brightness: 0, speed: 0
        })));
    }

    #[test]
    fn logo_indicator_visible_matches_paint_routing() {
        use crate::config::{LogoConfig, ObsColors};
        use crate::obs::{ObsCommand, ObsEvent};

        let solid = Some(RgbMode::Solid { r: 0, g: 0, b: 0 });
        let wave = Some(RgbMode::Wave {
            hue: 0, brightness: 0, speed: 0, reverse: false, bounce: false,
        });
        let new = |paused_breath| {
            ObsRuntime::new(None, ObsColors::default(), paused_breath, LogoConfig::default())
        };

        // Disconnected idle: shown under static modes (and no [rgb]),
        // hidden under a global animation that owns the logo.
        let obs = new(false);
        assert!(logo_indicator_visible(&obs, solid));
        assert!(logo_indicator_visible(&obs, None));
        assert!(!logo_indicator_visible(&obs, wave));

        // Connected idle: solid idle panel, logo writable even if [rgb] is
        // an animation (the animation isn't active while OBS-connected).
        let mut obs = new(false);
        obs.apply_event(ObsEvent::Connected);
        assert!(logo_indicator_visible(&obs, wave));

        // Recording: solid panel, indicator shown.
        let mut obs = new(false);
        obs.apply_event(ObsEvent::Connected);
        obs.apply_event(ObsEvent::RecordingActive);
        assert!(logo_indicator_visible(&obs, wave));

        // Paused with breath → global animation owns the logo, hidden.
        let mut obs = new(true);
        obs.apply_event(ObsEvent::Connected);
        obs.apply_event(ObsEvent::RecordingPaused);
        assert!(!logo_indicator_visible(&obs, solid));

        // Paused without breath → solid panel, indicator shown.
        let mut obs = new(false);
        obs.apply_event(ObsEvent::Connected);
        obs.apply_event(ObsEvent::RecordingPaused);
        assert!(logo_indicator_visible(&obs, solid));

        // A flash owns the whole panel including the logo → hidden, even in
        // an otherwise-writable state.
        let mut obs = new(false);
        obs.apply_event(ObsEvent::Connected);
        obs.apply_event(ObsEvent::CommandFailed(ObsCommand::SaveReplay, "boom".into()));
        assert!(obs.flash().is_some());
        assert!(!logo_indicator_visible(&obs, solid));
    }
}
