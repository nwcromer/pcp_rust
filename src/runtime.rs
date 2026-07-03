//! Runtime state owned by the main thread — OBS connection, recording state,
//! flash overlay, cached mic-mute. Pure state + event-application; LED writes
//! live in `paint`, config types in `config`.

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

/// How fresh a mic-mute confirmation must be to trust the cached `mic_muted`.
/// Past this, the logo shows the blinking "unknown" indicator so the user knows
/// the cache may not match the device. 4 poll intervals: a transient PA hiccup
/// won't trip the warning, a sustained outage will within ~1s.
const MIC_STALE_THRESHOLD: Duration = Duration::from_millis(1000);

/// On+off period for the mic-unknown blink (half on, half off). 500 ms is
/// attention-grabbing without being seizure-inducing.
const MIC_UNKNOWN_BLINK_CYCLE: Duration = Duration::from_millis(500);

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

    /// The color to display right now. Solid flashes return `self.color`; blinking
    /// flashes read `Instant::now()` and return `self.color` or black by phase — so
    /// this is *not* pure: two calls across a phase boundary differ.
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

/// All OBS-related state owned by the main thread. Bundling it keeps the main
/// loop and `handle_panel_event` from threading a long parameter list, and lets
/// `dispatch`/`drain_events` operate over a single self.
pub struct ObsRuntime {
    handle: Option<ObsHandle>,
    connected: bool,
    state: ObsState,
    colors: ObsColors,
    flash: Option<Flash>,
    /// Whether OBS's replay buffer is running. `None` until OBS reports it (initial
    /// status query on connect, or an event); reset to `None` on disconnect.
    replay_buffer_active: Option<bool>,
    /// From `ObsConfig.paused_use_breath`. True: paused renders as a global breath
    /// driving every LED including the logo (so the logo indicator is unavailable
    /// while paused). False: paused is a solid panel color and the logo indicator
    /// still shows.
    paused_use_breath: bool,
    /// Current default-mic mute state. Cached here rather than queried at paint
    /// time so it persists across paints and changes can mark the LEDs dirty.
    mic_muted: bool,
    /// Instant of the last successful mic-mute confirmation from PA; None before
    /// the first seed. Older than MIC_STALE_THRESHOLD → `logo_indicator_color`
    /// returns the unknown color instead of trusting `mic_muted`.
    mic_confirmed_at: Option<Instant>,
    /// Reference instant for blink phase. Set once at construction and stable for
    /// the runtime's life so blink phases are continuous.
    created_at: Instant,
    /// Which logo indicator is active and the per-state colors. `LogoIndicator::None`
    /// (the default) means the logo just matches the panel color.
    logo_cfg: LogoConfig,
    /// Whether OBS is capturing, per the capture-liveness poll. Updated only when
    /// `show_capture_status` is on, read only for the idle panel color. Defaults to
    /// true so we don't flash the not-capturing color before the first poll.
    capturing: bool,
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
            mic_confirmed_at: None,
            created_at: Instant::now(),
            logo_cfg,
            capturing: true,
        }
    }

    // ---- Field accessors. paint.rs/main.rs read via methods so the invariants
    // ---- apply_event enforces (e.g. !connected → replay_buffer_active is None)
    // ---- can't be violated by a stray direct write.

    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn state(&self) -> ObsState {
        self.state
    }

    pub fn colors(&self) -> ObsColors {
        self.colors
    }

    pub fn flash(&self) -> Option<Flash> {
        self.flash
    }

    pub fn paused_use_breath(&self) -> bool {
        self.paused_use_breath
    }

    pub fn capturing(&self) -> bool {
        self.capturing
    }

    /// Record a confirmed mic-mute state from PA: sets `mic_muted` and bumps
    /// `mic_confirmed_at` so the logo trusts it for the next MIC_STALE_THRESHOLD
    /// window (staleness resets only on real confirmations). Startup seed and
    /// post-resume refresh call this directly; button/poll paths use `update_mic_muted`.
    pub fn set_mic_muted(&mut self, muted: bool) {
        self.mic_muted = muted;
        self.mic_confirmed_at = Some(Instant::now());
    }

    /// Record a confirmed mic-mute state and report whether the logo needs a
    /// repaint: true if the value changed OR we were in the stale "unknown" state
    /// (so recovering from a PA outage stops the blink even when unchanged). No-op
    /// `false` when the mic indicator isn't selected — nothing reads `mic_muted`
    /// then. The single home for the button/poll dirty rule.
    pub fn update_mic_muted(&mut self, muted: bool) -> bool {
        if !self.mic_indicator_enabled() {
            return false;
        }
        let dirty = muted != self.mic_muted || self.mic_is_stale();
        self.set_mic_muted(muted);
        dirty
    }

    /// True if the cached `mic_muted` is older than MIC_STALE_THRESHOLD, or was
    /// never confirmed. Stale drives the logo's blinking `mic_unknown` warning.
    fn mic_is_stale(&self) -> bool {
        match self.mic_confirmed_at {
            None => true,
            Some(when) => when.elapsed() >= MIC_STALE_THRESHOLD,
        }
    }

    /// Phase of a free-running blink of the given cycle: true on the "on" half.
    /// Driven from `created_at` so the blink is continuous across the runtime's life.
    fn blink_phase(&self, cycle: Duration) -> bool {
        let elapsed_ms = self.created_at.elapsed().as_millis();
        let half = (cycle.as_millis() / 2).max(1);
        (elapsed_ms / half).is_multiple_of(2)
    }

    /// True if the mic indicator is selected and its state is stale. The main loop
    /// checks this to drive continuous repaints so the blink phases render.
    pub fn mic_indicator_needs_repaint(&self) -> bool {
        self.mic_indicator_enabled() && self.mic_is_stale()
    }

    /// The color the configured logo indicator wants now, or `None` if no indicator
    /// is selected or it has nothing to show (e.g. replay with OBS disconnected).
    pub fn logo_indicator_color(&self) -> Option<RgbColor> {
        match self.logo_cfg.indicator {
            LogoIndicator::None => None,
            LogoIndicator::Mic => Some(self.mic_logo_color()),
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

    /// Compute the mic indicator color, honoring the staleness check.
    /// Stale → blinking mic_unknown; fresh → mic_muted / mic_unmuted.
    fn mic_logo_color(&self) -> RgbColor {
        if self.mic_is_stale() {
            if self.blink_phase(MIC_UNKNOWN_BLINK_CYCLE) {
                self.logo_cfg.mic_unknown
            } else {
                RgbColor { r: 0, g: 0, b: 0 }
            }
        } else if self.mic_muted {
            self.logo_cfg.mic_muted
        } else {
            self.logo_cfg.mic_unmuted
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
                // Dirty only on the real transition: the first Connected flips us
                // out of `[rgb]` into the OBS-connected look. A redundant one is a
                // defensive no-op.
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
                // Assume capturing on the next connect until the poll says
                // otherwise, so a reconnect doesn't briefly show stale state.
                self.capturing = true;
                // Disconnected looks like Idle, but force a repaint anyway to switch
                // back to `[rgb]` from the OBS-connected-idle appearance.
                let _ = self.transition_to(ObsState::Idle);
                true
            }
            ObsEvent::CaptureLive(live) => {
                let changed = self.capturing != live;
                self.capturing = live;
                changed
            }
            ObsEvent::RecordingActive | ObsEvent::RecordingResumed => {
                self.transition_to(ObsState::Recording)
            }
            ObsEvent::RecordingPaused => self.transition_to(ObsState::RecordingPaused),
            ObsEvent::RecordingStopped => self.transition_to(ObsState::Idle),
            ObsEvent::CommandSucceeded(cmd) => {
                info!("OBS: {} succeeded", cmd.label());
                // Skip the success flash for commands whose effect already shows on
                // the LEDs via a state change. Save Replay and Split Recording change
                // no visible state, so they keep the flash as their only acknowledgement.
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
            ObsEvent::CanvasMatchFailed(msg) => {
                warn!("OBS canvas match failed; replay buffer not started: {msg}");
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

    /// Clear any expired flash. Returns `true` if a flash just expired OR a blinking
    /// flash is active (we keep repainting so the on/off phases render, one extra
    /// LED write per loop iteration for the flash duration).
    ///
    /// Some of those paints are redundant — the loop runs ~10 Hz while the blink
    /// flips every 100ms — but not worth tracking last-painted-phase: the default
    /// 500ms flash is ~5 paints, each microseconds of HID traffic.
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
        // try_send on the bounded channel: the OBS thread keeps the queue
        // near-empty, so Full means it's wedged. Both Full and Closed are
        // treated as "OBS unreachable" and surface an error flash.
        let send_result = match &self.handle {
            Some(h) if self.connected => h.commands_tx.try_send(cmd),
            _ => {
                // No [obs] in config (validation rejects OBS actions when absent,
                // so this is normally only hit when OBS is disconnected) or the
                // thread is gone.
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
    fn apply_capture_live_dirty_only_on_change() {
        let mut obs = fresh_runtime();
        assert!(obs.capturing()); // default: assume capturing

        // true → false is a change → dirty.
        assert!(obs.apply_event(ObsEvent::CaptureLive(false)));
        assert!(!obs.capturing());
        // Redundant false → not dirty.
        assert!(!obs.apply_event(ObsEvent::CaptureLive(false)));
        // Back to true → dirty.
        assert!(obs.apply_event(ObsEvent::CaptureLive(true)));
        assert!(obs.capturing());

        // Disconnect resets to capturing so a reconnect doesn't show stale state.
        obs.apply_event(ObsEvent::CaptureLive(false));
        obs.apply_event(ObsEvent::Disconnected);
        assert!(obs.capturing());
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
        // set_mic_muted bumps mic_confirmed_at, so the indicator returns
        // the confident color rather than the stale-state blink.
        obs.set_mic_muted(false);
        assert_eq!(obs.logo_indicator_color(), Some(green));
        obs.set_mic_muted(true);
        assert_eq!(obs.logo_indicator_color(), Some(red));
    }

    #[test]
    fn logo_indicator_mic_unconfirmed_returns_unknown_or_off() {
        // Fresh runtime → mic_confirmed_at None → stale → blinks between mic_unknown
        // and off. The phase depends on Instant::now(), so assert it's one of the two.
        let cfg = LogoConfig {
            indicator: LogoIndicator::Mic,
            ..Default::default()
        };
        let obs = ObsRuntime::new(None, ObsColors::default(), false, cfg);
        assert!(obs.mic_indicator_needs_repaint());
        let color = obs.logo_indicator_color().expect("Mic indicator returns a color");
        let off = RgbColor { r: 0, g: 0, b: 0 };
        assert!(
            color == cfg.mic_unknown || color == off,
            "expected mic_unknown {:?} or off, got {:?}",
            cfg.mic_unknown,
            color
        );
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
    fn canvas_match_failed_sets_error_flash() {
        // The buffer-start canvas-match failure must surface the same error
        // flash as a failed command (the magenta the user relies on), and
        // request a repaint.
        let mut obs = fresh_runtime();
        assert!(obs.flash().is_none());
        assert!(obs.apply_event(ObsEvent::CanvasMatchFailed("boom".into())));
        let flash = obs.flash().expect("error flash set");
        assert_eq!(flash.color, ObsColors::default().error_flash);
        assert!(flash.blink.is_some(), "error flash blinks");
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
