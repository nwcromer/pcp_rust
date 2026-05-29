# pcp_rust

A Linux controller for the [PCPanel Pro](https://www.getpcpanel.com/product-page/pcpanel-pro) USB audio mixer. Maps knobs, sliders, and buttons to system and application volume controls with KDE Plasma OSD integration.

## Features

- **Volume control** for system output and individual applications
- **Mute toggle** for system output, microphone, and individual applications
- **Multi-app mapping** - one control can target multiple apps
- **App matching** - case-insensitive substring matching against PulseAudio app names, binary names, and process names (including SDL apps via PipeWire client PID lookup)
- **RGB LED control** - solid, gradient, volume-gradient, wave, breath, and rainbow modes
- **OBS Studio integration** - buttons drive recording / replay buffer / pause / split-file via obs-websocket v5, and LEDs reflect OBS state (idle / recording / paused, with the logo as a replay-buffer indicator)
- **Logo indicator** - optionally turns the logo LED into a glanceable indicator for either microphone mute state or OBS replay-buffer state
- **KDE OSD** - native volume/mute popups with app icons
- **Sleep/resume** - automatically re-applies LED config after waking from sleep
- **Systemd service** - run as a background daemon with auto-start on login

## Requirements

- Linux with PipeWire (or PulseAudio)
- `libhidapi-dev` / `hidapi` (for USB HID access)
- `libpulse` (for audio control)
- KDE Plasma (optional, for OSD popups)
- OBS Studio 28+ (optional, for OBS integration — obs-websocket v5 is built in)

### Arch Linux

```
sudo pacman -S hidapi libpulse
```

## Building

```
cargo build --release
```

## Setup

### udev rule (required for non-root access)

```
sudo ./target/release/pcp_rust --create-udev-rules
sudo udevadm control --reload-rules
sudo udevadm trigger
```

### Find your audio apps

```
./target/release/pcp_rust --list-apps
```

Example output:

```
Audio applications currently running:
  Firefox                  (PID: 2185)
  Mumble                   (PID: 27328)
  SDL Application          (PID: 38481, binary: dota2)
```

### Configuration

Create `~/.config/pcpanel/config.toml`:

```toml
[slider1]
action = "volume"
app = "Mumble"

[slider2]
action = "volume"
app = "Firefox"

[slider3]
action = "volume"
app = ["Risk of Rain 2.exe", "dota2"]

[slider4]
action = "volume"
app = "system"

[knob5]
action = "volume"
app = "system"

[button3]
action = "toggle-mute"
app = "mic"

[button5]
action = "toggle-mute"
app = "system"

[rgb]
mode = "rainbow"
style = "horizontal"
```

#### Controls

- `knob1` - `knob5`
- `slider1` - `slider4`
- `button1` - `button5`

#### Actions

| Action | Controls | Description |
|---|---|---|
| `volume` | knobs, sliders | Set volume for one or more apps |
| `toggle-mute` | buttons | Toggle mute for one or more apps |

#### Special app values

| Value | Meaning |
|---|---|
| `system` | Default audio output (speakers/headphones) |
| `mic` | Default audio input (microphone) |

Any other value is matched as a substring against running audio applications.

The `app` field accepts a single string or an array of strings:

```toml
app = "Firefox"
app = ["dota2", "Risk of Rain 2.exe"]
```

#### RGB modes

The device speaks two color "languages" depending on the effect. Static effects (solid, gradient, volume-gradient) take full RGB hex colors. Animated effects (wave, breath) take a single `hue` byte (0–255) and animate the brightness/cycle internally.

**Solid color:**
```toml
[rgb]
mode = "solid"
color = "#E0FFFF"
```

**Rainbow:**
```toml
[rgb]
mode = "rainbow"
style = "horizontal"   # or "vertical" (may not work on all hardware revisions)
```

**Gradient** — two-color static gradient across knobs/sliders/labels:
```toml
[rgb]
mode = "gradient"
color1 = "#FF0000"
color2 = "#0000FF"
```

**Volume gradient** — sliders show their volume position via the gradient (LED color reflects current value); knobs and labels fall back to solid `color1`:
```toml
[rgb]
mode = "volume-gradient"
color1 = "#00FF00"
color2 = "#FF0000"
```

**Wave** — animated wave; `hue` selects a position on the color wheel (0=red, ~85=yellow/green, ~170=blue):
```toml
[rgb]
mode = "wave"
hue = 200              # required, 0-255
brightness = 200       # optional, default 200
speed = 64             # optional, default 64
reverse = false        # optional, default false
bounce = false         # optional, default false
```

**Breath** — breathing pulse:
```toml
[rgb]
mode = "breath"
hue = 200              # required, 0-255
brightness = 200       # optional, default 200
speed = 64             # optional, default 64
```

#### Logo indicator (optional)

The logo is a single LED, so it can show at most one thing at a time. Pick which state it should indicate via `indicator`:

```toml
[logo]
indicator = "mic"   # "none" (default), "mic", or "replay"
```

| `indicator` | What the logo shows |
|---|---|
| `"none"` | Matches the panel color (no separate indication). This is the default if `[logo]` is omitted. |
| `"mic"` | Default microphone mute state. Logo color depends on whether the mic is muted. Works regardless of OBS state. |
| `"replay"` | OBS replay-buffer state. Logo color depends on whether the replay buffer is running. Matches the panel color while OBS is disconnected (state is unknown). |

The colors for each state default to sensible values; override any of them in the same section:

```toml
[logo]
indicator = "mic"
mic_muted = "#FF0000"        # default: bright red
mic_unmuted = "#00FF00"      # default: bright green
mic_unknown = "#804000"      # default: burnt orange; shown blinking when PA
                             # can't confirm the mic state (see below)

# Used when indicator = "replay":
replay_active = "#00FFFF"    # default: cyan
replay_inactive = "#000000"  # default: off (logo dark; doesn't track the panel color)
```

**Trust contract for `indicator = "mic"`:** the logo tells you with certainty what state the microphone is in.

- **Green (or `mic_unmuted`)** — PulseAudio confirmed unmuted within the last second. Safe to speak.
- **Red (or `mic_muted`)** — PA confirmed muted within the last second.
- **Blinking burnt-orange (or `mic_unknown`)** — PA hasn't been able to confirm the mic state recently (daemon hiccup, source briefly unresolvable, button-toggle that PA didn't ack in time). The cached state may not match the device. **Treat the mic as possibly unmuted until the indicator returns to red/green.**

The blink is unmistakable so you don't miss it. The threshold for "stale" is 1 second (4 mic-mute poll intervals at 250 ms each) — a single transient PA failure won't trigger the warning, but a sustained outage will within ~1 second.

External mic mute changes (KDE volume key, OSD, other tools) are picked up by a 250 ms poll when `indicator = "mic"`, so there's a small delay; muting via a pcp_rust button updates instantly. Replay-buffer state is event-driven from OBS, no polling.

The indicator only applies in modes where the logo is independently writable — solid, gradient, volume-gradient, and all OBS-connected states except `paused_use_breath`. It does **not** apply during the global animations (rainbow, wave, breath) or the paused breath effect, since those drive every LED in lockstep.

#### Icons (optional)

Override the OSD icon for a control:

```toml
[slider2]
action = "volume"
app = "Firefox"
icon = "firefox"
```

If not specified, icons are resolved automatically from `.desktop` files or the app name.

### OBS Studio integration

pcp_rust can drive OBS recording / replay buffer / pause / split-file actions from buttons and reflect OBS state on the LEDs. The integration is event-driven: LEDs follow OBS's reported state, so if recording is started or stopped from the OBS GUI or any other client, the panel reflects it.

The OBS integration is opt-in. If you don't add an `[obs]` section to your config, none of this affects the rest of pcp_rust.

#### Prerequisites

1. **OBS 28 or newer** — obs-websocket v5 is built in.
2. In OBS: `Tools → WebSocket Server Settings`. Tick **Enable WebSocket server**. Note the port (default 4455) and the server password (or untick "Enable Authentication" if you'd rather not use one).
3. In OBS: enable the replay buffer if you want to use Save Replay (`Settings → Output → Replay Buffer → Enable Replay Buffer`), set a hotkey for "Save Replay" (`Settings → Hotkeys → Save Replay`), and start the replay buffer manually (`Controls → Start Replay Buffer`). **pcp_rust does not manage the replay buffer's start/stop state** — that's on you. If you press Save Replay while the replay buffer isn't running, the OBS call will fail and you'll see the error flash.

#### Connection config

```toml
[obs]
host = "localhost"            # optional, default "localhost"
port = 4455                   # optional, default 4455
password = "secret"           # optional; omit or leave empty if OBS auth is disabled
start_replay_buffer = false   # optional, default false; if true, pcp_rust starts
                              # OBS's replay buffer on every successful connection
                              # (including reconnects after OBS restarts or
                              # network blips). Does nothing if it's already
                              # running. Does not monitor or re-enable the buffer
                              # during a live session — if you stop it via OBS,
                              # it stays stopped until pcp_rust reconnects.
paused_use_breath = false     # optional, default false. If true, paused
                              # renders as a global breath animation (every
                              # LED including the logo, so the replay-buffer
                              # indicator is unavailable during paused). If
                              # false, paused is a solid color and the logo
                              # keeps its replay-buffer indicator.
```

pcp_rust connects on startup and reconnects automatically (with exponential backoff, max ~30s) when OBS isn't running, restarts, or crashes. While disconnected, OBS action buttons produce an error flash.

**Password handling.** The `password` field in `config.toml` is stored in plain text. obs-websocket is normally bound to localhost, so this is a personal-machine convenience rather than a transport concern, but two things to know:

- Set `$PCPANEL_OBS_PASSWORD` in the environment to override the config-file value. The env var wins if both are set. Useful so the password doesn't end up in dotfile backups or version control.
- If you do put the password in `config.toml`, tighten permissions: `chmod 600 ~/.config/pcpanel/config.toml`.

#### Action types

Four new action types, button-only:

| Action | What it does |
|---|---|
| `obs-save-replay` | Save the current replay buffer to a file |
| `obs-toggle-recording` | Start recording if stopped, stop if recording |
| `obs-pause-recording` | Pause if recording, resume if paused |
| `obs-split-recording` | Start a new recording file mid-session (OBS 30+) |

Example:
```toml
[button1]
action = "obs-save-replay"

[button2]
action = "obs-toggle-recording"

[button3]
action = "obs-pause-recording"

[button4]
action = "obs-split-recording"
```

`obs-toggle-recording` and `obs-pause-recording` change the LED state visibly (idle ↔ recording ↔ paused), so they don't add a green success flash — the state change is the feedback. `obs-save-replay` and `obs-split-recording` flash green on success since they don't otherwise change anything visible. All four flash magenta on failure.

#### LED behavior

When `[obs]` is configured, the LEDs follow OBS state:

| OBS state | Panel (knobs/sliders/labels) | Logo (with no `[logo]` indicators) |
|---|---|---|
| OBS disconnected | Your `[rgb]` mode (or off if omitted) | follows `[rgb]` |
| OBS connected, idle | Solid `idle_panel` color (configurable) | matches panel |
| Recording active | Solid red (configurable) | matches panel |
| Recording paused | Solid amber (configurable); breath if `paused_use_breath = true` | matches panel (or joins breath in `paused_use_breath` mode — hardware limit) |
| Any command succeeded | Brief green flash | follows flash |
| Any command failed | Brief magenta blink | follows flash |

The split between "disconnected → `[rgb]`" and "connected → status display" is deliberate: while OBS isn't running, pcp_rust behaves as if OBS doesn't exist; once OBS is up, the panel switches to a dashboard-style appearance.

By default the logo just mirrors the panel color. To turn it into a glanceable indicator — for mic-mute state or replay-buffer state — set `indicator` in `[logo]`. See [Logo indicator](#logo-indicator-optional).

`[obs.colors]` lets you override the panel colors and flash behavior:

```toml
[obs.colors]
recording = "#500000"           # solid color while recording
recording_paused = "#FFC000"    # paused color; used as full RGB for the solid panel, or as
                                # the hue source if paused_use_breath = true (the breath
                                # effect takes only a single hue byte)
success_flash = "#00FF00"       # flash on successful OBS commands
error_flash = "#FF00FF"         # blinking flash on failed OBS commands
flash_duration_ms = 500         # how long each flash stays before reverting

idle_panel = "#202020"          # panel color when OBS is connected and idle
```

Static effects (solid, gradient, volume-gradient) and the `recording` / flash colors take full hex; the paused color's hue is derived from the hex (saturation and brightness are managed by the breath effect itself).

For a replay-buffer logo indicator, see [Logo indicator](#logo-indicator-optional) — it's opt-in via the `[logo]` section.

## Running

### Foreground

```
./target/release/pcp_rust
```

With verbose output:

```
./target/release/pcp_rust --verbose
```

### Background (systemd)

Install and start as a user service:

```
./target/release/pcp_rust --install-service
```

Useful commands:

```
systemctl --user status pcpanel
journalctl --user -u pcpanel -f
systemctl --user restart pcpanel
```

Remove the service:

```
./target/release/pcp_rust --remove-service
```

## Dependencies note

`Cargo.toml` includes a `[patch.crates-io]` section pointing `libpulse-binding` and `libpulse-sys` at a personal fork ([nwcromer/pulse-binding-rust@fix-ext-stream-restore-write](https://github.com/nwcromer/pulse-binding-rust/tree/fix-ext-stream-restore-write)) that fixes a stream-restore write API issue. The upstream PR is still pending — once merged, this patch can go away. If you build from source you'll fetch that branch automatically.

## Protocol references

- [PCPanel (Java)](https://github.com/nvdweem/PCPanel)
- [PyPCPanelPro](https://github.com/Thebugger51/PyPCPanelPro)
- [PCPanel_Linux](https://github.com/taotien/PCPanel_Linux)
