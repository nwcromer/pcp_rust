# TODO

Things planned for future work on pcp_rust.

## Features

- **Show mic device** — when using `app = "mic"`, display which input device will be used.
- **Allow specifying other input devices** — if the default mic isn't the right one, let the user pick a different source.
- **Import Windows config** — ability to import a config from the Windows PCPanel software.
- **Deeper KDE integration** (details TBD).

## Cleanup

- **Reduce comment verbosity** — revisit the heavy comment-to-code ratio and tighten over-long comments. First pass should cover **`Review-accepted` comments only**, leaving the rest for a later pass. (A prior whole-codebase trim was reverted; start narrower.)
- **Review udev rule portability** — confirm it works across mainstream Linux distros.
- **Switch back to crates.io for libpulse-binding** — if [pulse-binding-rust PR #66](https://github.com/jnqnfe/pulse-binding-rust/pull/66) merges and a fixed version is released, remove the `[patch.crates-io]` block in `Cargo.toml` and use the published crate version instead.
- **Investigate vertical rainbow** — the protocol bytes we send for `style = "vertical"` match the nvdweem/PCPanel Java reference exactly, but the device shows a static cyan color instead of cycling. Possible firmware quirk on this hardware revision, or a parameter we're missing. Worth USB-capturing the Windows app's vertical-rainbow command to compare.

## Known limitations

- **Icon-resolution cache is process-lifetime** — `freedesktop_icon_resolves` caches lookup results in a static `HashMap` for the life of the process. If the user installs a new icon theme or icon mid-session, pcp_rust won't pick it up (apps that previously had no icon stay no-icon) until restart. Acceptable trade-off because the alternative is doing the full XDG icon spec walk on every slider tick.
