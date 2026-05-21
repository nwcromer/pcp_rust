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

fn spawn_resume_monitor() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        use std::process::{Command, Stdio};

        let mut child = match Command::new("gdbus")
            .args([
                "monitor",
                "--system",
                "--dest",
                "org.freedesktop.login1",
                "--object-path",
                "/org/freedesktop/login1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to start resume monitor: {e}");
                return;
            }
        };

        let Some(stdout) = child.stdout.take() else {
            warn!("resume monitor: failed to capture stdout");
            let _ = child.kill();
            return;
        };
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            // PrepareForSleep(false) means the system just woke up
            if line.contains("PrepareForSleep") && line.contains("false") {
                info!("detected system resume");
                if tx.send(()).is_err() {
                    break; // main thread gone
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    });
    rx
}

fn apply_rgb(panel: &PcPanelPro, mode: RgbMode) -> Result<()> {
    match mode {
        RgbMode::Solid { r, g, b } => {
            info!("RGB mode: solid (#{:02X}{:02X}{:02X})", r, g, b);
            let color = Rgb::new(r, g, b);
            let led = LedMode::Static(color);
            led::set_knob_colors(panel, &[led; 5])?;
            led::set_slider_colors(panel, &[led; 4])?;
            led::set_slider_label_colors(panel, &[led; 4])?;
            led::set_logo(panel, LogoMode::Static(color))?;
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

#[derive(Debug, Clone, Copy)]
struct Flash {
    color: RgbColor,
    expires_at: Instant,
}

fn start_flash(color: RgbColor, duration_ms: u64) -> Flash {
    Flash {
        color,
        expires_at: Instant::now() + Duration::from_millis(duration_ms),
    }
}

/// All OBS-related state owned by the main thread: the handle to the OBS
/// thread, the connection flag, the current recording state, the configured
/// colors, and any active flash overlay. Bundling them avoids threading a
/// long parameter list through `dispatch_obs` and `handle_panel_event`.
struct ObsRuntime {
    handle: Option<ObsHandle>,
    connected: bool,
    state: ObsState,
    colors: ObsColors,
    flash: Option<Flash>,
}

impl ObsRuntime {
    fn new(handle: Option<ObsHandle>, colors: ObsColors) -> Self {
        Self {
            handle,
            connected: false,
            state: ObsState::Idle,
            colors,
            flash: None,
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
                self.connected = true;
                false
            }
            ObsEvent::Disconnected => {
                self.connected = false;
                // Disconnected behaves visually like Idle.
                self.transition_to(ObsState::Idle)
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
                    self.set_flash(self.colors.success_flash);
                    true
                }
            }
            ObsEvent::CommandFailed(cmd, msg) => {
                warn!("OBS command failed ({}): {msg}", cmd.label());
                self.set_flash(self.colors.error_flash);
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

    /// Set the current flash overlay using the configured duration.
    fn set_flash(&mut self, color: RgbColor) {
        self.flash = Some(start_flash(color, self.colors.flash_duration_ms));
    }

    /// Clear any expired flash. Returns `true` if a flash just expired and
    /// the LEDs need a repaint.
    fn expire_flash(&mut self) -> bool {
        match self.flash {
            Some(f) if Instant::now() >= f.expires_at => {
                self.flash = None;
                true
            }
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
                self.set_flash(self.colors.error_flash);
                return true;
            }
        };
        if send_result.is_err() {
            // Defensive: the OBS thread runs an infinite loop that owns
            // cmd_rx for its entire lifetime, so this branch only fires if
            // the runtime itself collapsed. Treated the same as disconnected.
            warn!("OBS thread unreachable — {} skipped", cmd.label());
            self.set_flash(self.colors.error_flash);
            return true;
        }
        false
    }
}

/// Set every LED region (knobs, sliders, slider labels, logo) to one solid color.
fn set_all_solid(panel: &PcPanelPro, c: RgbColor) -> Result<()> {
    let rgb = Rgb::new(c.r, c.g, c.b);
    let led = LedMode::Static(rgb);
    led::set_knob_colors(panel, &[led; 5])?;
    led::set_slider_colors(panel, &[led; 4])?;
    led::set_slider_label_colors(panel, &[led; 4])?;
    led::set_logo(panel, LogoMode::Static(rgb))?;
    Ok(())
}

/// Repaint the LEDs based on the current OBS state and any active flash.
/// Flash takes precedence over state.
fn paint_leds(panel: &PcPanelPro, obs: &ObsRuntime, idle_rgb: Option<RgbMode>) -> Result<()> {
    if let Some(f) = obs.flash {
        return set_all_solid(panel, f.color);
    }
    match obs.state {
        ObsState::Idle => match idle_rgb {
            Some(mode) => apply_rgb(panel, mode)?,
            None => set_all_solid(panel, RgbColor { r: 0, g: 0, b: 0 })?,
        },
        ObsState::Recording => set_all_solid(panel, obs.colors.recording)?,
        ObsState::RecordingPaused => {
            let hue = led::rgb_to_hue(Rgb::new(
                obs.colors.paused.r,
                obs.colors.paused.g,
                obs.colors.paused.b,
            ));
            led::set_breath(panel, hue, config::DEFAULT_BRIGHTNESS, config::DEFAULT_SPEED)?;
        }
    }
    Ok(())
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
    let panel = PcPanelPro::open()?;

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
    let mut obs = ObsRuntime::new(obs_handle, obs_colors);

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
        if led_dirty {
            if let Err(e) = paint_leds(&panel, &obs, config.rgb) {
                warn!("failed to repaint LEDs: {e}");
            }
        }
    }
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
                    let matched = if Action::is_system(app) {
                        audio.set_system_volume(value)?;
                        true
                    } else if Action::is_mic(app) {
                        audio.set_mic_volume(value)?;
                        true
                    } else {
                        audio.set_app_volume(app, value)?
                    };
                    if matched {
                        matched_apps.push(app.clone());
                    }
                    if cli.verbose {
                        if Action::is_system(app) {
                            println!("System volume: {pct}%");
                        } else if Action::is_mic(app) {
                            println!("Mic volume: {pct}%");
                        } else {
                            println!("{app} volume: {pct}%");
                        }
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
                        if Action::is_system(app) {
                            let muted = audio.toggle_system_mute()?;
                            if cli.verbose {
                                println!("System mute: {}", if muted { "on" } else { "off" });
                            }
                            osd::show_mute("System", muted);
                        } else if Action::is_mic(app) {
                            let muted = audio.toggle_mic_mute()?;
                            if cli.verbose {
                                println!("Mic mute: {}", if muted { "on" } else { "off" });
                            }
                            osd::show_mic_mute(muted);
                        } else if let Some(muted) = audio.toggle_app_mute(app)? {
                            if cli.verbose {
                                println!("{app} mute: {}", if muted { "on" } else { "off" });
                            }
                            let icon_name = icons::resolve_mute(icon.as_deref(), apps, muted);
                            osd::show_text(&icon_name, &format!("{app}: {}", if muted { "Muted" } else { "Unmuted" }));
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
