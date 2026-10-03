use super::*;
use anyhow::Result;

impl Ssh {
    /// Get the profile pack onto the box: from its release, or over the push.
    pub fn install_pack(
        &self,
        layout: &crate::Layout,
        release: &crate::artifact::PackRelease,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        if let Some(line) = self.install_pack_delta(layout, release, live)? {
            return Ok(line);
        }
        match self.fetch_pack(release, live) {
            Ok(line) => {
                // The box records its own receipt on landing, but only on a
                let _ = self.write_pack_receipt(layout, release);
                Ok(line)
            }
            Err(FetchOutcome::Corrupt(e)) => {
                if let Some(l) = live {
                    let _ = l.send(format!(
                        "[{}] release {} disagrees with this checkout ({}), pushing assets/ instead — re-publish it (`tools/profile.sh pack {} --version {}` then `gh release upload {} {}.tar.zst --clobber`) to stop paying the uplink",
                        self.target, release.tag, e, release.name, release.version, release.tag, release.name,
                    ));
                }
                self.push_pack(layout)?;
                self.write_pack_receipt(layout, release)?;
                Ok(format!(
                    "pack {} v{} over the push (release disagreed: {})",
                    release.name,
                    release.version,
                    crate::util::head_chars(&e, 80)
                ))
            }
            Err(FetchOutcome::Unreachable(e)) => {
                if let Some(l) = live {
                    let _ = l.send(format!(
                        "[{}] release {} unreachable ({}), pushing assets/ instead",
                        self.target, release.tag, e
                    ));
                }
                self.push_pack(layout)?;
                self.write_pack_receipt(layout, release)?;
                Ok(format!(
                    "pack {} v{} over the push ({})",
                    release.name,
                    release.version,
                    crate::util::head_chars(&e, 80)
                ))
            }
        }
    }
    /// Sync the pack by receipt: only what moved travels.
    fn install_pack_delta(
        &self,
        layout: &crate::Layout,
        release: &crate::artifact::PackRelease,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<Option<String>> {
        let say = |m: String| {
            if let Some(l) = live {
                let _ = l.send(format!("[{}] {m}", self.target));
            }
        };
        let receipt_path = format!(
            "$HOME/{}/{}",
            super::super::REMOTE_DIR,
            crate::artifact::PACK_RECEIPT
        );
        let (code, stdout, _) = self.run(&format!("cat {receipt_path}"), 10)?;
        if code != 0 {
            return Ok(None);
        }
        let receipt: crate::profile::Manifest = match serde_json::from_str(&stdout) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };
        if receipt.version != release.version {
            return Ok(None);
        }
        let live_manifest = crate::profile::compute_manifest(
            layout,
            crate::profile::Piece::Pack,
            &release.name,
            &release.version,
        )?;
        let delta = crate::artifact::diff_manifests(&receipt.files, &live_manifest.files);
        if delta.changed.is_empty() && delta.removed.is_empty() {
            return Ok(Some(format!(
                "pack {} v{} already exact (receipt match, {} files)",
                release.name,
                release.version,
                live_manifest.files.len()
            )));
        }
        if delta.changed.len() + delta.removed.len() > live_manifest.files.len() / 2 {
            say(format!(
                "pack {} delta is most of the tree ({}/{} paths) — taking the whole tree instead",
                release.name,
                delta.changed.len() + delta.removed.len(),
                live_manifest.files.len()
            ));
            return Ok(None);
        }
        // The pack's `assets/…` paths are relative to the tree **in force's**
        let base = pack_base(layout);
        let bytes: u64 = delta
            .changed
            .iter()
            .map(|p| {
                std::fs::metadata(base.join(p))
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum();
        if let Err(e) = self.apply_pack_delta(layout, &delta) {
            say(format!(
                "pack {} delta failed ({e:#}) — taking the whole tree instead",
                release.name
            ));
            return Ok(None);
        }
        let text = crate::artifact::receipt_text(&live_manifest)?;
        if let Err(e) = self.write_remote_file(crate::artifact::PACK_RECEIPT, &text) {
            say(format!("pack {} delta landed but the receipt was not rewritten ({e:#}) — taking the whole tree instead", release.name));
            return Ok(None);
        }
        // The receipt is a claim: re-read it and check the hash, or the next
        let (code, stdout, _) = self.run(&format!("cat {receipt_path}"), 10)?;
        let verified = code == 0
            && serde_json::from_str::<crate::profile::Manifest>(&stdout)
                .map(|m| crate::profile::manifest_hash(&m.files))
                .unwrap_or_default()
                == crate::profile::manifest_hash(&live_manifest.files);
        if !verified {
            say(format!(
                "pack {} delta landed but did not verify — taking the whole tree instead",
                release.name
            ));
            return Ok(None);
        }
        Ok(Some(format!(
            "pack {} v{} delta: {} file(s) in, {} out ({:.1} MB over the uplink)",
            release.name,
            release.version,
            delta.changed.len(),
            delta.removed.len(),
            bytes as f64 / 1e6
        )))
    }
    /// Push the changed paths and delete the removed ones, worker-relative.
    fn apply_pack_delta(
        &self,
        layout: &crate::Layout,
        delta: &crate::artifact::PackDelta,
    ) -> Result<()> {
        if !delta.changed.is_empty() {
            self.rsync_push_files(&pack_base(layout), &delta.changed)?;
        }
        if !delta.removed.is_empty() {
            let mut script = String::from("set -e\n");
            for p in &delta.removed {
                script.push_str(&format!(
                    "rm -f \"$HOME/{}/{}\"\n",
                    super::super::REMOTE_DIR,
                    p.replace('\'', "'\\''")
                ));
            }
            let (code, _, stderr) = self.run(&script, 60)?;
            if code != 0 {
                anyhow::bail!(
                    "removing {} stale path(s): {}",
                    delta.removed.len(),
                    stderr.trim()
                );
            }
        }
        Ok(())
    }
    /// Record what a pushed pack holds: the live manifest as the box receipt,
    fn write_pack_receipt(
        &self,
        layout: &crate::Layout,
        release: &crate::artifact::PackRelease,
    ) -> Result<()> {
        let manifest = crate::profile::compute_manifest(
            layout,
            crate::profile::Piece::Pack,
            &release.name,
            &release.version,
        )?;
        let text = crate::artifact::receipt_text(&manifest)?;
        self.write_remote_file(crate::artifact::PACK_RECEIPT, &text)
    }
    /// Ask the box to fetch the profile pack, and read the answer the same way
    fn fetch_pack(
        &self,
        release: &crate::artifact::PackRelease,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> std::result::Result<String, FetchOutcome> {
        if let Some(l) = live {
            let _ = l.send(format!(
                "[{}] pack: fetching {} v{} from the release (a CDN, not this machine's uplink)",
                self.target, release.name, release.version
            ));
        }
        let (code, stdout, stderr) = self
            .run(&pack_fetch_script(release), 1800)
            .map_err(|e| FetchOutcome::Unreachable(e.to_string()))?;
        classify_fetch(code, &stdout, &stderr, &release.tag, "pack")
    }
    /// Put the live `assets/` on the box, for a box that could not fetch the
    pub fn push_pack(&self, layout: &crate::Layout) -> Result<()> {
        self.rsync_push_excluding(
            &layout.assets(),
            crate::artifact::PACK_DIR,
            true,
            None,
            &["/assets/_extends"],
        )
    }
}
