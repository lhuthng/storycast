//! Where this app's AWS identity comes from.
//!
//! **One source: the IAM user you create for this app.** Not your own machine's
//! AWS setup — not `AWS_PROFILE`, not SSO, not an instance role, and not
//! whatever `AWS_ACCESS_KEY_ID` happens to be exported in the shell you ran
//! from. A pool this tool launches spends real money and destroys real boxes,
//! and the permissions for that belong to a named user with one key, whose
//! access can be revoked without touching anyone's laptop.
//!
//! So the file is the identity, and its absence is a **refusal**
//! ([`require`]), never a quiet fallback. That is the whole difference from the
//! chain this replaced: a fallback is silent, and silence is what makes "why did
//! it work on my machine" unanswerable.
//!
//! The file is **`.bm/aws/credentials`** — 0600, gitignored, written in **AWS's
//! own INI format**. Not a format of our own: the `aws` CLI already parses that
//! format and already knows the precedence rules, so this code never parses,
//! logs or re-serialises a secret. It hands the child process
//! `AWS_SHARED_CREDENTIALS_FILE` and `AWS_PROFILE` and the CLI does the rest —
//! and the file stays exactly as portable as `~/.aws/credentials`.
//!
//! Credentials are never written to `aws.default.json`, `.bm/aws.json`,
//! `machines.json`, the ledger, or git. That is what makes the rest of the
//! pool definition shareable: two people with the same repo and different
//! accounts each log in as their own IAM user, and neither can commit the
//! other's key.
//!
//! Creating that user — the policy it needs, and the commands that attach it —
//! is [`POLICY_FILE`] and `docs/AWS-IAM-USER.md`; `bm-inductor aws policy`
//! prints both.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The profile name inside our own file.
///
/// Distinct from `default` on purpose. The file is ours and
/// `AWS_SHARED_CREDENTIALS_FILE` points the CLI straight at it, so a collision
/// is not possible — but a name that *cannot* be confused with a user's own
/// `default` costs nothing, and an accidental mix should be visible rather
/// than silent.
pub const PROFILE: &str = "storycast";

/// The tracked least-privilege policy for the app's IAM user, at the repo root.
///
/// Tracked for the same reason `aws.default.json` is: it is not a secret and it
/// is not personal — it is the answer to "what is this user allowed to do", and
/// that answer has to travel with the code that makes the calls. `aws policy`
/// prints it, and `aws iam put-user-policy --policy-document file://…` reads it,
/// so the document and the commands that install it cannot drift apart.
pub const POLICY_FILE: &str = "aws-policy.json";

/// Environment variables that would otherwise let *this machine's* identity
/// shadow the IAM user.
///
/// They are removed from every `aws` child process, and that is not hygiene —
/// it is the mechanism. Env-var keys outrank a shared credentials file in the
/// CLI's own resolution order, so a stray `AWS_ACCESS_KEY_ID` exported in the
/// shell would win silently while `aws show` reported the IAM user. A
/// credential source that can be overridden without saying so is not a source.
pub const SHADOWING_ENV: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
];

/// `.bm/aws/credentials`, beside the per-region private keys already there.
pub fn credentials_path(root: &Path) -> PathBuf {
    root.join(".bm").join("aws").join("credentials")
}

/// The environment every `aws` CLI call must run with.
///
/// Empty when there is no file — and that empty answer is not a fallback to
/// anything: [`require`] has already refused by then. It exists so the
/// function has an honest answer for the one state it cannot describe, and so
/// a caller that skipped [`require`] fails loudly at the CLI ("unable to locate
/// credentials") instead of quietly borrowing this machine's.
pub fn cli_env(root: &Path) -> Vec<(String, String)> {
    let p = credentials_path(root);
    if !p.is_file() {
        return Vec::new();
    }
    vec![
        (
            "AWS_SHARED_CREDENTIALS_FILE".to_string(),
            p.display().to_string(),
        ),
        ("AWS_PROFILE".to_string(), PROFILE.to_string()),
    ]
}

/// Refuse to make an AWS call without the app's IAM user.
///
/// Called by every path that shells out to `aws`. It checks the mode too,
/// because a credentials file the rest of the box can read is the one failure
/// that never announces itself: by the time anyone notices, it has already been
/// readable for as long as the file has existed.
pub fn require(root: &Path) -> Result<()> {
    let p = credentials_path(root);
    if !p.is_file() {
        anyhow::bail!(
            "no IAM user for this app yet — {} does not exist.\n  \
             Create the user (docs/AWS-IAM-USER.md; `bm-inductor aws policy` prints the \
             policy and the commands), then store its key:\n    \
             bm-inductor aws login --access-key-id AKIA…\n  \
             This app runs as that user and nothing else — it does not fall back to this \
             machine's own AWS identity.",
            p.display()
        );
    }
    check_mode(&p)
}

/// Which identity is in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// The IAM user this app was given, naming the key it holds and — when the
    /// login verified it — who that key belongs to.
    IamUser {
        /// The key id — an identifier, like a username, not a secret.
        key_id: String,
        /// A hash prefix of the secret, so an operator can tell *which* secret
        /// is loaded without the secret reaching a terminal or a log.
        fingerprint: String,
        /// `sts get-caller-identity` as of the last login, when it answered.
        identity: Option<Identity>,
    },
    /// No file. Nothing to run as — a state to fix, not a state to fall back
    /// from.
    None,
}

pub fn source(root: &Path) -> CredentialSource {
    let p = credentials_path(root);
    let Ok(text) = std::fs::read_to_string(&p) else {
        return CredentialSource::None;
    };
    match parse_profile(&text) {
        Some((key_id, secret)) => CredentialSource::IamUser {
            key_id,
            fingerprint: fingerprint(&secret),
            identity: parse_identity(&text),
        },
        // A file we cannot read is not a file we can claim to be using. Say so
        // rather than reporting an identity that may be half there.
        None => CredentialSource::None,
    }
}

impl CredentialSource {
    /// One line for `aws show`. Never contains the secret.
    pub fn describe(&self, root: &Path) -> String {
        let path = credentials_path(root);
        match self {
            CredentialSource::None => format!(
                "identity: none — no IAM user stored ({}). See docs/AWS-IAM-USER.md, \
                 then `aws login`",
                path.display()
            ),
            CredentialSource::IamUser {
                key_id,
                fingerprint,
                identity: Some(id),
            } => format!(
                "identity: IAM user {} · account {} · key {key_id} · secret {fingerprint} · {}",
                id.user_name().unwrap_or(&id.arn),
                id.account,
                path.display()
            ),
            CredentialSource::IamUser {
                key_id,
                fingerprint,
                identity: None,
            } => format!(
                "identity: IAM user · key {key_id} · secret {fingerprint} · {} \
                 (identity unverified — `aws login` again to name it)",
                path.display()
            ),
        }
    }
}

/// Who a set of credentials is, as `sts get-caller-identity` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub account: String,
    pub arn: String,
}

impl Identity {
    /// Whether this is an IAM **user** — the only kind of identity this app is
    /// meant to run as.
    ///
    /// `arn:aws:iam::<acct>:root`, `:assumed-role/…` and `:federated-user/…`
    /// are all *someone else's* identity borrowed for a while, which is exactly
    /// what a dedicated user exists to replace.
    pub fn is_user(&self) -> bool {
        self.arn.contains(":user/")
    }

    /// The name at the end of a user ARN, for a line a human reads.
    pub fn user_name(&self) -> Option<&str> {
        self.arn.rsplit_once(":user/").map(|(_, n)| n)
    }
}

/// Read `sts get-caller-identity --output json`.
///
/// `None` when the payload is not the shape we expect, so the caller can say
/// "the CLI answered something else" rather than reporting an identity it did
/// not actually see.
pub fn parse_identity_json(json: &str) -> Option<Identity> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    let account = doc.get("Account")?.as_str()?.trim().to_string();
    let arn = doc.get("Arn")?.as_str()?.trim().to_string();
    if account.is_empty() || arn.is_empty() {
        return None;
    }
    Some(Identity { account, arn })
}

/// A hash prefix of the secret: identity without disclosure.
pub fn fingerprint(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    h.finalize()
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Write the credentials, owner-only.
///
/// The secret is written here and nowhere else: not to `argv` (visible in
/// `ps`), not to a log, not to the pool definition.
///
/// `identity` is recorded as comments — AWS's INI ignores them, and it is what
/// lets `aws show` name the user and the account with no network call at all.
/// It is not a secret: the ARN and the account id are both public identifiers
/// within an account.
pub fn write(
    root: &Path,
    key_id: &str,
    secret: &str,
    identity: Option<&Identity>,
) -> Result<PathBuf> {
    let p = credentials_path(root);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut body = String::from(
        "# Written by `bm-inductor aws login`. Personal — never commit it.\n\
         # This is the app's IAM user, not this machine's AWS identity.\n",
    );
    if let Some(id) = identity {
        body.push_str(&format!(
            "# iam_user = {}\n# account = {}\n",
            id.arn, id.account
        ));
    }
    body.push_str(&format!(
        "[{PROFILE}]\n\
         aws_access_key_id = {}\n\
         aws_secret_access_key = {}\n",
        key_id.trim(),
        secret.trim(),
    ));
    crate::atomic_write(&p, &body)?;
    crate::util::restrict(&p)?;
    Ok(p)
}

/// Refuse a credentials file the rest of the box can read.
///
/// Checked on the way *in* rather than left to be discovered: a world-readable
/// secret fails silently, and by the time anyone notices it has already been
/// readable for as long as the file has existed.
pub fn check_mode(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(p)
            .with_context(|| format!("stat {}", p.display()))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} is mode {mode:o} — a credentials file must not be group- or world-readable; `chmod 600 {}`",
                p.display(),
                p.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = p;
    }
    Ok(())
}

/// Pull the key id and secret out of our profile.
///
/// A hand-rolled scan rather than a dependency, deliberately: the file holds
/// exactly the two lines [`write`] put there, and a general INI parser would
/// also be able to read files this app does not own.
fn parse_profile(text: &str) -> Option<(String, String)> {
    let mut in_ours = false;
    let mut id = None;
    let mut secret = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_ours = line.trim_start_matches('[').trim_end_matches(']').trim() == PROFILE;
            continue;
        }
        if !in_ours || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(v) = line.strip_prefix("aws_access_key_id") {
            id = Some(v.trim_start_matches([' ', '=']).trim().to_string());
        } else if let Some(v) = line.strip_prefix("aws_secret_access_key") {
            secret = Some(v.trim_start_matches([' ', '=']).trim().to_string());
        }
    }
    Some((id?, secret?))
}

/// The `# iam_user` / `# account` comments [`write`] records, if they are there.
///
/// Read outside the profile section on purpose: they describe the file, not the
/// profile, and a file whose section was hand-edited should still be able to say
/// whose key it claims to hold.
fn parse_identity(text: &str) -> Option<Identity> {
    let mut arn = None;
    let mut account = None;
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('#').trim();
        if let Some(v) = line.strip_prefix("iam_user") {
            arn = Some(v.trim_start_matches([' ', '=']).trim().to_string());
        } else if let Some(v) = line.strip_prefix("account") {
            account = Some(v.trim_start_matches([' ', '=']).trim().to_string());
        }
    }
    let (arn, account) = (arn?, account?);
    if arn.is_empty() || account.is_empty() {
        return None;
    }
    Some(Identity { account, arn })
}

/// Read the CSV the AWS console downloads when you create an access key.
///
/// The console's **Download .csv file** button is the natural handoff: the file
/// holds exactly the two values this app needs, and it is the one artifact the
/// console gives you that a terminal would otherwise make you retype. So
/// `aws login --csv <file>` reads it instead of asking for two pastes.
///
/// Deliberately tolerant, because the file is generated by someone else's UI and
/// has changed shape over the years: a BOM, CRLF line endings, quoted fields,
/// extra columns, and the two we want in either order are all accepted. The
/// header row is what identifies the columns — position is not assumed.
///
/// `None` when the file does not contain a key pair, so the caller can say "that
/// is not the file the console downloads" rather than storing half of one.
pub fn parse_access_key_csv(text: &str) -> Option<(String, String)> {
    // Where the two columns are, once a header row has told us.
    let mut cols: Option<(usize, usize)> = None;
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<String> = line
            .split(',')
            .map(|f| f.trim().trim_matches('"').trim().to_string())
            .collect();
        let find = |want: &str| {
            fields
                .iter()
                .position(|f| f.eq_ignore_ascii_case(want))
                .ok_or(())
        };
        match cols {
            // Look for the header. Not every line is it — the console has
            // shipped a title row above it — so a line without both column
            // names is skipped rather than ending the parse.
            None => {
                if let (Ok(id), Ok(secret)) = (find("Access key ID"), find("Secret access key")) {
                    cols = Some((id, secret));
                }
            }
            Some((id_col, secret_col)) => {
                let id = fields.get(id_col)?.trim().to_string();
                let secret = fields.get(secret_col)?.trim().to_string();
                if id.is_empty() || secret.is_empty() {
                    return None;
                }
                return Some((id, secret));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate's own scratch-dir idiom — `bm-core` has no `tempfile`
    /// dependency, and one `temp_dir` helper per module is cheaper than adding
    /// one for four tests.
    fn root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-awscreds-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn an_operator() -> Identity {
        Identity {
            account: "123456789012".into(),
            arn: "arn:aws:iam::123456789012:user/storycast-operator".into(),
        }
    }

    #[test]
    fn the_file_is_aws_format_owner_only_and_readable_back() {
        let d = root("format");
        let p = write(&d, "AKIAEXAMPLE", "s3cr3t-value", Some(&an_operator())).unwrap();
        assert_eq!(p, d.join(".bm/aws/credentials"));
        let text = std::fs::read_to_string(&p).unwrap();
        // AWS's own format, so the CLI parses it and nothing here has to.
        assert!(text.contains("[storycast]"), "{text}");
        assert!(text.contains("aws_access_key_id = AKIAEXAMPLE"), "{text}");
        assert!(
            text.contains("aws_secret_access_key = s3cr3t-value"),
            "{text}"
        );
        // The identity rides along as comments: INI ignores them, and it is
        // what lets `aws show` name the user without a network call.
        assert!(
            text.contains("# iam_user = arn:aws:iam::123456789012:user/storycast-operator"),
            "{text}"
        );
        check_mode(&p).expect("0600 by construction");

        match source(&d) {
            CredentialSource::IamUser {
                key_id,
                fingerprint,
                identity,
            } => {
                assert_eq!(key_id, "AKIAEXAMPLE");
                assert_eq!(fingerprint, self::fingerprint("s3cr3t-value"));
                assert_eq!(identity, Some(an_operator()));
                // The description identifies the secret; the secret itself is
                // not in it and must never be.
                let described = CredentialSource::IamUser {
                    key_id,
                    fingerprint,
                    identity,
                }
                .describe(&d);
                assert!(described.contains("AKIAEXAMPLE"), "{described}");
                assert!(described.contains("storycast-operator"), "{described}");
                assert!(!described.contains("s3cr3t-value"), "{described}");
            }
            other => panic!("expected the app's IAM user, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_file_is_a_refusal_and_never_a_fallback() {
        // The whole point of the module in one test. With nothing stored there
        // is no identity to run as: the call is refused and told how to fix it,
        // rather than silently becoming whatever this machine happens to be.
        let d = root("chain");
        assert!(cli_env(&d).is_empty());
        assert_eq!(source(&d), CredentialSource::None);
        let err = require(&d).unwrap_err().to_string();
        assert!(err.contains("no IAM user for this app"), "{err}");
        assert!(err.contains("aws login"), "names the fix: {err}");
        assert!(err.contains("AWS-IAM-USER.md"), "names the guide: {err}");
        // And the line `aws show` prints first says the same thing.
        assert!(source(&d).describe(&d).contains("identity: none"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_an_iam_user_is_an_identity_this_app_runs_as() {
        // `:root`, an assumed role and a federated user are all somebody else's
        // identity borrowed for a while — the thing a dedicated user replaces.
        assert!(an_operator().is_user());
        assert_eq!(an_operator().user_name(), Some("storycast-operator"));
        for arn in [
            "arn:aws:iam::123456789012:root",
            "arn:aws:sts::123456789012:assumed-role/storycast-worker/i-0abc",
            "arn:aws:sts::123456789012:assumed-role/AWSReservedSSO_Admin/me",
            "arn:aws:iam::123456789012:federated-user/me",
        ] {
            let id = Identity {
                account: "123456789012".into(),
                arn: arn.into(),
            };
            assert!(!id.is_user(), "{arn} must not be accepted");
            assert_eq!(id.user_name(), None, "{arn}");
        }
        // A role ARN that merely mentions `user/` inside a name is still not a
        // user; the marker is `:user/`, which this one does not carry.
        let role = Identity {
            account: "1".into(),
            arn: "arn:aws:iam::1:role/user-ops".into(),
        };
        assert!(!role.is_user());
    }

    #[test]
    fn the_identity_is_read_out_of_the_sts_payload_defensively() {
        let id = parse_identity_json(
            r#"{"UserId":"AIDA…","Account":"123456789012",
                "Arn":"arn:aws:iam::123456789012:user/storycast-operator"}"#,
        )
        .unwrap();
        assert_eq!(id, an_operator());
        // Anything we do not understand is `None`, never a half-filled
        // identity that `aws show` would then print as fact.
        assert_eq!(parse_identity_json("not json"), None);
        assert_eq!(parse_identity_json("{}"), None);
        assert_eq!(parse_identity_json(r#"{"Account":"1"}"#), None);
        assert_eq!(
            parse_identity_json(r#"{"Arn":"arn:aws:iam::1:user/x"}"#),
            None
        );
        assert_eq!(
            parse_identity_json(r#"{"Account":"","Arn":"arn:aws:iam::1:user/x"}"#),
            None
        );
    }

    #[test]
    fn a_loose_mode_is_refused_on_the_way_in() {
        let d = root("mode");
        let p = write(&d, "AKIAEXAMPLE", "s3cr3t-value", None).unwrap();
        // An unverified login still records the key; it just cannot name it.
        assert_eq!(
            source(&d),
            CredentialSource::IamUser {
                key_id: "AKIAEXAMPLE".into(),
                fingerprint: fingerprint("s3cr3t-value"),
                identity: None,
            }
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            let err = check_mode(&p).unwrap_err().to_string();
            assert!(err.contains("world-readable"), "{err}");
            assert!(err.contains("chmod 600"), "names the fix: {err}");
            // And `require` is where it is actually enforced, so no call is
            // made with a secret the rest of the box can read.
            let err = require(&d).unwrap_err().to_string();
            assert!(err.contains("world-readable"), "{err}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_foreign_profile_in_the_same_file_is_not_mistaken_for_ours() {
        // `AWS_SHARED_CREDENTIALS_FILE` points at our file, but an operator may
        // have put other profiles in it. Ours is the one named `storycast`, and
        // reading someone else's key would be the worst possible bug here.
        let d = root("foreign");
        let p = credentials_path(&d);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            "[default]\naws_access_key_id = AKIAOTHER\naws_secret_access_key = other-secret\n\
             [storycast]\naws_access_key_id = AKIAOURS\naws_secret_access_key = our-secret\n",
        )
        .unwrap();
        match source(&d) {
            CredentialSource::IamUser { key_id, .. } => assert_eq!(key_id, "AKIAOURS"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            cli_env(&d),
            vec![
                (
                    "AWS_SHARED_CREDENTIALS_FILE".to_string(),
                    p.display().to_string()
                ),
                ("AWS_PROFILE".to_string(), "storycast".to_string()),
            ]
        );
        // Nothing recorded, so nothing invented: an unverified file is named as
        // unverified rather than reported as somebody.
        assert!(source(&d).describe(&d).contains("unverified"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_shadowing_list_covers_every_way_the_shell_can_win() {
        // Env-var keys outrank a shared credentials file, so this list is the
        // mechanism that makes the IAM user the identity rather than a
        // suggestion. Dropping a name here silently reopens the hole.
        for name in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_PROFILE",
            "AWS_DEFAULT_PROFILE",
        ] {
            assert!(SHADOWING_ENV.contains(&name), "{name} must be scrubbed");
        }
    }

    #[test]
    fn the_console_csv_is_read_however_it_is_shaped() {
        // What the console actually downloads.
        assert_eq!(
            parse_access_key_csv(
                "Access key ID,Secret access key\nAKIAIOSFODNN7EXAMPLE,wJalrXUtnFEMI\n"
            ),
            Some(("AKIAIOSFODNN7EXAMPLE".into(), "wJalrXUtnFEMI".into()))
        );
        // A BOM and CRLF, because the file comes from a browser on a machine
        // this app knows nothing about.
        assert_eq!(
            parse_access_key_csv(
                "\u{feff}Access key ID,Secret access key\r\nAKIAEXAMPLE,s3cr3t\r\n"
            ),
            Some(("AKIAEXAMPLE".into(), "s3cr3t".into()))
        );
        // Columns in the other order, and quoted fields.
        assert_eq!(
            parse_access_key_csv(
                "\"Secret access key\",\"Access key ID\"\n\"s3cr3t\",\"AKIAEXAMPLE\"\n"
            ),
            Some(("AKIAEXAMPLE".into(), "s3cr3t".into()))
        );
        // Extra columns, and a title row above the header.
        assert_eq!(
            parse_access_key_csv(
                "Access key,Notes\nAccess key ID,Secret access key,Created\nAKIAEXAMPLE,s3cr3t,2026-09-20\n"
            ),
            Some(("AKIAEXAMPLE".into(), "s3cr3t".into()))
        );
    }

    #[test]
    fn a_csv_that_is_not_the_console_download_is_refused() {
        // Anything we cannot read is `None`, never half a credential — the
        // caller says "that is not the file the console downloads" instead of
        // storing a key with no secret.
        assert_eq!(parse_access_key_csv(""), None);
        assert_eq!(parse_access_key_csv("hello,world\n"), None);
        assert_eq!(
            parse_access_key_csv("Access key ID,Secret access key\n"),
            None
        );
        assert_eq!(
            parse_access_key_csv("Access key ID,Secret access key\nAKIAEXAMPLE,\n"),
            None
        );
        assert_eq!(
            parse_access_key_csv("Access key ID,Secret access key\n,secret\n"),
            None
        );
    }
}
