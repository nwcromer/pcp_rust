use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Deref;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use libpulse_binding as pulse;
use log::{debug, info, warn};
use pulse::callbacks::ListResult;
use pulse::context::ext_stream_restore::Info as SrInfo;
use pulse::context::{self, FlagSet};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::{Proplist, UpdateMode};
use pulse::volume::{ChannelVolumes, Volume};

/// How long an app's volume must be stable before we persist it to the
/// stream-restore database. Coalesces bursts of slider events into a
/// single DB write after the user stops moving the slider.
const PERSIST_IDLE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy)]
struct PendingPersist {
    volume: Volume,
    last_update: Instant,
}

/// Info collected from a sink input for matching and display.
#[derive(Clone)]
struct SinkInputEntry {
    index: u32,
    name: String,
    binary: Option<String>,
    pid: Option<String>,
    /// Cached lower-cased `/proc/<pid>/comm`. Populated once during
    /// `collect_sink_inputs` so `matches()` is pure RAM comparison —
    /// avoids re-reading /proc on every match attempt (especially for
    /// sliders with multiple configured apps).
    comm: Option<String>,
    client_index: u32,
    channels: u8,
    muted: bool,
}

impl SinkInputEntry {
    /// Check if this entry matches a target app name (case-insensitive substring).
    /// Checks PA app name, then binary, then cached /proc/<pid>/comm.
    fn matches(&self, target: &str) -> bool {
        if self.name.to_lowercase().contains(target) {
            return true;
        }
        if let Some(ref binary) = self.binary {
            if binary.to_lowercase().contains(target) {
                return true;
            }
        }
        if let Some(ref comm) = self.comm {
            if comm.contains(target) {
                return true;
            }
        }
        false
    }
}

pub struct AudioController {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<context::Context>>,
    /// Per-app deferred stream-restore writes. `set_app_volume` updates
    /// the entry on every tick; `flush_persist_writes` (called from the
    /// main loop) actually writes to PA once the entry has been idle for
    /// `PERSIST_IDLE`.
    pending_persist: HashMap<String, PendingPersist>,
}

#[derive(Clone)]
pub struct AppInfo {
    pub name: String,
    pub binary: Option<String>,
    pub pid: Option<String>,
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

        Ok(Self { mainloop, context, pending_persist: HashMap::new() })
    }

    fn wait_for(&self, done: &RefCell<bool>) -> Result<()> {
        self.wait_until(|| *done.borrow())
    }

    /// Drive the PulseAudio mainloop until `is_done` returns true, or fail
    /// with an error if the PA context disconnects, the mainloop quits, or
    /// the operation exceeds the wall-clock deadline / iteration cap.
    ///
    /// Surfacing a real error rather than silently breaking out is the
    /// "practical fallback" for the absent reconnect path: callers now see
    /// the failure and propagate it; the user gets a visible error in the
    /// journal instead of stale defaults from an apparently-successful call.
    fn wait_until<F: Fn() -> bool>(&self, is_done: F) -> Result<()> {
        const MAX_ITERATIONS: usize = 1000;
        const DEADLINE: std::time::Duration = std::time::Duration::from_millis(250);
        let started = std::time::Instant::now();
        for _ in 0..MAX_ITERATIONS {
            if is_done() {
                return Ok(());
            }
            if started.elapsed() >= DEADLINE {
                bail!("PulseAudio call exceeded {:?} deadline", DEADLINE);
            }
            match self.mainloop.borrow_mut().iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Err(e) => bail!("PulseAudio mainloop error: {e}"),
                IterateResult::Quit(_) => bail!("PulseAudio mainloop quit"),
            }
            // PA context can transition out of Ready (server restart, etc.)
            // mid-operation; iterate() returns Success while the context
            // becomes unusable. Check explicitly so subsequent calls don't
            // silently use stale data.
            match self.context.borrow().get_state() {
                context::State::Ready => {}
                other => bail!("PulseAudio context not ready: {other:?}"),
            }
        }
        if !is_done() {
            bail!("PulseAudio call exceeded {MAX_ITERATIONS} iterations");
        }
        Ok(())
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
                    comm: None, // populated after PID resolution below
                    client_index: info.client.unwrap_or(u32::MAX),
                    channels: info.volume.len() as u8,
                    muted: info.mute,
                });
            }
            ListResult::End | ListResult::Error => {
                *done_clone.borrow_mut() = true;
            }
        });

        self.wait_for(&done)?;
        let mut result = entries.borrow().clone();

        // Resolve PIDs via client lookup for entries missing a PID
        for entry in &mut result {
            if entry.pid.is_none() && entry.client_index != u32::MAX {
                entry.pid = self.get_client_pid(entry.client_index)?;
            }
        }

        // Cache /proc/<pid>/comm (lower-cased) so matches() doesn't read it
        // per match attempt.
        for entry in &mut result {
            if let Some(ref pid) = entry.pid {
                if pid.chars().all(|c| c.is_ascii_digit())
                    && let Ok(raw) = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                {
                    entry.comm = Some(raw.trim().to_lowercase());
                }
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

        self.wait_for(&done)?;
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

        self.wait_for(&done)?;

        let ch = *channels.borrow();
        let mut cv = ChannelVolumes::default();
        cv.set(ch.into(), volume);
        let mut introspect = self.context.borrow().introspect();
        let _op = introspect.set_sink_volume_by_name("@DEFAULT_SINK@", &cv, None);
        self.drain();

        debug!("set system volume: {} ({}%)", value, value as f32 / 255.0 * 100.0);
        Ok(())
    }

    pub fn set_mic_volume(&self, value: u8) -> Result<()> {
        let volume = volume_from_u8(value);

        // Query default source to get its channel count
        let channels = Rc::new(RefCell::new(1u8));
        let done = Rc::new(RefCell::new(false));

        let channels_clone = channels.clone();
        let done_clone = done.clone();
        let introspect = self.context.borrow().introspect();
        let _op = introspect.get_source_info_by_name("@DEFAULT_SOURCE@", move |result| {
            if let ListResult::Item(source) = result {
                *channels_clone.borrow_mut() = source.volume.len() as u8;
            }
            *done_clone.borrow_mut() = true;
        });

        self.wait_for(&done)?;

        let ch = *channels.borrow();
        let mut cv = ChannelVolumes::default();
        cv.set(ch.into(), volume);
        let mut introspect = self.context.borrow().introspect();
        let _op = introspect.set_source_volume_by_name("@DEFAULT_SOURCE@", &cv, None);
        self.drain();

        debug!("set mic volume: {} ({}%)", value, value as f32 / 255.0 * 100.0);
        Ok(())
    }

    /// Returns true if any matching app was found.
    pub fn set_app_volume(&mut self, app_name: &str, value: u8) -> Result<bool> {
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
        } else {
            // Stash the new volume for deferred stream-restore persistence —
            // a burst of slider ticks coalesces to a single DB write once
            // the user stops moving the slider for `PERSIST_IDLE`. The
            // actual playback volume above is set on every tick.
            self.pending_persist.insert(
                target,
                PendingPersist { volume, last_update: Instant::now() },
            );
        }

        Ok(!matched.is_empty())
    }

    /// Flush any per-app volume persistence writes that have been idle for
    /// `PERSIST_IDLE`. Should be called from the main loop. PA write
    /// failures are logged and the entry is dropped (no infinite retry).
    pub fn flush_persist_writes(&mut self) {
        let now = Instant::now();
        let ready: Vec<(String, Volume)> = self
            .pending_persist
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_update) >= PERSIST_IDLE)
            .map(|(k, p)| (k.clone(), p.volume))
            .collect();
        for (target, volume) in ready {
            self.pending_persist.remove(&target);
            let result = self.update_stream_restore(&target, |entry| {
                let channels = entry.volume.len();
                let mut cv = ChannelVolumes::default();
                cv.set(channels, volume);
                SrInfo {
                    name: entry.name.clone(),
                    channel_map: entry.channel_map,
                    volume: cv,
                    device: entry.device.clone(),
                    mute: entry.mute,
                }
            });
            if let Err(e) = result {
                warn!("audio: deferred stream-restore write for {target} failed: {e}");
            }
        }
    }

    /// Updates module-stream-restore database entries whose name contains
    /// `target` (case-insensitive). The `updater` closure receives each
    /// matching existing entry and returns the new entry that should
    /// replace it. Use it to bump volume, mute state, or both.
    fn update_stream_restore<F>(&self, target: &str, mut updater: F) -> Result<()>
    where
        F: FnMut(&SrInfo<'static>) -> SrInfo<'static>,
    {
        let entries: Rc<RefCell<Vec<SrInfo<'static>>>> = Rc::new(RefCell::new(Vec::new()));
        let done = Rc::new(RefCell::new(false));

        let entries_clone = entries.clone();
        let done_clone = done.clone();

        let mut sr = self.context.borrow().stream_restore();
        let _op = sr.read(move |result| match result {
            ListResult::Item(info) => {
                entries_clone.borrow_mut().push(info.to_owned());
            }
            ListResult::End | ListResult::Error => {
                *done_clone.borrow_mut() = true;
            }
        });
        self.wait_for(&done)?;

        let mut updated: Vec<SrInfo<'static>> = Vec::new();
        for entry in entries.borrow().iter() {
            let name_matches = entry
                .name
                .as_ref()
                .map(|n| n.to_lowercase().contains(target))
                .unwrap_or(false);
            if !name_matches {
                continue;
            }
            updated.push(updater(entry));
        }

        if updated.is_empty() {
            return Ok(());
        }

        let refs: Vec<&SrInfo> = updated.iter().collect();
        let result: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        let result_clone = result.clone();
        let _op = sr.write(UpdateMode::Replace, &refs, false, move |success| {
            *result_clone.borrow_mut() = Some(success);
        });
        self.wait_until(|| result.borrow().is_some())?;

        match *result.borrow() {
            None => bail!("stream-restore write did not complete"),
            Some(false) => bail!("stream-restore write rejected by server"),
            Some(true) => {}
        }

        debug!("updated {} stream-restore entries for {target}", updated.len());
        Ok(())
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
            }
            if matches!(result, ListResult::Item(_) | ListResult::End | ListResult::Error) {
                *done_clone.borrow_mut() = true;
            }
        });

        self.wait_for(&done)?;

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
            }
            if matches!(result, ListResult::Item(_) | ListResult::End | ListResult::Error) {
                *done_clone.borrow_mut() = true;
            }
        });

        self.wait_for(&done)?;

        let new_mute = !*current_mute.borrow();
        let mut introspect = self.context.borrow().introspect();
        let _op = introspect.set_source_mute_by_name("@DEFAULT_SOURCE@", new_mute, None);
        self.drain();

        Ok(new_mute)
    }

    /// Returns the new mute state, or None if the app wasn't found.
    /// If any matched streams are unmuted, mutes all. If all are muted, unmutes all.
    pub fn toggle_app_mute(&self, app_name: &str) -> Result<Option<bool>> {
        let target = app_name.to_lowercase();
        let entries = self.collect_sink_inputs()?;

        let matched: Vec<_> = entries.iter().filter(|e| e.matches(&target)).collect();

        if matched.is_empty() {
            debug!("app not found for mute toggle: {app_name}");
            return Ok(None);
        }

        // If any are unmuted, mute all. If all are muted, unmute all.
        let any_unmuted = matched.iter().any(|e| !e.muted);
        let new_mute = any_unmuted;

        let mut introspect = self.context.borrow().introspect();
        for entry in &matched {
            let _op = introspect.set_sink_input_mute(entry.index, new_mute, None);
        }
        self.drain();

        // Persist the mute state to the stream-restore database so that new
        // streams for this app pick it up automatically — mirrors the
        // volume-persist behavior in set_app_volume.
        let result = self.update_stream_restore(&target, |entry| SrInfo {
            name: entry.name.clone(),
            channel_map: entry.channel_map,
            volume: entry.volume,
            device: entry.device.clone(),
            mute: new_mute,
        });
        if let Err(e) = result {
            warn!("failed to update stream-restore mute for {app_name}: {e}");
        }

        Ok(Some(new_mute))
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
    let raw = (fraction * f64::from(Volume::NORMAL.0) + 0.5) as u32;
    Volume(raw)
}
