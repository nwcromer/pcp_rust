use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{bail, Result};

const RULES_PATH: &str = "/etc/udev/rules.d/99-pcpanel.rules";

// Single source of truth: the same file shipped at `udev/99-pcpanel.rules`
// is embedded at compile time so the installed rule and the repo copy can't
// drift apart.
const RULES_CONTENT: &str = include_str!("../udev/99-pcpanel.rules");

pub fn create_udev_rules() -> Result<()> {
    if !running_as_root() {
        bail!(
            "Creating udev rules requires root privileges.\n\
             Run again with: sudo {} --create-udev-rules",
            std::env::args().next().unwrap_or_else(|| "pcp_rust".into())
        );
    }

    let path = Path::new(RULES_PATH);
    if path.exists() {
        eprint!("{RULES_PATH} already exists. Overwrite? [y/N] ");
        io::stderr().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    fs::write(path, RULES_CONTENT)?;
    println!("Created {RULES_PATH}");
    println!("Reload rules with:");
    println!("  sudo udevadm control --reload-rules");
    println!("  sudo udevadm trigger");

    Ok(())
}

fn running_as_root() -> bool {
    // SAFETY: geteuid() has no preconditions and no side effects — it just
    // reads the effective UID. The libc binding is marked unsafe purely
    // because all FFI calls are; this one is always safe to invoke.
    unsafe { libc::geteuid() == 0 }
}
