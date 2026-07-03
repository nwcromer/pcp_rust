use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
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
use pulse::time::MicroSeconds;
use pulse::volume::{ChannelVolumes, Volume};

/// How long an app's volume must be stable before we persist it to the
/// stream-restore database — coalesces a slider burst into one DB write.
const PERSIST_IDLE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy)]
struct PendingPersist {
    volume: Volume,
    last_update: Instant,
}

/// Distinguishes the two "default audio device" targets we drive — system
/// output (sink) and mic input (source). Dedupes the near-identical
/// per-system/per-mic methods.
///
/// Review-accepted: this centralizes the *data* differences (pa_name, label,
/// default_channels), but four methods keep a sink/source match differing only
/// in the libpulse method name — those are distinct non-generic methods over
/// distinct payload types, so collapsing needs a macro or bridging trait, not
/// worth it for a fixed 2-variant set.
#[derive(Clone, Copy)]
enum Target {
    DefaultSink,
    DefaultSource,
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

/// Info collected from a sink input for matching and display. Lower-cased
/// `name`/`binary` are precomputed in `collect_sink_inputs` so a match is a
/// pure `&str::contains` on the ~10 Hz hot path. The `/proc/<pid>/comm`
/// fallback is resolved lazily in `entry_matches`, not eagerly here.
#[derive(Clone)]
struct SinkInputEntry {
    index: u32,
    name: String,
    name_lower: String,
    binary: Option<String>,
    binary_lower: Option<String>,
    pid: Option<String>,
    client_index: u32,
    channels: u8,
    muted: bool,
}

pub struct AudioController {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<context::Context>>,
    /// Per-app deferred stream-restore writes. `set_app_volumes` updates the
    /// entry each tick; `flush_persist_writes` writes to PA once it's been
    /// idle for `PERSIST_IDLE`.
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

        // Wait for Ready, bounded by a wall-clock deadline covering the full
        // handshake (connect → auth → negotiate → Ready) — longer than the
        // per-op `wait_until` (250ms, run on an already-Ready context). Kept
        // under AudioReconnector's MAX_BACKOFF so a wedged server can't freeze
        // the reconnect path past a backoff gap.
        //
        // Poll non-blocking (`iterate(false)`) plus a short sleep, not
        // `iterate(true)`: a blocking iterate can park in poll() forever if the
        // peer accepts the socket but never replies — the wedged handshake this
        // deadline defends against. The sleep caps the spin at ~100 wakeups/sec.
        const CONNECT_DEADLINE: Duration = Duration::from_secs(3);
        let started = Instant::now();
        loop {
            match mainloop.borrow_mut().iterate(false) {
                IterateResult::Success(_) => {}
                IterateResult::Err(e) => bail!("mainloop iterate error: {e}"),
                IterateResult::Quit(_) => bail!("mainloop quit unexpectedly"),
            }
            match context.borrow().get_state() {
                context::State::Ready => break,
                context::State::Failed | context::State::Terminated => {
                    bail!("PulseAudio connection failed");
                }
                _ => {
                    if started.elapsed() >= CONNECT_DEADLINE {
                        bail!("PulseAudio connect did not reach Ready within {CONNECT_DEADLINE:?}");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }

        info!("connected to PulseAudio");

        Ok(Self { mainloop, context, pending_persist: HashMap::new() })
    }

    /// True while the PA context is `Ready`. Reads the state cached from the
    /// last mainloop iteration (no I/O), so it reflects a server death only
    /// after some op has iterated the loop — which any failed op already does
    /// via `wait_until`. The main loop polls this to decide whether to reconnect.
    pub fn is_connected(&self) -> bool {
        matches!(self.context.borrow().get_state(), context::State::Ready)
    }

    /// Rebuild the connection after the server dropped (PA/PipeWire restart,
    /// suspend/resume). Builds a fresh controller first, replacing the old one
    /// only on success — a still-down server leaves `self` intact to retry.
    /// Replacing runs the old `Drop` (disconnects the stale context) and
    /// discards `pending_persist` (deferred writes for a session that's gone).
    pub fn reconnect(&mut self) -> Result<()> {
        *self = Self::connect()?;
        Ok(())
    }

    fn wait_for(&self, done: &RefCell<bool>) -> Result<()> {
        self.wait_until(|| *done.borrow())
    }

    /// Drive the PulseAudio mainloop until `is_done`, or error if the context
    /// disconnects, the mainloop quits, or the wall-clock deadline trips.
    ///
    /// Erroring rather than silently breaking out means callers propagate the
    /// failure — a visible journal error instead of stale defaults from an
    /// apparently-successful call.
    fn wait_until<F: Fn() -> bool>(&self, is_done: F) -> Result<()> {
        const DEADLINE: Duration = Duration::from_millis(250);
        let started = Instant::now();
        loop {
            if is_done() {
                return Ok(());
            }
            // Budget before the hard deadline; bail once gone. This ceiling stops
            // a server that holds the socket but never replies from hanging us.
            let Some(remaining) = DEADLINE
                .checked_sub(started.elapsed())
                .filter(|r| !r.is_zero())
            else {
                bail!("PulseAudio call exceeded {DEADLINE:?} deadline");
            };

            // Block inside poll() for up to `remaining` rather than spinning a
            // fixed sleep: we wake the instant PA has data (no latency on the
            // volume hot path), yet prepare()'s timeout keeps poll from parking
            // past the deadline. The canonical prepare/poll/dispatch triple —
            // like iterate(), but with a bounded poll timeout it doesn't expose.
            //
            // Holding the mainloop borrow across the blocking poll is safe: the
            // controller is single-threaded and the introspect callbacks run in
            // dispatch(), touching only the per-op cells, never the mainloop.
            let timeout = MicroSeconds(remaining.as_micros().min(i32::MAX as u128) as u64);
            {
                let mut ml = self.mainloop.borrow_mut();
                ml.prepare(Some(timeout))
                    .map_err(|e| anyhow::anyhow!("PulseAudio mainloop prepare failed: {e:?}"))?;
                // EINTR: libpulse's pa_mainloop_poll remaps a signal-interrupted
                // poll to Ok(0), same as a plain timeout — we fall through to
                // dispatch (no-op) and loop, re-polling the remaining time. That
                // re-poll IS the EINTR retry; the top-of-loop deadline bounds it.
                //
                // Review-accepted: do NOT add an explicit EINTR retry on the Err
                // arm. EINTR never reaches it (remapped to Ok(0) above), and a
                // genuine poll Err leaves the mainloop failed where prepare aborts
                // via pa_assert — retrying could crash the daemon; bailing is safe.
                ml.poll()
                    .map_err(|e| anyhow::anyhow!("PulseAudio mainloop poll failed: {e:?}"))?;
                ml.dispatch()
                    .map_err(|e| anyhow::anyhow!("PulseAudio mainloop dispatch failed: {e:?}"))?;
            }

            // PA context can transition out of Ready (server restart) mid-op;
            // the iteration succeeds while the context becomes unusable. Check
            // explicitly so subsequent calls don't silently use stale data.
            match self.context.borrow().get_state() {
                context::State::Ready => {}
                other => bail!("PulseAudio context not ready: {other:?}"),
            }
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

    /// Run a PA write that reports success/failure via a bool callback (e.g.
    /// `sr.write`). The `register` closure must return the `Operation` handle
    /// so we can `cancel()` it if the wait deadline trips. Returns the reported
    /// success state, or an error if the op didn't complete in time.
    ///
    /// Note on cancel(): `pa_operation_cancel` tells PA to abort the op, which
    /// is what prevents a UAF — without it PA could invoke the boxed callback
    /// into the `Rc<RefCell<Option<bool>>>` we just dropped. It does NOT free
    /// the boxed callback in libpulse-binding 2.30.1: `Operation::from_raw` has
    /// an inverted `saved_cb` condition that stores `None` for any non-null
    /// callback pointer, so cancel()'s drop branch never runs — the boxed FnMut
    /// leaks on each deadline trip (bounded by how often PA fails, so small).
    /// Our fork patch (`fix-ext-stream-restore-write`) doesn't fix this.
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
            // Tell PA to abort so it doesn't fire the callback into the
            // about-to-be-dropped result Rc. See the doc comment's leak caveat.
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
    /// label-distinguished message for "enumeration ended without an Item"
    /// (not present) vs. an enumeration Error (server-side failure).
    ///
    /// The pa_query value is `(Option<bool>, bool)` = `(mute, errored)`. Initial
    /// `(None, false)` covers both "no callback yet" and "End without Item" (not
    /// present), so End is a no-op; Item-then-End leaves the value alone. Error
    /// only sets the flag and never rolls back, so arrival order doesn't matter.
    fn query_default_mute(&self, target: Target) -> Result<bool> {
        let name = target.pa_name();
        let (mute, errored): (Option<bool>, bool) =
            self.pa_query((None, false), |out, done| {
                let introspect = self.context.borrow().introspect();
                match target {
                    Target::DefaultSink => {
                        let _op = introspect.get_sink_info_by_name(name, move |result| {
                            match result {
                                ListResult::Item(sink) => {
                                    out.borrow_mut().0 = Some(sink.mute);
                                }
                                ListResult::End => {}
                                ListResult::Error => {
                                    out.borrow_mut().1 = true;
                                }
                            }
                            *done.borrow_mut() = true;
                        });
                    }
                    Target::DefaultSource => {
                        let _op = introspect.get_source_info_by_name(name, move |result| {
                            match result {
                                ListResult::Item(source) => {
                                    out.borrow_mut().0 = Some(source.mute);
                                }
                                ListResult::End => {}
                                ListResult::Error => {
                                    out.borrow_mut().1 = true;
                                }
                            }
                            *done.borrow_mut() = true;
                        });
                    }
                }
            })?;
        if errored {
            bail!("PulseAudio enumeration error while querying {}", target.label());
        }
        mute.with_context(|| format!("{} not found", target.label()))
    }

    fn set_default_volume(&self, target: Target, value: u8) -> Result<()> {
        let volume = volume_from_u8(value);
        // Review-accepted: re-queried every tick (not cached like the app path)
        // so a default-device switch picks up the new channel count.
        let channels = self.query_default_channels(target)?;
        let mut cv = ChannelVolumes::default();
        // Review-accepted: cv.set writes one level to all channels, flattening
        // L/R balance — intended; a hardware fader sets an absolute level.
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
        debug!("set {} volume: {} ({}%)", target.label(), value, value_to_percent(value));
        Ok(())
    }

    fn toggle_default_mute(&self, target: Target) -> Result<bool> {
        let new_mute = !self.query_default_mute(target)?;
        // Wait for PA to apply the mute before returning, so the caller can
        // cache the new state without racing the next mic-mute poll (which
        // would read the pre-toggle value and momentarily revert the cache).
        //
        // If PA reports !success or the wait deadline trips, re-query and
        // return the actual state: the set-mute may have taken effect even when
        // the ack didn't arrive in time, so re-querying gives the truth instead
        // of erroring out and leaving the UI silent on transient PA hiccups.
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

    /// Enumerate all sink inputs with the fields PA gives us directly.
    /// Deliberately does NOT resolve PIDs via the client (a PA round-trip) or
    /// read `/proc/<pid>/comm` — deferred to `entry_matches` (lazy) and
    /// `list_apps` (cold path) so the ~10 Hz hot path skips them per tick.
    fn collect_sink_inputs(&self) -> Result<Vec<SinkInputEntry>> {
        self.pa_collect(|entries, done| {
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
                        client_index: info.client.unwrap_or(u32::MAX),
                        channels: info.volume.len(),
                        muted: info.mute,
                    });
                }
                ListResult::End | ListResult::Error => {
                    *done.borrow_mut() = true;
                }
            });
        })
    }

    /// Whether a sink input matches `target` (already lower-cased). Checks the
    /// cheap `name`/`binary` fields first and only falls back to the PA
    /// client-PID query + `/proc/comm` read on a miss, so the hot path rarely
    /// resolves a PID; the comm read is further memoized by `comm_for_pid`.
    fn entry_matches(&self, entry: &SinkInputEntry, target: &str) -> Result<bool> {
        // Cheap legs first (name/binary), so a hit avoids resolving the PID.
        if entry_matches_target(entry, target, None) {
            return Ok(true);
        }
        // Fallback: the stream's lower-cased /proc/<pid>/comm.
        Ok(entry_matches_target(entry, target, self.resolve_comm(entry)?.as_deref()))
    }

    /// Resolve the lower-cased `/proc/<pid>/comm` for a sink input (possibly
    /// via a PA client-PID query). `Ok(None)` when there's no usable numeric
    /// PID or the comm can't be read. Factored out so the comm is resolved once
    /// and reused across match targets (see `set_app_volumes`).
    fn resolve_comm(&self, entry: &SinkInputEntry) -> Result<Option<String>> {
        let Some(pid) = self.resolve_pid(entry)? else {
            return Ok(None);
        };
        if !pid.chars().all(|c| c.is_ascii_digit()) {
            return Ok(None);
        }
        Ok(comm_for_pid(&pid))
    }

    /// The PID for a sink input: the inline proplist value if present, else
    /// a PA client-info query (pipewire.sec.pid / application.process.id).
    fn resolve_pid(&self, entry: &SinkInputEntry) -> Result<Option<String>> {
        match &entry.pid {
            Some(p) => Ok(Some(p.clone())),
            None if entry.client_index != u32::MAX => self.get_client_pid(entry.client_index),
            None => Ok(None),
        }
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
        let mut entries = self.collect_sink_inputs()?;
        // Resolve PIDs via the client for streams whose proplist lacked one.
        // Only on the cold `--list-apps` path, never on the volume hot path.
        for entry in &mut entries {
            if entry.pid.is_none() && entry.client_index != u32::MAX {
                entry.pid = self.get_client_pid(entry.client_index)?;
            }
        }
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

    /// Set volume for several named apps in ONE sink-input enumeration.
    /// Returns, per input name (same order), whether any stream matched. Each
    /// stream's `/proc/comm` is resolved at most once and tested against every
    /// target, so a multi-app control doesn't re-enumerate per target per tick.
    pub fn set_app_volumes(&mut self, app_names: &[&str], value: u8) -> Result<Vec<bool>> {
        if app_names.is_empty() {
            return Ok(Vec::new());
        }
        let volume = volume_from_u8(value);
        let targets: Vec<String> = app_names.iter().map(|n| n.to_lowercase()).collect();
        let entries = self.collect_sink_inputs()?;

        // Match every stream against every target. Name/binary legs are free; a
        // stream only pays a PA client-PID query + /proc/comm read if a target
        // misses those, and then the comm is resolved once and reused.
        let mut matched = vec![false; targets.len()];
        let mut to_set: Vec<(u32, u8)> = Vec::new(); // streams to write, one per matched stream
        for entry in &entries {
            let mut comm: Option<Option<String>> = None; // outer None = not yet resolved
            let mut entry_hit = false;
            for (i, target) in targets.iter().enumerate() {
                let hit = if entry_matches_target(entry, target, None) {
                    true
                } else {
                    // Resolve (once) and reuse this stream's comm. A failed query
                    // is logged as "no comm", so it can't drop name/binary matches.
                    let comm = comm.get_or_insert_with(|| match self.resolve_comm(entry) {
                        Ok(c) => c,
                        Err(e) => {
                            debug!("match check failed for sink-input {}: {e}", entry.index);
                            None
                        }
                    });
                    entry_matches_target(entry, target, comm.as_deref())
                };
                if hit {
                    matched[i] = true;
                    entry_hit = true;
                }
            }
            if entry_hit {
                to_set.push((entry.index, entry.channels));
            }
        }

        let mut introspect = self.context.borrow().introspect();
        for &(index, channels) in &to_set {
            let mut cv = ChannelVolumes::default();
            // Review-accepted: flattens L/R balance by design — see set_default_volume.
            cv.set(channels, volume);
            let _op = introspect.set_sink_input_volume(index, &cv, None);
        }
        drop(introspect);
        self.drain();

        // Per matched target: stash the new volume for deferred stream-restore
        // persistence (a slider burst coalesces to one DB write per app once
        // idle for PERSIST_IDLE). The playback volume above is set every tick.
        //
        // Only *matched* (running) targets are persisted, deliberately.
        // Review-accepted: do NOT "fix" this to persist offline targets so a
        // fader move on a closed app pre-sets its next launch — verified a no-op
        // on PipeWire/WirePlumber, which restores from its own `stream-properties`
        // store (updated only by observing a *live* stream) and never consults
        // our pulse-side `module-stream-restore` writes (confirmed: the write
        // succeeds, WP's store untouched). Persistence rides on WP seeing the live
        // `set` above; no live stream, nothing to observe. (A real PulseAudio
        // server WOULD honor these, so we keep them.)
        for (i, target) in targets.iter().enumerate() {
            if matched[i] {
                self.pending_persist.insert(
                    target.clone(),
                    PendingPersist { volume, last_update: Instant::now() },
                );
            } else {
                debug!("app not found: {}", app_names[i]);
            }
        }

        Ok(matched)
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

    /// Updates module-stream-restore entries whose name contains `target`
    /// (case-insensitive). The `updater` closure receives each matching entry
    /// and returns its replacement — use it to bump volume, mute, or both.
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
        // Debug-only test hook: with PCPANEL_FORCE_MIC_ERROR set, always fail —
        // lets you verify the mic indicator's stale ("unknown") blink without
        // bringing down PulseAudio. Stripped from release by cfg!(debug_assertions).
        if cfg!(debug_assertions) && std::env::var_os("PCPANEL_FORCE_MIC_ERROR").is_some() {
            bail!("forced failure (PCPANEL_FORCE_MIC_ERROR set)");
        }
        self.query_default_mute(Target::DefaultSource)
    }

    /// Returns the new mute state, or None if the app wasn't found.
    /// If any matched streams are unmuted, mutes all. If all are muted, unmutes all.
    pub fn toggle_app_mute(&self, app_name: &str) -> Result<Option<bool>> {
        let target = app_name.to_lowercase();
        let entries = self.collect_sink_inputs()?;

        // Same lazy match as set_app_volumes (name/binary free, comm only on
        // miss) so the slider and the button match an app identically.
        let mut matched: Vec<(u32, bool)> = Vec::new(); // (index, muted)
        for entry in &entries {
            // Skip (don't abort) a stream we can't classify — see the
            // matching note in set_app_volumes.
            match self.entry_matches(entry, &target) {
                Ok(true) => matched.push((entry.index, entry.muted)),
                Ok(false) => {}
                Err(e) => debug!("match check failed for sink-input {}: {e}", entry.index),
            }
        }

        if matched.is_empty() {
            debug!("app not found for mute toggle: {app_name}");
            return Ok(None);
        }

        // If any are unmuted, mute all. If all are muted, unmute all.
        let any_unmuted = matched.iter().any(|&(_, muted)| !muted);
        let new_mute = any_unmuted;

        let mut introspect = self.context.borrow().introspect();
        for &(index, _) in &matched {
            let _op = introspect.set_sink_input_mute(index, new_mute, None);
        }
        self.drain();

        // Persist the mute state so new streams for this app pick it up —
        // mirrors the volume-persist behavior in set_app_volumes.
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

/// Whether a sink-input entry matches `target`. Pure: uses only the entry's
/// resolved fields plus an optional pre-resolved `comm` (lower-cased
/// `/proc/<pid>/comm`, fetched separately as it can need a PA round-trip).
/// All inputs are lower-cased, so this is a case-insensitive substring test
/// with name → binary → comm precedence; `comm = None` checks only name/binary.
fn entry_matches_target(entry: &SinkInputEntry, target: &str, comm: Option<&str>) -> bool {
    entry.name_lower.contains(target)
        || entry.binary_lower.as_deref().is_some_and(|b| b.contains(target))
        || comm.is_some_and(|c| c.contains(target))
}

/// Lower-cased `/proc/<pid>/comm`, memoized process-wide. A PID's comm is
/// stable for that process's lifetime, so caching turns the repeated
/// filesystem reads on the ~10 Hz slider hot path into one read per PID.
///
/// Review-accepted: `main.rs` `list_apps` reads `/proc/<pid>/comm` too, but
/// that cold path keeps original case and skips this cache on purpose, so the
/// small overlap is left un-factored. `None` (no such process / unreadable) is
/// cached too, so a never-matching stream isn't re-read every tick.
///
/// PID reuse after a process exits could return a stale comm, but the only
/// consequence is an app-name match decision for a volume/mute change —
/// low-stakes and self-correcting once the stream goes away.
///
/// Review-accepted: keyed by PID and never evicts, so a long-lived daemon
/// accrues one entry per distinct PID seen on a non-name/binary match — in
/// practice single-digit MB even under pathological churn; no eviction needed.
fn comm_for_pid(pid: &str) -> Option<String> {
    static CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    if let Some(cached) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(pid) {
        return cached.clone();
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|raw| raw.trim().to_lowercase());
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pid.to_string(), comm.clone());
    comm
}

fn volume_from_u8(value: u8) -> Volume {
    // Map 0-255 to 0-100% (PA_VOLUME_NORM)
    let fraction = value as f64 / 255.0;
    let raw = (fraction * f64::from(Volume::NORMAL.0) + 0.5) as u32;
    Volume(raw)
}

/// Map a raw slider/knob value (0-255) to a display percentage (0-100),
/// rounded to nearest. Mirrors `volume_from_u8`'s rounding so the OSD,
/// `--verbose`, and the debug log agree with the volume applied to PA — a
/// plain `as u8` truncation reads up to 1% low. Endpoints are exact.
pub fn value_to_percent(value: u8) -> u8 {
    (value as f32 / 255.0 * 100.0).round() as u8
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
    fn value_to_percent_endpoints_exact() {
        assert_eq!(value_to_percent(0), 0);
        assert_eq!(value_to_percent(255), 100);
    }

    #[test]
    fn value_to_percent_rounds_to_nearest() {
        // 129 is ~50.6%: truncation gave 50, rounding gives 51 to match the
        // volume actually applied (volume_from_u8 rounds the same way).
        assert_eq!(value_to_percent(129), 51);
        // 128 is ~50.2% → 50 either way.
        assert_eq!(value_to_percent(128), 50);
    }

    #[test]
    fn value_to_percent_is_monotonic() {
        let mut prev = 0;
        for v in 0..=255u8 {
            let p = value_to_percent(v);
            assert!(p >= prev, "percent decreased at value {v}: {p} < {prev}");
            prev = p;
        }
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

    /// Build a SinkInputEntry with the fields `entry_matches_target` reads
    /// (rest are filler); `name`/`binary` lower-cased as `collect_sink_inputs`.
    fn entry(name: &str, binary: Option<&str>) -> SinkInputEntry {
        SinkInputEntry {
            index: 0,
            name: name.to_string(),
            name_lower: name.to_lowercase(),
            binary: binary.map(String::from),
            binary_lower: binary.map(|b| b.to_lowercase()),
            pid: None,
            client_index: u32::MAX,
            channels: 2,
            muted: false,
        }
    }

    #[test]
    fn entry_matches_target_by_name_substring_case_insensitive() {
        let e = entry("Firefox", Some("firefox-bin"));
        // target is lower-cased by the caller; name_lower contains it.
        assert!(entry_matches_target(&e, "fire", None));
        assert!(entry_matches_target(&e, "firefox", None));
    }

    #[test]
    fn entry_matches_target_by_binary() {
        // Generic display name, but the binary identifies the app.
        let e = entry("AudioStream", Some("dota2"));
        assert!(entry_matches_target(&e, "dota2", None));
    }

    #[test]
    fn entry_matches_target_no_match_on_name_or_binary() {
        let e = entry("AudioStream", Some("dota2"));
        assert!(!entry_matches_target(&e, "mumble", None));
    }

    #[test]
    fn entry_matches_target_comm_fallback_only_when_provided() {
        // Name and binary don't match; the comm does. With comm = None
        // (cheap legs only) it must miss; with the comm supplied it matches.
        let e = entry("pipewire stream", None);
        assert!(!entry_matches_target(&e, "dota", None));
        assert!(entry_matches_target(&e, "dota", Some("dota2")));
    }

    #[test]
    fn entry_matches_target_no_binary_is_safe() {
        let e = entry("Mumble", None);
        assert!(entry_matches_target(&e, "mumble", None));
        assert!(!entry_matches_target(&e, "nope", None));
    }
}
