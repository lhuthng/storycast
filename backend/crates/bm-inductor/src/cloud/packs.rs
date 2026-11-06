//! The `packs` step: is everything a launched box fetches already published, or
//! would provisioning quietly push it up this machine's uplink instead?

use bm_core::artifact::{is_preset_url, ModelsRelease, PackRelease};
use bm_core::config::Settings;
use bm_core::Layout;

/// One thing that is not on the release plane, and the command that puts it
/// there. Both halves are content, printed as they read.
pub(crate) struct Gap {
    pub what: String,
    pub fix: String,
}

impl Gap {
    pub(crate) fn line(&self) -> String {
        format!("{} — fix: {}", self.what, self.fix)
    }
}

/// Every reason a box launched now would take its files from here.
///
/// The checks are the two planes provisioning resolves before it pushes (the
/// model bake and the profile pack), plus the voice pins a box re-fetches on
/// its own. Empty means nothing about this launch depends on the uplink.
pub(crate) fn gaps(layout: &Layout, settings: &Settings) -> Vec<Gap> {
    let mut out = Vec::new();

    // The weights. A foreign bake is worse than none: the box checks the
    // bundle against this host's own checksums and fails after landing it.
    let repo = settings.models_release.trim();
    if repo.is_empty() {
        out.push(Gap {
            what: "no models release: every box would pull the weights from this machine".into(),
            fix: ":release <owner>/<name> names the repo the bake is published under".into(),
        });
    } else if is_preset_url(repo) {
        // The bucket, resolved exactly as provisioning resolves it: the
        // engine's latest.json, then **this** host's bake. A bucket that has
        // moved on without the host is a fetch that lands and then fails.
        match ModelsRelease::for_r2(repo, &layout.engine) {
            Ok(r) if r.is_bake(&layout.models_dir()) => {}
            Ok(r) => out.push(Gap {
                what: format!(
                    "the bucket publishes {} ({}), not the models under {} — a box would fetch \
                     it and fail its own checksum",
                    r.tag,
                    bm_core::artifact::tag_for(&r.hash),
                    layout.models_dir().display()
                ),
                fix: "re-publish this bake (`tools/preset-publish.sh models <engine> auto \
                     engines/<engine>/models/models.tar.zst`), then launch again"
                    .into(),
            }),
            Err(e) => out.push(Gap {
                what: format!("the preset bucket could not be read ({e:#}), so the weights would travel over the push"),
                fix: "check `models_release` and the network, or pass --from-machine to push deliberately".into(),
            }),
        }
    } else if ModelsRelease::resolve(&layout.models_dir(), repo).is_none() {
        out.push(Gap {
            what: format!(
                "the models under {} are not the bake `{repo}` publishes — a box would fetch \
                 someone else's and fail its own checksum",
                layout.models_dir().display()
            ),
            fix: "`:release` publishes this bake under that repo, then launch again".into(),
        });
    }

    // The profile pack: clip registries, the scene map, prompts, voices.json.
    // A workspace composing its own assets/ ships them in the sources bundle on
    // purpose, so it is not a gap.
    if !layout.owns_assets() {
        let prepo = settings.packs_release.trim();
        if prepo.is_empty() {
            out.push(Gap {
                what: "no packs release: assets/ would be pushed from here on every launch".into(),
                fix: ":packrelease <owner>/<name> (or a preset bucket URL)".into(),
            });
        } else if is_preset_url(prepo) {
            let name = bm_core::profile::in_force(layout)
                .ok()
                .map(|b| b.pack.name)
                .unwrap_or_default();
            if name.is_empty() {
                out.push(Gap {
                    what: "no profile is loaded, so there is no pack for the bucket to serve"
                        .into(),
                    fix: ":profile in the dashboard, or tools/profile.sh fetch <name>".into(),
                });
            } else {
                match PackRelease::in_bucket(prepo, &name) {
                    Ok(Some(_)) => {}
                    Ok(None) => out.push(Gap {
                        what: format!("profile `{name}` is not published at {prepo}"),
                        fix: format!(
                            "publish it: tools/profile.sh pack {name}, then tools/preset-publish.sh \
                             sounds {name} auto profiles/{name}.tar.zst"
                        ),
                    }),
                    Err(e) => out.push(Gap {
                        what: format!("the preset bucket could not be read ({e:#}), so assets/ would travel over the push"),
                        fix: "check `packs_release` and the network, or pass --from-machine to push deliberately".into(),
                    }),
                }
            }
        } else if PackRelease::resolve(&layout.root, prepo).is_none() {
            out.push(Gap {
                what: "`packs_release` is set but the loaded profile names no versioned pack"
                    .into(),
                fix: "tools/profile.sh pack <name> --version <v>, then publish that tag".into(),
            });
        }
    }

    // Voice packs a box re-fetches itself: a checkout-only tree is a local path
    // that exists on no other machine.
    let local: Vec<String> = bm_core::voices::packs::Packs::load(layout)
        .packs
        .into_iter()
        .filter(|p| p.source != bm_core::voices::packs::PUBLISHED || p.sha256.is_empty())
        .map(|p| p.name)
        .collect();
    if !local.is_empty() {
        out.push(Gap {
            what: format!(
                "voice pack(s) only exist here: {}",
                local.join(", ")
            ),
            fix: "publish each (tools/preset-publish.sh voices <name>) or install it from the release plane".into(),
        });
    }

    out
}

#[cfg(test)]
mod tests;
