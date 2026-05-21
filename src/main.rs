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

/// Convert an RGB color to a hue byte (0-255) for the device's breath/wave
/// effects, which take only a hue value. Saturation and value information
/// is discarded — only the dominant color wheel position is preserved.
fn rgb_to_hue(c: RgbColor) -> u8 {
    let r = c.r as f32 / 255.0;
    let g = c.g as f32 / 255.0;
    let b = c.b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue_deg = if delta < 1e-6 {
        0.0
    } else if (max - r).abs() < 1e-6 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-6 {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    let hue_deg = if hue_deg < 0.0 { hue_deg + 360.0 } else { hue_deg };
    ((hue_deg / 360.0 * 255.0).round() as u32).min(255) as u8
}

/// Repaint the LEDs based on the current OBS state and any active flash.
/// Flash takes precedence over state.
fn paint_leds(
    panel: &PcPanelPro,
    obs_state: ObsState,
    flash: Option<Flash>,
    idle_rgb: Option<RgbMode>,
    colors: &ObsColors,
) -> Result<()> {
    if let Some(f) = flash {
        return set_all_solid(panel, f.color);
    }
    match obs_state {
        ObsState::Idle => match idle_rgb {
            Some(mode) => apply_rgb(panel, mode)?,
            None => set_all_solid(panel, RgbColor { r: 0, g: 0, b: 0 })?,
        },
        ObsState::Recording => set_all_solid(panel, colors.recording)?,
        ObsState::RecordingPaused => {
            let hue = rgb_to_hue(colors.paused);
            led::set_breath(panel, hue, config::DEFAULT_BRIGHTNESS, config::DEFAULT_SPEED)?;
        }
    }
    Ok(())
}

fn start_flash(color: RgbColor, duration_ms: u64) -> Flash {
    Flash {
        color,
        expires_at: Instant::now() + Duration::from_millis(duration_ms),
    }
}

/// Send an OBS command to the OBS thread, or immediately produce an error
/// flash if OBS is disconnected / not configured. Sets `led_dirty` if the
/// LED state needs repainting (i.e., a local error flash was set).
fn dispatch_obs(
    cmd: ObsCommand,
    handle: &Option<ObsHandle>,
    connected: bool,
    colors: &ObsColors,
    flash: &mut Option<Flash>,
    led_dirty: &mut bool,
    verbose: bool,
) {
    if verbose {
        println!("OBS: {}", cmd.label());
    }
    let send_result = match handle {
        Some(h) if connected => h.commands_tx.send(cmd),
        _ => {
            // Either no [obs] in config (shouldn't happen — config validation
            // rejects OBS actions when [obs] is absent) or OBS is disconnected.
            warn!("OBS not connected — {} skipped", cmd.label());
            *flash = Some(start_flash(colors.error_flash, colors.flash_duration_ms));
            *led_dirty = true;
            return;
        }
    };
    if send_result.is_err() {
        warn!("OBS thread is gone — {} skipped", cmd.label());
        *flash = Some(start_flash(colors.error_flash, colors.flash_duration_ms));
        *led_dirty = true;
    }
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

    // Spawn the OBS background thread if [obs] is configured.
    let mut obs_handle: Option<ObsHandle> = config
        .obs
        .as_ref()
        .map(|cfg| obs::spawn_obs_thread(cfg.clone()));
    // Default colors when no [obs] is configured (only used to type-check
    // paint_leds calls; the OBS code paths never fire without [obs]).
    let obs_colors: ObsColors = config
        .obs
        .as_ref()
        .map(|c| c.colors)
        .unwrap_or_default();
    let mut obs_state = ObsState::Idle;
    let mut obs_connected = false;
    let mut flash: Option<Flash> = None;

    // Monitor for system resume to re-apply LED state
    let resume_rx = spawn_resume_monitor();

    info!("listening for events (Ctrl+C to quit)...");
    loop {
        let mut led_dirty = false;

        // Drain OBS events (non-blocking).
        if let Some(ref mut h) = obs_handle {
            while let Ok(event) = h.events_rx.try_recv() {
                match event {
                    ObsEvent::Connected => {
                        obs_connected = true;
                    }
                    ObsEvent::Disconnected => {
                        obs_connected = false;
                        // Disconnected behaves visually like Idle.
                        if obs_state != ObsState::Idle {
                            obs_state = ObsState::Idle;
                            led_dirty = true;
                        }
                    }
                    ObsEvent::RecordingActive | ObsEvent::RecordingResumed => {
                        if obs_state != ObsState::Recording {
                            obs_state = ObsState::Recording;
                            led_dirty = true;
                        }
                    }
                    ObsEvent::RecordingPaused => {
                        if obs_state != ObsState::RecordingPaused {
                            obs_state = ObsState::RecordingPaused;
                            led_dirty = true;
                        }
                    }
                    ObsEvent::RecordingStopped => {
                        if obs_state != ObsState::Idle {
                            obs_state = ObsState::Idle;
                            led_dirty = true;
                        }
                    }
                    ObsEvent::CommandSucceeded(cmd) => {
                        info!("OBS: {} succeeded", cmd.label());
                        // Skip the success flash for commands whose effect is
                        // already visible on the LEDs via a state change
                        // (toggle-record flips Idle ↔ Recording, toggle-pause
                        // flips Recording ↔ Paused). Save Replay and Split
                        // Record cause no visible state change, so they still
                        // get the flash as the only acknowledgement.
                        let visibly_changes_state = matches!(
                            cmd,
                            ObsCommand::ToggleRecording | ObsCommand::PauseRecording
                        );
                        if !visibly_changes_state {
                            flash = Some(start_flash(
                                obs_colors.success_flash,
                                obs_colors.flash_duration_ms,
                            ));
                            led_dirty = true;
                        }
                    }
                    ObsEvent::CommandFailed(cmd, msg) => {
                        warn!("OBS command failed ({}): {msg}", cmd.label());
                        flash = Some(start_flash(
                            obs_colors.error_flash,
                            obs_colors.flash_duration_ms,
                        ));
                        led_dirty = true;
                    }
                }
            }
        }

        // Check flash expiry.
        if let Some(f) = flash {
            if Instant::now() >= f.expires_at {
                flash = None;
                led_dirty = true;
            }
        }

        // Check for resume signal — repaint everything from current state.
        if resume_rx.try_recv().is_ok() {
            info!("system resumed from sleep, re-applying LED state");
            led_dirty = true;
        }

        // Read a panel event (may block ~100ms). Process before painting so
        // any button-triggered state changes (e.g., a local error flash from
        // dispatch_obs when OBS is disconnected) get painted this iteration.
        if let Some(event) = panel.read_event()? {
            handle_panel_event(
                event,
                &cli,
                &config,
                &mut audio,
                &obs_handle,
                obs_connected,
                &obs_colors,
                &mut flash,
                &mut led_dirty,
            )?;
        }

        // Repaint LEDs if anything changed (OBS event, flash expiry, resume,
        // or button-press-induced flash).
        if led_dirty {
            if let Err(e) = paint_leds(&panel, obs_state, flash, config.rgb, &obs_colors) {
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
    obs_handle: &Option<ObsHandle>,
    obs_connected: bool,
    obs_colors: &ObsColors,
    flash: &mut Option<Flash>,
    led_dirty: &mut bool,
) -> Result<()> {
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
                Some(Action::ObsSaveReplay) => dispatch_obs(
                    ObsCommand::SaveReplay,
                    obs_handle,
                    obs_connected,
                    obs_colors,
                    flash,
                    led_dirty,
                    cli.verbose,
                ),
                Some(Action::ObsToggleRecording) => dispatch_obs(
                    ObsCommand::ToggleRecording,
                    obs_handle,
                    obs_connected,
                    obs_colors,
                    flash,
                    led_dirty,
                    cli.verbose,
                ),
                Some(Action::ObsPauseRecording) => dispatch_obs(
                    ObsCommand::PauseRecording,
                    obs_handle,
                    obs_connected,
                    obs_colors,
                    flash,
                    led_dirty,
                    cli.verbose,
                ),
                Some(Action::ObsSplitRecording) => dispatch_obs(
                    ObsCommand::SplitRecording,
                    obs_handle,
                    obs_connected,
                    obs_colors,
                    flash,
                    led_dirty,
                    cli.verbose,
                ),
                _ => {}
            }
        }
        Event::ButtonRelease { .. } => {}
    }
    Ok(())
}
