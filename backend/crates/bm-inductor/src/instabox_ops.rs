//! `bm-inductor ibx …`: the InstaBox definition and its key, off the dashboard.
//!
//! The key is the one thing that has to be supplied; `login` proves it against
//! the API before leaving it on disk, so a typo is not a launch that fails later.

use crate::cli::IbxCmd;
use crate::cloud::instabox::InstaboxConfig;
use anyhow::{anyhow, Context, Result};
use std::path::Path;

/// The flags `ibx login` takes. The dashboard asks for the key alone.
#[derive(clap::Args, Debug, Default)]
pub(crate) struct LoginArgs {
    /// The workspace's API key. Leave it out and be prompted, so it is not
    /// left in a shell's history.
    #[arg(long)]
    pub(crate) api_key: Option<String>,
    /// Where the API answers, when it is not `https://api.instabox.sh`.
    #[arg(long)]
    pub(crate) api_url: Option<String>,
}

/// `ibx show`: the definition, which fields are missing, and whether the key
/// works. Read-only, so it is safe before every launch.
pub(crate) fn show(root: &Path) -> Vec<String> {
    let path = InstaboxConfig::path(root);
    let cfg = InstaboxConfig::load(root);
    let mut out = Vec::new();
    out.push(format!("definition: {}", path.display()));
    if !path.is_file() {
        out.push("  (none yet — `ibx init` writes it, `ibx login` stores the key)".into());
    }
    out.push(format!("  {}", cfg.summary()));
    out.push(format!(
        "  key: {}",
        if cfg.key().is_empty() {
            "none — `ibx login`, or INSTABOX_API_KEY".to_string()
        } else if cfg.api_key.trim().is_empty() {
            "from INSTABOX_API_KEY".to_string()
        } else {
            format!("{} (stored)", mask(&cfg.key()))
        }
    ));
    let missing = cfg.missing();
    if missing.is_empty() {
        out.push("nothing missing — `:up instabox` in the dashboard".into());
        return out;
    }
    out.push(format!("{} thing(s) still to set:", missing.len()));
    for m in missing {
        out.push(format!("  - {m}"));
    }
    out
}

/// `ibx init`: write the definition to fill in.
pub(crate) fn init(root: &Path, force: bool) -> Result<Vec<String>> {
    let path = InstaboxConfig::path(root);
    if path.is_file() && !force {
        return Ok(vec![format!(
            "{} already exists — `ibx init --force` overwrites it (the stored key included)",
            path.display()
        )]);
    }
    InstaboxConfig::default()
        .save(root)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(vec![
        format!("wrote {} (mode 600: it holds the key)", path.display()),
        "next: `ibx login` stores the key, `ibx show` says what is left".into(),
    ])
}

/// `ibx login`: store the key, prove it, and put the old one back if the API
/// refuses it — a bad key left on disk turns every launch into a confusing
/// failure instead of a refusal that names the key.
pub(crate) fn login(
    root: &Path,
    args: LoginArgs,
    typed_key: Option<String>,
) -> Result<Vec<String>> {
    let previous = InstaboxConfig::load(root).api_key;
    let key = match args.api_key.or(typed_key) {
        Some(k) => k.trim().to_string(),
        None => anyhow::bail!("no key given — pass --api-key or let `ibx login` prompt for one"),
    };
    if key.is_empty() {
        InstaboxConfig::store_key(root, "")?;
        return Ok(vec![
            "key cleared — `:up instabox` refuses until one is stored".to_string(),
        ]);
    }
    if key.split_whitespace().count() != 1 {
        anyhow::bail!("a key has no spaces — paste the whole key, one token");
    }

    let mut next = InstaboxConfig::load(root);
    next.api_key = key;
    if let Some(url) = args
        .api_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        next.api_url = url.to_string();
    }
    next.save(root)?;

    // The same call a launch makes first: the org, then the machine list.
    let provider = crate::cloud::instabox::Instabox::new(root, None);
    match bm_core::cloud::Cloud::smoke(&provider) {
        Ok(smoke) => {
            let mut out = vec![format!(
                "stored in {}",
                InstaboxConfig::path(root).display()
            )];
            out.extend(smoke.lines);
            Ok(out)
        }
        Err(s) => {
            let mut back = InstaboxConfig::load(root);
            back.api_key = previous;
            back.save(root)?;
            Err(anyhow!(
                "the key was not accepted — nothing stored: {}",
                s.line()
            ))
        }
    }
}

/// `abcd…wxyz`: enough to tell two keys apart, never enough to use one.
pub(crate) fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 12 {
        return "…".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// `bm-inductor ibx …`.
pub(crate) fn cmd(root: &Path, cmd: IbxCmd) -> Result<Vec<String>> {
    match cmd {
        IbxCmd::Show => Ok(show(root)),
        IbxCmd::Init { force } => init(root, force),
        IbxCmd::Login { args } => {
            let typed = if args.api_key.is_some() {
                None
            } else {
                Some(ask_key()?)
            };
            login(root, args, typed)
        }
    }
}

/// Prompt for the key without echoing it: it must not land in a scrollback.
fn ask_key() -> Result<String> {
    use std::io::Write as _;
    eprint!("InstaBox API key (not echoed): ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| anyhow!("reading the key: {e}"))?;
    eprintln!();
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests;
