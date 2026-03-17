use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{bail, Result};

const RULES_PATH: &str = "/etc/udev/rules.d/99-pcpanel.rules";

const RULES_CONTENT: &str = "\
# PCPanel Pro
SUBSYSTEM==\"usb\", ATTR{idVendor}==\"0483\", ATTR{idProduct}==\"a3c5\", MODE=\"0666\"
SUBSYSTEM==\"hidraw\", ATTRS{idVendor}==\"0483\", ATTRS{idProduct}==\"a3c5\", MODE=\"0666\"
";

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
    unsafe { libc::geteuid() == 0 }
}
