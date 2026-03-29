mod audio;
mod config;
mod device;
mod icons;
mod led;
mod osd;
mod service;
mod udev;

use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use log::{info, warn};

use config::{Action, ControlId, RainbowStyle, RgbMode};
use device::{Control, Event, PcPanelPro};
use led::{LedMode, LogoMode, Rgb};

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
    });
    rx
}

fn apply_rgb(panel: &PcPanelPro, mode: RgbMode) -> Result<()> {
    match mode {
        RgbMode::Solid { r, g, b } => {
            let color = Rgb::new(r, g, b);
            let led = LedMode::Static(color);
            led::set_knob_colors(panel, &[led; 5])?;
            led::set_slider_colors(panel, &[led; 4])?;
            led::set_slider_label_colors(panel, &[led; 4])?;
            led::set_logo(panel, LogoMode::Static(color))?;
        }
        RgbMode::Rainbow { style } => {
            let rainbow_type = match style {
                RainbowStyle::Horizontal => 0x01,
                RainbowStyle::Vertical => 0x02,
            };
            led::set_rainbow(panel, rainbow_type, 200, 64)?;
        }
    }
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let config_path = cli
        .config
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

    let audio = audio::AudioController::connect()?;

    info!("connecting to PCPanel Pro...");
    let panel = PcPanelPro::open()?;

    // Apply RGB config
    if let Some(rgb_mode) = config.rgb {
        match rgb_mode {
            RgbMode::Solid { r, g, b } => {
                info!("RGB mode: solid (#{:02X}{:02X}{:02X})", r, g, b);
            }
            RgbMode::Rainbow { style } => {
                let style_name = match style {
                    RainbowStyle::Horizontal => "horizontal",
                    RainbowStyle::Vertical => "vertical",
                };
                info!("RGB mode: rainbow ({style_name})");
            }
        }
        apply_rgb(&panel, rgb_mode)?;
    }

    // Monitor for system resume to re-apply LED state
    let resume_rx = spawn_resume_monitor();

    info!("listening for events (Ctrl+C to quit)...");
    loop {
        // Check for resume signal
        if resume_rx.try_recv().is_ok() {
            info!("system resumed from sleep, re-applying LED config");
            if let Some(rgb_mode) = config.rgb {
                if let Err(e) = apply_rgb(&panel, rgb_mode) {
                    warn!("failed to re-apply RGB after resume: {e}");
                }
            }
        }

        let event = match panel.read_event()? {
            Some(e) => e,
            None => continue,
        };

        match event {
            Event::AnalogChange { control, value } => {
                let control_id = match control {
                    Control::Knob(i) => ControlId::Knob(i),
                    Control::Slider(i) => ControlId::Slider(i),
                    Control::Button(_) => unreachable!(),
                };

                if let Some(Action::Volume { apps, icon }) = config.mappings.get(&control_id) {
                    let pct = (value as f32 / 255.0 * 100.0) as u8;
                    let mut any_matched = false;
                    for app in apps {
                        if Action::is_system(app) {
                            audio.set_system_volume(value)?;
                            any_matched = true;
                        } else if audio.set_app_volume(app, value)? {
                            any_matched = true;
                        }
                        if cli.verbose {
                            if Action::is_system(app) {
                                println!("System volume: {pct}%");
                            } else {
                                println!("{app} volume: {pct}%");
                            }
                        }
                    }
                    // Show OSD once per control event, only if something matched
                    if any_matched {
                        if apps.iter().any(|a| Action::is_system(a)) {
                            osd::volume_changed(pct as i32);
                        } else {
                            let label = apps.join("\n");
                            let icon_name = icons::resolve(icon.as_deref(), apps);
                            osd::media_player_volume_changed(pct as i32, &label, &icon_name);
                        }
                    }
                }
            }
            Event::ButtonPress { index } => {
                let control_id = ControlId::Button(index);
                if let Some(Action::ToggleMute { apps, icon }) = config.mappings.get(&control_id) {
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
            }
            Event::ButtonRelease { .. } => {}
        }
    }
}
