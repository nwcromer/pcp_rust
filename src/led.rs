use anyhow::Result;

use crate::device::PcPanelPro;

const PRO_PREFIX: u8 = 0x05;
const CMD_SLIDERS: u8 = 0x00;
const CMD_SLIDER_LABELS: u8 = 0x01;
const CMD_KNOBS: u8 = 0x02;
const CMD_LOGO: u8 = 0x03;
const CMD_GLOBAL_ANIMATION: u8 = 0x04;

// Animation types for CMD_GLOBAL_ANIMATION (per nvdweem/PCPanel reference).
pub const ANIM_RAINBOW_HORIZONTAL: u8 = 0x01;
pub const ANIM_RAINBOW_VERTICAL: u8 = 0x02;
const ANIM_WAVE: u8 = 0x03;
const ANIM_BREATH: u8 = 0x04;

#[derive(Debug, Clone, Copy)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Convert an RGB color to a hue byte (0-255) for the device's breath/wave
/// effects, which take only a hue value. Saturation and value information
/// is discarded — only the dominant color wheel position is preserved.
pub fn rgb_to_hue(c: Rgb) -> u8 {
    let r = c.r as f32 / 255.0;
    let g = c.g as f32 / 255.0;
    let b = c.b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue_deg = if delta < 1e-6 {
        0.0
    } else if (max - r).abs() < 1e-6 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-6 {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    let hue_deg = if hue_deg < 0.0 { hue_deg + 360.0 } else { hue_deg };
    ((hue_deg / 360.0 * 255.0).round() as u32).min(255) as u8
}

#[derive(Debug, Clone, Copy)]
pub enum LedMode {
    Static(Rgb),
    Gradient(Rgb, Rgb),
    VolumeGradient(Rgb, Rgb), // sliders only
}

impl LedMode {
    fn to_bytes(self) -> [u8; 7] {
        match self {
            LedMode::Static(c) => [0x01, c.r, c.g, c.b, 0x00, 0x00, 0x00],
            LedMode::Gradient(a, b) => [0x02, a.r, a.g, a.b, b.r, b.g, b.b],
            LedMode::VolumeGradient(a, b) => [0x03, a.r, a.g, a.b, b.r, b.g, b.b],
        }
    }
}

// Rainbow and Breath are intentionally not exposed in config — pcp_rust
// keeps the config logo-agnostic. Kept as protocol documentation for
// anyone wanting to use them programmatically.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum LogoMode {
    Static(Rgb),
    Rainbow { brightness: u8, speed: u8 },
    Breath { hue: u8, brightness: u8, speed: u8 },
}

pub fn set_knob_colors(device: &PcPanelPro, knobs: &[LedMode; 5]) -> Result<()> {
    let mut packet = vec![PRO_PREFIX, CMD_KNOBS];
    for mode in knobs {
        packet.extend_from_slice(&mode.to_bytes());
    }
    device.set_led(&packet)
}

pub fn set_slider_colors(device: &PcPanelPro, sliders: &[LedMode; 4]) -> Result<()> {
    let mut packet = vec![PRO_PREFIX, CMD_SLIDERS];
    for mode in sliders {
        packet.extend_from_slice(&mode.to_bytes());
    }
    device.set_led(&packet)
}

pub fn set_slider_label_colors(device: &PcPanelPro, labels: &[LedMode; 4]) -> Result<()> {
    let mut packet = vec![PRO_PREFIX, CMD_SLIDER_LABELS];
    for mode in labels {
        packet.extend_from_slice(&mode.to_bytes());
    }
    device.set_led(&packet)
}

pub fn set_logo(device: &PcPanelPro, mode: LogoMode) -> Result<()> {
    let packet = match mode {
        LogoMode::Static(c) => vec![PRO_PREFIX, CMD_LOGO, 0x01, c.r, c.g, c.b],
        LogoMode::Rainbow { brightness, speed } => {
            vec![PRO_PREFIX, CMD_LOGO, 0x02, 0xFF, brightness, speed]
        }
        LogoMode::Breath {
            hue,
            brightness,
            speed,
        } => vec![PRO_PREFIX, CMD_LOGO, 0x03, hue, brightness, speed],
    };
    device.set_led(&packet)
}

/// Rainbow animation type: 0x01 = horizontal, 0x02 = vertical
pub fn set_rainbow(device: &PcPanelPro, rainbow_type: u8, brightness: u8, speed: u8) -> Result<()> {
    let packet = vec![
        PRO_PREFIX, CMD_GLOBAL_ANIMATION, rainbow_type,
        0x00,        // phase
        0xFF,        // placeholder
        brightness,
        speed,
        0x00,        // no reverse
    ];
    device.set_led(&packet)
}

pub fn set_wave(
    device: &PcPanelPro,
    hue: u8,
    brightness: u8,
    speed: u8,
    reverse: bool,
    bounce: bool,
) -> Result<()> {
    let packet = vec![
        PRO_PREFIX, CMD_GLOBAL_ANIMATION, ANIM_WAVE,
        hue,
        0xFF, // placeholder
        brightness,
        speed,
        u8::from(reverse),
        u8::from(bounce),
    ];
    device.set_led(&packet)
}

pub fn set_breath(device: &PcPanelPro, hue: u8, brightness: u8, speed: u8) -> Result<()> {
    let packet = vec![
        PRO_PREFIX, CMD_GLOBAL_ANIMATION, ANIM_BREATH,
        hue,
        0xFF, // placeholder
        brightness,
        speed,
    ];
    device.set_led(&packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_to_hue_primaries() {
        // Red, green, blue map to 0°, 120°, 240° respectively. Scaled to
        // 0-255: 0, 85, 170.
        assert_eq!(rgb_to_hue(Rgb::new(0xFF, 0, 0)), 0);
        assert_eq!(rgb_to_hue(Rgb::new(0, 0xFF, 0)), 85);
        assert_eq!(rgb_to_hue(Rgb::new(0, 0, 0xFF)), 170);
    }

    #[test]
    fn rgb_to_hue_amber() {
        // Amber #FFC000 has hue ≈ 45° → scaled byte ≈ 32.
        let h = rgb_to_hue(Rgb::new(0xFF, 0xC0, 0));
        assert!((30..=34).contains(&h), "expected hue near 32 for amber, got {h}");
    }

    #[test]
    fn rgb_to_hue_achromatic() {
        // Pure white, black, and gray have no hue; the function returns 0
        // by convention (matches HSL/HSV).
        assert_eq!(rgb_to_hue(Rgb::new(0xFF, 0xFF, 0xFF)), 0);
        assert_eq!(rgb_to_hue(Rgb::new(0x00, 0x00, 0x00)), 0);
        assert_eq!(rgb_to_hue(Rgb::new(0x80, 0x80, 0x80)), 0);
    }
}
