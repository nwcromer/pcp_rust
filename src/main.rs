mod audio;
mod config;
mod device;
mod icons;
mod led;
mod obs;
mod osd;
mod paint;
mod prompt;
mod runtime;
mod service;
mod udev;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use log::{info, warn};

use config::{Action, AppTarget, ControlId, ObsColors};
use device::{Control, Event, PcPanelPro};
use obs::{ObsCommand, ObsHandle};
use paint::{log_rgb_mode, paint_leds};
use runtime::ObsRuntime;

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

        // All three pinned values (sender, interface, member) are hardcoded
        // valid D-Bus identifiers, so the builder calls can't realistically
        // error in production. The match arm exists as a defensive catch,
        // not because we expect to hit it. If the sender pin ever did fail
        // on some exotic D-Bus setup, resume detection would silently
        // disable until the daemon is restarted — acceptable for the
        // spoof-defense it buys.
        let rule = match MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.login1")
            .and_then(|b| b.interface("org.freedesktop.login1.Manager"))
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

/// Query PA for the current mic-mute state and update `obs.mic_muted`.
/// Returns `true` if the query succeeded (caller may want to bump the
/// poll-cadence timer). On PA error, leaves the field unchanged, logs at
/// warn level, and returns `false`.
fn refresh_mic_muted(audio: &audio::AudioController, obs: &mut ObsRuntime) -> bool {
    match audio.is_mic_muted() {
        Ok(muted) => {
            obs.set_mic_muted(muted);
            true
        }
        Err(e) => {
            warn!("audio: mic-mute query failed: {e}");
            false
        }
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
    let mut panel = PcPanelPro::open()?;

    // Log the configured [rgb] mode (or warn if omitted). The actual initial
    // LED write happens via paint_leds below — doing it here too would
    // restart firmware animations (rainbow/wave/breath) on the second write.
    match config.rgb {
        Some(rgb_mode) => log_rgb_mode(rgb_mode),
        None => warn!("no [rgb] section in config; LEDs will be off"),
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
    let mut obs = ObsRuntime::new(obs_handle, obs_colors, paused_use_breath, config.logo);

    // Seed initial mic-mute state so the first paint reflects reality
    // instead of the `false` default. A PA failure here is non-fatal —
    // we'll just start with mic_muted=false and pick up the real state
    // on the next poll.
    if obs.mic_indicator_enabled() {
        refresh_mic_muted(&audio, &mut obs);
    }

    // Initial LED write. paint_leds handles the disconnected-idle case
    // (apply_rgb for the [rgb] mode, or solid black if no [rgb]) plus the
    // mic-indicator override when seeded mic_muted is true.
    if let Err(e) = paint_leds(&panel, &obs, config.rgb) {
        warn!("initial LED paint failed: {e}");
    }

    // Monitor for system resume to re-apply LED state
    let resume_rx = spawn_resume_monitor();

    // Poll mic mute state periodically so external mute changes (system
    // hotkeys, KDE's tray, other tools) are reflected on the logo. Button
    // presses bypass this by updating obs.mic_muted directly. Skipped
    // entirely if the user didn't configure a mic-muted color.
    const MIC_POLL_INTERVAL: Duration = Duration::from_millis(250);
    // None = "never polled" → the first loop iteration polls immediately,
    // so a failed startup seed (PA still booting) doesn't leave a stale
    // mic_muted=false visible for up to MIC_POLL_INTERVAL.
    let mut last_mic_poll: Option<Instant> = None;
    // Track repeated PA failures so we warn once per outage instead of
    // every 250 ms. Reset to 0 on a successful poll.
    let mut mic_poll_failures: u32 = 0;

    info!("listening for events (Ctrl+C to quit)...");
    loop {
        let mut led_dirty = obs.drain_events();
        led_dirty |= obs.expire_flash();
        // While the mic indicator is in its "unknown" state, force a
        // repaint each iteration so the logo's blink renders. The blink
        // phase is time-driven inside runtime, so just keeping led_dirty
        // true is enough. Gate on logo_indicator_visible: if the current
        // appearance can't show the indicator (a flash owns the panel, or
        // a global animation owns the logo), forcing a repaint would only
        // restart that animation every iteration without rendering any
        // blink. Recovery/state-change repaints still happen via the
        // poll/resume paths below, so suppressing the blink-only repaint
        // here can't strand a stale frame.
        led_dirty |= obs.mic_indicator_needs_repaint()
            && paint::logo_indicator_visible(&obs, config.rgb);

        // Check for resume signal — re-poll mic so the post-resume paint
        // doesn't show stale state if the mic was toggled during suspend.
        // Only bump last_mic_poll on a successful query so that a
        // not-yet-ready PA gets re-polled on the next loop iteration
        // instead of waiting a full MIC_POLL_INTERVAL.
        if resume_rx.try_recv().is_ok() {
            info!("system resumed from sleep, re-applying LED state");
            if obs.mic_indicator_enabled() && refresh_mic_muted(&audio, &mut obs) {
                last_mic_poll = Some(Instant::now());
            }
            led_dirty = true;
        }

        let mic_poll_due = obs.mic_indicator_enabled()
            && last_mic_poll.is_none_or(|t| t.elapsed() >= MIC_POLL_INTERVAL);
        if mic_poll_due {
            last_mic_poll = Some(Instant::now());
            match audio.is_mic_muted() {
                Ok(muted) => {
                    let was_stale = obs.mic_indicator_needs_repaint();
                    if mic_poll_failures > 0 {
                        // Approximate outage duration. Duration's Debug
                        // formatter picks an appropriate unit (250ms,
                        // 1s, 1.25s, etc.) — better than an integer
                        // second count that truncates short outages to 0.
                        let outage = MIC_POLL_INTERVAL * mic_poll_failures;
                        info!("audio: mic-mute poll recovered after ~{outage:?}");
                        mic_poll_failures = 0;
                    }
                    let changed = muted != obs.mic_muted();
                    // Always set on a successful poll — even when the value
                    // hasn't changed — because set_mic_muted is the entry
                    // point for refreshing mic_confirmed_at, which is what
                    // keeps the logo out of its "unknown" stale state.
                    obs.set_mic_muted(muted);
                    if changed || was_stale {
                        led_dirty = true;
                    }
                }
                Err(e) => {
                    if mic_poll_failures == 0 {
                        warn!("audio: mic-mute poll failed: {e} (suppressing further warnings until recovery)");
                    }
                    mic_poll_failures = mic_poll_failures.saturating_add(1);
                }
            }
        }

        // Flush any deferred stream-restore persistence writes that have
        // been idle long enough. Coalesces slider-scrub bursts into a
        // single DB write per app.
        audio.flush_persist_writes();

        // Read a panel event (may block ~100ms). Process before painting so
        // any button-triggered state changes (e.g., a local error flash from
        // a dispatch when OBS is disconnected) get painted this iteration.
        if let Some(event) = panel.read_event()? {
            led_dirty |= handle_panel_event(event, &cli, &config, &mut audio, &mut obs);
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

/// Apply a volume slider/knob change to one configured target. Dispatches
/// by `AppTarget` variant. Returns `true` if the change matched something
/// (always true for `System`/`Mic`, only true for `Named` when at least
/// one sink-input matched). PA failures are logged and treated as no-match.
fn apply_volume_to(audio: &mut audio::AudioController, target: &AppTarget, value: u8) -> bool {
    let result = match target {
        AppTarget::System => audio.set_system_volume(value).map(|()| true),
        AppTarget::Mic => audio.set_mic_volume(value).map(|()| true),
        AppTarget::Named(name) => audio.set_app_volume(name, value),
    };
    result.unwrap_or_else(|e| {
        warn!("audio: set volume for {} failed: {e}", target.label());
        false
    })
}

/// Toggle mute for one target. Returns the new mute state, or `None` if
/// the named app wasn't running or PA failed. PA failures are logged.
fn toggle_mute_for(audio: &mut audio::AudioController, target: &AppTarget) -> Option<bool> {
    let result = match target {
        AppTarget::System => audio.toggle_system_mute().map(Some),
        AppTarget::Mic => audio.toggle_mic_mute().map(Some),
        AppTarget::Named(name) => audio.toggle_app_mute(name),
    };
    result.unwrap_or_else(|e| {
        warn!("audio: toggle mute for {} failed: {e}", target.label());
        None
    })
}

/// Extract the configured names from a slice of target refs, dropping
/// `System`/`Mic` (which aren't candidates for icon lookup). Takes a slice
/// of `&AppTarget` so callers can pass either the configured list or a
/// matched-only filtered subset without re-cloning.
fn named_target_names<'a>(targets: impl IntoIterator<Item = &'a AppTarget>) -> Vec<String> {
    targets
        .into_iter()
        .filter_map(|t| match t {
            AppTarget::Named(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn handle_panel_event(
    event: Event,
    cli: &Cli,
    config: &config::Config,
    audio: &mut audio::AudioController,
    obs: &mut ObsRuntime,
) -> bool {
    let mut led_dirty = false;
    match event {
        Event::AnalogChange { control, value } => {
            let control_id = match control {
                Control::Knob(i) => ControlId::Knob(i),
                Control::Slider(i) => ControlId::Slider(i),
            };

            if let Some(Action::Volume(action)) = config.mappings.get(&control_id) {
                let pct = (value as f32 / 255.0 * 100.0) as u8;
                let mut matched: Vec<&AppTarget> = Vec::new();
                for target in &action.targets {
                    // Only trace targets that actually changed — a miss
                    // (app not running) or PA error is already logged at
                    // debug!/warn! in the audio layer, so printing here too
                    // would falsely claim a change happened.
                    if apply_volume_to(audio, target, value) {
                        matched.push(target);
                        if cli.verbose {
                            println!("{} volume: {pct}%", target.label());
                        }
                    }
                }
                // Show OSD once per control event, only if something matched.
                // Priority: system → mic → media-player (apps).
                if matched.iter().any(|t| matches!(t, AppTarget::System)) {
                    osd::volume_changed(pct as i32);
                } else if matched.iter().any(|t| matches!(t, AppTarget::Mic)) {
                    osd::microphone_volume_changed(pct as i32);
                } else if !matched.is_empty() {
                    // Icon lookup uses only the matched Named targets so the
                    // icon corresponds to apps that actually changed volume
                    // (not configured-but-not-running apps that would
                    // otherwise win the .desktop substring search).
                    let names = named_target_names(matched.iter().copied());
                    let label = matched
                        .iter()
                        .map(|t| t.label())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let icon_name = icons::resolve(action.icon.as_deref(), &names);
                    osd::media_player_volume_changed(pct as i32, &label, &icon_name);
                }
            }
        }
        Event::ButtonPress { index } => {
            let control_id = ControlId::Button(index);
            match config.mappings.get(&control_id) {
                Some(Action::ToggleMute(action)) => {
                    for target in &action.targets {
                        let Some(muted) = toggle_mute_for(audio, target) else {
                            continue;
                        };
                        if cli.verbose {
                            println!(
                                "{} mute: {}",
                                target.label(),
                                if muted { "on" } else { "off" }
                            );
                        }
                        match target {
                            AppTarget::System => osd::show_mute("System", muted),
                            AppTarget::Mic => {
                                // Update the cached mic-mute state immediately
                                // so the logo repaints this iteration instead
                                // of waiting up to MIC_POLL_INTERVAL. Always
                                // call set_mic_muted on a confirmed reading
                                // so mic_confirmed_at stays fresh (the
                                // stale-detection trust contract depends on
                                // every successful confirmation refreshing
                                // it, not just the changed ones).
                                if obs.mic_indicator_enabled() {
                                    let changed = obs.mic_muted() != muted;
                                    let was_stale = obs.mic_indicator_needs_repaint();
                                    obs.set_mic_muted(muted);
                                    if changed || was_stale {
                                        led_dirty = true;
                                    }
                                }
                                osd::show_mic_mute(muted);
                            }
                            AppTarget::Named(name) => {
                                // Icon lookup uses only this target's name,
                                // matching the OSD text shown next to it.
                                let names = [name.clone()];
                                let icon_name =
                                    icons::resolve_mute(action.icon.as_deref(), &names, muted);
                                osd::show_text(
                                    &icon_name,
                                    &format!(
                                        "{name}: {}",
                                        if muted { "Muted" } else { "Unmuted" }
                                    ),
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
    led_dirty
}

