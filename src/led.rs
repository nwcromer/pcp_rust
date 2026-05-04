use anyhow::Result;

use crate::device::PcPanelPro;

const PRO_PREFIX: u8 = 0x05;
const CMD_SLIDERS: u8 = 0x00;
const CMD_SLIDER_LABELS: u8 = 0x01;
const CMD_KNOBS: u8 = 0x02;
const CMD_LOGO: u8 = 0x03;
const CMD_GLOBAL_ANIMATION: u8 = 0x04;

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

// Gradient/VolumeGradient are unused but kept as protocol documentation:
// VolumeGradient (byte 0x03) is the device's "show volume level via LED color"
// mode for sliders, a likely future feature.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum LedMode {
    Static(Rgb),
    Gradient(Rgb, Rgb),
    VolumeGradient(Rgb, Rgb), // sliders only
}

impl LedMode {
    fn to_bytes(&self) -> [u8; 7] {
        match self {
            LedMode::Static(c) => [0x01, c.r, c.g, c.b, 0x00, 0x00, 0x00],
            LedMode::Gradient(a, b) => [0x02, a.r, a.g, a.b, b.r, b.g, b.b],
            LedMode::VolumeGradient(a, b) => [0x03, a.r, a.g, a.b, b.r, b.g, b.b],
        }
    }
}

// Rainbow/Breath are unused but kept as protocol documentation: bytes 0x02
// and 0x03 are the device's animated logo modes, likely future config options.
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
