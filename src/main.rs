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
use log::{debug, info, warn};

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

        // Try to find a useful extra identifier: binary name or /proc/comm.
        // Review-accepted: the /proc/comm read below duplicates the one in
        // audio.rs `comm_for_pid`, but deliberately — this cold display path
        // keeps original case and skips the match-cache, where the hot-path
        // matcher lower-cases and memoizes. A shared helper would be net
        // worse. See the matching note in `comm_for_pid`.
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

/// Periodic poll of the default mic's mute state so external changes (system
/// hotkeys, KDE tray, other tools) show up on the logo. Owns its cadence
/// timer and the consecutive-failure counter, so the main loop just calls
/// `tick` and reads the returned dirty flag.
struct MicPoller {
    /// `None` = never polled, so the first `tick` polls immediately — a failed
    /// startup seed (PA still booting) doesn't leave a stale value visible for
    /// up to one interval.
    ///
    /// Review-accepted: because `run()` also seeds the mic state once before
    /// the loop, that immediate first `tick` is a redundant PA query at
    /// startup. Left as-is deliberately — the immediate poll is what avoids a
    /// stale-value window, and seeding `last_poll` to defer it would
    /// reintroduce exactly that window when the startup seed fails. One extra
    /// query, once, is the cheaper trade.
    last_poll: Option<Instant>,
    /// Consecutive failed polls. Used to warn once at the start of an outage
    /// and report the recovery duration when it clears. Reset to 0 on success.
    failures: u32,
}

impl MicPoller {
    const INTERVAL: Duration = Duration::from_millis(250);

    fn new() -> Self {
        Self { last_poll: None, failures: 0 }
    }

    /// Poll the mic state if the indicator is enabled and the interval has
    /// elapsed. Returns whether the LEDs need a repaint.
    fn tick(&mut self, audio: &audio::AudioController, obs: &mut ObsRuntime) -> bool {
        let due = obs.mic_indicator_enabled()
            && self.last_poll.is_none_or(|t| t.elapsed() >= Self::INTERVAL);
        if !due {
            return false;
        }
        self.last_poll = Some(Instant::now());
        match audio.is_mic_muted() {
            Ok(muted) => {
                if self.failures > 0 {
                    // Approximate outage duration. Duration's Debug formatter
                    // picks an appropriate unit (250ms, 1s, 1.25s, etc.) —
                    // better than an integer second count that truncates short
                    // outages to 0.
                    let outage = Self::INTERVAL * self.failures;
                    info!("audio: mic-mute poll recovered after ~{outage:?}");
                    self.failures = 0;
                }
                // Always record on a successful poll — even when unchanged — so
                // mic_confirmed_at stays fresh and the logo leaves the "unknown"
                // stale state. update_mic_muted reports whether a repaint is
                // needed (value changed, or we just recovered).
                obs.update_mic_muted(muted)
            }
            Err(e) => {
                if self.failures == 0 {
                    warn!("audio: mic-mute poll failed: {e} (suppressing further warnings until recovery)");
                }
                self.failures = self.failures.saturating_add(1);
                false
            }
        }
    }

    /// Force an immediate re-poll (used on resume, so a mic toggle during
    /// suspend is reflected without waiting for the cadence). Resets the timer
    /// only on a successful query, so a not-yet-ready PA gets retried on the
    /// next loop iteration instead of waiting a full INTERVAL.
    fn refresh_now(&mut self, audio: &audio::AudioController, obs: &mut ObsRuntime) {
        if obs.mic_indicator_enabled() && refresh_mic_muted(audio, obs) {
            self.last_poll = Some(Instant::now());
        }
    }
}

/// Re-establishes the PulseAudio connection after the server drops (PA/
/// PipeWire restart, suspend/resume). Unlike the HID reconnect — which blocks
/// the loop because there's nothing to do without a panel — this is
/// non-blocking and throttled: PA being down must not freeze HID input, OBS
/// LED updates, or flashes, so we attempt at most one reconnect per backoff
/// interval and let the loop keep running in between.
///
/// Detection is on-demand: `audio.is_connected()` reads the state cached from
/// the last mainloop iteration, so an outage with no audio activity at all
/// (mic indicator off, no slider movement) isn't noticed until the next op
/// fails — after which this reconnects. Worst case is one lost action, versus
/// today's "broken until the daemon is restarted."
struct AudioReconnector {
    /// Earliest instant of the next reconnect attempt. `None` means "attempt
    /// now if disconnected" (no attempt is pending). Set after a failed
    /// attempt to space out retries.
    next_attempt: Option<Instant>,
    backoff: Duration,
}

impl AudioReconnector {
    const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
    // Capped low (5s, vs the HID reconnect's 30s) so that when PA returns after
    // a long outage the panel notices and re-seeds within ~5s rather than up to
    // 30s. Each extra attempt against a still-dead server is a cheap fast `Err`,
    // so the tighter ceiling costs little.
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    fn new() -> Self {
        Self { next_attempt: None, backoff: Self::INITIAL_BACKOFF }
    }

    /// Called every loop iteration. Returns `true` only on the iteration where
    /// a reconnect just succeeded, so the caller can re-seed mic state and
    /// force a repaint. A no-op (returning `false`) while PA is healthy.
    fn tick(&mut self, audio: &mut audio::AudioController) -> bool {
        if audio.is_connected() {
            // Healthy — clear any armed backoff so the next outage retries
            // immediately rather than inheriting a stale interval.
            self.next_attempt = None;
            self.backoff = Self::INITIAL_BACKOFF;
            return false;
        }
        let now = Instant::now();
        if self.next_attempt.is_some_and(|t| now < t) {
            return false; // still within the backoff window
        }
        match audio.reconnect() {
            Ok(()) => {
                info!("audio: reconnected to PulseAudio");
                self.next_attempt = None;
                self.backoff = Self::INITIAL_BACKOFF;
                true
            }
            Err(e) => {
                warn!("audio: reconnect failed ({e}); retrying in {:?}", self.backoff);
                self.next_attempt = Some(now + self.backoff);
                self.backoff = (self.backoff * 2).min(Self::MAX_BACKOFF);
                false
            }
        }
    }
}


/// Re-open the PCPanel after a read error (USB unplug / hub power-cycle /
/// suspend), retrying forever with exponential backoff. Blocks the main loop
/// while it runs — there's no panel to service until it returns, so OBS/mic
/// updates simply pause (and resume on the next iteration once a panel is
/// back). Mirrors the OBS reconnect backoff in `obs.rs`.
///
/// The backoff sleep runs before every attempt, including the first, so even a
/// device that accepts open() but immediately fails the next read can't spin
/// the loop — the worst case is a 2s-paced retry, never a busy wait.
fn reconnect_panel() -> PcPanelPro {
    const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    let mut backoff = INITIAL_BACKOFF;
    loop {
        std::thread::sleep(backoff);
        match PcPanelPro::open() {
            Ok(panel) => {
                info!("PCPanel reconnected");
                return panel;
            }
            Err(e) => {
                debug!("PCPanel reconnect failed ({e}); retrying in {backoff:?}");
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
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

    // Poll mic-mute state periodically so external mute changes (system
    // hotkeys, KDE's tray, other tools) are reflected on the logo. Button
    // presses bypass the cadence by updating the cached state directly.
    let mut mic_poller = MicPoller::new();

    // Re-establish the PulseAudio connection if the server drops (PA/PipeWire
    // restart, suspend/resume) instead of failing every audio call until the
    // daemon is restarted.
    let mut audio_reconnect = AudioReconnector::new();

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
        //
        // Review-accepted: each forced repaint runs paint_leds, which rewrites
        // all four LED regions (knobs/sliders/labels/logo) even though only the
        // logo color changes during the blink — 4 USB writes ~10x/sec where 1
        // would do. Left as-is: it's bounded to the PA-outage duration (steady
        // 250ms polling keeps the indicator fresh otherwise), each write is
        // microseconds of HID traffic, and it mirrors the blinking-flash path's
        // accepted "repaint every iteration" trade (see expire_flash in
        // runtime.rs). A logo-only repaint path would add dirty-region state to
        // paint_leds for no user-visible gain.
        led_dirty |= obs.mic_indicator_needs_repaint()
            && paint::logo_indicator_visible(&obs, config.rgb);

        // Check for resume signal — re-poll mic so the post-resume paint
        // doesn't show stale state if the mic was toggled during suspend.
        if resume_rx.try_recv().is_ok() {
            info!("system resumed from sleep, re-applying LED state");
            mic_poller.refresh_now(&audio, &mut obs);
            led_dirty = true;
        }

        // Reconnect PulseAudio if it dropped. On a fresh reconnect, re-seed
        // the mic state and repaint so the logo doesn't linger on the stale
        // "unknown" blink it fell into during the outage.
        // Review-accepted: tick() can block this loop up to CONNECT_DEADLINE
        // (~3s) per attempt, delaying HID — but only while PA is down.
        if audio_reconnect.tick(&mut audio) {
            mic_poller.refresh_now(&audio, &mut obs);
            led_dirty = true;
        }

        led_dirty |= mic_poller.tick(&audio, &mut obs);

        // Flush any deferred stream-restore persistence writes that have
        // been idle long enough. Coalesces slider-scrub bursts into a
        // single DB write per app.
        audio.flush_persist_writes();

        // Read a panel event (may block ~100ms). Process before painting so
        // any button-triggered state changes (e.g., a local error flash from
        // a dispatch when OBS is disconnected) get painted this iteration.
        //
        // A read error means the HID device went away (USB unplug, hub
        // power-cycle, suspend transition). Rather than propagate it and let
        // systemd respawn us — losing OBS/PA state and any in-flight flash —
        // reconnect in place with backoff. PcPanelPro::open() re-runs init()
        // (absorbing the calibration burst), and forcing led_dirty makes the
        // repaint below restore the configured appearance from `obs` state,
        // exactly like the resume-from-sleep path above.
        match panel.read_event() {
            Ok(Some(event)) => {
                led_dirty |= handle_panel_event(event, &cli, &config, &mut audio, &mut obs);
            }
            Ok(None) => {}
            Err(e) => {
                warn!("panel read failed ({e}); attempting to reconnect");
                // While reconnect_panel sleeps, the main loop doesn't drain
                // OBS events — they queue in the bounded channel and the OBS
                // thread parks on a full send. apply_event is last-write-wins,
                // so drain_events collapses the backlog to the current state on
                // the next iteration; no events are lost meaningfully.
                panel = reconnect_panel();
                led_dirty = true;
            }
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

/// Apply a volume slider/knob change to a default-device target (`System`
/// or `Mic`). Returns `true` on success (these always drive a single
/// default device, so a success is a match). PA failures are logged and
/// treated as no-match.
///
/// `Named` targets are deliberately NOT handled here — they're batched into
/// one sink-input enumeration by `set_app_volumes` at the call site, so this
/// helper only ever sees the two default-device variants.
fn apply_volume_to(audio: &mut audio::AudioController, target: &AppTarget, value: u8) -> bool {
    let result = match target {
        AppTarget::System => audio.set_system_volume(value),
        AppTarget::Mic => audio.set_mic_volume(value),
        AppTarget::Named(_) => {
            unreachable!("Named volume targets are batched via set_app_volumes")
        }
    };
    result.map(|()| true).unwrap_or_else(|e| {
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
                let pct = audio::value_to_percent(value);

                // Batch all named targets into ONE sink-input enumeration
                // (one PA round-trip for the whole control rather than one per
                // app). System/Mic each drive a single default device, so they
                // stay individual.
                let named: Vec<&str> = action
                    .targets
                    .iter()
                    .filter_map(|t| match t {
                        AppTarget::Named(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect();
                let named_matched = if named.is_empty() {
                    Vec::new()
                } else {
                    audio.set_app_volumes(&named, value).unwrap_or_else(|e| {
                        warn!("audio: set volume for [{}] failed: {e}", named.join(", "));
                        vec![false; named.len()]
                    })
                };

                // Walk targets in config order so the verbose log and the OSD
                // label keep that order; named results come from the batch.
                let mut matched: Vec<&AppTarget> = Vec::new();
                let mut named_i = 0;
                for target in &action.targets {
                    let hit = match target {
                        AppTarget::System | AppTarget::Mic => apply_volume_to(audio, target, value),
                        AppTarget::Named(_) => {
                            let h = named_matched.get(named_i).copied().unwrap_or(false);
                            named_i += 1;
                            h
                        }
                    };
                    // Only trace targets that actually changed — a miss (app
                    // not running) or PA error is already logged at debug!/warn!
                    // in the audio layer, so printing here too would falsely
                    // claim a change happened.
                    if hit {
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
                                // of waiting up to MIC_POLL_INTERVAL.
                                led_dirty |= obs.update_mic_muted(muted);
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

