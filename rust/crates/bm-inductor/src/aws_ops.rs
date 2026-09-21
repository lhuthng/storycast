//! The AWS account verbs, callable from two front ends.
//!
//! `main.rs::aws_cmd` builds the lines the operator reads for the setup
//! commands. The verbs a *second* front end needs live here instead, so the
//! dashboard does not become a second implementation of any of them: list the
//! pool, launch into it, terminate from it, store the IAM user's key, and read
//! the account into the pool. The low-level `aws` CLI plumbing stays at the
//! crate root and is shared through `crate::aws_cli_*`: one place runs the
//! subprocess and explains a missing CLI, a rejected key, or a policy that
//! forbids the call.
//!
//! Everything here returns lines or structured instances, and prints nothing —
//! the CLI prints what it gets back, the dashboard logs it and, for a launch,
//! links what came back into the registry.
//!
//! The flags for `login` and `discover` are defined **once**, as clap `Args`
//! structs, and used by both front ends: the CLI derives its subcommand from
//! them and the TUI parses the same tokens through the same definition, so a
//! flag cannot mean two things depending on where it was typed.

use anyhow::Result;
use bm_core::provision::{
    admits_port, default_security_group_args, default_subnet_args, describe_image_args,
    describe_security_group_args, instance_line, instance_profile_names_args, keypair_names_args,
    parse_instances, parse_name_list, run_instances_args, sole_name, terminate_args,
    ubuntu_ami_args, AwsConfig, AwsInstance, REQUIRED_INGRESS, UBUNTU_LTS,
};
use std::path::{Path, PathBuf};

/// Flags for `aws login`, one definition shared by both front ends.
///
/// The CSV is the natural handoff and the only route a TUI can offer: the
/// secret is read from stdin by the CLI, and a screen must never echo it. A
/// front end with no terminal passes `csv`; `access_key_id` alone is for a CLI
/// that can still prompt.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct LoginArgs {
    /// The key id. Omit to be prompted for it.
    #[arg(long)]
    pub(crate) access_key_id: Option<String>,
    /// The CSV the console's **Download .csv file** button gives you.
    ///
    /// It already holds both halves, so this replaces the two prompts. It
    /// is the one artifact the console produces that a terminal would
    /// otherwise make you retype by hand.
    #[arg(long)]
    pub(crate) csv: Option<PathBuf>,
}

impl LoginArgs {
    /// Parse the flags a dashboard prompt supplied.
    ///
    /// The same clap definition the CLI derives from, so `--csv` means the
    /// same thing in both places; only the token source differs.
    pub(crate) fn parse_tokens(tokens: &[String]) -> Result<Self> {
        use clap::FromArgMatches as _;
        // `no_binary_name`: the tokens come from a dashboard prompt and carry
        // no `argv[0]`, so the first flag would otherwise be eaten as one.
        let cmd = <LoginArgs as clap::Args>::augment_args(
            clap::Command::new("login").no_binary_name(true),
        );
        let matches = cmd
            .try_get_matches_from(tokens)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        LoginArgs::from_arg_matches(&matches).map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// Flags for `aws discover`, one definition shared by both front ends.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct DiscoverArgs {
    /// Region to discover for. Written to the pool.
    #[arg(long)]
    pub(crate) region: Option<String>,
    /// Pin this AMI instead of resolving the current Ubuntu LTS.
    #[arg(long)]
    pub(crate) ami: Option<String>,
    /// Import the `.pem` you downloaded from the console, as
    /// `.bm/aws/<region>.pem` (0600).
    #[arg(long)]
    pub(crate) pem: Option<PathBuf>,
    /// Replace a `.bm/aws/<region>.pem` that already exists with the one you
    /// pass.
    ///
    /// Only needed when the two are *different* keys: an identical file is
    /// left alone, and a different one is refused without this, because the
    /// old private half cannot be recovered from AWS.
    #[arg(long)]
    pub(crate) force: bool,
    /// Name the instance profile to use.
    ///
    /// Needed when the account holds more than one — which is the normal
    /// case for an account that also runs something else — because `discover`
    /// will not guess between them. Checked against the account, so a typo is
    /// caught here rather than at `RunInstances`.
    #[arg(long)]
    pub(crate) instance_profile: Option<String>,
    /// Name the subnet to use, instead of the default VPC's.
    #[arg(long)]
    pub(crate) subnet: Option<String>,
    /// Name the security group to use, instead of the account's default.
    ///
    /// The one to reach for on an account that already runs something else:
    /// the default group is shared, so opening port 22 on it opens it for
    /// every instance using that group, including ones this tool has never
    /// heard of. A group of your own costs nothing and changes nothing else.
    #[arg(long)]
    pub(crate) security_group: Option<String>,
}

impl DiscoverArgs {
    /// Parse the flags a dashboard prompt supplied. Empty means "re-run with
    /// what the pool already holds", which is a legitimate refresh.
    pub(crate) fn parse_tokens(tokens: &[String]) -> Result<Self> {
        use clap::FromArgMatches as _;
        // `no_binary_name`, for the same reason `login` sets it: a prompt
        // supplies flags, not argv.
        let cmd = <DiscoverArgs as clap::Args>::augment_args(
            clap::Command::new("discover").no_binary_name(true),
        );
        let matches = cmd
            .try_get_matches_from(tokens)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        DiscoverArgs::from_arg_matches(&matches).map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// What the account holds, filtered to our marker tag.
///
/// The credential and region check, and the first thing a fresh dashboard
/// wants: if this answers, `up` can too. A payload we do not understand is a
/// refusal, never an empty list — "nothing running" is the one wrong answer that
/// costs money.
pub(crate) fn pool(root: &Path) -> Result<(AwsConfig, Vec<AwsInstance>)> {
    let cfg = AwsConfig::load_layered(root);
    if cfg.region.trim().is_empty() {
        anyhow::bail!("no region set — `aws init`, then fill it in");
    }
    let json = crate::aws_cli_instances(root, &cfg.region, &cfg.tag_key)?;
    let instances = parse_instances(&json, &cfg.tag_key).ok_or_else(|| {
        anyhow::anyhow!(
            "the aws CLI answered something this does not understand — reporting an empty account here would be the one wrong answer that costs money"
        )
    })?;
    Ok((cfg, instances))
}

/// The lines `aws ls` prints: the pool header followed by one line per box.
pub(crate) fn pool_lines(root: &Path) -> Result<Vec<String>> {
    let (cfg, instances) = pool(root)?;
    let mut out = vec![format!(
        "{} · tag {} · spot={}",
        cfg.region, cfg.tag_key, cfg.spot
    )];
    if instances.is_empty() {
        out.push("no boxes running (nothing carries this tag)".into());
    }
    for i in &instances {
        out.push(instance_line(i));
    }
    Ok(out)
}

/// Store the app's IAM user, after proving the key is one.
///
/// The two halves are handed in rather than prompted for: the CLI reads them
/// off a terminal, the dashboard passes the console's CSV — and neither front
/// end gets its own copy of the verify-then-write order.
///
/// `csv` carries both halves and wins; `key_id` + `secret` are the typed
/// route, which only a front end with a hidden stdin can offer.
pub(crate) fn login(
    root: &Path,
    csv: Option<PathBuf>,
    key_id: Option<String>,
    secret: Option<String>,
) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let (key_id, secret) = match csv {
        Some(p) => {
            if key_id.is_some() || secret.is_some() {
                anyhow::bail!(
                    "--csv already carries both halves — drop the other value, or drop --csv"
                );
            }
            let text = std::fs::read_to_string(&p)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
            let (id, secret) = bm_core::provision::aws_credentials::parse_access_key_csv(&text)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{} does not hold an access key pair — this is not the file the \
                         console's \"Download .csv file\" button gives you",
                        p.display()
                    )
                })?;
            (id, secret)
        }
        None => {
            let key_id = key_id.ok_or_else(|| {
                anyhow::anyhow!(
                    "no access key id — pass the console's CSV (`--csv <path>`), which carries \
                     both halves and needs no secret typed"
                )
            })?;
            let secret = secret.ok_or_else(|| {
                anyhow::anyhow!(
                    "no secret given — the secret is never typed on a screen, so pass the \
                     console's CSV instead (`--csv <path>`)"
                )
            })?;
            if key_id.trim().is_empty() {
                anyhow::bail!("no access key id given");
            }
            if secret.trim().is_empty() {
                anyhow::bail!("no secret given");
            }
            (key_id, secret)
        }
    };
    // Verify **before** storing, with the key supplied to this one call.
    // The order matters now that the stored identity has to be an IAM user:
    // writing first would leave a file we refuse to use, and the next `aws ls`
    // would quietly run as whatever it holds.
    let region = AwsConfig::load_layered(root).region;
    let mut args: Vec<String> = vec![
        "sts".into(),
        "get-caller-identity".into(),
        "--output".into(),
        "json".into(),
    ];
    if !region.trim().is_empty() {
        args.push("--region".into());
        args.push(region);
    }
    let supplied = [
        ("AWS_ACCESS_KEY_ID".to_string(), key_id.trim().to_string()),
        (
            "AWS_SECRET_ACCESS_KEY".to_string(),
            secret.trim().to_string(),
        ),
    ];
    match crate::aws_cli_with(root, &args, &supplied) {
        Ok(json) => match bm_core::provision::aws_credentials::parse_identity_json(&json) {
            Some(who) if !who.is_user() => anyhow::bail!(
                "those credentials belong to {}, which is not an IAM user.\n  \
                 This app runs as an IAM user created for it and nothing else — a root \
                 key or an assumed role is exactly the identity this exists to replace.\n  \
                 Create one: `bm-inductor aws policy` prints the policy and the \
                 commands; docs/AWS-IAM-USER.md walks through it.",
                who.arn
            ),
            Some(who) => {
                let file =
                    bm_core::provision::aws_credentials::write(root, &key_id, &secret, Some(&who))?;
                out.push(format!("wrote {} (0600)", file.display()));
                out.push(format!(
                    "verified: IAM user {} · account {}",
                    who.user_name().unwrap_or(&who.arn),
                    who.account
                ));
            }
            // A verified call we cannot read is not an identity we can
            // claim to have stored — say so rather than record a guess.
            None => out.push(format!(
                "stored nothing: `sts get-caller-identity` answered something this does not \
                 understand, so the identity could not be confirmed. Raw answer: {}",
                bm_core::util::head_chars(json.trim(), 200)
            )),
        },
        // No account to check against (offline, no CLI, a policy that
        // forbids nothing because it is not attached yet). Store it
        // unverified — the key is what the operator just supplied, and
        // `aws show` will say it is unverified rather than pretend.
        Err(e) => {
            let file = bm_core::provision::aws_credentials::write(root, &key_id, &secret, None)?;
            out.push(format!("wrote {} (0600)", file.display()));
            out.push(format!("stored, but not verified: {e:#}"));
        }
    }
    // The key id is an identifier, like a username; the secret is not
    // printed, only a hash of it, so an operator can tell *which*
    // secret is loaded without it reaching a terminal or a log.
    out.push(format!(
        "key {} · secret {}",
        key_id.trim(),
        bm_core::provision::aws_credentials::fingerprint(&secret)
    ));
    Ok(out)
}

/// Read the account and write what it answers into the pool definition.
///
/// Read-modify-writes the local document rather than `cfg.save()`: the layered
/// config is template + local, so re-serialising it would flatten the tracked
/// template into the operator's file and drop the `_note`s they edit against.
pub(crate) fn discover(root: &Path, args: DiscoverArgs) -> Result<Vec<String>> {
    let DiscoverArgs {
        region,
        ami,
        pem,
        force,
        instance_profile,
        subnet,
        security_group,
    } = args;
    let path = root.join(".bm").join("aws.json");
    let mut out: Vec<String> = Vec::new();
    // Seed the pool if it has never been written, so this works as the
    // first command anyone runs and still leaves the `_note`s that
    // explain every field.
    if !path.exists() {
        out.push(crate::seed_pool(root, &path, false)?);
    }
    let cfg = AwsConfig::load_layered(root);
    let region = region
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| cfg.region.clone());
    if region.is_empty() {
        anyhow::bail!(
            "no region — `bm-inductor aws discover --region eu-central-1` (it is written to {})",
            path.display()
        );
    }
    let mut doc = crate::read_pool_doc(&path)?;
    crate::set_json(&mut doc, &["region"], serde_json::json!(region));
    out.push(format!("region {region}"));

    // The AMI: the one field that cannot be read off a console page
    // without hunting through the AMI catalogue. Resolved only when
    // there is nothing there — a pinned `images.<region>` is a decision
    // and this must not quietly undo it on the next run.
    if let Some(a) = ami.map(|a| a.trim().to_string()).filter(|a| !a.is_empty()) {
        crate::set_json(&mut doc, &["images", region.as_str()], serde_json::json!(a));
        out.push(format!("  AMI {a}   (pinned by --ami)"));
    } else if let Some(existing) = cfg.image() {
        out.push(format!(
            "  AMI {existing}   (kept — pass --ami to change it)"
        ));
    } else {
        match crate::aws_cli_opt(root, &ubuntu_ami_args(&region)) {
            Ok(Some(a)) => {
                crate::set_json(&mut doc, &["images", region.as_str()], serde_json::json!(a));
                out.push(format!("  AMI {a}   (Ubuntu {UBUNTU_LTS})"));
            }
            Ok(None) => out.push(
                "  AMI: unresolved — pass --ami ami-… , or set `images.<region>` by hand".into(),
            ),
            Err(e) => out.push(format!("  AMI: lookup failed — {e:#}")),
        }
    }

    if let Some(s) = subnet
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        crate::set_json(&mut doc, &["subnet_id"], serde_json::json!(s));
        out.push(format!("  subnet {s}   (named by --subnet)"));
    } else if !cfg.subnet_id.trim().is_empty() {
        out.push(format!("  subnet {}   (kept)", cfg.subnet_id));
    } else {
        match crate::aws_cli_opt(root, &default_subnet_args(&region)) {
            Ok(Some(s)) => {
                crate::set_json(&mut doc, &["subnet_id"], serde_json::json!(s));
                out.push(format!("  subnet {s}   (the default VPC's)"));
            }
            Ok(None) => out.push(
                "  subnet: none found — this account has no default VPC subnet; pass --subnet <subnet-…>"
                    .into(),
            ),
            Err(e) => out.push(format!("  subnet: lookup failed — {e:#}")),
        }
    }

    // The security group. `--security-group` settles it, which is the
    // answer on an account that already runs something else: the default
    // group is shared, so an inbound rule on it is an inbound rule on
    // every instance that uses it.
    let chosen_sg = if let Some(g) = security_group
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
    {
        // Checked before it is written, like `--instance-profile` and
        // `--pem`. A group that is not in this region is the same trap
        // as a keypair that is not: an id read off a console page in
        // another region looks perfectly valid. Writing it anyway would
        // also skip the ingress check below on the strength of an
        // unverified id, which is how a pool ends up launching into a
        // group that does not exist.
        match crate::aws_cli_opt(root, &describe_security_group_args(&region, &g)) {
            Ok(Some(_)) => {
                crate::set_json(&mut doc, &["security_group_id"], serde_json::json!(g));
                out.push(format!(
                    "  security group {g}   (named by --security-group)"
                ));
                Some(g)
            }
            Ok(None) => {
                out.push(format!(
                    "  security group {g}   — WARNING: {region} has no such group"
                ));
                out.push(
                    "    a group belongs to one region and one VPC — check the console's region selector, or pass --region to match"
                        .into(),
                );
                None
            }
            Err(e) => {
                out.push(format!(
                    "  security group {g}   — WARNING: not readable in {region}"
                ));
                out.push(format!("    {e:#}"));
                out.push(
                    "    a group belongs to one region and one VPC — check the console's region selector, or pass --region to match"
                        .into(),
                );
                None
            }
        }
    } else if !cfg.security_group_id.trim().is_empty() {
        out.push(format!(
            "  security group {}   (kept)",
            cfg.security_group_id
        ));
        Some(cfg.security_group_id.clone())
    } else {
        match crate::aws_cli_opt(root, &default_security_group_args(&region)) {
            Ok(Some(s)) => {
                crate::set_json(&mut doc, &["security_group_id"], serde_json::json!(s));
                out.push(format!("  security group {s}   (the account's default)"));
                Some(s)
            }
            Ok(None) => {
                out.push("  security group: none found — pass --security-group <sg-…>".into());
                None
            }
            Err(e) => {
                out.push(format!("  security group: lookup failed — {e:#}"));
                None
            }
        }
    };
    // Whether that group actually admits you, on **every** port the
    // cluster needs. Said here, while the group is being chosen, because
    // both failures are invisible later: a closed port does not refuse a
    // connection, it swallows it. Port 22 hangs and you notice; the task
    // port gives you a box that launches, looks healthy and is never
    // driven.
    if let Some(g) = &chosen_sg {
        let task_port = bm_proto::DEFAULT_TASK_PORT;
        if let Ok(Some(json)) = crate::aws_cli_opt(root, &describe_security_group_args(&region, g))
        {
            let missing: Vec<String> = REQUIRED_INGRESS
                .iter()
                .filter(|p| admits_port(&json, **p) == Some(false))
                .map(u16::to_string)
                .collect();
            if missing.is_empty() {
                out.push(format!(
                    "    ingress: {} admitted",
                    REQUIRED_INGRESS
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(" + ")
                ));
            } else {
                out.push(format!(
                    "    NOTE: no inbound rule admits {} from outside this group:",
                    missing.join(" or ")
                ));
                out.push("    a closed port is swallowed, not refused — 22 hangs, and an".into());
                out.push("    unadmitted task port gives you a box that launches, looks".into());
                out.push("    healthy and is never driven.".into());
                out.push(format!(
                    "    EC2 → Security Groups → {g} → Edit inbound rules → Add rule:"
                ));
                out.push(format!(
                    "      SSH (22) and Custom TCP ({task_port}), source = wherever you run the inductor"
                ));
                out.push(
                    "    (a rule naming only this group does not count — that is group-to-group traffic)"
                        .into(),
                );
            }
        }
    }

    // The keypair first from the `.pem` if one was handed over, because
    // the console names the download after the key pair — so the file
    // name *is* the name — but only *in the region it was created in*.
    // EC2 keypairs are region-scoped, so an inferred name is checked
    // against the region rather than trusted: a name that is not there
    // fails at `RunInstances`, and `aws show` saying "ready" while the
    // launch would be refused is the one answer that costs a debugging
    // round. (This was found the hard way: a keypair created in the
    // console's default region while the pool points elsewhere.)
    let mut have_keypair = cfg.keypair().is_some();
    let region_keypairs: Option<Vec<String>> = if have_keypair {
        Some(Vec::new())
    } else {
        match crate::aws_cli_opt(root, &keypair_names_args(&region)) {
            Ok(v) => Some(v.map(|t| parse_name_list(&t)).unwrap_or_default()),
            Err(e) => {
                out.push(format!("  keypair: lookup failed — {e:#}"));
                None
            }
        }
    };
    let had_pem = pem.is_some();
    let mut key_refused = false;
    if let Some(src) = pem {
        // The path follows the *resolved* region, not `cfg.key_file()`:
        // the region may have come from `--region` on this very
        // command, in which case the config still has none.
        let dest = root.join(".bm").join("aws").join(format!("{region}.pem"));
        // Identical bytes are the idempotent re-run. *Different* bytes
        // mean a key is being replaced, and the old private half cannot
        // be recovered from AWS — so that takes `--force`, the same
        // idiom `init` uses for a file that already exists. Keeping the
        // stale file and reporting the keypair as verified would be the
        // worst answer: "ready" with a key that cannot open the box.
        let current = std::fs::read(&dest).ok();
        let same = current.is_some() && current == std::fs::read(&src).ok();
        let replacing = current.is_some() && !same;
        if same {
            out.push(format!("  key {}   (already this key)", dest.display()));
        } else if replacing && !force {
            out.push(format!(
                "  key {} already exists and is a DIFFERENT key — pass --force to replace it",
                dest.display()
            ));
            out.push(
                "    the old private half cannot be recovered from AWS, which is why this is not automatic"
                    .into(),
            );
            key_refused = true;
        } else {
            std::fs::copy(&src, &dest).map_err(|e| {
                anyhow::anyhow!("copying {} to {}: {e}", src.display(), dest.display())
            })?;
            bm_core::util::restrict(&dest)?;
            out.push(format!(
                "  key {} → {} (0600{})",
                src.display(),
                dest.display(),
                if replacing { ", replaced" } else { "" }
            ));
        }
        // The name is checked whether or not the copy happened: a `.pem`
        // already in place from an earlier run is exactly when the
        // mismatch is easiest to miss, and that is how this was found.
        if !have_keypair {
            if let Some(name) = src.file_stem().and_then(|s| s.to_str()) {
                match &region_keypairs {
                    // Verified against the region: the file name is the
                    // keypair name the console gave it.
                    Some(names) if names.iter().any(|n| n == name) => {
                        crate::set_json(
                            &mut doc,
                            &["keypairs", region.as_str()],
                            serde_json::json!(name),
                        );
                        out.push(format!("  keypair {name}   (from the file name)"));
                        have_keypair = true;
                    }
                    // Not written on purpose. The name came from a file,
                    // not from this region, and writing it would make
                    // `aws show` claim a readiness the launch does not
                    // have.
                    Some(names) => {
                        out.push(format!(
                            "  keypair {name}   — WARNING: {region} has no such keypair ({})",
                            if names.is_empty() {
                                "it has none".to_string()
                            } else {
                                names.join(", ")
                            }
                        ));
                        out.push(format!(
                            "    EC2 keypairs are region-scoped. Create one in {region} (the console's region selector must match), or point --region at the one you have."
                        ));
                    }
                    // The lookup failed, so nothing could be checked:
                    // use it, and say it is unverified.
                    None => {
                        crate::set_json(
                            &mut doc,
                            &["keypairs", region.as_str()],
                            serde_json::json!(name),
                        );
                        out.push(format!(
                            "  keypair {name}   (from the file name — NOT verified against {region})"
                        ));
                        have_keypair = true;
                    }
                }
            }
        }
    }
    // Only when no `--pem` was given: the block above already printed the
    // diagnosis — verified, warned, or unverified — and repeating the
    // generic version under it just adds a line to read past.
    if !have_keypair && !had_pem {
        if let Some(names) = &region_keypairs {
            match sole_name(names) {
                Some(n) => {
                    crate::set_json(
                        &mut doc,
                        &["keypairs", region.as_str()],
                        serde_json::json!(n),
                    );
                    out.push(format!("  keypair {n}   (the only one in {region})"));
                }
                None if names.is_empty() => out.push(format!(
                    "  keypair: none in {region} — create one in the console (EC2 → Key pairs → Create key pair) with the region selector set to {region}, then pass its .pem"
                )),
                None => out.push(format!(
                    "  keypair: {} in {region} ({}) — pass --pem, or set `keypairs.{region}` by hand",
                    names.len(),
                    names.join(", ")
                )),
            }
        }
    }

    // The instance profile. `--instance-profile` settles it when the
    // account holds more than one — the normal case for an account that
    // also runs something else — and it is checked against the account so
    // a typo is caught here rather than at `RunInstances`.
    if let Some(p) = instance_profile
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
    {
        let known = crate::aws_cli_opt(root, &instance_profile_names_args())
            .ok()
            .flatten()
            .map(|t| parse_name_list(&t))
            .unwrap_or_default();
        if !known.is_empty() && !known.contains(&p) {
            out.push(format!(
                "  instance profile {p}   — WARNING: not in this account, which has {}: {}",
                known.len(),
                known.join(", ")
            ));
            out.push("    the launch will refuse until the name matches".into());
        } else {
            out.push(format!(
                "  instance profile {p}   (named by --instance-profile)"
            ));
        }
        crate::set_json(&mut doc, &["iam_instance_profile"], serde_json::json!(p));
    } else if cfg.iam_instance_profile.trim().is_empty() {
        match crate::aws_cli_opt(root, &instance_profile_names_args()) {
            Ok(Some(text)) => {
                let names = parse_name_list(&text);
                match sole_name(&names) {
                    Some(n) => {
                        crate::set_json(
                            &mut doc,
                            &["iam_instance_profile"],
                            serde_json::json!(n),
                        );
                        out.push(format!("  instance profile {n}   (the only one in the account)"));
                    }
                    None => out.push(format!(
                        "  instance profile: {} in the account ({}) — pass --instance-profile <name>",
                        names.len(),
                        names.join(", ")
                    )),
                }
            }
            Ok(None) => out.push(
                "  instance profile: none in the account — create the role in the console (IAM → Roles → Create role → EC2), then run this again"
                    .into(),
            ),
            Err(e) => out.push(format!("  instance profile: lookup failed — {e:#}")),
        }
    } else {
        out.push(format!(
            "  instance profile {}   (kept)",
            cfg.iam_instance_profile
        ));
    }

    let text = serde_json::to_string_pretty(&doc)
        .map_err(|e| anyhow::anyhow!("serialising {}: {e}", path.display()))?;
    bm_core::util::atomic_write(&path, &format!("{text}\n"))?;
    out.push(String::new());
    out.push(format!("wrote {}", path.display()));
    // Whatever is left is a decision rather than a lookup, so the next
    // command names it instead of this one guessing.
    let missing = AwsConfig::load_layered(root).missing();
    if missing.is_empty() {
        // Two separate claims, kept separate: the *launch* is ready (the
        // box gets the public half from AWS, so the local file is not
        // needed to start one), while a refused key means the box could
        // not be reached afterwards. Saying only "ready" would be true
        // and useless.
        out.push(if key_refused {
            "ready to launch — but the private key above was NOT replaced, so ssh to the box will fail until it matches"
                .into()
        } else {
            "ready: nothing missing — `aws up --dry-run` shows the launch".into()
        });
    } else {
        out.push(format!(
            "{} still to set (these are decisions, not lookups):",
            missing.len()
        ));
        for m in missing {
            out.push(format!("  - {m}"));
        }
    }
    Ok(out)
}

/// Launch `count` boxes, returning the lines to show, what the account answered,
/// and the pool they were launched into.
///
/// The config rides back with the instances because the caller has to turn each
/// one into a [`Machine`](bm_proto::Machine) with the same pool key and login —
/// and re-reading the file to do it would be a second, quieter source of truth.
///
/// The cap is checked against the *total*, not this call: a cap that only counts
/// what one invocation asked for is not a cap.
pub(crate) fn launch(
    root: &Path,
    count: u32,
) -> Result<(AwsConfig, Vec<String>, Vec<AwsInstance>)> {
    let cfg = AwsConfig::load_layered(root);
    let missing = cfg.missing();
    if !missing.is_empty() {
        let mut msg = String::from("the pool is not ready to launch:");
        for m in missing {
            msg.push_str(&format!("\n  - {m}"));
        }
        anyhow::bail!("{msg}");
    }
    // The marker tag's value *is* the profile hash, so without one the box would
    // be untraceable in `ls` and unprovisionable anyway — `provision` refuses
    // without a loaded profile.
    let hash = bm_core::profile::read_pointer(root)
        .map(|p| p.hash)
        .unwrap_or_default();
    if hash.is_empty() {
        anyhow::bail!(
            "no profile loaded — the marker tag records which profile a box was built for, and provisioning refuses without one; load one first: `:profile` in the dashboard (pack <name>, then <name>), or `tools/profile.sh fetch/unpack <name>`"
        );
    }
    let json = crate::aws_cli_instances(root, &cfg.region, &cfg.tag_key)?;
    let live = parse_instances(&json, &cfg.tag_key)
        .ok_or_else(|| anyhow::anyhow!("the aws CLI answered something unexpected"))?
        .iter()
        .filter(|i| matches!(i.state.as_str(), "pending" | "running" | "stopping"))
        .count() as u32;
    if live + count > cfg.max_workers {
        anyhow::bail!(
            "{live} box(es) already live and {count} asked for, over max_workers={} — raise it in `.bm/aws.json` or launch fewer",
            cfg.max_workers
        );
    }
    // The mapping must name the image's own root device (`/dev/sda1` on Ubuntu,
    // `/dev/xvda` on Amazon Linux), so it is resolved rather than assumed —
    // otherwise `disk_gb` is silently ignored.
    let image = cfg.image().unwrap_or_default().to_string();
    let root_device = crate::aws_cli_text(root, &describe_image_args(&cfg, &image))?;
    let argv = run_instances_args(&cfg, count, root_device.trim(), &hash);
    let mut out = vec![
        format!(
            "{live} live, cap {}, launching {count} tagged {}",
            cfg.max_workers, cfg.tag_key
        ),
        format!("aws {}", argv.join(" ")),
    ];
    let raw = crate::aws_cli_raw(root, &argv)?;
    let launched = parse_instances(&raw, &cfg.tag_key).unwrap_or_default();
    if launched.is_empty() {
        out.push("the launch answered without any instances — check the account".into());
    }
    for i in &launched {
        out.push(format!("launched {}", instance_line(i)));
    }
    out.push(String::new());
    out.push("Next: `provision --addr <ip>` each one, then `:B` to start their workers.".into());
    Ok((cfg, out, launched))
}

/// Terminate exactly the ids given, and report what the API said.
///
/// Takes ids rather than a filter on purpose: the destructive step is always
/// over a list someone could read. The caller resolves the ids from the marker
/// tag (CLI) or from the Cloud view (dashboard).
pub(crate) fn terminate(root: &Path, ids: &[String]) -> Result<Vec<String>> {
    let cfg = AwsConfig::load_layered(root);
    if cfg.region.trim().is_empty() {
        anyhow::bail!("no region set — `aws init`, then fill it in");
    }
    let raw = crate::aws_cli_raw(root, &terminate_args(&cfg.region, ids))?;
    let mut out = Vec::new();
    for i in parse_instances(&raw, &cfg.tag_key).unwrap_or_default() {
        out.push(format!("terminating {} ({})", i.id, i.state));
    }
    Ok(out)
}
