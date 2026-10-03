use super::cli::AwsCmd;
use super::*;

/// The AWS pool: the app's IAM user, the definition, and what the account
/// holds.
///
/// Returns the lines to show, like [`workspace_cmd`], the CLI prints them and
/// the dashboard could log the same operation. `policy` and `show` read no
/// network at all; `login` verifies against the account; `up`/`down` are the
/// two that spend money and destroy things.
pub(crate) fn aws_cmd(root: &std::path::Path, cmd: AwsCmd) -> anyhow::Result<Vec<String>> {
    use bm_core::provision::{
        describe_image_args, instance_line, parse_instances, run_instances_args, terminate_args,
        AwsConfig,
    };
    let path = root.join(".bm").join("aws.json");
    let mut out: Vec<String> = Vec::new();
    match cmd {
        AwsCmd::Init { force } => {
            out.push(seed_pool(root, &path, force)?);
            out.push(String::new());
            // Only two things have to be typed, and neither is a lookup: the
            // identity (console work, then `aws login`) and the region.
            // Everything else `discover` reads off the account.
            out.push("Then, in order:".into());
            out.push("  bm-inductor aws login --csv ~/Downloads/accessKeys.csv".into());
            out.push("  bm-inductor aws discover --region eu-central-1 \\".into());
            out.push("      --pem ~/Downloads/storycast.pem".into());
            out.push(String::new());
            out.push("`discover` resolves the AMI, the keypair, the instance profile and".into());
            out.push("the default subnet and security group, prints each one, and writes".into());
            out.push("them here — so nothing has to be looked up by hand and nothing is".into());
            out.push("re-resolved on the next launch. `aws show` lists whatever is still".into());
            out.push("missing.".into());
            out.push(String::new());
            out.push("Add these when the account cannot choose for you. Each is checked".into());
            out.push("before it is written and kept once set, so a typo costs a sentence".into());
            out.push("rather than a failed launch:".into());
            out.push("  --instance-profile <name>   the account holds more than one".into());
            out.push(
                "  --security-group <sg-…>     the default group is shared — use your own".into(),
            );
            out.push(
                "  --subnet <subnet-…>         the default subnet is one AZ of several".into(),
            );
            out.push(String::new());
            out.push("It also reads the chosen security group's inbound rules and says so".into());
            out.push("when none admits you: a closed port makes ssh HANG, it does not".into());
            out.push("refuse, so the symptom is silence.".into());
            out.push(String::new());
            out.push("`region` is the only field you must decide; `aws discover` fills".into());
            out.push("the rest. Assets reach a box by rsync from this machine, so there".into());
            out.push("is no bucket to create.".into());
            out.push(String::new());
            // The identity is not "whatever you already have on this machine".
            // Naming the user here matters: `init` is the first command anyone
            // runs, and the old text told them to grant EC2 to their own
            // identity, which is the thing this replaces.
            out.push("The identity this runs as is an IAM user created for the app —".into());
            out.push("created entirely in the AWS console:".into());
            out.push(
                "  bm-inductor aws policy    # the policy, and the commands that create it".into(),
            );
            out.push("  step by step: docs/AWS-IAM-USER.md".into());
            out.push(String::new());
            // Console work, all of it, and named here rather than assumed,
            // because the console is where the account gets set up.
            out.push("In the console, alongside the user:".into());
            out.push("  the SSH keypair — EC2 → Key pairs → Create key pair, and keep".into());
            out.push("  the downloaded .pem; `aws discover --pem` puts it where the".into());
            out.push("  boxes expect it (.bm/aws/<region>.pem, 0600)".into());
            out.push("  a security group — EC2 → Security Groups → Create, SSH inbound".into());
            out.push("  only. The account's default group is shared with everything else".into());
            out.push("  in the default VPC, so a rule on it is a rule on all of that too.".into());
            out.push("  Name yours with `--security-group` (docs/AWS-IAM-USER.md step 6)".into());
            out.push(String::new());
            out.push("Creating, tagging and terminating boxes is money and destruction — those are `aws up` / `aws down`, next.".into());
            Ok(out)
        }
        AwsCmd::Policy => {
            // The tracked document is the single source: `aws policy` prints it
            // and the guide says to install it with `--policy-document`, so
            // there is one policy rather than a JSON block in a doc and an
            // action list in the code that quietly disagree.
            let path = root.join(bm_core::provision::aws_credentials::POLICY_FILE);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            // Refuse to print something that is not a policy: pasted into IAM
            // it fails with a message about the *document*, not about this.
            let doc: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?;
            if doc.get("Statement").and_then(|s| s.as_array()).is_none() {
                anyhow::bail!(
                    "{} has no `Statement` array — it does not look like a policy document",
                    path.display()
                );
            }
            out.push(format!(
                "the policy for this app's IAM user — {} (tracked)",
                path.display()
            ));
            out.push(String::new());
            out.push(text.trim_end().to_string());
            out.push(String::new());
            out.push("Two placeholders to replace: <ACCOUNT_ID> and <WORKER_ROLE>".into());
            out.push("  (the role the boxes assume — `aws init` names the profile).".into());
            out.push(String::new());
            out.push("Create the user and attach it — as an account admin, once:".into());
            out.push("  aws iam create-user --user-name storycast-operator".into());
            out.push("  aws iam put-user-policy --user-name storycast-operator \\".into());
            out.push(
                "      --policy-name storycast-operator --policy-document file://<the file above, placeholders replaced>"
                    .into(),
            );
            out.push("  aws iam create-access-key --user-name storycast-operator   # secret is shown once".into());
            out.push(String::new());
            out.push("Then, as the operator:".into());
            out.push("  bm-inductor aws login --access-key-id <that key id>".into());
            out.push("  bm-inductor aws ls       # the check: reaches the API as that user".into());
            out.push(String::new());
            out.push("Full walkthrough, including the console path: docs/AWS-IAM-USER.md".into());
            Ok(out)
        }
        AwsCmd::Login { args } => {
            // Prompting is a terminal affordance and stays here: a CLI has a
            // hidden stdin, the dashboard does not. The verify-then-write order
            // lives in `aws_ops::login`, shared with the TUI, which hands over
            // the console's CSV instead of typing a secret.
            let aws_ops::LoginArgs { access_key_id, csv } = args;
            let (key_id, secret) = match csv {
                // Passed through when a CSV is present, so the shared check
                // refuses the two answers rather than silently preferring one.
                Some(_) => (access_key_id, None),
                None => {
                    let key_id = match access_key_id {
                        Some(k) => k,
                        None => ask("AWS access key id: ")?,
                    };
                    let secret = ask_secret("AWS secret access key (not echoed): ")?;
                    (Some(key_id), Some(secret))
                }
            };
            out.extend(aws_ops::login(root, csv, key_id, secret)?);
            Ok(out)
        }
        AwsCmd::Discover { args } => {
            out.extend(aws_ops::discover(root, args)?);
            Ok(out)
        }
        AwsCmd::Show => {
            let cfg = AwsConfig::load_layered(root);
            // Which identity is in force, named before anything else: "why
            // can't it launch" is usually the IAM user, missing, or not the
            // one the operator thinks, and the answer should not require
            // running a command that spends money.
            out.push(bm_core::provision::aws_credentials::source(root).describe(root));
            if !path.exists() {
                out.push(format!(
                    "no pool defined yet — `aws init` writes {}",
                    path.display()
                ));
            }
            out.push(cfg.summary());
            // The key file is named after the region, so there is nothing
            // meaningful to print before one is set, a path ending in `.pem`
            // with no name in it reads as a missing file rather than as a
            // missing region.
            if cfg.region.trim().is_empty() {
                out.push("private key: (no region yet — the file is named after it)".into());
            } else {
                let key = root.join(cfg.key_file());
                out.push(format!(
                    "private key: {} ({})",
                    key.display(),
                    if key.is_file() {
                        "present"
                    } else {
                        "MISSING — the box cannot be reached without it"
                    }
                ));
            }
            // One asset plane, and it is this machine's disk. Named rather than
            // left implicit because it is ~668 MB of upload per box, and this
            // line is read before a launch spends money.
            out.push(
                "asset plane: rsync from this machine (~668 MB per box) — publishing it as a release artifact is designed, not built (docs/ARTIFACTS.md)".into(),
            );
            let missing = cfg.missing();
            if missing.is_empty() {
                // "Ready" means ready to *launch*, which is what `missing()`
                // knows about, and the firewall is deliberately not part of it,
                // because this command reads no network. Said out loud, because
                // a box behind a closed port launches perfectly and then does
                // nothing, and this is the line someone will read before
                // spending money.
                out.push(
                    "ready: nothing missing — the firewall is not checked here; `aws discover` checks it"
                        .into(),
                );
            } else {
                out.push(String::new());
                out.push(format!("{} thing(s) still to set:", missing.len()));
                for m in missing {
                    out.push(format!("  - {m}"));
                }
            }
            Ok(out)
        }
        AwsCmd::Ls => {
            out.extend(aws_ops::pool_lines(root)?);
            Ok(out)
        }
        AwsCmd::Up { count, dry_run } => {
            let cfg = AwsConfig::load_layered(root);
            let missing = cfg.missing();
            let layout = bm_core::Layout::resolve_or_root(root).0;
            let hash = bm_core::profile::in_force(&layout)
                .map(|b| b.pack.hash)
                .unwrap_or_default();
            if dry_run {
                // No call at all, and that is the point: a dry run has to work
                // *before* the account is set up, which is exactly when the
                // permissions are missing and when seeing the call matters
                // most. The two lookups the real path needs are shown rather
                // than performed.
                out.push("--dry-run: no call is made and nothing is launched.".into());
                out.push(format!(
                    "would run: aws {}",
                    run_instances_args(&cfg, count, "<the AMI's root device>", &hash).join(" ")
                ));
                out.push(format!(
                    "  after resolving it: aws {}",
                    describe_image_args(&cfg, cfg.image().unwrap_or("<AMI>")).join(" ")
                ));
                out.push(format!(
                    "  and counting what is live: aws ec2 describe-instances --filters Name=tag-key,Values={}",
                    cfg.tag_key
                ));
                if !missing.is_empty() {
                    out.push(String::new());
                    out.push(format!(
                        "{} thing(s) to set before the real run:",
                        missing.len()
                    ));
                    for m in missing {
                        out.push(format!("  - {m}"));
                    }
                }
                return Ok(out);
            }
            let (_, lines, _) = aws_ops::launch(root, count)?;
            out.extend(lines);
            Ok(out)
        }
        AwsCmd::Down { dry_run } => {
            let cfg = AwsConfig::load_layered(root);
            if cfg.region.trim().is_empty() {
                anyhow::bail!("no region set — `aws init`, then fill it in");
            }
            let json = aws_cli_instances(root, &cfg.region, &cfg.tag_key)?;
            let found = parse_instances(&json, &cfg.tag_key).ok_or_else(|| {
                anyhow::anyhow!(
                    "the aws CLI answered something unexpected — not terminating on a guess"
                )
            })?;
            let live: Vec<_> = found
                .iter()
                .filter(|i| matches!(i.state.as_str(), "pending" | "running" | "stopping"))
                .collect();
            if live.is_empty() {
                out.push(format!(
                    "nothing to terminate (no live box carries {})",
                    cfg.tag_key
                ));
                return Ok(out);
            }
            out.push(format!("{} box(es) carry {}:", live.len(), cfg.tag_key));
            for i in &live {
                out.push(format!("  {}", instance_line(i)));
            }
            let ids: Vec<String> = live.iter().map(|i| i.id.clone()).collect();
            if dry_run {
                out.push(String::new());
                out.push(format!(
                    "--dry-run: nothing terminated. Would run: aws {}",
                    terminate_args(&cfg.region, &ids).join(" ")
                ));
                return Ok(out);
            }
            out.extend(aws_ops::terminate(root, &ids)?);
            Ok(out)
        }
    }
}

/// Write `.bm/aws.json` from the tracked template, once.
///
/// Split out of `init` rather than duplicated, because `discover` may be the
/// first command anyone runs and must not be a second implementation of the
/// same seeding.
pub(crate) fn seed_pool(
    root: &std::path::Path,
    path: &std::path::Path,
    force: bool,
) -> anyhow::Result<String> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists — edit it, or pass --force to replace it",
            path.display()
        );
    }
    let template = root.join(bm_core::provision::DEFAULT_FILE);
    // Seed from the tracked template when it is there, so the file the operator
    // edits carries the `_note`s explaining each field rather than a bare
    // struct dump. The notes are ignored on read, serde drops what the struct
    // does not name, and the round trip proves a hand-edited template that has
    // drifted from the shape cannot seed a broken config.
    if template.is_file() {
        let text = std::fs::read_to_string(&template)?;
        serde_json::from_str::<bm_core::provision::AwsConfig>(&text)
            .map_err(|e| anyhow::anyhow!("{} does not parse: {e}", template.display()))?;
        bm_core::util::atomic_write(path, &text)?;
        Ok(format!(
            "wrote {} (from {})",
            path.display(),
            template.display()
        ))
    } else {
        bm_core::provision::AwsConfig::default().save(path)?;
        Ok(format!("wrote {}", path.display()))
    }
}

/// The local pool document, as raw JSON.
///
/// Refuses to start from an empty document when the file exists but does not
/// parse: quietly writing `{}` back over it would discard every field the
/// operator had already set, and the file is theirs.
pub(crate) fn read_pool_doc(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let text = std::fs::read_to_string(path)?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} does not parse: {e}", path.display()))?;
    if !doc.is_object() {
        anyhow::bail!("{} is not a JSON object", path.display());
    }
    Ok(doc)
}

/// Set one key in a pool document, creating the objects on the way down.
pub(crate) fn set_json(doc: &mut serde_json::Value, path: &[&str], value: serde_json::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = doc;
    for key in parents {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        let Some(obj) = cur.as_object_mut() else {
            return;
        };
        cur = obj
            .entry((*key).to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    if !cur.is_object() {
        *cur = serde_json::json!({});
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.insert((*last).to_string(), value);
    }
}

/// One `aws` call whose answer is a single value, or nothing.
///
/// `None` means "the call worked and there was nothing to find", an account
/// with no default subnet, a region with no keypairs. For `discover` that is an
/// answer to report rather than a failure, which is why it is not
/// [`aws_cli_text`], where an empty answer is a refusal.
pub(crate) fn aws_cli_opt(
    root: &std::path::Path,
    args: &[String],
) -> anyhow::Result<Option<String>> {
    let text = aws_cli_raw(root, args)?.trim().to_string();
    if text.is_empty() || text == "None" {
        return Ok(None);
    }
    Ok(Some(text))
}

/// `aws ec2 describe-instances`, filtered to the boxes we tagged.
///
/// The filter is `tag-key`, not a value: it matches every box we started
/// whatever profile it was built for, and it cannot match a stranger's
/// instances.
pub(crate) fn aws_cli_instances(
    root: &std::path::Path,
    region: &str,
    tag_key: &str,
) -> anyhow::Result<String> {
    aws_cli_raw(
        root,
        &[
            "ec2".into(),
            "describe-instances".into(),
            "--region".into(),
            region.into(),
            "--filters".into(),
            format!("Name=tag-key,Values={tag_key}"),
            "--filters".into(),
            "Name=instance-state-name,Values=pending,running,stopping,stopped".into(),
            "--output".into(),
            "json".into(),
        ],
    )
}

/// Run one `aws` subcommand and hand back stdout, or an error naming the fix.
///
/// Shared by `ls`/`up`/`down` so the three cannot disagree about what a missing
/// CLI, missing credentials, or a policy that forbids the call means. Those are
/// all normal first-run states, so each gets a sentence naming the fix instead
/// of a raw exit status.
/// Ask for one visible line. Used for the key id, which is an identifier
/// rather than a secret and hiding it only makes a typo harder to spot.
fn ask(prompt: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, Write};
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Ask for one line without echoing it when stdin is a terminal.
///
/// `stty -echo` rather than an `rpassword` dependency, one call, Unix-only,
/// the same reasoning the cluster token uses for `/dev/urandom`. Piped input
/// is read plainly, which is what makes `printf '%s\n' "$SECRET" | …` work.
///
/// If `stty` is missing the echo is simply not suppressed; the value is still
/// read correctly, and nothing is written anywhere it should not be.
fn ask_secret(prompt: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let tty = std::io::stdin().is_terminal();
    print!("{prompt}");
    std::io::stdout().flush()?;
    if tty {
        let _ = std::process::Command::new("stty").arg("-echo").status();
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if tty {
        let _ = std::process::Command::new("stty").arg("echo").status();
        println!();
    }
    read?;
    Ok(line.trim().to_string())
}

pub(crate) fn aws_cli_raw(root: &std::path::Path, args: &[String]) -> anyhow::Result<String> {
    // Every AWS call this tool makes goes through here, and this is the line
    // that makes "the app runs as the IAM user you created for it" true rather
    // than aspirational: no user stored, no call made. Never a fallback.
    bm_core::provision::aws_credentials::require(root)?;
    aws_cli_with(root, args, &[])
}

/// Run one `aws` subcommand with credentials supplied for this call alone.
///
/// Only `aws login` uses it, and it has to: the identity must be proven, and
/// proven to be an IAM *user*, before there is a file to point the CLI at.
///
/// The shadowing variables are stripped either way. Env-var keys outrank a
/// shared credentials file, so a stray `AWS_ACCESS_KEY_ID` exported in the
/// shell would otherwise win silently while `aws show` reported the IAM user.
pub(crate) fn aws_cli_with(
    root: &std::path::Path,
    args: &[String],
    supplied: &[(String, String)],
) -> anyhow::Result<String> {
    let mut cmd = std::process::Command::new("aws");
    cmd.args(args);
    for k in bm_core::provision::aws_credentials::SHADOWING_ENV {
        cmd.env_remove(k);
    }
    for (k, v) in bm_core::provision::aws_credentials::cli_env(root) {
        cmd.env(k, v);
    }
    for (k, v) in supplied {
        cmd.env(k, v);
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "the `aws` CLI is not on PATH — install the AWS CLI v2, or run `aws show` for the definition alone"
        ),
        Err(e) => anyhow::bail!("could not run the aws CLI: {e}"),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Three first-run states, three different fixes. The middle one is the
        // one that wastes an afternoon: the credentials *are* found, so nothing
        // says "credentials", the call is simply not allowed, and the account
        // and user in the message are the only clue about which policy to edit.
        let hint = if err.contains("Unable to locate credentials") {
            " — the stored key was not accepted; re-run `bm-inductor aws login` (docs/AWS-IAM-USER.md)"
        } else if err.contains("UnauthorizedOperation") || err.contains("not authorized") {
            " — this IAM user is not allowed to make this call; `bm-inductor aws policy` prints the policy it needs"
        } else if err.contains("InvalidClientTokenId") || err.contains("ExpiredToken") {
            " — the stored key is stale or deleted; `bm-inductor aws login` with a fresh one"
        } else {
            ""
        };
        anyhow::bail!(
            "aws {} failed{hint}: {}",
            args.first().map(String::as_str).unwrap_or("?"),
            bm_core::util::head_chars(err.trim(), 300)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One `--query`-driven value: a single line, trimmed. `None` and empty are
/// both "the call succeeded and answered nothing", which for a root device name
/// is a refusal rather than a default.
pub(crate) fn aws_cli_text(root: &std::path::Path, args: &[String]) -> anyhow::Result<String> {
    let text = aws_cli_raw(root, args)?.trim().to_string();
    if text.is_empty() || text == "None" {
        anyhow::bail!("aws {} answered nothing", args.join(" "));
    }
    Ok(text)
}
