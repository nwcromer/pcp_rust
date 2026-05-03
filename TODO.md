# TODO

Things planned for future work on pcp_rust.

## Features

- **Mic volume via knob/slider** — let `app = "mic"` work with `action = "volume"`, not just `toggle-mute`. A previous attempt was rolled back; the feature is still wanted.
- **Show mic device** — when using `app = "mic"`, display which input device will be used.
- **Allow specifying other input devices** — if the default mic isn't the right one, let the user pick a different source.
- **Import Windows config** — ability to import a config from the Windows PCPanel software.
- **Deeper KDE integration** (details TBD).

## Cleanup

- **Clean up dead-code warnings** flagged by rustc. Only remove items that won't be needed for upcoming features — evaluate each before deleting. Currently flagged: `AppInfo.sink_input_index`, `Control::Button`, `Rgb` color constants, `LedMode::Gradient`/`VolumeGradient`, `LogoMode::Rainbow`/`Breath`, `osd::microphone_volume_changed`.
- **Review udev rule portability** — confirm it works across mainstream Linux distros.
- **Switch back to crates.io for libpulse-binding** — if [pulse-binding-rust PR #66](https://github.com/jnqnfe/pulse-binding-rust/pull/66) merges and a fixed version is released, remove the `[patch.crates-io]` block in `Cargo.toml` and use the published crate version instead.

## Invariants

- **Volume changes must never unmute any device.** If a target (system, mic, or app) is muted, a slider/knob movement may update the stored volume but must not flip the mute flag.
