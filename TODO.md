# TODO

Things planned for future work on pcp_rust.

## Features

- **Show mic device** — when using `app = "mic"`, display which input device will be used.
- **Allow specifying other input devices** — if the default mic isn't the right one, let the user pick a different source.
- **Import Windows config** — ability to import a config from the Windows PCPanel software.
- **Deeper KDE integration** (details TBD).

## Cleanup

- **Review udev rule portability** — confirm it works across mainstream Linux distros.
- **Switch back to crates.io for libpulse-binding** — if [pulse-binding-rust PR #66](https://github.com/jnqnfe/pulse-binding-rust/pull/66) merges and a fixed version is released, remove the `[patch.crates-io]` block in `Cargo.toml` and use the published crate version instead.
- **Investigate vertical rainbow** — the protocol bytes we send for `style = "vertical"` match the nvdweem/PCPanel Java reference exactly, but the device shows a static cyan color instead of cycling. Possible firmware quirk on this hardware revision, or a parameter we're missing. Worth USB-capturing the Windows app's vertical-rainbow command to compare.
