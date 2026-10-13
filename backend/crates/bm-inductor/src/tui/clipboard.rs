//! The operator's clipboard — the manual digest's only input and output.

use std::io::Write;
use std::process::{Command, Stdio};

/// Put `text` on the clipboard, or say what went wrong.
pub(crate) fn copy(text: &str) -> Result<(), String> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                "no `pbcopy` on this machine — copy the prompt from the log instead".to_string()
            }
            _ => format!("could not run pbcopy: {e}"),
        })?;
    let mut stdin = child.stdin.take().ok_or("pbcopy gave us no stdin")?;
    stdin
        .write_all(text.as_bytes())
        .map_err(|e| format!("writing to pbcopy: {e}"))?;
    // Closing the pipe is what tells `pbcopy` the paste is complete: it reads to
    drop(stdin);
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting for pbcopy: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pbcopy exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Read the clipboard.
pub(crate) fn paste() -> Result<String, String> {
    let out = Command::new("pbpaste")
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "no `pbpaste` on this machine".to_string(),
            _ => format!("could not run pbpaste: {e}"),
        })?;
    if !out.status.success() {
        return Err(format!(
            "pbpaste exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether this machine has the pair at all. A box without them is not a
    fn available() -> bool {
        Command::new("pbpaste").arg("--help").output().is_ok()
    }

    #[test]
    fn a_round_trip_through_the_real_clipboard_puts_it_back() {
        if !available() {
            println!("SKIP: no pbcopy/pbpaste — the clipboard was NOT checked here");
            return;
        }
        // **Save, write, read, restore — in that order, and the restore happens
        let saved = paste().expect("reading the clipboard to save it");
        let probe = "bm-digest-manager clipboard round trip";
        copy(probe).expect("writing the clipboard");
        let back = paste().expect("reading the clipboard back");
        copy(&saved).expect("restoring the clipboard");
        assert_eq!(back, probe, "what went on came back");
    }
}
