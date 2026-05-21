# pcp_rust

A Linux controller for the [PCPanel Pro](https://www.getpcpanel.com/product-page/pcpanel-pro) USB audio mixer. Maps knobs, sliders, and buttons to system and application volume controls with KDE Plasma OSD integration.

## Features

- **Volume control** for system output and individual applications
- **Mute toggle** for system output, microphone, and individual applications
- **Multi-app mapping** - one control can target multiple apps
- **App matching** - case-insensitive substring matching against PulseAudio app names, binary names, and process names (including SDL apps via PipeWire client PID lookup)
- **RGB LED control** - solid colors or rainbow animation
- **KDE OSD** - native volume/mute popups with app icons
- **Sleep/resume** - automatically re-applies LED config after waking from sleep
- **Systemd service** - run as a background daemon with auto-start on login

## Requirements

- Linux with PipeWire (or PulseAudio)
- `libhidapi-dev` / `hidapi` (for USB HID access)
- `libpulse` (for audio control)
- `gdbus` (for KDE OSD integration, included with glib2)
- KDE Plasma (optional, for OSD popups)

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

The `password` is stored in plain text in `config.toml`. The file lives under your config directory (`~/.config/pcpanel/`) with default user-only permissions, and obs-websocket is normally bound to localhost, so this is a personal-machine convenience rather than a transport concern — but worth knowing.

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

| OBS state | Panel (knobs/sliders/labels) | Logo |
|---|---|---|
| OBS disconnected | Your `[rgb]` mode (or off if omitted) | follows `[rgb]` |
| OBS connected, idle | Solid `idle_panel` color (configurable) | green if replay buffer running, off if stopped |
| Recording active | Solid red (configurable) | red |
| Recording paused | Solid amber (configurable); breath if `paused_use_breath = true` | green if replay buffer running (or joins breath in `paused_use_breath` mode — hardware limit) |
| Any command succeeded | Brief green flash | green |
| Any command failed | Brief magenta blink | magenta blink |

The split between "disconnected → `[rgb]`" and "connected → status display" is deliberate: while OBS isn't running, pcp_rust behaves as if OBS doesn't exist; once OBS is up, the panel switches to a dashboard-style appearance with the logo as a glanceable replay-buffer indicator.

`[obs.colors]` lets you override these colors:

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
replay_active = "#00FF00"       # logo color when replay buffer is running
replay_inactive = "#000000"     # logo color when replay buffer is stopped (off = invisible)
```

Static effects (solid, gradient, volume-gradient) and the `recording` / flash colors take full hex; the paused color's hue is derived from the hex (saturation and brightness are managed by the breath effect itself).

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

## Protocol references

- [PCPanel (Java)](https://github.com/nvdweem/PCPanel)
- [PyPCPanelPro](https://github.com/Thebugger51/PyPCPanelPro)
- [PCPanel_Linux](https://github.com/taotien/PCPanel_Linux)
