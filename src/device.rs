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
    /// starting up. Drained by `read_event` before falling back to the
    /// HID device.
    pending: Vec<[u8; PACKET_SIZE]>,
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

        let mut panel = Self { device, pending: Vec::new() };
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
                        self.pending.push(read_buf);
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
        let buf = if let Some(frame) = self.pending.pop() {
            frame
        } else {
            let mut buf = [0u8; PACKET_SIZE];
            let n = self.device.read_timeout(&mut buf, 100)?;
            if n < 3 {
                return Ok(None);
            }
            buf
        };

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
                        return Ok(None);
                    }
                };
                Ok(Some(Event::AnalogChange { control, value }))
            }
            MSG_BUTTON => {
                if !(BUTTON_FIRST..=BUTTON_LAST).contains(&index) {
                    warn!("unknown button index: {}", index);
                    return Ok(None);
                }
                match value {
                    0x01 => Ok(Some(Event::ButtonPress { index })),
                    0x00 => Ok(Some(Event::ButtonRelease { index })),
                    _ => {
                        warn!("unknown button value: {:#04x}", value);
                        Ok(None)
                    }
                }
            }
            _ => {
                debug!("unknown message type: {:#04x}", msg_type);
                Ok(None)
            }
        }
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
