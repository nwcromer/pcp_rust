//! OBS Studio integration via obs-websocket v5 (using the obws crate).
//!
//! Runs on a dedicated OS thread that owns a tokio runtime. Communicates
//! with the rest of pcp_rust over two bounded mpsc channels:
//!   main → OBS thread:  `ObsCommand` (button-triggered actions)
//!   OBS thread → main:  `ObsEvent`   (state changes and command results)
//!
//! Reconnects automatically with exponential backoff when OBS is absent.

use std::time::{Duration, Instant};

use anyhow::Result;
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
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
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
                // thread closes the command channel.
                if let Err(e) = run_session(&client, &config, &mut cmd_rx, &event_tx).await {
                    info!("OBS: disconnected ({e})");
                }

                let _ = event_tx.send(ObsEvent::Disconnected).await;

                if session_start.elapsed() >= STABLE_SESSION_DWELL {
                    // Stable session — reset backoff so the next reconnect
                    // (e.g. user restarted OBS) is quick.
                    backoff = BACKOFF_INITIAL_SECS;
                } else {
                    // Flapping — session died too quickly. Treat as a failed
                    // attempt so we don't hammer the network/journal.
                    debug!(
                        "OBS: session ended after {}s; backing off {backoff}s before retry",
                        session_start.elapsed().as_secs()
                    );
                    sleep(Duration::from_secs(backoff)).await;
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
    // after connection (without waiting for the first event).
    if let Ok(status) = client.recording().status().await {
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
            Ok(false) => match client.replay_buffer().start().await {
                Ok(()) => {
                    info!("OBS: started replay buffer");
                    Some(true)
                }
                Err(e) => {
                    warn!("OBS: failed to start replay buffer: {e}");
                    Some(false)
                }
            },
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

    loop {
        // `biased` polls cmd_rx first every iteration so button presses
        // can't get starved when the events arm is parked on
        // `event_tx.send(...).await` (which happens if the main thread
        // can't drain the bounded events channel fast enough).
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => handle_command(client, cmd, event_tx).await,
                    None => return Ok(()), // main thread closed the channel
                }
            }
            evt = events.next() => {
                match evt {
                    Some(evt) => handle_obs_event(evt, event_tx).await,
                    None => return Err(anyhow::anyhow!("event stream ended")),
                }
            }
        }
    }
}

async fn handle_command(
    client: &obws::Client,
    cmd: ObsCommand,
    event_tx: &Sender<ObsEvent>,
) {
    let result: Result<(), obws::error::Error> = match cmd {
        ObsCommand::SaveReplay => client.replay_buffer().save().await,
        ObsCommand::ToggleRecording => client.recording().toggle().await.map(|_| ()),
        ObsCommand::PauseRecording => client.recording().toggle_pause().await.map(|_| ()),
        ObsCommand::SplitRecording => client.recording().split_file().await,
    };

    match result {
        Ok(()) => {
            debug!("OBS: {} succeeded", cmd.label());
            let _ = event_tx.send(ObsEvent::CommandSucceeded(cmd)).await;
        }
        Err(e) => {
            warn!("OBS: {} failed: {e}", cmd.label());
            let _ = event_tx.send(ObsEvent::CommandFailed(cmd, e.to_string())).await;
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
