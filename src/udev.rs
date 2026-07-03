use std::fs;
use std::path::Path;

use anyhow::{bail, Result};

use crate::prompt::confirm_overwrite;

// Numbered 70-* (below 71-seat.rules / 73-seat-late.rules) so its `uaccess`
// tag is set before the seat rules read it; a 99-* file would tag too late and
// the ACL would never apply. Full rationale in udev/70-pcpanel.rules's header.
const RULES_PATH: &str = "/etc/udev/rules.d/70-pcpanel.rules";

// Single source of truth: the file shipped at `udev/70-pcpanel.rules` is
// embedded at compile time so the installed rule and repo copy can't drift.
const RULES_CONTENT: &str = include_str!("../udev/70-pcpanel.rules");

pub fn create_udev_rules() -> Result<()> {
    if !running_as_root() {
        bail!(
            "Creating udev rules requires root privileges.\n\
             Run again with: sudo {} --create-udev-rules",
            std::env::args().next().unwrap_or_else(|| "pcp_rust".into())
        );
    }

    let path = Path::new(RULES_PATH);
    if path.exists() && !confirm_overwrite(RULES_PATH)? {
        return Ok(());
    }

    fs::write(path, RULES_CONTENT)?;
    println!("Created {RULES_PATH}");
    println!("Reload rules with:");
    println!("  sudo udevadm control --reload-rules");
    println!("  sudo udevadm trigger");

    Ok(())
}

fn running_as_root() -> bool {
    // SAFETY: geteuid() has no preconditions or side effects — it just reads
    // the effective UID; the binding is `unsafe` only because all FFI is.
    unsafe { libc::geteuid() == 0 }
}
