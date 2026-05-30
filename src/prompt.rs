use std::io::{self, Write};

use anyhow::Result;

/// Prompt on stderr asking whether to overwrite an existing `label` (a path
/// or filename). Reads a line from stdin and returns `Ok(true)` if the user
/// answered "y"/"Y". For any other answer it prints "Aborted." and returns
/// `Ok(false)`, so callers can early-return without re-printing.
pub fn confirm_overwrite(label: &str) -> Result<bool> {
    eprint!("{label} already exists. Overwrite? [y/N] ");
    io::stderr().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if input.trim().eq_ignore_ascii_case("y") {
        Ok(true)
    } else {
        println!("Aborted.");
        Ok(false)
    }
}
