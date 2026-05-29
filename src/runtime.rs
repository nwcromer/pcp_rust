//! Runtime state owned by the main thread: the OBS connection, recording
//! state, flash overlay, and the cached mic-mute state used by the logo
//! indicator. Pure state and event-application logic — LED writes live in
//! `paint`, configuration types in `config`.

use std::time::{Duration, Instant};

use log::{info, warn};

use crate::config::{LogoConfig, LogoIndicator, ObsColors, RgbColor};
use crate::obs::{ObsCommand, ObsEvent, ObsHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsState {
    Idle,
    Recording,
    RecordingPaused,
}

/// Total on+off period for blinking flashes. Half is "on", half is "off".
const BLINK_CYCLE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
pub struct Flash {
    pub color: RgbColor,
    pub expires_at: Instant,
    /// If `Some`, the flash blinks between `color` and off using this cycle.
    pub blink: Option<BlinkConfig>,
}

#[derive(Debug, Clone, Copy)]
pub struct BlinkConfig {
    pub started_at: Instant,
    /// Total on+off cycle length. Half is "on", half is "off".
    pub cycle: Duration,
}

impl Flash {
    fn new_solid(color: RgbColor, duration_ms: u64) -> Self {
        Self {
            color,
            expires_at: Instant::now() + Duration::from_millis(duration_ms),
            blink: None,
        }
    }

    fn new_blink(color: RgbColor, duration_ms: u64) -> Self {
        let now = Instant::now();
        Self {
            color,
            expires_at: now + Duration::from_millis(duration_ms),
            blink: Some(BlinkConfig {
                started_at: now,
                cycle: BLINK_CYCLE,
            }),
        }
    }

    /// The color to display right now. For solid flashes this just returns
    /// `self.color`. For blinking flashes it reads `Instant::now()` and
    /// returns either `self.color` or black depending on the current phase
    /// of the blink cycle — so the return value is *not* pure: two calls
    /// across a phase boundary will yield different results.
    pub fn current_color(&self) -> RgbColor {
        self.current_color_at(Instant::now())
    }

    /// `current_color` factored out so tests can pass a deterministic time.
    fn current_color_at(&self, now: Instant) -> RgbColor {
        let Some(blink) = self.blink else {
            return self.color;
        };
        let elapsed_ms = now.saturating_duration_since(blink.started_at).as_millis();
        let half_ms = (blink.cycle.as_millis() / 2).max(1);
        if (elapsed_ms / half_ms).is_multiple_of(2) {
            self.color
        } else {
            RgbColor { r: 0, g: 0, b: 0 }
        }
    }
}

/// All OBS-related state owned by the main thread: the handle to the OBS
/// thread, the connection flag, the current recording state, the configured
/// colors, and any active flash overlay. Bundling them keeps the main loop
/// and `handle_panel_event` from threading a long parameter list, and lets
/// `dispatch`/`drain_events` operate over a single self.
pub struct ObsRuntime {
    handle: Option<ObsHandle>,
    pub connected: bool,
    pub state: ObsState,
    pub colors: ObsColors,
    pub flash: Option<Flash>,
    /// Whether OBS's replay buffer is currently running. `None` until OBS
    /// reports its state (during initial-status query on connect or via an
    /// event). Reset to `None` on disconnect.
    pub replay_buffer_active: Option<bool>,
    /// Pulled from `ObsConfig.paused_use_breath`. When true, paused state
    /// renders as a global breath animation that drives every LED including
    /// the logo (so any configured logo indicator is unavailable while
    /// paused). When false, paused is a solid panel color and the logo
    /// continues to show whatever the configured indicator says.
    pub paused_use_breath: bool,
    /// Current default-microphone mute state. Tracked here (rather than
    /// queried fresh at paint time) so it persists across paints and so
    /// changes can mark the LEDs dirty for a repaint.
    pub mic_muted: bool,
    /// Which logo indicator is active and the colors for each state.
    /// `LogoIndicator::None` (the default) means the logo just matches the
    /// panel color.
    pub logo_cfg: LogoConfig,
}

impl ObsRuntime {
    pub fn new(
        handle: Option<ObsHandle>,
        colors: ObsColors,
        paused_use_breath: bool,
        logo_cfg: LogoConfig,
    ) -> Self {
        Self {
            handle,
            connected: false,
            state: ObsState::Idle,
            colors,
            flash: None,
            replay_buffer_active: None,
            paused_use_breath,
            mic_muted: false,
            logo_cfg,
        }
    }

    /// The color the configured logo indicator wants right now, or `None`
    /// if no indicator is selected or the indicator has nothing useful to
    /// show (e.g. replay indicator with OBS disconnected).
    pub fn logo_indicator_color(&self) -> Option<RgbColor> {
        match self.logo_cfg.indicator {
            LogoIndicator::None => None,
            LogoIndicator::Mic => Some(if self.mic_muted {
                self.logo_cfg.mic_muted
            } else {
                self.logo_cfg.mic_unmuted
            }),
            LogoIndicator::Replay => {
                if !self.connected {
                    return None;
                }
                Some(match self.replay_buffer_active {
                    Some(true) => self.logo_cfg.replay_active,
                    Some(false) | None => self.logo_cfg.replay_inactive,
                })
            }
        }
    }

    /// True if the mic indicator is selected — i.e. we need to poll mic
    /// state to keep the logo accurate.
    pub fn mic_indicator_enabled(&self) -> bool {
        self.logo_cfg.indicator == LogoIndicator::Mic
    }

    /// Drain pending events from the OBS thread. Returns `true` if any
    /// event changed something that requires a LED repaint.
    pub fn drain_events(&mut self) -> bool {
        let mut dirty = false;
        while let Some(event) = self.next_event() {
            dirty |= self.apply_event(event);
        }
        dirty
    }

    /// Try to receive one event from the OBS thread without blocking.
    /// Returns `None` if there's no event ready or no OBS thread.
    fn next_event(&mut self) -> Option<ObsEvent> {
        self.handle
            .as_mut()
            .and_then(|h| h.events_rx.try_recv().ok())
    }

    /// Apply a single event to the runtime's state. Returns `true` if the
    /// LEDs need a repaint as a result.
    pub fn apply_event(&mut self, event: ObsEvent) -> bool {
        match event {
            ObsEvent::Connected => {
                // Dirty only on the actual transition. The first Connected
                // event flips us out of `[rgb]` and into the OBS-connected
                // appearance; a redundant Connected (shouldn't happen, but
                // defensive) is a no-op.
                let was_disconnected = !self.connected;
                self.connected = true;
                was_disconnected
            }
            ObsEvent::ReplayBufferActive => {
                let changed = self.replay_buffer_active != Some(true);
                self.replay_buffer_active = Some(true);
                changed
            }
            ObsEvent::ReplayBufferInactive => {
                let changed = self.replay_buffer_active != Some(false);
                self.replay_buffer_active = Some(false);
                changed
            }
            ObsEvent::Disconnected => {
                self.connected = false;
                self.replay_buffer_active = None;
                // Disconnected behaves visually like Idle. Also force a
                // repaint so we switch back to `[rgb]` mode from the
                // OBS-connected-idle appearance.
                let _ = self.transition_to(ObsState::Idle);
                true
            }
            ObsEvent::RecordingActive | ObsEvent::RecordingResumed => {
                self.transition_to(ObsState::Recording)
            }
            ObsEvent::RecordingPaused => self.transition_to(ObsState::RecordingPaused),
            ObsEvent::RecordingStopped => self.transition_to(ObsState::Idle),
            ObsEvent::CommandSucceeded(cmd) => {
                info!("OBS: {} succeeded", cmd.label());
                // Skip the success flash for commands whose effect is
                // already visible on the LEDs via a state change.
                // Save Replay and Split Recording cause no visible state
                // change, so they still get the flash as the only
                // acknowledgement.
                let visibly_changes_state = matches!(
                    cmd,
                    ObsCommand::ToggleRecording | ObsCommand::PauseRecording
                );
                if visibly_changes_state {
                    false
                } else {
                    self.set_success_flash();
                    true
                }
            }
            ObsEvent::CommandFailed(cmd, msg) => {
                warn!("OBS command failed ({}): {msg}", cmd.label());
                self.set_error_flash();
                true
            }
        }
    }

    /// Move to a new recording state. Returns `true` if the state actually
    /// changed (and the LEDs need a repaint).
    fn transition_to(&mut self, new_state: ObsState) -> bool {
        if self.state != new_state {
            self.state = new_state;
            true
        } else {
            false
        }
    }

    /// Set a solid success flash using the configured duration.
    fn set_success_flash(&mut self) {
        self.flash = Some(Flash::new_solid(
            self.colors.success_flash,
            self.colors.flash_duration_ms,
        ));
    }

    /// Set an error flash. Blinks between the error color and off to make
    /// failures more visually obvious than a steady color change.
    fn set_error_flash(&mut self) {
        self.flash = Some(Flash::new_blink(
            self.colors.error_flash,
            self.colors.flash_duration_ms,
        ));
    }

    /// Clear any expired flash. Returns `true` if a flash just expired OR
    /// if a blinking flash is active (in which case we keep repainting so
    /// the on/off phases render — the simplest implementation, costs an
    /// extra LED write per main-loop iteration for the flash duration).
    pub fn expire_flash(&mut self) -> bool {
        match self.flash {
            Some(f) if Instant::now() >= f.expires_at => {
                self.flash = None;
                true
            }
            Some(f) if f.blink.is_some() => true,
            _ => false,
        }
    }

    /// Send a command to the OBS thread. Returns `true` if the LEDs need a
    /// repaint (a local error flash was set because OBS is unreachable).
    pub fn dispatch(&mut self, cmd: ObsCommand, verbose: bool) -> bool {
        if verbose {
            println!("OBS: {}", cmd.label());
        }
        // Use try_send on the bounded channel: in normal operation the OBS
        // thread keeps the queue near-empty, so a Full result would indicate
        // the OBS thread is wedged. Either Full or Closed gets treated as
        // "OBS unreachable" and surfaces an error flash.
        let send_result = match &self.handle {
            Some(h) if self.connected => h.commands_tx.try_send(cmd),
            _ => {
                // Either no [obs] in config (config validation rejects OBS
                // actions when [obs] is absent, so this is normally only
                // hit when OBS is disconnected) or the thread is gone.
                warn!("OBS not connected — {} skipped", cmd.label());
                self.set_error_flash();
                return true;
            }
        };
        if let Err(e) = send_result {
            warn!("OBS thread unreachable — {} skipped ({e})", cmd.label());
            self.set_error_flash();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: RgbColor = RgbColor { r: 0xFF, g: 0, b: 0 };
    const BLACK: RgbColor = RgbColor { r: 0, g: 0, b: 0 };

    fn solid(started: Instant) -> Flash {
        Flash {
            color: RED,
            expires_at: started + Duration::from_secs(60),
            blink: None,
        }
    }

    fn blink(started: Instant) -> Flash {
        Flash {
            color: RED,
            expires_at: started + Duration::from_secs(60),
            blink: Some(BlinkConfig {
                started_at: started,
                cycle: BLINK_CYCLE,
            }),
        }
    }

    #[test]
    fn solid_flash_is_time_invariant() {
        let t0 = Instant::now();
        let f = solid(t0);
        assert_eq!(f.current_color_at(t0), RED);
        assert_eq!(f.current_color_at(t0 + Duration::from_millis(50)), RED);
        assert_eq!(f.current_color_at(t0 + Duration::from_secs(10)), RED);
    }

    #[test]
    fn blink_flash_phases() {
        // BLINK_CYCLE = 200ms, so half = 100ms.
        // [0..100) → on, [100..200) → off, [200..300) → on, ...
        let t0 = Instant::now();
        let f = blink(t0);
        let at = |offset_ms| f.current_color_at(t0 + Duration::from_millis(offset_ms));
        assert_eq!(at(0), RED);
        assert_eq!(at(50), RED);
        assert_eq!(at(99), RED);
        assert_eq!(at(100), BLACK);
        assert_eq!(at(150), BLACK);
        assert_eq!(at(199), BLACK);
        assert_eq!(at(200), RED);
        assert_eq!(at(299), RED);
        assert_eq!(at(300), BLACK);
        assert_eq!(at(400), RED);
    }

    #[test]
    fn blink_flash_before_started_clamps_to_on() {
        // saturating_duration_since means a `now` before `started_at` clamps to 0,
        // which puts us at the start of the on phase.
        let t0 = Instant::now();
        let f = blink(t0);
        assert_eq!(f.current_color_at(t0 - Duration::from_millis(50)), RED);
    }

    fn fresh_runtime() -> ObsRuntime {
        ObsRuntime::new(None, ObsColors::default(), false, LogoConfig::default())
    }

    #[test]
    fn apply_replay_buffer_state_transitions() {
        let mut obs = fresh_runtime();
        assert_eq!(obs.replay_buffer_active, None);

        // First Active: None → Some(true), dirty.
        assert!(obs.apply_event(ObsEvent::ReplayBufferActive));
        assert_eq!(obs.replay_buffer_active, Some(true));

        // Redundant Active: no change, not dirty.
        assert!(!obs.apply_event(ObsEvent::ReplayBufferActive));
        assert_eq!(obs.replay_buffer_active, Some(true));

        // Inactive: Some(true) → Some(false), dirty.
        assert!(obs.apply_event(ObsEvent::ReplayBufferInactive));
        assert_eq!(obs.replay_buffer_active, Some(false));

        // Redundant Inactive: no change, not dirty.
        assert!(!obs.apply_event(ObsEvent::ReplayBufferInactive));
    }

    #[test]
    fn apply_disconnect_resets_replay_state_and_is_dirty() {
        let mut obs = fresh_runtime();
        obs.apply_event(ObsEvent::Connected);
        obs.apply_event(ObsEvent::ReplayBufferActive);
        assert_eq!(obs.replay_buffer_active, Some(true));

        // Disconnect always repaints (we need to drop OBS-connected
        // appearance back to `[rgb]`) and clears the replay-buffer state.
        assert!(obs.apply_event(ObsEvent::Disconnected));
        assert_eq!(obs.replay_buffer_active, None);
        assert!(!obs.connected);
    }

    #[test]
    fn logo_indicator_mic_picks_color_per_mute_state() {
        let red = RgbColor { r: 0xFF, g: 0, b: 0 };
        let green = RgbColor { r: 0, g: 0xFF, b: 0 };
        let cfg = LogoConfig {
            indicator: LogoIndicator::Mic,
            mic_muted: red,
            mic_unmuted: green,
            ..Default::default()
        };
        let mut obs = ObsRuntime::new(None, ObsColors::default(), false, cfg);
        assert!(obs.mic_indicator_enabled());
        assert_eq!(obs.logo_indicator_color(), Some(green));
        obs.mic_muted = true;
        assert_eq!(obs.logo_indicator_color(), Some(red));
    }

    #[test]
    fn logo_indicator_replay_requires_connected() {
        let green = RgbColor { r: 0, g: 0xFF, b: 0 };
        let black = RgbColor { r: 0, g: 0, b: 0 };
        let cfg = LogoConfig {
            indicator: LogoIndicator::Replay,
            replay_active: green,
            replay_inactive: black,
            ..Default::default()
        };
        let mut obs = ObsRuntime::new(None, ObsColors::default(), false, cfg);
        // Disconnected → no indicator color; logo falls back to panel.
        assert!(!obs.mic_indicator_enabled());
        assert_eq!(obs.logo_indicator_color(), None);

        obs.apply_event(ObsEvent::Connected);
        // Connected but state unknown → inactive color.
        assert_eq!(obs.logo_indicator_color(), Some(black));
        obs.apply_event(ObsEvent::ReplayBufferActive);
        assert_eq!(obs.logo_indicator_color(), Some(green));
    }

    #[test]
    fn logo_indicator_none_never_overrides() {
        let obs = ObsRuntime::new(None, ObsColors::default(), false, LogoConfig::default());
        assert_eq!(obs.logo_cfg.indicator, LogoIndicator::None);
        assert!(!obs.mic_indicator_enabled());
        assert_eq!(obs.logo_indicator_color(), None);
    }

    #[test]
    fn apply_connected_dirty_only_on_transition() {
        let mut obs = fresh_runtime();
        // First Connected: false → true, dirty.
        assert!(obs.apply_event(ObsEvent::Connected));
        assert!(obs.connected);

        // Redundant Connected: already true, not dirty.
        assert!(!obs.apply_event(ObsEvent::Connected));
    }
}
