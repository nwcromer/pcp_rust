use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use libpulse_binding as pulse;
use log::{debug, info};
use pulse::callbacks::ListResult;
use pulse::context::{self, FlagSet};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::Proplist;
use pulse::volume::{ChannelVolumes, Volume};

/// Info collected from a sink input for matching and display.
#[derive(Clone)]
struct SinkInputEntry {
    index: u32,
    name: String,
    binary: Option<String>,
    pid: Option<String>,
    client_index: u32,
    channels: u8,
    muted: bool,
}

impl SinkInputEntry {
    /// Check if this entry matches a target app name (case-insensitive substring).
    /// Checks PA app name, then binary, then /proc/<pid>/comm.
    fn matches(&self, target: &str) -> bool {
        if self.name.to_lowercase().contains(target) {
            return true;
        }
        if let Some(ref binary) = self.binary {
            if binary.to_lowercase().contains(target) {
                return true;
            }
        }
        if let Some(ref pid) = self.pid {
            if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
                if comm.trim().to_lowercase().contains(target) {
                    return true;
                }
            }
        }
        false
    }
}

pub struct AudioController {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<context::Context>>,
}

#[derive(Clone)]
pub struct AppInfo {
    pub name: String,
    pub binary: Option<String>,
    pub pid: Option<String>,
    pub sink_input_index: u32,
}

impl AudioController {
    pub fn connect() -> Result<Self> {
        let mainloop =
            Rc::new(RefCell::new(Mainloop::new().context("failed to create PulseAudio mainloop")?));

        let mut proplist = Proplist::new().context("failed to create proplist")?;
        proplist
            .set_str(
                pulse::proplist::properties::APPLICATION_NAME,
                "PCPanel Pro Controller",
            )
            .map_err(|()| anyhow::anyhow!("failed to set application name"))?;

        let context = Rc::new(RefCell::new(
            context::Context::new_with_proplist(
                mainloop.borrow().deref(),
                "pcpanel",
                &proplist,
            )
            .context("failed to create PulseAudio context")?,
        ));

        context
            .borrow_mut()
            .connect(None, FlagSet::NOFLAGS, None)
            .context("failed to connect to PulseAudio")?;

        // Wait for connection
        loop {
            match mainloop.borrow_mut().iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Err(e) => bail!("mainloop iterate error: {e}"),
                IterateResult::Quit(_) => bail!("mainloop quit unexpectedly"),
            }
            match context.borrow().get_state() {
                context::State::Ready => break,
                context::State::Failed | context::State::Terminated => {
                    bail!("PulseAudio connection failed");
                }
                _ => {}
            }
        }

        info!("connected to PulseAudio");
        Ok(Self { mainloop, context })
    }

    fn wait_for(&self, done: &RefCell<bool>) {
        const MAX_ITERATIONS: usize = 1000;
        for _ in 0..MAX_ITERATIONS {
            if *done.borrow() {
                return;
            }
            match self.mainloop.borrow_mut().iterate(true) {
                IterateResult::Success(_) => {}
                _ => break,
            }
        }
        if !*done.borrow() {
            log::warn!("PulseAudio operation timed out");
        }
    }

    fn drain(&self) {
        loop {
            match self.mainloop.borrow_mut().iterate(false) {
                IterateResult::Success(0) => break, // no more pending events
                IterateResult::Success(_) => {}
                _ => break,
            }
        }
    }

    /// Collect all sink inputs with their properties.
    fn collect_sink_inputs(&self) -> Result<Vec<SinkInputEntry>> {
        let entries: Rc<RefCell<Vec<SinkInputEntry>>> = Rc::new(RefCell::new(Vec::new()));
        let done = Rc::new(RefCell::new(false));

        let entries_clone = entries.clone();
        let done_clone = done.clone();
        let introspect = self.context.borrow().introspect();
        let _op = introspect.get_sink_input_info_list(move |result| match result {
            ListResult::Item(info) => {
                let name = info
                    .proplist
                    .get_str(pulse::proplist::properties::APPLICATION_NAME)
                    .unwrap_or_else(|| info.name.as_deref().unwrap_or("unknown").to_string());
                let binary = info
                    .proplist
                    .get_str(pulse::proplist::properties::APPLICATION_PROCESS_BINARY);
                let pid = info
                    .proplist
                    .get_str(pulse::proplist::properties::APPLICATION_PROCESS_ID);
                entries_clone.borrow_mut().push(SinkInputEntry {
                    index: info.index,
                    name,
                    binary,
                    pid,
                    client_index: info.client.unwrap_or(u32::MAX),
                    channels: info.volume.len() as u8,
                    muted: info.mute,
                });
            }
            ListResult::End | ListResult::Error => {
                *done_clone.borrow_mut() = true;
            }
        });

        self.wait_for(&done);
        let mut result = entries.borrow().clone();

        // Resolve PIDs via client lookup for entries missing a PID
        for entry in &mut result {
            if entry.pid.is_none() && entry.client_index != u32::MAX {
                entry.pid = self.get_client_pid(entry.client_index)?;
            }
        }

        Ok(result)
    }

    /// Look up a client's PID via pipewire.sec.pid property.
    fn get_client_pid(&self, client_index: u32) -> Result<Option<String>> {
        let pid: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let done = Rc::new(RefCell::new(false));

        let pid_clone = pid.clone();
        let done_clone = done.clone();
        let introspect = self.context.borrow().introspect();
        let _op = introspect.get_client_info(client_index, move |result| {
            if let ListResult::Item(client) = result {
                // Try pipewire.sec.pid first, then application.process.id
                let p = client
                    .proplist
                    .get_str("pipewire.sec.pid")
                    .or_else(|| {
                        client
                            .proplist
                            .get_str(pulse::proplist::properties::APPLICATION_PROCESS_ID)
                    });
                *pid_clone.borrow_mut() = p;
            }
            *done_clone.borrow_mut() = true;
        });

        self.wait_for(&done);
        Ok(pid.borrow().clone())
    }

    pub fn list_apps(&self) -> Result<Vec<AppInfo>> {
        let entries = self.collect_sink_inputs()?;
        let apps = entries
            .into_iter()
            .map(|e| AppInfo {
                name: e.name,
                binary: e.binary,
                pid: e.pid,
                sink_input_index: e.index,
            })
            .collect();
        Ok(apps)
    }

    pub fn set_system_volume(&self, value: u8) -> Result<()> {
        let volume = volume_from_u8(value);

        // Query default sink to get its channel count
        let channels = Rc::new(RefCell::new(2u8));
        let done = Rc::new(RefCell::new(false));

        let channels_clone = channels.clone();
        let done_clone = done.clone();
        let introspect = self.context.borrow().introspect();
        let _op = introspect.get_sink_info_by_name("@DEFAULT_SINK@", move |result| {
            if let ListResult::Item(sink) = result {
                *channels_clone.borrow_mut() = sink.volume.len() as u8;
            }
            *done_clone.borrow_mut() = true;
        });

        self.wait_for(&done);

        let ch = *channels.borrow();
        let mut cv = ChannelVolumes::default();
        cv.set(ch.into(), volume);
        let mut introspect = self.context.borrow().introspect();
        let _op = introspect.set_sink_volume_by_name("@DEFAULT_SINK@", &cv, None);
        self.drain();

        debug!("set system volume: {} ({}%)", value, value as f32 / 255.0 * 100.0);
        Ok(())
    }

    /// Returns true if any matching app was found.
    pub fn set_app_volume(&self, app_name: &str, value: u8) -> Result<bool> {
        let volume = volume_from_u8(value);
        let target = app_name.to_lowercase();
        let entries = self.collect_sink_inputs()?;

        let matched: Vec<_> = entries.iter().filter(|e| e.matches(&target)).collect();

        let mut introspect = self.context.borrow().introspect();
        for entry in &matched {
            let mut cv = ChannelVolumes::default();
            cv.set(entry.channels.into(), volume);
            let _op = introspect.set_sink_input_volume(entry.index, &cv, None);
        }
        self.drain();

        if matched.is_empty() {
            debug!("app not found: {app_name}");
        }

        Ok(!matched.is_empty())
    }

    /// Returns the new mute state (true = muted).
    pub fn toggle_system_mute(&self) -> Result<bool> {
        let current_mute = Rc::new(RefCell::new(false));
        let done = Rc::new(RefCell::new(false));

        let mute_clone = current_mute.clone();
        let done_clone = done.clone();
        let introspect = self.context.borrow().introspect();
        let _op = introspect.get_sink_info_by_name("@DEFAULT_SINK@", move |result| {
            if let ListResult::Item(sink) = result {
                *mute_clone.borrow_mut() = sink.mute;
                *done_clone.borrow_mut() = true;
            }
        });

        self.wait_for(&done);

        let new_mute = !*current_mute.borrow();
        let mut introspect = self.context.borrow().introspect();
        let _op = introspect.set_sink_mute_by_name("@DEFAULT_SINK@", new_mute, None);
        self.drain();

        Ok(new_mute)
    }

    /// Returns the new mute state (true = muted).
    pub fn toggle_mic_mute(&self) -> Result<bool> {
        let current_mute = Rc::new(RefCell::new(false));
        let done = Rc::new(RefCell::new(false));

        let mute_clone = current_mute.clone();
        let done_clone = done.clone();
        let introspect = self.context.borrow().introspect();
        let _op = introspect.get_source_info_by_name("@DEFAULT_SOURCE@", move |result| {
            if let ListResult::Item(source) = result {
                *mute_clone.borrow_mut() = source.mute;
                *done_clone.borrow_mut() = true;
            }
        });

        self.wait_for(&done);

        let new_mute = !*current_mute.borrow();
        let mut introspect = self.context.borrow().introspect();
        let _op = introspect.set_source_mute_by_name("@DEFAULT_SOURCE@", new_mute, None);
        self.drain();

        Ok(new_mute)
    }

    /// Returns the new mute state, or None if the app wasn't found.
    pub fn toggle_app_mute(&self, app_name: &str) -> Result<Option<bool>> {
        let target = app_name.to_lowercase();
        let entries = self.collect_sink_inputs()?;

        let matched: Vec<_> = entries.iter().filter(|e| e.matches(&target)).collect();

        if matched.is_empty() {
            debug!("app not found for mute toggle: {app_name}");
            return Ok(None);
        }

        let mut introspect = self.context.borrow().introspect();
        for entry in &matched {
            let _op = introspect.set_sink_input_mute(entry.index, !entry.muted, None);
        }
        self.drain();

        Ok(Some(!matched.first().unwrap().muted))
    }
}

impl Drop for AudioController {
    fn drop(&mut self) {
        self.context.borrow_mut().disconnect();
    }
}


fn volume_from_u8(value: u8) -> Volume {
    // Map 0-255 to 0-100% (PA_VOLUME_NORM)
    let fraction = value as f64 / 255.0;
    let raw = (fraction * f64::from(Volume::NORMAL.0)) as u32;
    Volume(raw)
}
