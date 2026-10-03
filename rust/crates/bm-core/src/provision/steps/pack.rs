use super::*;
use anyhow::Result;

impl Ssh {
    /// Get the profile pack onto the box: from its release, or over the push.
    ///
    /// **Why a pack is worth publishing at all.** It is 60-odd MB of mp3 and
    /// JSON that is byte-identical on every machine and changes only when the
    /// operator publishes a new profile — the same argument that took the
    /// weights off the uplink. Before this, a fresh box paid for the whole
    /// profile over the operator's connection, once per box, for content that
    /// was already on a CDN at a hash it could check.
    ///
    /// **The split of failures is the models one, and for the same reason.**
    /// *Unreachable* — no such release, no route, a GitHub incident, an agent
    /// too old to have the verb — falls back to the push and says so in the
    /// log. *Corrupt* also falls back to the push, unlike the weights: the
    /// live tree *is* the expectation the fetch checked against, so pushing it
    /// lands exactly the bytes the box was asked for. Stopping instead used to
    /// leave the box with no `assets/` at all — the bundle extract prunes that
    /// tree before this runs — which is worse than any disagreement the check
    /// exists to surface. Either way the log names the stale release and how
    /// to re-publish it, so falling back does not hide it.
    ///
    /// What the box ends up with is the same either way. The release is
    /// verified against the *live* tree's hash, so a pushed `assets/` and a
    /// fetched one are the same bytes — which is why the stamp records the
    /// release hash on a box that took the push, and why the next provision
    /// does not try to fetch what is already there.
    ///
    /// Before any of that, the delta: a box whose receipt names this same
    /// release version gets only what moved — changed files over rsync,
    /// removed paths deleted, receipt rewritten — and the preset it holds is
    /// never pruned, re-pushed or re-fetched. See [`Self::install_pack_delta`].
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
                // new agent — so the inductor records it too, best-effort. A
                // box that cannot record is diffed as unknown next time, never
                // refused now.
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
    ///
    /// Returns `Ok(None)` when there is no receipt to diff against — missing,
    /// unparseable, or naming another version — and the caller takes the whole
    /// tree exactly as before. A receipt is a claim about bytes, so the land
    /// is verified the same way a fetch is: the receipt is re-read afterwards
    /// and its hash compared, and anything but a match falls through to the
    /// whole tree rather than recording a lie.
    ///
    /// The delta is capped: more than half the manifest moved means the tree
    /// was re-cut rather than edited, and a whole fetch is fewer round trips
    /// than a file list longer than the tree. Same `Ok(None)` fall-through.
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
        // parent — the workspace's when it composes its own, the checkout's
        // otherwise — the same base `push_pack` rsyncs from.
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
        // provision diffs garbage against a good tree.
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
    /// so the next provision diffs instead of refetching. The fetch path is
    /// recorded by the box itself on landing; a box that cannot record is
    /// diffed as unknown next time, never refused now.
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
    /// as the weights: `0` landed, `21` fall back to the push, `20` stop.
    ///
    /// The timeout is the models one for the same reason, and a pack is the
    /// *smaller* of the two artifacts, so it is not the transfer that is at
    /// risk here — it is an agent predating `fetch-artifact`, which answers
    /// with a usage error and has to read as absence.
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
    /// release.
    ///
    /// The push half of the same decision, and deliberately a **directory**
    /// push rather than more bundle members: a box whose fetch failed has to
    /// end up with the profile either way, and re-adding those files to
    /// `sources.tar.zst` would mean the operator's uplink pays for them on
    /// every box even when the release works for all the others.
    ///
    /// Excludes `assets/_extends/` for the same reason the release does: those
    /// are composition *inputs*, and a worker resolves a composed pack from the
    /// flattened tree — shipping them would re-fold into a bundle that is
    /// supposed to be content.
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
