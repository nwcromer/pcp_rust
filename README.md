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
style = "horizontal"
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

- [PCPanel reverse engineering](https://github.com/arnarg/wiki/blob/master/pcpanel-reverse-engineering.md)
- [PCPanel (Java)](https://github.com/nvdweem/PCPanel)
- [PyPCPanelPro](https://github.com/Thebugger51/PyPCPanelPro)
- [PCPanel_Linux](https://github.com/taotien/PCPanel_Linux)
