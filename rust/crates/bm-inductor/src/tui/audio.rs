//! Playing a rendered wav on the operator's machine.
//!
//! The inductor renders, the TUI plays. That split is deliberate: the inductor
//! is a server that may be on another box, and "where the sound comes out" is a
//! property of the desk the TUI is sitting on. So the inductor ships the *bytes*
//! (`OpResult::audio_b64`) rather than a path, and this module puts them where
//! the speaker is.
//!
//! **One file, reused.** Every audition overwrites the same scratch path, and
//! `Drop` removes it. An A/B is one sound at a time by construction — the
//! in-flight marker in `input/audition.rs` enforces it — so a file per voice
//! would buy nothing and leave a directory of clips nobody asked for. It lives
//! in the OS temp dir, not the repo, because an audition is not a pipeline
//! artifact: it is a sound that has already been made by the time you can name
//! it.
//!
//! Playback is deliberately **not** a `Job`. The job worker is a single
//! sequential task that provision flows already queue behind, and parking a
//! five-second sample in it would stall every ssh flow behind a sound. The
//! player therefore spawns the process and returns immediately, and starting a
//! sample kills the previous one — one voice at a time is the entire point of an
//! A/B audition.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The only external player this repo assumes. macOS ships it, and this pipeline
/// is macOS-hosted. There is no cross-platform story here; inventing one that
/// silently does nothing would be worse than saying so, so a missing binary is
/// reported rather than swallowed.
const PLAYER: &str = "afplay";

pub(crate) struct Player {
    child: Option<Child>,
    /// Where the current sample is written. One path for the whole session.
    ///
    /// Carries the pid so two TUIs on one machine cannot overwrite each other's
    /// audio mid-playback, which would surface as a sample cutting out.
    scratch: PathBuf,
    program: String,
}

impl Player {
    pub(crate) fn new() -> Self {
        Self::with_scratch(
            std::env::temp_dir().join(format!("bm-audition-{}.wav", std::process::id())),
        )
    }

    fn with_scratch(scratch: PathBuf) -> Self {
        Player {
            child: None,
            scratch,
            program: PLAYER.to_string(),
        }
    }

    /// Write `wav` to the scratch file and play it, stopping whatever was
    /// already playing.
    ///
    /// Returns once the player has been spawned — the sample plays on in the
    /// background while the dashboard keeps redrawing.
    pub(crate) fn play_bytes(&mut self, wav: &[u8]) -> Result<(), String> {
        // Refuse before touching the scratch file: an empty render would
        // otherwise truncate the previous sample and then fail to play, which
        // looks exactly like a broken speaker.
        if wav.is_empty() {
            return Err("the inductor sent no audio — nothing to play".into());
        }
        self.stop();
        std::fs::write(&self.scratch, wav)
            .map_err(|e| format!("could not write {}: {e}", self.scratch.display()))?;
        let child = spawn(&self.program, &self.scratch)?;
        self.child = Some(child);
        Ok(())
    }

    /// Stop the current sample, if one is still running.
    ///
    /// Called before every new sample and on drop, so a finished sample never
    /// leaves a zombie and a long one never talks over the next.
    pub(crate) fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // A sample that already finished is the common case, not a failure:
            // `kill` on an exited child is an error, so ask first.
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
            }
            let _ = c.wait();
        }
    }

    /// A `Player` that runs `true` instead of afplay: it spawns for real and
    /// writes the sample to `scratch` where a test can read it back, but makes
    /// no sound. Silence matters — a test that actually plays audio through the
    /// developer's speakers is a test nobody can run.
    #[cfg(test)]
    pub(crate) fn silent_for_test(scratch: PathBuf) -> Self {
        Player {
            child: None,
            scratch,
            program: "true".into(),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
        // The sample is not a deliverable. Leaving it behind would be exactly
        // the accumulation the single scratch path exists to prevent.
        let _ = std::fs::remove_file(&self.scratch);
    }
}

/// Spawn the player on `path`.
///
/// The existence check is not ceremony. `afplay` exits non-zero on a file it
/// cannot open, but we deliberately never wait on it, so that exit status is
/// thrown away — meaning a write that produced nothing would look like a sample
/// that simply did not play. Checking here turns the two into different
/// sentences.
fn spawn(program: &str, path: &Path) -> Result<Child, String> {
    if !path.is_file() {
        return Err(format!(
            "nothing to play at {} — the render did not produce a file",
            path.display()
        ));
    }
    Command::new(program)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        // Name the mechanism: "no player installed" and "no audio rendered" are
        // different problems with different next moves, and from the status line
        // they would otherwise read the same.
        .map_err(|e| format!("{program} could not play {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch path unique to this test. The pid alone is not enough: every
    /// test in the binary shares one process, so two of them would collide on a
    /// file one of them is mid-way through deleting.
    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bmaud-{}-{tag}.wav", std::process::id()))
    }

    fn silent(tag: &str) -> Player {
        let p = Player::silent_for_test(scratch(tag));
        let _ = std::fs::remove_file(&p.scratch);
        p
    }

    #[test]
    fn stopping_an_idle_player_is_a_no_op() {
        // `stop` runs on every play and on drop, so "nothing playing" must not
        // panic or error.
        let mut p = silent("idle");
        p.stop();
        p.stop();
        assert!(p.child.is_none());
    }

    #[test]
    fn every_audition_overwrites_one_file_rather_than_adding_another() {
        // The reason this module exists: auditioning twenty voices must leave
        // one clip behind, not twenty. Asserted on the *directory*, because
        // "the file has the right bytes" is also true of a fresh file per play.
        let mut p = silent("reuse");
        let dir = p.scratch.parent().unwrap().to_path_buf();
        let prefix = format!("bmaud-{}-", std::process::id());

        p.play_bytes(b"first sample").expect("spawned");
        p.play_bytes(b"second sample").expect("spawned");

        assert_eq!(std::fs::read(&p.scratch).unwrap(), b"second sample");
        let ours: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(&prefix) && n.contains("reuse"))
            .collect();
        assert_eq!(ours.len(), 1, "one scratch file per player, got {ours:?}");

        let _ = std::fs::remove_file(&p.scratch);
    }

    #[test]
    fn an_empty_render_is_refused_before_it_can_truncate_the_last_sample() {
        let mut p = silent("empty");
        std::fs::write(&p.scratch, b"the previous sample").unwrap();

        let err = p.play_bytes(b"").unwrap_err();
        assert!(err.contains("no audio"), "{err}");
        assert!(
            p.child.is_none(),
            "a refused play must not claim the child slot"
        );
        assert_eq!(
            std::fs::read(&p.scratch).unwrap(),
            b"the previous sample",
            "refusing must not clobber the sample that is still on screen"
        );

        let _ = std::fs::remove_file(&p.scratch);
    }

    #[test]
    fn a_missing_player_binary_names_itself_and_the_file() {
        // The other half: the audio is there, the speaker is not. Exercised with
        // a bogus program name rather than by uninstalling afplay.
        let mut p = silent("noplayer");
        p.program = "definitely-not-a-real-player-xyz".into();

        let err = p.play_bytes(b"RIFF").unwrap_err();
        assert!(err.contains("definitely-not-a-real-player-xyz"), "{err}");
        assert!(err.contains("noplayer"), "name the file too: {err}");

        let _ = std::fs::remove_file(&p.scratch);
    }

    #[test]
    fn dropping_the_player_takes_the_sample_with_it() {
        let path = scratch("drop");
        let _ = std::fs::remove_file(&path);
        {
            let mut p = Player::silent_for_test(path.clone());
            p.play_bytes(b"RIFF").expect("spawned");
            assert!(path.is_file(), "the sample was written");
        }
        assert!(
            !path.exists(),
            "and removed on the way out: {}",
            path.display()
        );
    }
}
