use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

const SERVICE_NAME: &str = "pcpanel";

fn service_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|d| d.join("systemd").join("user"))
        .context("could not determine systemd user config directory")
}

fn service_path() -> Result<PathBuf> {
    Ok(service_dir()?.join(format!("{SERVICE_NAME}.service")))
}

fn binary_path() -> Result<String> {
    std::env::current_exe()
        .context("could not determine binary path")?
        .to_str()
        .map(String::from)
        .context("binary path is not valid UTF-8")
}

fn generate_service_file(bin_path: &str) -> String {
    format!(
        "\
[Unit]
Description=PCPanel Pro Controller
After=graphical-session.target
Wants=graphical-session.target

[Service]
ExecStart=\"{bin_path}\"
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"
    )
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("failed to run systemctl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("systemctl --user {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

pub fn install() -> Result<()> {
    let path = service_path()?;
    let bin = binary_path()?;

    if path.exists() {
        eprint!("{} already exists. Overwrite? [y/N] ", path.display());
        io::stderr().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }

        // Stop existing service before overwriting
        let _ = systemctl(&["stop", SERVICE_NAME]);
        let _ = systemctl(&["disable", SERVICE_NAME]);
    }

    // Create directory if needed
    let dir = service_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;

    // Write service file
    let content = generate_service_file(&bin);
    fs::write(&path, &content)
        .with_context(|| format!("failed to write {}", path.display()))?;

    println!("Created {}", path.display());
    println!("Binary: {bin}");

    // Reload, enable, and start
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", SERVICE_NAME])?;
    systemctl(&["start", SERVICE_NAME])?;

    println!("Service enabled and started.");
    println!();
    println!("Useful commands:");
    println!("  systemctl --user status {SERVICE_NAME}    # check status");
    println!("  journalctl --user -u {SERVICE_NAME} -f    # follow logs");
    println!("  systemctl --user restart {SERVICE_NAME}   # restart");
    println!("  systemctl --user stop {SERVICE_NAME}      # stop");

    Ok(())
}

pub fn remove() -> Result<()> {
    let path = service_path()?;

    if !path.exists() {
        println!("Service is not installed.");
        return Ok(());
    }

    // Stop and disable
    let _ = systemctl(&["stop", SERVICE_NAME]);
    let _ = systemctl(&["disable", SERVICE_NAME]);

    // Remove service file
    fs::remove_file(&path)
        .with_context(|| format!("failed to remove {}", path.display()))?;

    systemctl(&["daemon-reload"])?;

    println!("Service stopped, disabled, and removed.");

    Ok(())
}
