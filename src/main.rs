mod audio;
mod config;
mod device;
mod icons;
mod led;
mod obs;
mod osd;
mod service;
mod udev;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use log::{info, warn};

use config::{Action, ControlId, ObsColors, RainbowStyle, RgbColor, RgbMode};
use device::{Control, Event, PcPanelPro};
use led::{LedMode, LogoMode, Rgb};
use obs::{ObsCommand, ObsEvent, ObsHandle};

#[derive(Parser)]
#[command(name = "pcp_rust", about = "PCPanel Pro controller for Linux")]
struct Cli {
    /// Install udev rules for non-root device access (requires root)
    #[arg(long)]
    create_udev_rules: bool,

    /// List currently running audio applications
    #[arg(long)]
    list_apps: bool,

    /// Path to config file [default: ~/.config/pcpanel/config.toml]
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Print volume changes to stdout
    #[arg(long, short)]
    verbose: bool,

    /// Install systemd user service for running in the background
    #[arg(long)]
    install_service: bool,

    /// Remove systemd user service
    #[arg(long)]
    remove_service: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    if cli.create_udev_rules {
        return udev::create_udev_rules();
    }

    if cli.install_service {
        return service::install();
    }

    if cli.remove_service {
        return service::remove();
    }

    if cli.list_apps {
        return list_apps();
    }

    run(cli)
}

fn list_apps() -> Result<()> {
    let audio = audio::AudioController::connect()?;
    let apps = audio.list_apps()?;

    if apps.is_empty() {
        println!("No audio applications currently running.");
        return Ok(());
    }

    println!("Audio applications currently running:");
    for app in &apps {
        let pid = app.pid.as_deref().unwrap_or("?");

        // Try to find a useful extra identifier: binary name or /proc/comm
        let extra = app
            .binary
            .as_deref()
            .filter(|b| !b.eq_ignore_ascii_case(&app.name))
            .map(|b| b.to_string())
            .or_else(|| {
                app.pid
                    .as_deref()
                    .filter(|p| p.chars().all(|c| c.is_ascii_digit()))
                    .and_then(|p| std::fs::read_to_string(format!("/proc/{p}/comm")).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|c| !c.eq_ignore_ascii_case(&app.name))
            });

        match extra {
            Some(name) => {
                println!("  {:<24} (PID: {}, binary: {})", app.name, pid, name);
            }
            None => {
                println!("  {:<24} (PID: {})", app.name, pid);
            }
        }
    }
    println!();
    println!("Use these names in your config file as the \"app\" value.");

    Ok(())
}

/// Subscribe to logind's `PrepareForSleep` signal on the system bus and
/// send `()` through the returned channel whenever the system resumes
/// from sleep. Uses zbus directly instead of forking `gdbus monitor` —
/// no subprocess, no text parsing, robust to gdbus binary not being on PATH.
fn spawn_resume_monitor() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use zbus::MatchRule;
        use zbus::blocking::{Connection, MessageIterator};

        let conn = match Connection::system() {
            Ok(c) => c,
            Err(e) => {
                warn!("resume monitor: failed to connect to system bus: {e}");
                return;
            }
        };

        let rule = match MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.login1.Manager")
            .and_then(|b| b.member("PrepareForSleep"))
            .map(|b| b.build())
        {
            Ok(r) => r,
            Err(e) => {
                warn!("resume monitor: failed to build match rule: {e}");
                return;
            }
        };

        let iter = match MessageIterator::for_match_rule(rule, &conn, None) {
            Ok(i) => i,
            Err(e) => {
                warn!("resume monitor: failed to subscribe to PrepareForSleep: {e}");
                return;
            }
        };

        for msg in iter {
            let Ok(msg) = msg else { continue };
            // The signal body is a single bool: true = about to sleep,
            // false = just woke up. We only care about resume.
            let body = msg.body();
            if let Ok(going_to_sleep) = body.deserialize::<bool>()
                && !going_to_sleep {
                    info!("detected system resume");
                    if tx.send(()).is_err() {
                        break; // main thread gone
                    }
                }
        }
    });
    rx
}

fn apply_rgb(panel: &PcPanelPro, mode: RgbMode) -> Result<()> {
    match mode {
        RgbMode::Solid { r, g, b } => {
            info!("RGB mode: solid (#{:02X}{:02X}{:02X})", r, g, b);
            set_all_solid(panel, RgbColor { r, g, b })?;
        }
        RgbMode::Rainbow { style } => {
            let (rainbow_type, style_name) = match style {
                RainbowStyle::Horizontal => (led::ANIM_RAINBOW_HORIZONTAL, "horizontal"),
                RainbowStyle::Vertical => (led::ANIM_RAINBOW_VERTICAL, "vertical"),
            };
            info!("RGB mode: rainbow ({style_name})");
            led::set_rainbow(panel, rainbow_type, config::DEFAULT_BRIGHTNESS, config::DEFAULT_SPEED)?;
        }
        RgbMode::Gradient { color1, color2 } => {
            info!(
                "RGB mode: gradient (#{:02X}{:02X}{:02X} -> #{:02X}{:02X}{:02X})",
                color1.r, color1.g, color1.b, color2.r, color2.g, color2.b
            );
            let c1 = Rgb::new(color1.r, color1.g, color1.b);
            let c2 = Rgb::new(color2.r, color2.g, color2.b);
            let led = LedMode::Gradient(c1, c2);
            led::set_knob_colors(panel, &[led; 5])?;
            led::set_slider_colors(panel, &[led; 4])?;
            led::set_slider_label_colors(panel, &[led; 4])?;
            led::set_logo(panel, LogoMode::Static(c1))?;
        }
        RgbMode::VolumeGradient { color1, color2 } => {
            info!(
                "RGB mode: volume-gradient (#{:02X}{:02X}{:02X} -> #{:02X}{:02X}{:02X})",
                color1.r, color1.g, color1.b, color2.r, color2.g, color2.b
            );
            let c1 = Rgb::new(color1.r, color1.g, color1.b);
            let c2 = Rgb::new(color2.r, color2.g, color2.b);
            let static_mode = LedMode::Static(c1);
            led::set_knob_colors(panel, &[static_mode; 5])?;
            led::set_slider_colors(panel, &[LedMode::VolumeGradient(c1, c2); 4])?;
            led::set_slider_label_colors(panel, &[static_mode; 4])?;
            led::set_logo(panel, LogoMode::Static(c1))?;
        }
        RgbMode::Wave { hue, brightness, speed, reverse, bounce } => {
            info!("RGB mode: wave (hue={hue})");
            led::set_wave(panel, hue, brightness, speed, reverse, bounce)?;
        }
        RgbMode::Breath { hue, brightness, speed } => {
            info!("RGB mode: breath (hue={hue})");
            led::set_breath(panel, hue, brightness, speed)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObsState {
    Idle,
    Recording,
    RecordingPaused,
}

/// Total on+off period for blinking flashes. Half is "on", half is "off".
const BLINK_CYCLE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
struct Flash {
    color: RgbColor,
    expires_at: Instant,
    /// If `Some`, the flash blinks between `color` and off using this cycle.
    blink: Option<BlinkConfig>,
}

#[derive(Debug, Clone, Copy)]
struct BlinkConfig {
    started_at: Instant,
    /// Total on+off cycle length. Half is "on", half is "off".
    cycle: Duration,
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
    fn current_color(&self) -> RgbColor {
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
struct ObsRuntime {
    handle: Option<ObsHandle>,
    connected: bool,
    state: ObsState,
    colors: ObsColors,
    flash: Option<Flash>,
    /// Whether OBS's replay buffer is currently running. `None` until OBS
    /// reports its state (during initial-status query on connect or via an
    /// event). Reset to `None` on disconnect.
    replay_buffer_active: Option<bool>,
    /// Pulled from `ObsConfig.paused_use_breath`. When true, paused state
    /// renders as a global breath animation; when false, solid color so
    /// the logo can keep its replay-buffer indicator.
    paused_use_breath: bool,
}

impl ObsRuntime {
    fn new(handle: Option<ObsHandle>, colors: ObsColors, paused_use_breath: bool) -> Self {
        Self {
            handle,
            connected: false,
            state: ObsState::Idle,
            colors,
            flash: None,
            replay_buffer_active: None,
            paused_use_breath,
        }
    }

    /// Drain pending events from the OBS thread. Returns `true` if any
    /// event changed something that requires a LED repaint.
    fn drain_events(&mut self) -> bool {
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
    fn apply_event(&mut self, event: ObsEvent) -> bool {
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
    fn expire_flash(&mut self) -> bool {
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
    fn dispatch(&mut self, cmd: ObsCommand, verbose: bool) -> bool {
        if verbose {
            println!("OBS: {}", cmd.label());
        }
        let send_result = match &self.handle {
            Some(h) if self.connected => h.commands_tx.send(cmd),
            _ => {
                // Either no [obs] in config (config validation rejects OBS
                // actions when [obs] is absent, so this is normally only
                // hit when OBS is disconnected) or the thread is gone.
                warn!("OBS not connected — {} skipped", cmd.label());
                self.set_error_flash();
                return true;
            }
        };
        if send_result.is_err() {
            // Defensive: the OBS thread runs an infinite loop that owns
            // cmd_rx for its entire lifetime, so this branch only fires if
            // the runtime itself collapsed. Treated the same as disconnected.
            warn!("OBS thread unreachable — {} skipped", cmd.label());
            self.set_error_flash();
            return true;
        }
        false
    }
}

/// Set every LED region (knobs, sliders, slider labels, logo) to one solid color.
///
/// The slider strips render `LedMode::Static` with a desaturation gradient
/// toward the top — they look whiter the higher you go. Sending a uniform
/// `Gradient(c, c)` (two identical colors) bypasses that firmware behavior
/// and forces an even color across the whole strip. Knobs, slider labels,
/// and the logo render `Static` correctly.
fn set_all_solid(panel: &PcPanelPro, c: RgbColor) -> Result<()> {
    let rgb = Rgb::new(c.r, c.g, c.b);
    let static_mode = LedMode::Static(rgb);
    let uniform_slider = LedMode::Gradient(rgb, rgb);
    led::set_knob_colors(panel, &[static_mode; 5])?;
    led::set_slider_colors(panel, &[uniform_slider; 4])?;
    led::set_slider_label_colors(panel, &[static_mode; 4])?;
    led::set_logo(panel, LogoMode::Static(rgb))?;
    Ok(())
}

/// Repaint the LEDs based on the current OBS state and any active flash.
/// Flash takes precedence over state.
///
/// When OBS is connected, the logo is owned by replay-buffer status and
/// stays on its indicator color across the recording / idle / paused /
/// flash states. The one exception is `RecordingPaused` when
/// `paused_use_breath` is set: that uses the device's global breath
/// animation, which drives every LED including the logo, so the logo
/// joins the breath in that mode.
///
/// When OBS is disconnected, the panel and logo follow the user's `[rgb]`
/// mode together — pcp_rust pretends OBS doesn't exist.
fn paint_leds(panel: &PcPanelPro, obs: &ObsRuntime, idle_rgb: Option<RgbMode>) -> Result<()> {
    if let Some(f) = obs.flash {
        // Panel flashes; logo stays on its replay-buffer indicator when
        // connected (so the user keeps glanceable replay state even during
        // a flash). Disconnected flashes paint the logo too — there's
        // no indicator to preserve.
        paint_panel_solid(panel, f.current_color())?;
        if obs.connected {
            paint_logo_replay_status(panel, obs)?;
        } else {
            paint_logo_solid(panel, f.current_color())?;
        }
        return Ok(());
    }
    match obs.state {
        ObsState::Idle if obs.connected => {
            paint_panel_solid(panel, obs.colors.idle_panel)?;
            paint_logo_replay_status(panel, obs)?;
        }
        ObsState::Idle => match idle_rgb {
            Some(mode) => apply_rgb(panel, mode)?,
            None => set_all_solid(panel, RgbColor { r: 0, g: 0, b: 0 })?,
        },
        ObsState::Recording => {
            paint_panel_solid(panel, obs.colors.recording)?;
            paint_logo_replay_status(panel, obs)?;
        }
        ObsState::RecordingPaused => {
            if obs.paused_use_breath {
                // Global breath animation — drives every LED including the
                // logo, so the replay-buffer indicator is unavailable during
                // paused.
                let hue = led::rgb_to_hue(Rgb::new(
                    obs.colors.paused.r,
                    obs.colors.paused.g,
                    obs.colors.paused.b,
                ));
                led::set_breath(panel, hue, config::DEFAULT_BRIGHTNESS, config::DEFAULT_SPEED)?;
            } else {
                paint_panel_solid(panel, obs.colors.paused)?;
                paint_logo_replay_status(panel, obs)?;
            }
        }
    }
    Ok(())
}

/// Paint knobs/sliders/labels to one solid color. Leaves the logo alone.
///
/// The slider strips have no firmware mode that lights all LEDs uniformly.
/// Two options exist and we picked the second:
///   - `Static(c)` lights every LED but applies a bottom-bright/top-dim
///     brightness ramp — washed-out appearance.
///   - `Gradient(c, c)` renders the strip as a level meter: only LEDs
///     below the physical slider position are lit, with uniform color.
///
/// Gradient looks more deliberate (it reads as "your slider is at X")
/// than Static's brightness ramp, so we use it.
fn paint_panel_solid(panel: &PcPanelPro, c: RgbColor) -> Result<()> {
    let rgb = Rgb::new(c.r, c.g, c.b);
    let static_mode = LedMode::Static(rgb);
    let uniform_slider = LedMode::Gradient(rgb, rgb);
    led::set_knob_colors(panel, &[static_mode; 5])?;
    led::set_slider_colors(panel, &[uniform_slider; 4])?;
    led::set_slider_label_colors(panel, &[static_mode; 4])
}

fn paint_logo_solid(panel: &PcPanelPro, c: RgbColor) -> Result<()> {
    led::set_logo(panel, LogoMode::Static(Rgb::new(c.r, c.g, c.b)))
}

fn paint_logo_replay_status(panel: &PcPanelPro, obs: &ObsRuntime) -> Result<()> {
    let color = match obs.replay_buffer_active {
        Some(true) => obs.colors.replay_active,
        Some(false) | None => obs.colors.replay_inactive,
    };
    paint_logo_solid(panel, color)
}

fn run(cli: Cli) -> Result<()> {
    let config_path = cli
        .config
        .clone()
        .or_else(config::default_config_path)
        .context("could not determine config path")?;

    if !config_path.exists() {
        bail!(
            "Config file not found: {}\n\
             Create one or specify a path with --config",
            config_path.display()
        );
    }

    let config = config::load_config(&config_path)?;
    info!("loaded config from {}", config_path.display());
    info!("{} control(s) mapped", config.mappings.len());

    let mut audio = audio::AudioController::connect()?;

    info!("connecting to PCPanel Pro...");
    let mut panel = PcPanelPro::open()?;

    // Apply initial LED state. If [rgb] is omitted, turn all LEDs off and warn.
    if let Some(rgb_mode) = config.rgb {
        apply_rgb(&panel, rgb_mode)?;
    } else {
        warn!("no [rgb] section in config; LEDs will be off");
        set_all_solid(&panel, RgbColor { r: 0, g: 0, b: 0 })?;
    }

    // Spawn the OBS background thread if [obs] is configured. spawn_obs_thread
    // itself returns Option (None if the thread can't be spawned), so we
    // .and_then to flatten Option<Option<_>>.
    let obs_handle: Option<ObsHandle> = config
        .obs
        .as_ref()
        .and_then(|cfg| obs::spawn_obs_thread(cfg.clone()));
    // ObsColors is always present so `paint_leds` and `ObsRuntime` have a
    // value to read. When [obs] is absent, no OBS events fire and no OBS
    // button dispatches happen, so the defaulted struct is never actually
    // consulted — it just keeps the types simple.
    let obs_colors: ObsColors = config
        .obs
        .as_ref()
        .map(|c| c.colors)
        .unwrap_or_default();
    let paused_use_breath = config
        .obs
        .as_ref()
        .map(|c| c.paused_use_breath)
        .unwrap_or(false);
    let mut obs = ObsRuntime::new(obs_handle, obs_colors, paused_use_breath);

    // Monitor for system resume to re-apply LED state
    let resume_rx = spawn_resume_monitor();

    info!("listening for events (Ctrl+C to quit)...");
    loop {
        let mut led_dirty = obs.drain_events();
        led_dirty |= obs.expire_flash();

        // Check for resume signal — repaint everything from current state.
        if resume_rx.try_recv().is_ok() {
            info!("system resumed from sleep, re-applying LED state");
            led_dirty = true;
        }

        // Flush any deferred stream-restore persistence writes that have
        // been idle long enough. Coalesces slider-scrub bursts into a
        // single DB write per app.
        audio.flush_persist_writes();

        // Read a panel event (may block ~100ms). Process before painting so
        // any button-triggered state changes (e.g., a local error flash from
        // a dispatch when OBS is disconnected) get painted this iteration.
        if let Some(event) = panel.read_event()? {
            led_dirty |= handle_panel_event(event, &cli, &config, &mut audio, &mut obs)?;
        }

        // Repaint LEDs if anything changed (OBS event, flash expiry, resume,
        // or button-press-induced flash). LED-write failures are logged and
        // swallowed — they shouldn't kill the main loop. If one of the four
        // region writes inside paint_leds fails, the remaining ones are
        // skipped for this iteration but will be retried on the next dirty
        // repaint.
        if led_dirty
            && let Err(e) = paint_leds(&panel, &obs, config.rgb) {
                warn!("failed to repaint LEDs: {e}");
            }
    }
}

/// Apply a volume slider/knob change to one configured app. Dispatches to
/// the right audio API based on whether the app is the system output,
/// the mic, or a regular app. Returns `true` if the change matched
/// something (always true for system/mic, only true for apps when at
/// least one sink-input matched). PA failures are logged and treated as
/// no-match — they never propagate out of the main loop.
fn apply_volume_to(audio: &mut audio::AudioController, app: &str, value: u8) -> bool {
    let result = if Action::is_system(app) {
        audio.set_system_volume(value).map(|()| true)
    } else if Action::is_mic(app) {
        audio.set_mic_volume(value).map(|()| true)
    } else {
        audio.set_app_volume(app, value)
    };
    result.unwrap_or_else(|e| {
        warn!("audio: set volume for {app} failed: {e}");
        false
    })
}

/// Toggle mute for one configured app. Returns the new mute state, or
/// `None` if the app wasn't running (for app targets) or PA failed. PA
/// failures are logged and treated as no-match — never propagated.
fn toggle_mute_for(audio: &mut audio::AudioController, app: &str) -> Option<bool> {
    let result = if Action::is_system(app) {
        audio.toggle_system_mute().map(Some)
    } else if Action::is_mic(app) {
        audio.toggle_mic_mute().map(Some)
    } else {
        audio.toggle_app_mute(app)
    };
    result.unwrap_or_else(|e| {
        warn!("audio: toggle mute for {app} failed: {e}");
        None
    })
}

fn handle_panel_event(
    event: Event,
    cli: &Cli,
    config: &config::Config,
    audio: &mut audio::AudioController,
    obs: &mut ObsRuntime,
) -> Result<bool> {
    let mut led_dirty = false;
    match event {
        Event::AnalogChange { control, value } => {
            let control_id = match control {
                Control::Knob(i) => ControlId::Knob(i),
                Control::Slider(i) => ControlId::Slider(i),
            };

            if let Some(Action::Volume { apps, icon }) = config.mappings.get(&control_id) {
                let pct = (value as f32 / 255.0 * 100.0) as u8;
                let mut matched_apps: Vec<String> = Vec::new();
                for app in apps {
                    if apply_volume_to(audio, app, value) {
                        matched_apps.push(app.clone());
                    }
                    if cli.verbose {
                        let label = if Action::is_system(app) {
                            "System"
                        } else if Action::is_mic(app) {
                            "Mic"
                        } else {
                            app.as_str()
                        };
                        println!("{label} volume: {pct}%");
                    }
                }
                // Show OSD once per control event, only if something matched
                if !matched_apps.is_empty() {
                    if matched_apps.iter().any(|a| Action::is_system(a)) {
                        osd::volume_changed(pct as i32);
                    } else if matched_apps.iter().any(|a| Action::is_mic(a)) {
                        osd::microphone_volume_changed(pct as i32);
                    } else {
                        let label = matched_apps.join("\n");
                        let icon_name = icons::resolve(icon.as_deref(), &matched_apps);
                        osd::media_player_volume_changed(pct as i32, &label, &icon_name);
                    }
                }
            }
        }
        Event::ButtonPress { index } => {
            let control_id = ControlId::Button(index);
            match config.mappings.get(&control_id) {
                Some(Action::ToggleMute { apps, icon }) => {
                    for app in apps {
                        if let Some(muted) = toggle_mute_for(audio, app) {
                            if cli.verbose {
                                let label = if Action::is_system(app) {
                                    "System"
                                } else if Action::is_mic(app) {
                                    "Mic"
                                } else {
                                    app.as_str()
                                };
                                println!("{label} mute: {}", if muted { "on" } else { "off" });
                            }
                            if Action::is_system(app) {
                                osd::show_mute("System", muted);
                            } else if Action::is_mic(app) {
                                osd::show_mic_mute(muted);
                            } else {
                                let icon_name = icons::resolve_mute(icon.as_deref(), apps, muted);
                                osd::show_text(
                                    &icon_name,
                                    &format!("{app}: {}", if muted { "Muted" } else { "Unmuted" }),
                                );
                            }
                        }
                    }
                }
                Some(Action::ObsSaveReplay) => {
                    led_dirty |= obs.dispatch(ObsCommand::SaveReplay, cli.verbose);
                }
                Some(Action::ObsToggleRecording) => {
                    led_dirty |= obs.dispatch(ObsCommand::ToggleRecording, cli.verbose);
                }
                Some(Action::ObsPauseRecording) => {
                    led_dirty |= obs.dispatch(ObsCommand::PauseRecording, cli.verbose);
                }
                Some(Action::ObsSplitRecording) => {
                    led_dirty |= obs.dispatch(ObsCommand::SplitRecording, cli.verbose);
                }
                _ => {}
            }
        }
        Event::ButtonRelease { .. } => {}
    }
    Ok(led_dirty)
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
        ObsRuntime::new(None, ObsColors::default(), false)
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
    fn apply_connected_dirty_only_on_transition() {
        let mut obs = fresh_runtime();
        // First Connected: false → true, dirty.
        assert!(obs.apply_event(ObsEvent::Connected));
        assert!(obs.connected);

        // Redundant Connected: already true, not dirty.
        assert!(!obs.apply_event(ObsEvent::Connected));
    }
}
