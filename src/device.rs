use anyhow::{Context, Result};
use hidapi::HidApi;
use log::{debug, info, warn};

const VENDOR_ID: u16 = 0x0483;
const PRODUCT_ID: u16 = 0xA3C5;
const PACKET_SIZE: usize = 64;
const CALIBRATION_READS: usize = 20;

pub struct PcPanelPro {
    device: hidapi::HidDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Knob(u8),   // 0-4
    Slider(u8), // 0-3
    Button(u8), // 0-4
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

        let mut panel = Self { device };
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

        // Calibration reads — analog controls report initial positions
        let mut read_buf = [0u8; PACKET_SIZE];
        for i in 0..CALIBRATION_READS {
            match self.device.read_timeout(&mut read_buf, 250) {
                Ok(n) if n > 0 => {
                    debug!("calibration read {}: {:02x?}", i, &read_buf[..n.min(3)]);
                }
                Ok(_) => {}
                Err(e) => warn!("calibration read {} failed: {}", i, e),
            }
        }
        info!("calibration complete");
        Ok(())
    }

    pub fn read_event(&self) -> Result<Option<Event>> {
        let mut buf = [0u8; PACKET_SIZE];
        let n = self.device.read_timeout(&mut buf, 100)?;
        if n < 3 {
            return Ok(None);
        }

        let msg_type = buf[0];
        let index = buf[1];
        let value = buf[2];

        match msg_type {
            0x01 => {
                let control = match index {
                    0..=4 => Control::Knob(index),
                    5..=8 => Control::Slider(index - 5),
                    _ => {
                        warn!("unknown analog index: {}", index);
                        return Ok(None);
                    }
                };
                Ok(Some(Event::AnalogChange { control, value }))
            }
            0x02 => match value {
                0x01 => Ok(Some(Event::ButtonPress { index })),
                0x00 => Ok(Some(Event::ButtonRelease { index })),
                _ => {
                    warn!("unknown button value: {:#04x}", value);
                    Ok(None)
                }
            },
            _ => {
                debug!("unknown message type: {:#04x}", msg_type);
                Ok(None)
            }
        }
    }

    pub fn set_led(&self, packet: &[u8]) -> Result<()> {
        let mut buf = [0u8; PACKET_SIZE];
        let len = packet.len().min(PACKET_SIZE);
        buf[..len].copy_from_slice(&packet[..len]);
        self.device
            .write(&buf)
            .context("failed to write LED command")?;
        Ok(())
    }
}
