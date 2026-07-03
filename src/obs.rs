//! OBS Studio integration via obs-websocket v5 (using the obws crate).
//!
//! Runs on a dedicated OS thread that owns a tokio runtime. Communicates
//! with the rest of pcp_rust over two bounded mpsc channels:
//!   main → OBS thread:  `ObsCommand` (button-triggered actions)
//!   OBS thread → main:  `ObsEvent`   (state changes and command results)
//!
//! Reconnects automatically with exponential backoff when OBS is absent.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::sleep;
use tokio_stream::StreamExt;

use crate::config::ObsConfig;

#[derive(Debug, Clone, Copy)]
pub enum ObsCommand {
    SaveReplay,
    ToggleRecording,
    PauseRecording,
    SplitRecording,
}

impl ObsCommand {
    pub fn label(self) -> &'static str {
        match self {
            ObsCommand::SaveReplay => "save replay buffer",
            ObsCommand::ToggleRecording => "toggle recording",
            ObsCommand::PauseRecording => "pause/resume recording",
            ObsCommand::SplitRecording => "split recording file",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ObsEvent {
    Connected,
    Disconnected,
    /// Recording became active and is not paused.
    RecordingActive,
    /// Recording stopped (no longer active).
    RecordingStopped,
    /// Recording is active but paused.
    RecordingPaused,
    /// Recording resumed from paused (active, not paused).
    RecordingResumed,
    /// Replay buffer is running and accepting frames.
    ReplayBufferActive,
    /// Replay buffer is stopped.
    ReplayBufferInactive,
    /// An OBS command completed successfully.
    CommandSucceeded(ObsCommand),
    /// An OBS command failed (e.g., OBS rejected it or we're disconnected).
    CommandFailed(ObsCommand, String),
    /// Matching the canvas to the monitor failed while pcp_rust was about to
    /// start the replay buffer on connect, so the buffer was *not* started.
    /// Surfaced as an error flash, mirroring a failed record-start — this is
    /// not tied to a button press, so it can't reuse `CommandFailed`.
    CanvasMatchFailed(String),
    /// Capture-liveness poll result: `true` = OBS is capturing (or has no
    /// capture source), `false` = an enabled capture is present but blind
    /// (0x0). Only emitted when `show_capture_status` is on.
    CaptureLive(bool),
}

/// Bound for the cross-thread mpsc channels. Both directions carry events
/// at human rates (button presses, recording state transitions) so 64 is
/// generous; the bound exists only so a wedged consumer can't accumulate
/// unbounded memory.
const CHANNEL_CAP: usize = 64;

pub struct ObsHandle {
    pub commands_tx: Sender<ObsCommand>,
    pub events_rx: Receiver<ObsEvent>,
}

/// Spawn the OBS background thread and return channels for the main thread
/// to interact with it. Returns `None` if the OS won't let us spawn the
/// thread (extreme resource exhaustion) — callers treat that the same as
/// "no [obs] configured": OBS actions error-flash, audio control keeps
/// working.
pub fn spawn_obs_thread(config: ObsConfig) -> Option<ObsHandle> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<ObsCommand>(CHANNEL_CAP);
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<ObsEvent>(CHANNEL_CAP);

    let spawn_result = std::thread::Builder::new()
        .name("pcp-obs".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("OBS thread: failed to create tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(obs_main_loop(config, cmd_rx, event_tx));
        });

    match spawn_result {
        Ok(_) => Some(ObsHandle { commands_tx: cmd_tx, events_rx: event_rx }),
        Err(e) => {
            warn!("failed to spawn OBS thread: {e}; OBS integration disabled");
            None
        }
    }
}

/// How often to re-check capture liveness for the `idle_not_capturing` panel
/// color. Capture state only changes on user action, so a slow poll is plenty.
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_secs(1);

const BACKOFF_INITIAL_SECS: u64 = 2;
const BACKOFF_MAX_SECS: u64 = 30;
/// A session must dwell at least this long for us to consider it "stable"
/// and reset the reconnect backoff. Shorter sessions (e.g. OBS accepts the
/// WebSocket handshake but immediately rejects the event subscription
/// because of an auth version mismatch or a plugin restart) get treated
/// as failed attempts so the backoff keeps growing — otherwise the
/// reconnect loop fires as fast as the network allows.
const STABLE_SESSION_DWELL: Duration = Duration::from_secs(30);

/// Best-effort check for whether the configured OBS host is on the local
/// machine. obs-websocket is unencrypted plain ws://, so a non-localhost
/// host means we send the configured password over the network in
/// cleartext. We warn once at startup rather than refuse — the user may
/// have a secure tunnel (SSH, VPN, etc.) the program can't see.
fn host_is_local(host: &str) -> bool {
    // Tolerate an FQDN trailing dot ("localhost.") and IPv6 brackets
    // ("[::1]"), then accept case-insensitive "localhost" or any address std
    // considers loopback (the whole 127.0.0.0/8 block plus ::1). Best-effort
    // string heuristic, not name resolution: an /etc/hosts alias pointing at
    // loopback still reads as non-local and warns — a harmless spurious
    // advisory, not worth a startup DNS lookup.
    let h = host.trim_end_matches('.');
    let h = h.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(h);
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

async fn obs_main_loop(
    config: ObsConfig,
    mut cmd_rx: Receiver<ObsCommand>,
    event_tx: Sender<ObsEvent>,
) {
    if !host_is_local(&config.host) && config.password.is_some() {
        warn!(
            "OBS: host {:?} is not localhost — the obs-websocket protocol is \
             plain ws:// (no TLS), so your password will be sent over the \
             network in cleartext. Tunnel through SSH/VPN if this matters.",
            config.host
        );
    }
    debug!("OBS: connecting to {}:{}", config.host, config.port);
    let mut backoff = BACKOFF_INITIAL_SECS;

    loop {
        match try_connect(&config).await {
            Ok(client) => {
                info!("OBS: connected to {}:{}", config.host, config.port);
                let _ = event_tx.send(ObsEvent::Connected).await;
                let session_start = Instant::now();

                // Run the session until the connection drops or the main
                // thread closes the command channel. Err = connection-level
                // failure; Ok = main thread closed cmd_rx (graceful
                // shutdown — the process is exiting, don't reconnect).
                match run_session(&client, &config, &mut cmd_rx, &event_tx).await {
                    Ok(()) => {
                        debug!("OBS: main thread closed command channel; exiting thread");
                        return;
                    }
                    Err(e) => info!("OBS: disconnected ({e})"),
                }

                // Snapshot the dwell BEFORE the post-disconnect send. The
                // send below is a `.await` on a bounded channel and can
                // block if main is slow to drain events — measuring after
                // would inflate dwell by main's stall time and classify
                // an instantly-dying session as stable.
                let dwell = session_start.elapsed();
                let _ = event_tx.send(ObsEvent::Disconnected).await;

                if dwell >= STABLE_SESSION_DWELL {
                    // Stable session — reset backoff so the next reconnect
                    // (e.g. user restarted OBS) is quick.
                    backoff = BACKOFF_INITIAL_SECS;
                } else {
                    // Flapping — session died too quickly. Treat as a failed
                    // attempt so we don't hammer the network/journal.
                    debug!(
                        "OBS: session ended after {}s; backing off {backoff}s before retry",
                        dwell.as_secs()
                    );
                    sleep(Duration::from_secs(backoff)).await;
                    // Review-accepted: duplicated backoff arithmetic (both retry paths) —
                    // see AudioReconnector::tick.
                    backoff = (backoff * 2).min(BACKOFF_MAX_SECS);
                }
            }
            Err(e) => {
                debug!("OBS: connect failed ({e}); retrying in {backoff}s");
                sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX_SECS);
            }
        }
    }
}

async fn try_connect(config: &ObsConfig) -> Result<obws::Client> {
    let client = obws::Client::connect(
        &config.host,
        config.port,
        config.password.as_deref(),
    )
    .await?;
    Ok(client)
}

async fn run_session(
    client: &obws::Client,
    config: &ObsConfig,
    cmd_rx: &mut Receiver<ObsCommand>,
    event_tx: &Sender<ObsEvent>,
) -> Result<()> {
    // Subscribe to the OBS event stream FIRST — before sending any commands
    // or queries. obws's stream queues incoming events internally, so any
    // state-change events fired by OBS during the rest of this setup (e.g.,
    // ReplayBufferStateChanged when we start the buffer below) are captured
    // and processed by the main loop once we enter it. Subscribing later
    // would lose events fired between command completion and subscription.
    let events = client.events()?;
    tokio::pin!(events);

    // Publish initial recording state so the LEDs reflect reality immediately
    // after connection. OBS only emits RecordStateChanged on transitions, so a
    // failed seed while OBS is already recording strands the LEDs at idle for
    // the whole session — retry, then warn so it's not a silent drop.
    let mut status = client.recording().status().await;
    for _ in 0..2 {
        if status.is_ok() {
            break;
        }
        status = client.recording().status().await;
    }
    match status {
        Ok(status) => {
            if status.active {
                if status.paused {
                    let _ = event_tx.send(ObsEvent::RecordingPaused).await;
                } else {
                    let _ = event_tx.send(ObsEvent::RecordingActive).await;
                }
            } else {
                let _ = event_tx.send(ObsEvent::RecordingStopped).await;
            }
        }
        Err(e) => warn!(
            "OBS: failed to seed recording status; LEDs may show idle until the \
             next record event: {e}"
        ),
    }

    // Determine the replay-buffer state and optionally start it. Runs on EVERY
    // successful connect, including reconnects. We don't monitor or re-enable
    // during a session — if the user stops it via OBS, it stays stopped until
    // the next reconnect.
    //
    // After a successful `start()` we know the buffer is active without
    // needing another status query (which would race with OBS's internal
    // state transition). If start fails, or we're not configured to start,
    // we fall back to querying the current state.
    let initial_replay_active: Option<bool> = if config.start_replay_buffer {
        match client.replay_buffer().status().await {
            Ok(true) => {
                info!("OBS: replay buffer already running");
                Some(true)
            }
            Ok(false) => {
                // Match the canvas BEFORE starting the buffer. The buffer is
                // an output, so once it's running OBS locks the resolution
                // (SetVideoSettings → OutputRunning). Starting it at the right
                // resolution means the canvas is already correct by
                // record-start. Fail-closed, exactly like record-start: if the
                // match fails, don't start the buffer and surface an error
                // flash rather than locking the canvas at the wrong size.
                if config.match_canvas_to_display {
                    if let Err(e) = match_canvas_to_display(client, config).await {
                        warn!("OBS: canvas match failed; not starting replay buffer: {e:#}");
                        let _ = event_tx.send(ObsEvent::CanvasMatchFailed(format!("{e:#}"))).await;
                        Some(false)
                    } else {
                        start_replay_buffer(client).await
                    }
                } else {
                    start_replay_buffer(client).await
                }
            }
            Err(e) => {
                warn!("OBS: failed to query replay buffer status: {e}");
                None
            }
        }
    } else {
        client.replay_buffer().status().await.ok()
    };
    match initial_replay_active {
        Some(true) => {
            let _ = event_tx.send(ObsEvent::ReplayBufferActive).await;
        }
        Some(false) => {
            let _ = event_tx.send(ObsEvent::ReplayBufferInactive).await;
        }
        None => {} // unknown — wait for next event to populate
    }

    // Poll capture liveness only when the user opted into the status indicator.
    let mut capture_poll = config.show_capture_status.then(|| {
        let mut i = tokio::time::interval(CAPTURE_POLL_INTERVAL);
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        i
    });
    let mut last_capture_live: Option<bool> = None;

    loop {
        // `biased` polls the arms in written order, so when both cmd_rx and
        // the event stream are ready at the same select point, a button
        // command wins over an OBS event instead of tokio picking randomly.
        // (This only governs the choice at the select point — it does not
        // preempt an arm body already awaiting, e.g. handle_obs_event parked
        // on a full event_tx.send(). In practice the main thread drains the
        // bounded events channel every loop iteration, so that send rarely
        // parks for long.)
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => handle_command(client, config, cmd, event_tx).await,
                    None => return Ok(()), // main thread closed the channel
                }
            }
            evt = events.next() => {
                match evt {
                    Some(evt) => handle_obs_event(evt, event_tx).await,
                    None => return Err(anyhow::anyhow!("event stream ended")),
                }
            }
            _ = async { capture_poll.as_mut().unwrap().tick().await }, if capture_poll.is_some() => {
                match scan_captures(client).await {
                    Ok((found, live)) => {
                        let capturing = !found || live;
                        if last_capture_live != Some(capturing) {
                            last_capture_live = Some(capturing);
                            let _ = event_tx.send(ObsEvent::CaptureLive(capturing)).await;
                        }
                    }
                    Err(e) => debug!("OBS: capture poll failed: {e:#}"),
                }
            }
        }
    }
}

async fn handle_command(
    client: &obws::Client,
    config: &ObsConfig,
    cmd: ObsCommand,
    event_tx: &Sender<ObsEvent>,
) {
    // ToggleRecording goes through `toggle_recording` (which may match the
    // canvas to the monitor before starting); the rest map directly to a
    // single obws call. We unify on anyhow::Result so the canvas-match
    // path's errors flow through the same failure handling.
    let result: Result<()> = match cmd {
        ObsCommand::SaveReplay => client.replay_buffer().save().await.map_err(Into::into),
        ObsCommand::ToggleRecording => toggle_recording(client, config).await,
        ObsCommand::PauseRecording => {
            client.recording().toggle_pause().await.map(|_| ()).map_err(Into::into)
        }
        ObsCommand::SplitRecording => client.recording().split_file().await.map_err(Into::into),
    };

    match result {
        Ok(()) => {
            debug!("OBS: {} succeeded", cmd.label());
            let _ = event_tx.send(ObsEvent::CommandSucceeded(cmd)).await;
        }
        Err(e) => {
            warn!("OBS: {} failed: {e:#}", cmd.label());
            let _ = event_tx.send(ObsEvent::CommandFailed(cmd, format!("{e:#}"))).await;
        }
    }
}

/// Start or stop recording. On the *start* edge, run the capture-live guard
/// and (if enabled) match OBS's canvas to the monitor resolution first.
///
/// Both pre-start checks must run *before* recording begins, and OBS rejects
/// resolution changes while active, so we can't use obws's atomic `toggle()`
/// (it leaves no window between flip and start). Instead we read the current
/// state for the direction, then `stop()` or (checks first →) `start()`. The
/// brief TOCTOU window between the status read and start/stop is irrelevant for
/// a single user pressing a button. LED feedback is unaffected — it's driven by
/// OBS's RecordStateChanged events, which fire for explicit start/stop.
async fn toggle_recording(client: &obws::Client, config: &ObsConfig) -> Result<()> {
    let status = client.recording().status().await?;
    if status.active {
        // Stopping — the pre-start checks only apply to the start edge (a
        // running recording already has whatever capture/resolution it has),
        // so there's nothing to do but stop.
        client.recording().stop().await?;
        return Ok(());
    }
    // Starting. Both checks are fail-closed: an error aborts the start, so the
    // caller surfaces the error flash instead of recording nothing (capture
    // guard) or at a stale resolution (canvas match).
    ensure_capture_live(client).await?;
    if config.match_canvas_to_display {
        match_canvas_to_display(client, config).await?;
    }
    client.recording().start().await?;
    Ok(())
}

/// Read the target monitor's resolution and, if OBS's canvas (base + output)
/// doesn't already match it, set both to it. Only writes when something
/// differs, to avoid a needless video-pipeline reset. Leaves the FPS settings
/// untouched.
async fn match_canvas_to_display(client: &obws::Client, config: &ObsConfig) -> Result<()> {
    let (w, h) = crate::display::current_display_resolution(config.capture_display.as_deref())
        .await
        .context("detecting monitor resolution")?;

    let current = client
        .config()
        .video_settings()
        .await
        .context("reading OBS video settings")?;

    if current.base_width == w
        && current.base_height == h
        && current.output_width == w
        && current.output_height == h
    {
        debug!("OBS: canvas already {w}x{h}; leaving video settings unchanged");
        return Ok(());
    }

    info!(
        "OBS: setting canvas to {w}x{h} (was base {}x{}, output {}x{})",
        current.base_width, current.base_height, current.output_width, current.output_height
    );
    client
        .config()
        .set_video_settings(obws::requests::config::SetVideoSettings {
            base_width: Some(w),
            base_height: Some(h),
            output_width: Some(w),
            output_height: Some(h),
            ..Default::default()
        })
        .await
        .context("setting OBS video settings")?;
    Ok(())
}

/// Screen/window video captures — the Linux PipeWire portal sources, whose
/// kinds contain `capture-source`. Excludes audio captures (they'd read 0x0).
fn is_capture_kind(kind: &str) -> bool {
    kind.contains("capture-source")
}

/// Record-start guard: bail (→ error flash) if OBS has an enabled screen/window
/// capture but every one is blind (0x0) — the Wayland "dismissed the
/// Share-screen picker, so OBS records nothing" case. See `scan_captures` for
/// scope and fail-open behavior.
async fn ensure_capture_live(client: &obws::Client) -> Result<()> {
    // Review-accepted: a capture can briefly read 0x0 while its portal stream
    // (re)negotiates; not worth a settle loop — the error flash prompts a retry.
    let (found, live) = scan_captures(client).await?;
    if found && !live {
        bail!(
            "OBS isn't capturing anything — its screen/window capture is 0x0 \
             (was the \"Share screen\" picker dismissed?); refusing to record"
        );
    }
    Ok(())
}

/// Scan the current program scene for enabled screen/window captures, returning
/// `(found, live)`: whether any enabled capture is present, and whether at
/// least one is producing video. Scope is the scene's top-level items plus one
/// level of enabled groups (OBS groups can't nest); nested scenes aren't
/// descended, and disabled items are ignored since they composite nothing.
/// Shared by the record-start guard and the idle capture-status poll.
async fn scan_captures(client: &obws::Client) -> Result<(bool, bool)> {
    let scene = client
        .scenes()
        .current_program_scene()
        .await
        .context("reading OBS current program scene")?;
    let scene_id = obws::requests::scenes::SceneId::Name(&scene.id.name);

    let items = client
        .scene_items()
        .list(scene_id)
        .await
        .context("listing OBS scene items")?;

    let mut found = false;
    let mut live = false;
    for item in &items {
        if let Some(l) =
            capture_liveness(client, scene_id, item.input_kind.as_deref(), item.id).await?
        {
            found = true;
            live |= l;
        }
        // Descend into a group only if the group itself is enabled — a
        // disabled group composites none of its members.
        if item.is_group == Some(true)
            && client
                .scene_items()
                .enabled(scene_id, item.id)
                .await
                .with_context(|| {
                    format!("reading OBS group {:?} enabled state", item.source_name)
                })?
        {
            let group_id = obws::requests::scenes::SceneId::Name(&item.source_name);
            let members = client
                .scene_items()
                .list_group(group_id)
                .await
                .with_context(|| format!("listing OBS group {:?}", item.source_name))?;
            for m in &members {
                if let Some(l) =
                    capture_liveness(client, group_id, m.input_kind.as_deref(), m.id).await?
                {
                    found = true;
                    live |= l;
                }
            }
        }
    }
    Ok((found, live))
}

/// Liveness of a scene item viewed as a capture source:
/// `None` — not a screen/window capture, or one that's disabled/hidden
/// (ignore it); `Some(true)` — an enabled capture with real (non-zero)
/// dimensions; `Some(false)` — an enabled capture reporting 0x0, i.e. no live
/// stream. `scene` is the item's containing scene or group.
async fn capture_liveness(
    client: &obws::Client,
    scene: obws::requests::scenes::SceneId<'_>,
    input_kind: Option<&str>,
    item_id: i64,
) -> Result<Option<bool>> {
    let Some(kind) = input_kind else {
        return Ok(None);
    };
    if !is_capture_kind(kind) {
        return Ok(None);
    }
    // A disabled item isn't composited, so it must not count either way (can't
    // block, can't mask a dead visible capture). Skip before reading dimensions.
    if !client
        .scene_items()
        .enabled(scene, item_id)
        .await
        .context("reading OBS scene item enabled state")?
    {
        return Ok(None);
    }
    let t = client
        .scene_items()
        .transform(scene, item_id)
        .await
        .context("reading OBS scene item transform")?;
    // Live captures report the surface's pixel size; a stream-less one is 0x0.
    // `>= 1.0` avoids float-equality pitfalls on the f32 the API returns.
    Ok(Some(t.source_width >= 1.0 && t.source_height >= 1.0))
}

/// Start the replay buffer, mapping the outcome to the `Some(bool)` running
/// state `run_session` tracks. A start failure is non-fatal (logged, reported
/// as inactive) — the canvas-match that gates this call is the fail-closed
/// part; an OBS-side buffer-start error is a separate, rarer condition.
async fn start_replay_buffer(client: &obws::Client) -> Option<bool> {
    match client.replay_buffer().start().await {
        Ok(()) => {
            info!("OBS: started replay buffer");
            Some(true)
        }
        Err(e) => {
            warn!("OBS: failed to start replay buffer: {e}");
            Some(false)
        }
    }
}

async fn handle_obs_event(event: obws::events::Event, event_tx: &Sender<ObsEvent>) {
    use obws::events::Event::*;
    use obws::events::OutputState;
    match event {
        RecordStateChanged { active, state, .. } => {
            match state {
                OutputState::Started => {
                    let _ = event_tx.send(ObsEvent::RecordingActive).await;
                }
                OutputState::Stopped => {
                    let _ = event_tx.send(ObsEvent::RecordingStopped).await;
                }
                OutputState::Paused => {
                    let _ = event_tx.send(ObsEvent::RecordingPaused).await;
                }
                OutputState::Resumed => {
                    let _ = event_tx.send(ObsEvent::RecordingResumed).await;
                }
                _ => {
                    // Other transitions (Starting, Stopping, etc.) — derive from `active`
                    if active {
                        let _ = event_tx.send(ObsEvent::RecordingActive).await;
                    } else {
                        let _ = event_tx.send(ObsEvent::RecordingStopped).await;
                    }
                }
            }
        }
        ReplayBufferStateChanged { active, .. } => {
            if active {
                let _ = event_tx.send(ObsEvent::ReplayBufferActive).await;
            } else {
                let _ = event_tx.send(ObsEvent::ReplayBufferInactive).await;
            }
        }
        // Other OBS events (StreamStateChanged, SceneItemTransformChanged,
        // ReplayBufferSaved with the saved filename, etc.) are ignored.
        // If we ever want to surface the replay-buffer-saved file path or
        // react to stream state, add match arms here and map them to new
        // ObsEvent variants.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_local_accepts_loopback_forms() {
        // Case-insensitive localhost (hostnames are case-insensitive).
        assert!(host_is_local("localhost"));
        assert!(host_is_local("LOCALHOST"));
        assert!(host_is_local("Localhost"));
        // FQDN trailing dot.
        assert!(host_is_local("localhost."));
        // Any 127.0.0.0/8 address, not just 127.0.0.1.
        assert!(host_is_local("127.0.0.1"));
        assert!(host_is_local("127.0.0.2"));
        assert!(host_is_local("127.1.2.3"));
        // IPv6 loopback, bracketed or bare.
        assert!(host_is_local("::1"));
        assert!(host_is_local("[::1]"));
    }

    #[test]
    fn host_is_local_rejects_remote_hosts() {
        assert!(!host_is_local("192.168.1.10"));
        assert!(!host_is_local("10.0.0.5"));
        assert!(!host_is_local("obs.example.com"));
        assert!(!host_is_local("example.com"));
        // Not loopback, just shares a leading digit pattern.
        assert!(!host_is_local("128.0.0.1"));
    }
}
