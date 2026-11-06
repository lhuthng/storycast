//! Launch providers: one `Cloud` impl per cloud, sharing bm-core's vocabulary.
pub(crate) mod aws;
pub(crate) mod instabox;
mod packs;
mod reap;

pub(crate) use packs::gaps as pack_gaps;
pub(crate) use reap::sweep;

use bm_core::cloud::{Cloud, Provider};
use std::path::Path;

/// The provider a bare `:up` targets.
pub(crate) fn default_provider() -> Provider {
    Provider::Aws
}

/// The provider a command targets, or why this build cannot launch on it.
pub(crate) fn resolve(provider: Option<Provider>) -> anyhow::Result<Provider> {
    Ok(provider.unwrap_or_else(default_provider))
}

/// The one `Cloud` a command targets.
pub(crate) fn for_provider(
    provider: Option<Provider>,
    root: &Path,
    ttl_minutes: Option<u32>,
) -> anyhow::Result<Box<dyn Cloud>> {
    match resolve(provider)? {
        Provider::Aws => Ok(Box::new(aws::Aws::new(root, ttl_minutes))),
        Provider::InstaBox => Ok(Box::new(instabox::Instabox::new(root, ttl_minutes))),
    }
}
