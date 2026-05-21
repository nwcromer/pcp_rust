//! OBS Studio integration via obs-websocket v5 (using the obws crate).
//!
//! Runs on a dedicated OS thread that owns a tokio runtime. Communicates
//! with the rest of pcp_rust over two unbounded mpsc channels:
//!   main → OBS thread:  `ObsCommand` (button-triggered actions)
//!   OBS thread → main:  `ObsEvent`   (state changes and command results)
//!
//! Reconnects automatically with exponential backoff when OBS is absent.

use std::time::Duration;

use anyhow::Result;
use log::{debug, info, warn};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
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
    /// An OBS command completed successfully.
    CommandSucceeded(ObsCommand),
    /// An OBS command failed (e.g., OBS rejected it or we're disconnected).
    CommandFailed(ObsCommand, String),
}

pub struct ObsHandle {
    pub commands_tx: UnboundedSender<ObsCommand>,
    pub events_rx: UnboundedReceiver<ObsEvent>,
}

/// Spawn the OBS background thread and return channels for the main thread
/// to interact with it. Returns `None` if the OS won't let us spawn the
/// thread (extreme resource exhaustion) — callers treat that the same as
/// "no [obs] configured": OBS actions error-flash, audio control keeps
/// working.
pub fn spawn_obs_thread(config: ObsConfig) -> Option<ObsHandle> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ObsCommand>();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<ObsEvent>();

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

async fn obs_main_loop(
    config: ObsConfig,
    mut cmd_rx: UnboundedReceiver<ObsCommand>,
    event_tx: UnboundedSender<ObsEvent>,
) {
    info!("OBS: attempting to connect to {}:{}", config.host, config.port);
    let mut backoff = BACKOFF_INITIAL_SECS;
    // Log the first failure at info so the user knows the loop is alive;
    // subsequent retries log at debug to avoid filling the journal.
    let mut first_failure_logged = false;

    loop {
        match try_connect(&config).await {
            Ok(client) => {
                info!("OBS: connected to {}:{}", config.host, config.port);
                let _ = event_tx.send(ObsEvent::Connected);
                backoff = BACKOFF_INITIAL_SECS;
                first_failure_logged = false;

                // Run the session until the connection drops or the main
                // thread closes the command channel.
                if let Err(e) = run_session(&client, &config, &mut cmd_rx, &event_tx).await {
                    warn!("OBS: session ended: {e}");
                }

                let _ = event_tx.send(ObsEvent::Disconnected);
                // Continue to outer loop to reconnect (no backoff — we just
                // had a working connection, so reconnect attempt is cheap).
            }
            Err(e) => {
                if !first_failure_logged {
                    info!("OBS: connect failed ({e}); will keep retrying in background");
                    first_failure_logged = true;
                } else {
                    debug!("OBS: connect failed ({e}); retrying in {backoff}s");
                }
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
    cmd_rx: &mut UnboundedReceiver<ObsCommand>,
    event_tx: &UnboundedSender<ObsEvent>,
) -> Result<()> {
    // Publish initial recording state so the LEDs reflect reality immediately
    // after connection (without waiting for the first event).
    if let Ok(status) = client.recording().status().await {
        if status.active {
            if status.paused {
                let _ = event_tx.send(ObsEvent::RecordingPaused);
            } else {
                let _ = event_tx.send(ObsEvent::RecordingActive);
            }
        } else {
            let _ = event_tx.send(ObsEvent::RecordingStopped);
        }
    }

    // Optionally start the replay buffer if the user asked for it via config.
    // This runs on EVERY successful connect — including reconnects after OBS
    // restarts or network blips. During an active session we don't monitor or
    // re-enable the buffer; if the user stops it via OBS, it stays stopped
    // until the next reconnect.
    if config.start_replay_buffer {
        match client.replay_buffer().status().await {
            Ok(true) => info!("OBS: replay buffer already running"),
            Ok(false) => match client.replay_buffer().start().await {
                Ok(()) => info!("OBS: started replay buffer"),
                Err(e) => warn!("OBS: failed to start replay buffer: {e}"),
            },
            Err(e) => warn!("OBS: failed to query replay buffer status: {e}"),
        }
    }

    // Subscribe to the OBS event stream. Note: this happens *after* the
    // replay-buffer start above, so a `ReplayBufferStateChanged` event fired
    // by OBS between our `start()` returning and this `events()` call would
    // be missed. Doesn't matter today because we don't handle that event,
    // but if we ever do, subscribe before sending replay-buffer commands.
    let events = client.events()?;
    tokio::pin!(events);

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => handle_command(client, cmd, event_tx).await,
                    None => return Ok(()), // main thread closed the channel
                }
            }
            evt = events.next() => {
                match evt {
                    Some(evt) => handle_obs_event(evt, event_tx),
                    None => return Err(anyhow::anyhow!("event stream ended")),
                }
            }
        }
    }
}

async fn handle_command(
    client: &obws::Client,
    cmd: ObsCommand,
    event_tx: &UnboundedSender<ObsEvent>,
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
            let _ = event_tx.send(ObsEvent::CommandSucceeded(cmd));
        }
        Err(e) => {
            warn!("OBS: {} failed: {e}", cmd.label());
            let _ = event_tx.send(ObsEvent::CommandFailed(cmd, e.to_string()));
        }
    }
}

fn handle_obs_event(event: obws::events::Event, event_tx: &UnboundedSender<ObsEvent>) {
    use obws::events::Event::*;
    match event {
        RecordStateChanged { active, state, .. } => {
            // The OutputState enum has variants like Started, Stopped, Paused, Resumed, etc.
            use obws::events::OutputState;
            match state {
                OutputState::Started => {
                    let _ = event_tx.send(ObsEvent::RecordingActive);
                }
                OutputState::Stopped => {
                    let _ = event_tx.send(ObsEvent::RecordingStopped);
                }
                OutputState::Paused => {
                    let _ = event_tx.send(ObsEvent::RecordingPaused);
                }
                OutputState::Resumed => {
                    let _ = event_tx.send(ObsEvent::RecordingResumed);
                }
                _ => {
                    // Other transitions (Starting, Stopping, etc.) — derive from `active`
                    if active {
                        let _ = event_tx.send(ObsEvent::RecordingActive);
                    } else {
                        let _ = event_tx.send(ObsEvent::RecordingStopped);
                    }
                }
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
