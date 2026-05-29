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
use pulse::operation::Operation;
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

/// Distinguishes the two "default audio device" targets we drive — system
/// output (sink) and mic input (source). Used to deduplicate the
/// per-system/per-mic methods which were near-identical except for which
/// PA API they called.
#[derive(Clone, Copy)]
enum Target {
    DefaultSink,
    DefaultSource,
}

/// Three-state outcome of `query_default_mute`'s introspect callback.
/// Distinguishes "device genuinely absent" (callback ran to End without
/// ever yielding Item) from "PA-side enumeration error" (Error variant
/// fired) so the surfaced error message is accurate during triage.
#[derive(Clone)]
enum MuteOutcome {
    Pending,
    Item(bool),
    NotFound,
    Error,
}

impl Target {
    fn pa_name(self) -> &'static str {
        match self {
            Self::DefaultSink => "@DEFAULT_SINK@",
            Self::DefaultSource => "@DEFAULT_SOURCE@",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::DefaultSink => "system",
            Self::DefaultSource => "mic",
        }
    }

    /// Default channel count to assume if the PA query fails to populate one.
    fn default_channels(self) -> u8 {
        match self {
            Self::DefaultSink => 2,
            Self::DefaultSource => 1,
        }
    }
}

/// Info collected from a sink input for matching and display. The lower-
/// cased forms of `name`, `binary`, and `comm` are computed once during
/// `collect_sink_inputs` so `matches()` is a pure `&str::contains` —
/// sliders fire ~10 Hz × multiple configured apps and the per-call
/// `to_lowercase()` was real work.
#[derive(Clone)]
struct SinkInputEntry {
    index: u32,
    name: String,
    name_lower: String,
    binary: Option<String>,
    binary_lower: Option<String>,
    pid: Option<String>,
    /// Cached lower-cased `/proc/<pid>/comm`. Populated once during
    /// `collect_sink_inputs` so `matches()` doesn't re-read /proc per
    /// match attempt.
    comm: Option<String>,
    client_index: u32,
    channels: u8,
    muted: bool,
}

impl SinkInputEntry {
    /// Check if this entry matches a target app name (case-insensitive
    /// substring). `target` is expected to already be lower-cased by the
    /// caller — `matches` does no per-call allocation.
    fn matches(&self, target: &str) -> bool {
        self.name_lower.contains(target)
            || self
                .binary_lower
                .as_deref()
                .is_some_and(|b| b.contains(target))
            || self.comm.as_deref().is_some_and(|c| c.contains(target))
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

    /// Run a PA call that fills a single value. The `register` closure
    /// receives a (value, done) pair: it should invoke the PA introspect
    /// method whose callback writes to `value` and sets `done` on completion.
    fn pa_query<T, R>(&self, default: T, register: R) -> Result<T>
    where
        T: Clone + 'static,
        R: FnOnce(Rc<RefCell<T>>, Rc<RefCell<bool>>),
    {
        let value: Rc<RefCell<T>> = Rc::new(RefCell::new(default));
        let done = Rc::new(RefCell::new(false));
        register(value.clone(), done.clone());
        self.wait_for(&done)?;
        Ok(value.borrow().clone())
    }

    /// Run a PA list call. The `register` closure registers the PA method
    /// whose callback pushes items into the provided Vec and sets `done`
    /// on `End`/`Error`.
    fn pa_collect<T, R>(&self, register: R) -> Result<Vec<T>>
    where
        T: 'static,
        R: FnOnce(Rc<RefCell<Vec<T>>>, Rc<RefCell<bool>>),
    {
        let items: Rc<RefCell<Vec<T>>> = Rc::new(RefCell::new(Vec::new()));
        let done = Rc::new(RefCell::new(false));
        register(items.clone(), done.clone());
        self.wait_for(&done)?;
        // Take ownership of the inner Vec (avoids requiring T: Clone for
        // types like SrInfo that don't implement Clone).
        Ok(std::mem::take(&mut *items.borrow_mut()))
    }

    /// Run a PA write that reports success/failure via a bool callback
    /// (e.g., `sr.write`). The `register` closure must return the
    /// `Operation` handle so we can cancel it if the wait deadline trips —
    /// otherwise libpulse-binding's `Operation::drop` deliberately leaks
    /// the boxed callback (so PA can still invoke it after drop without
    /// UAF). Returns the reported success state or an error if the
    /// operation didn't complete in time.
    fn pa_write_with_result<R>(&self, register: R) -> Result<bool>
    where
        R: FnOnce(Rc<RefCell<Option<bool>>>) -> Option<Operation<dyn FnMut(bool) + 'static>>,
    {
        let result: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        let mut op = register(result.clone());
        let wait_result = self.wait_until(|| result.borrow().is_some());
        if wait_result.is_err()
            && let Some(op) = op.as_mut()
        {
            // Cancel before drop so the boxed FnMut can be freed cleanly.
            op.cancel();
        }
        wait_result?;
        result
            .borrow()
            .ok_or_else(|| anyhow::anyhow!("PA write did not complete"))
    }

    /// Channel count of the named default sink/source.
    fn query_default_channels(&self, target: Target) -> Result<u8> {
        let name = target.pa_name();
        self.pa_query(target.default_channels(), |channels, done| {
            let introspect = self.context.borrow().introspect();
            match target {
                Target::DefaultSink => {
                    let _op = introspect.get_sink_info_by_name(name, move |result| {
                        if let ListResult::Item(sink) = result {
                            *channels.borrow_mut() = sink.volume.len();
                        }
                        *done.borrow_mut() = true;
                    });
                }
                Target::DefaultSource => {
                    let _op = introspect.get_source_info_by_name(name, move |result| {
                        if let ListResult::Item(source) = result {
                            *channels.borrow_mut() = source.volume.len();
                        }
                        *done.borrow_mut() = true;
                    });
                }
            }
        })
    }

    /// Current mute state of the named default sink/source. Errors with a
    /// label-distinguished message depending on whether PA finished
    /// enumeration without ever yielding an Item (genuinely not present)
    /// vs. reported an enumeration Error (server-side failure).
    fn query_default_mute(&self, target: Target) -> Result<bool> {
        let name = target.pa_name();
        let outcome: MuteOutcome = self.pa_query(MuteOutcome::Pending, |out, done| {
            let introspect = self.context.borrow().introspect();
            match target {
                Target::DefaultSink => {
                    let _op = introspect.get_sink_info_by_name(name, move |result| {
                        match result {
                            ListResult::Item(sink) => {
                                *out.borrow_mut() = MuteOutcome::Item(sink.mute);
                            }
                            ListResult::End => {
                                if matches!(*out.borrow(), MuteOutcome::Pending) {
                                    *out.borrow_mut() = MuteOutcome::NotFound;
                                }
                            }
                            ListResult::Error => {
                                *out.borrow_mut() = MuteOutcome::Error;
                            }
                        }
                        *done.borrow_mut() = true;
                    });
                }
                Target::DefaultSource => {
                    let _op = introspect.get_source_info_by_name(name, move |result| {
                        match result {
                            ListResult::Item(source) => {
                                *out.borrow_mut() = MuteOutcome::Item(source.mute);
                            }
                            ListResult::End => {
                                if matches!(*out.borrow(), MuteOutcome::Pending) {
                                    *out.borrow_mut() = MuteOutcome::NotFound;
                                }
                            }
                            ListResult::Error => {
                                *out.borrow_mut() = MuteOutcome::Error;
                            }
                        }
                        *done.borrow_mut() = true;
                    });
                }
            }
        })?;
        match outcome {
            MuteOutcome::Item(b) => Ok(b),
            MuteOutcome::NotFound | MuteOutcome::Pending => {
                bail!("{} not found", target.label())
            }
            MuteOutcome::Error => {
                bail!("PulseAudio enumeration error while querying {}", target.label())
            }
        }
    }

    fn set_default_volume(&self, target: Target, value: u8) -> Result<()> {
        let volume = volume_from_u8(value);
        let channels = self.query_default_channels(target)?;
        let mut cv = ChannelVolumes::default();
        cv.set(channels, volume);

        let mut introspect = self.context.borrow().introspect();
        match target {
            Target::DefaultSink => {
                let _op = introspect.set_sink_volume_by_name(target.pa_name(), &cv, None);
            }
            Target::DefaultSource => {
                let _op = introspect.set_source_volume_by_name(target.pa_name(), &cv, None);
            }
        }
        self.drain();
        debug!("set {} volume: {} ({}%)", target.label(), value, value as f32 / 255.0 * 100.0);
        Ok(())
    }

    fn toggle_default_mute(&self, target: Target) -> Result<bool> {
        let new_mute = !self.query_default_mute(target)?;
        // Wait for PA to actually apply the mute before returning, so the
        // caller can synchronously cache the new state without racing the
        // next mic-mute poll (which would otherwise read the pre-toggle
        // value and momentarily revert the cached state).
        //
        // If PA reports !success or the wait deadline trips, treat the
        // attempt optimistically: re-query the current state and return
        // that. The set-mute call may still have taken effect on the
        // device even when the ack didn't come back in time; re-querying
        // gives the caller (and the OSD) the truth instead of erroring
        // out and leaving the UI silent on transient PA hiccups.
        let write = self.pa_write_with_result(|done| {
            let mut introspect = self.context.borrow().introspect();
            let cb: Box<dyn FnMut(bool) + 'static> = Box::new(move |ok| {
                *done.borrow_mut() = Some(ok);
            });
            match target {
                Target::DefaultSink => Some(introspect.set_sink_mute_by_name(
                    target.pa_name(),
                    new_mute,
                    Some(cb),
                )),
                Target::DefaultSource => Some(introspect.set_source_mute_by_name(
                    target.pa_name(),
                    new_mute,
                    Some(cb),
                )),
            }
        });
        match write {
            Ok(true) => Ok(new_mute),
            Ok(false) => {
                warn!("PA reported failure setting mute on {}; re-querying actual state", target.label());
                self.query_default_mute(target)
            }
            Err(e) => {
                warn!("PA mute write did not complete on {} ({e}); re-querying actual state", target.label());
                self.query_default_mute(target)
            }
        }
    }

    /// Collect all sink inputs with their properties.
    fn collect_sink_inputs(&self) -> Result<Vec<SinkInputEntry>> {
        let mut result: Vec<SinkInputEntry> = self.pa_collect(|entries, done| {
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
                    let name_lower = name.to_lowercase();
                    let binary_lower = binary.as_deref().map(str::to_lowercase);
                    entries.borrow_mut().push(SinkInputEntry {
                        index: info.index,
                        name,
                        name_lower,
                        binary,
                        binary_lower,
                        pid,
                        comm: None, // populated after PID resolution below
                        client_index: info.client.unwrap_or(u32::MAX),
                        channels: info.volume.len(),
                        muted: info.mute,
                    });
                }
                ListResult::End | ListResult::Error => {
                    *done.borrow_mut() = true;
                }
            });
        })?;

        // Resolve PIDs via client lookup for entries missing a PID
        for entry in &mut result {
            if entry.pid.is_none() && entry.client_index != u32::MAX {
                entry.pid = self.get_client_pid(entry.client_index)?;
            }
        }

        // Cache /proc/<pid>/comm (lower-cased) so matches() doesn't read it
        // per match attempt.
        for entry in &mut result {
            if let Some(ref pid) = entry.pid
                && pid.chars().all(|c| c.is_ascii_digit())
                    && let Ok(raw) = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                {
                    entry.comm = Some(raw.trim().to_lowercase());
                }
        }

        Ok(result)
    }

    /// Look up a client's PID via pipewire.sec.pid property.
    fn get_client_pid(&self, client_index: u32) -> Result<Option<String>> {
        self.pa_query::<Option<String>, _>(None, |pid, done| {
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
                    *pid.borrow_mut() = p;
                }
                *done.borrow_mut() = true;
            });
        })
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
        self.set_default_volume(Target::DefaultSink, value)
    }

    pub fn set_mic_volume(&self, value: u8) -> Result<()> {
        self.set_default_volume(Target::DefaultSource, value)
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
            cv.set(entry.channels, volume);
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
        let entries: Vec<SrInfo<'static>> = self.pa_collect(|entries, done| {
            let mut sr = self.context.borrow().stream_restore();
            let _op = sr.read(move |result| match result {
                ListResult::Item(info) => {
                    entries.borrow_mut().push(info.to_owned());
                }
                ListResult::End | ListResult::Error => {
                    *done.borrow_mut() = true;
                }
            });
        })?;

        let mut updated: Vec<SrInfo<'static>> = Vec::new();
        for entry in entries.iter() {
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
        let success = self.pa_write_with_result(|result| {
            let mut sr = self.context.borrow().stream_restore();
            Some(sr.write(UpdateMode::Replace, &refs, false, move |success| {
                *result.borrow_mut() = Some(success);
            }))
        })?;
        if !success {
            bail!("stream-restore write rejected by server");
        }

        let touched: Vec<&str> = updated
            .iter()
            .filter_map(|e| e.name.as_deref())
            .collect();
        debug!(
            "updated {} stream-restore entries for {target}: {touched:?}",
            updated.len()
        );
        Ok(())
    }

    /// Returns the new mute state (true = muted).
    pub fn toggle_system_mute(&self) -> Result<bool> {
        self.toggle_default_mute(Target::DefaultSink)
    }

    /// Returns the new mute state (true = muted).
    pub fn toggle_mic_mute(&self) -> Result<bool> {
        self.toggle_default_mute(Target::DefaultSource)
    }

    /// Current mute state of the default microphone (true = muted).
    pub fn is_mic_muted(&self) -> Result<bool> {
        self.query_default_mute(Target::DefaultSource)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_from_u8_endpoints() {
        // 0 → silent, 255 → exactly NORMAL (100%).
        assert_eq!(volume_from_u8(0).0, 0);
        assert_eq!(volume_from_u8(255).0, Volume::NORMAL.0);
    }

    #[test]
    fn volume_from_u8_midpoint() {
        // 127 ≈ 49.8%, 128 ≈ 50.2% — round-to-nearest puts midpoint near 50%.
        let mid_lo = volume_from_u8(127).0 as f64 / Volume::NORMAL.0 as f64;
        let mid_hi = volume_from_u8(128).0 as f64 / Volume::NORMAL.0 as f64;
        assert!((0.49..=0.51).contains(&mid_lo), "127 → {mid_lo}");
        assert!((0.49..=0.51).contains(&mid_hi), "128 → {mid_hi}");
    }

    #[test]
    fn volume_from_u8_monotonic_at_low_end() {
        // 1 must produce a strictly higher volume than 0 (a slider just
        // off the floor should be audibly distinguishable from muted).
        assert!(volume_from_u8(1).0 > volume_from_u8(0).0);
    }

    #[test]
    fn volume_from_u8_near_max_doesnt_exceed_normal() {
        // 254 must be strictly below NORMAL (255 = NORMAL); the cast
        // shouldn't overshoot.
        assert!(volume_from_u8(254).0 < Volume::NORMAL.0);
    }
}
