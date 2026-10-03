//! Playing a rendered wav on the operator's machine.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The only external player this repo assumes. macOS ships it, and this pipeline
const PLAYER: &str = "afplay";

pub(crate) struct Player {
    child: Option<Child>,
    /// Where the current sample is written. One path for the whole session.
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
    pub(crate) fn play_bytes(&mut self, wav: &[u8]) -> Result<(), String> {
        // Refuse before touching the scratch file: an empty render would
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
    pub(crate) fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // A sample that already finished is the common case, not a failure:
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
            }
            let _ = c.wait();
        }
    }

    /// A `Player` that runs `true` instead of afplay: it spawns for real and
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
        let _ = std::fs::remove_file(&self.scratch);
    }
}

/// Spawn the player on `path`.
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
        .map_err(|e| format!("{program} could not play {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch path unique to this test. The pid alone is not enough: every
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
        let mut p = silent("idle");
        p.stop();
        p.stop();
        assert!(p.child.is_none());
    }

    #[test]
    fn every_audition_overwrites_one_file_rather_than_adding_another() {
        // The reason this module exists: auditioning twenty voices must leave
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
