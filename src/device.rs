use std::collections::VecDeque;

use anyhow::{Context, Result};
use hidapi::HidApi;
use log::{debug, info, warn};

const VENDOR_ID: u16 = 0x0483;
const PRODUCT_ID: u16 = 0xA3C5;
const PACKET_SIZE: usize = 64;
const CALIBRATION_READS: usize = 20;

// Control index ranges in HID reports
const KNOB_FIRST: u8 = 0;
const KNOB_LAST: u8 = 4;
const SLIDER_FIRST: u8 = 5;
const SLIDER_LAST: u8 = 8;

const BUTTON_FIRST: u8 = 0;
const BUTTON_LAST: u8 = 4;

// Message types
const MSG_ANALOG: u8 = 0x01;
const MSG_BUTTON: u8 = 0x02;

pub struct PcPanelPro {
    device: hidapi::HidDevice,
    /// Frames captured during init() that are NOT calibration-burst analog
    /// readings — e.g., a button press the user made while pcp_rust was
    /// starting up. Drained FIFO by `read_event` before falling back to
    /// the HID device.
    pending: VecDeque<[u8; PACKET_SIZE]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Knob(u8),   // 0-4
    Slider(u8), // 0-3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    AnalogChange { control: Control, value: u8 },
    ButtonPress { index: u8 },
    ButtonRelease { index: u8 },
}

impl PcPanelPro {
    pub fn open() -> Result<Self> {
        let api = HidApi::new().context("failed to initialize HID API")?;

        let device = api
            .open(VENDOR_ID, PRODUCT_ID)
            .context("failed to open PCPanel Pro — is it plugged in?")?;

        info!(
            "opened PCPanel Pro (VID={:#06x}, PID={:#06x})",
            VENDOR_ID, PRODUCT_ID
        );

        let mut panel = Self { device, pending: VecDeque::new() };
        panel.init()?;
        Ok(panel)
    }

    fn init(&mut self) -> Result<()> {
        // Send init command
        let mut buf = [0u8; PACKET_SIZE];
        buf[0] = 0x01;
        self.device
            .write(&buf)
            .context("failed to send init command")?;
        info!("sent init command");

        // Calibration reads — the device burst-emits the current analog
        // position for every knob/slider after init. We discard those
        // (we'll see them again on the next real change anyway) but
        // forward any button frames into a pending queue so a user press
        // during startup isn't silently dropped.
        let mut read_buf = [0u8; PACKET_SIZE];
        for i in 0..CALIBRATION_READS {
            match self.device.read_timeout(&mut read_buf, 250) {
                Ok(n) if n >= 3 => {
                    debug!("calibration read {}: {:02x?}", i, &read_buf[..3]);
                    if read_buf[0] != MSG_ANALOG {
                        // Non-analog (button) frame — preserve it.
                        self.pending.push_back(read_buf);
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("calibration read {} failed: {}", i, e),
            }
        }
        info!("calibration complete");
        Ok(())
    }

    pub fn read_event(&mut self) -> Result<Option<Event>> {
        // Drain any frames captured during calibration before reading from
        // the device.
        let buf = if let Some(frame) = self.pending.pop_front() {
            frame
        } else {
            let mut buf = [0u8; PACKET_SIZE];
            let n = self.device.read_timeout(&mut buf, 100)?;
            if n < 3 {
                return Ok(None);
            }
            buf
        };

        Ok(parse_event(&buf))
    }

    pub fn set_led(&self, packet: &[u8]) -> Result<()> {
        anyhow::ensure!(
            packet.len() <= PACKET_SIZE,
            "LED packet too large: {} bytes > {}",
            packet.len(),
            PACKET_SIZE
        );
        let mut buf = [0u8; PACKET_SIZE];
        buf[..packet.len()].copy_from_slice(packet);
        self.device
            .write(&buf)
            .context("failed to write LED command")?;
        Ok(())
    }
}

/// Parse a 64-byte HID report into an Event. Pure function (no I/O) so it
/// can be unit-tested. Returns `None` for messages we don't recognize —
/// the daemon logs at warn/debug and continues rather than bailing, since
/// firmware versions may emit message types we don't care about.
fn parse_event(buf: &[u8; PACKET_SIZE]) -> Option<Event> {
    let msg_type = buf[0];
    let index = buf[1];
    let value = buf[2];

    match msg_type {
        MSG_ANALOG => {
            let control = match index {
                KNOB_FIRST..=KNOB_LAST => Control::Knob(index - KNOB_FIRST),
                SLIDER_FIRST..=SLIDER_LAST => Control::Slider(index - SLIDER_FIRST),
                _ => {
                    warn!("unknown analog index: {}", index);
                    return None;
                }
            };
            Some(Event::AnalogChange { control, value })
        }
        MSG_BUTTON => {
            if !(BUTTON_FIRST..=BUTTON_LAST).contains(&index) {
                warn!("unknown button index: {}", index);
                return None;
            }
            match value {
                0x01 => Some(Event::ButtonPress { index }),
                0x00 => Some(Event::ButtonRelease { index }),
                _ => {
                    warn!("unknown button value: {:#04x}", value);
                    None
                }
            }
        }
        _ => {
            debug!("unknown message type: {:#04x}", msg_type);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(msg: u8, index: u8, value: u8) -> [u8; PACKET_SIZE] {
        let mut b = [0u8; PACKET_SIZE];
        b[0] = msg;
        b[1] = index;
        b[2] = value;
        b
    }

    #[test]
    fn parse_knob_boundaries() {
        assert_eq!(
            parse_event(&frame(MSG_ANALOG, 0, 128)),
            Some(Event::AnalogChange { control: Control::Knob(0), value: 128 })
        );
        assert_eq!(
            parse_event(&frame(MSG_ANALOG, 4, 255)),
            Some(Event::AnalogChange { control: Control::Knob(4), value: 255 })
        );
    }

    #[test]
    fn parse_slider_boundaries() {
        assert_eq!(
            parse_event(&frame(MSG_ANALOG, 5, 0)),
            Some(Event::AnalogChange { control: Control::Slider(0), value: 0 })
        );
        assert_eq!(
            parse_event(&frame(MSG_ANALOG, 8, 200)),
            Some(Event::AnalogChange { control: Control::Slider(3), value: 200 })
        );
    }

    #[test]
    fn parse_analog_out_of_range_indices_dropped() {
        // Index 9 is one past the last slider.
        assert_eq!(parse_event(&frame(MSG_ANALOG, 9, 100)), None);
        // 0xFF is well outside any defined range.
        assert_eq!(parse_event(&frame(MSG_ANALOG, 0xFF, 100)), None);
    }

    #[test]
    fn parse_button_press_and_release() {
        assert_eq!(
            parse_event(&frame(MSG_BUTTON, 0, 0x01)),
            Some(Event::ButtonPress { index: 0 })
        );
        assert_eq!(
            parse_event(&frame(MSG_BUTTON, 4, 0x00)),
            Some(Event::ButtonRelease { index: 4 })
        );
    }

    #[test]
    fn parse_button_out_of_range_dropped() {
        assert_eq!(parse_event(&frame(MSG_BUTTON, 5, 0x01)), None);
        assert_eq!(parse_event(&frame(MSG_BUTTON, 0xFF, 0x01)), None);
    }

    #[test]
    fn parse_button_unknown_value_dropped() {
        // Only 0x00 / 0x01 are press/release; anything else is dropped.
        assert_eq!(parse_event(&frame(MSG_BUTTON, 0, 0x02)), None);
        assert_eq!(parse_event(&frame(MSG_BUTTON, 0, 0xFF)), None);
    }

    #[test]
    fn parse_unknown_message_type_dropped() {
        assert_eq!(parse_event(&frame(0x00, 0, 0)), None);
        assert_eq!(parse_event(&frame(0xAB, 0, 0)), None);
    }
}
