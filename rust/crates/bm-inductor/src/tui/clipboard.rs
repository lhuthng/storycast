//! The operator's clipboard — the manual digest's only input and output.
//!
//! The manual route exists because a model the operator already has open in a
//! browser is better than a fallback that is rate limited. That means the prompt
//! has to leave the dashboard and the answer has to come back, and the clipboard
//! is the only channel that needs no cooperation from the model's UI.
//!
//! **Shelling out to `pbcopy`/`pbpaste`, not a terminal escape.** The TUI has the
//! terminal in raw mode for the whole session, so it cannot read a paste from
//! stdin; OSC 52 would work but has to be enabled by the emulator and *silently
//! does nothing* where it is not — which is the one failure mode this must not
//! have. A command either works or says why not.
//!
//! **Every failure is a message, never a silent no-op.** A `c` that copies
//! nothing and says nothing leaves the operator pasting a stale clipboard into
//! their model and then reading a validator complaint about text they never saw.
//! That is a bug report about the wrong component.

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
    // EOF, so dropping this handle *is* the end of the copy, not a nicety.
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
    /// failure of this code, so the tests below report a skip rather than
    /// failing for the environment's sake — the same rule the `opencode` deadline
    /// test follows.
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
        // before any assertion can fail.** This touches the operator's real
        // clipboard, so a test that left its own value behind on a panic would be
        // taking something that is not the suite's to take. Restoring first means
        // there is no path out of this function that does not put it back.
        let saved = paste().expect("reading the clipboard to save it");
        let probe = "bm-digest-manager clipboard round trip";
        copy(probe).expect("writing the clipboard");
        let back = paste().expect("reading the clipboard back");
        copy(&saved).expect("restoring the clipboard");
        assert_eq!(back, probe, "what went on came back");
    }
}
