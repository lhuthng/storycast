use super::*;

#[derive(Parser)]
#[command(
    name = "bm-inductor",
    about = "Cluster orchestrator for the novel pipeline"
)]
pub(crate) struct Cli {
    /// Repo root (discovered when omitted: `rust/Cargo.toml` in a checkout,
    /// `.bm/profile` on a provisioned worker).
    #[arg(long, global = true)]
    pub(crate) root: Option<std::path::PathBuf>,
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Run the control API + scheduler.
    Serve {
        /// Control API port.
        #[arg(long, default_value = "8901")]
        port: u16,
        /// Bind address (127.0.0.1 for solo, 0.0.0.0 when remote workers join).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Chapter range to reconcile on startup.
        #[arg(long, default_value = "1")]
        start: u32,
        #[arg(long, default_value = "1")]
        count: u32,
        /// Start distributing at once instead of holding until `:go`.
        ///
        /// A run comes up **held**: the range is reconciled (rows, not offers)
        /// and nothing is handed to a worker until an operator says go. That is
        /// what makes a restart stop rather than resume — a box that reboots
        /// into a pending ledger must not silently begin working. This flag is
        /// how an unattended run asks for the old behaviour: a script, a test,
        /// a scheduled job.
        #[arg(long)]
        go: bool,
    },
    /// Onboard one machine by address: probe, push what's missing, verify.
    Provision {
        /// Linked box name (from `link`); skips retyping addr/user/key.
        #[arg(long)]
        r#box: Option<String>,
        /// Machine address (IP or hostname). Required unless --box is given.
        #[arg(long)]
        addr: Option<String>,
        /// SSH user.
        #[arg(long, default_value = "thang")]
        user: String,
        /// SSH port.
        #[arg(long, default_value = "22")]
        port: u16,
        /// SSH key path.
        #[arg(long)]
        key: Option<String>,
        /// Inductor API port (for post-provision registration).
        #[arg(long, default_value = "8901")]
        api_port: u16,
        /// Rebuild even when already configured.
        #[arg(long)]
        force: bool,
        /// GitHub `owner/name` whose releases host the model artifact, so the
        /// box fetches the weights from a CDN instead of receiving them over
        /// this machine's uplink. Overrides the workspace's `models_release`
        /// for this run; unset reads that setting, and an unset setting pushes.
        #[arg(long)]
        release_repo: Option<String>,
    },
    /// Live cluster dashboard (talks to a running inductor API).
    Tui {
        /// Inductor API base URL.
        #[arg(long, default_value = "http://127.0.0.1:8901")]
        api: String,
        /// Print one plain-text snapshot and exit instead of drawing the
        /// dashboard. Needs no terminal, so it works with screen readers,
        /// `watch(1)` and shell pipelines.
        #[arg(long)]
        once: bool,
    },
    /// Voice roster maintenance. Local only, touches no worker.
    Roster {
        #[command(subcommand)]
        cmd: RosterCmd,
    },
    /// Segment inventory across the cluster: per chapter per machine, diffed
    /// against what the scripts+cast actually require. Report-only by default;
    /// `--collect` pulls what remotes hold that this inductor lacks.
    Segments {
        /// Only these machines (address or link name), instead of all linked.
        #[arg(long)]
        from: Vec<String>,
        /// Pull remote-held segments the inductor lacks into `data/audio/`.
        #[arg(long)]
        collect: bool,
        /// Delete local files no chapter expects (stale voices). DESTRUCTIVE:
        /// prints every path as it deletes. Without this flag the command
        /// only reports.
        #[arg(long)]
        prune: bool,
        /// Report what `--collect` would pull, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rewrite written-out non-verbal sounds into engine tags across every
    /// script (`Ha ha ha!` → `[cười]`), and requeue the chapters it touches.
    /// Posts to a running inductor, which queues it behind the digests and
    /// renders the rewrite would disturb. Report-only with `--dry-run`.
    Retag {
        /// Inductor API base URL.
        #[arg(long, default_value = "http://127.0.0.1:8901")]
        api: String,
        /// Show every change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Ask the analyzer for ONE chapter and print what it said.
    ///
    /// This is the digest stage's question with none of its consequences: no
    /// render is queued, no merge runs, no ledger task is touched, and the
    /// bible is not merged. Nothing is written at all unless `--write` is
    /// passed, and even then it is only the script file, the same function the
    /// digest worker calls, so what you read here is exactly what a run would
    /// have produced.
    Digest {
        /// Chapter number (needs `data/chapters/chNN.txt`).
        chapter: u32,
        /// Analyzer to use. Default: the active provider in `.bm/llm.json` (TUI: `L`).
        #[arg(long)]
        analyzer: Option<String>,
        /// Also write `data/script-NN.json`. Nothing else happens: caches are
        /// NOT invalidated and nothing is requeued, so a hand-written script
        /// can disagree with segments already on disk.
        #[arg(long)]
        write: bool,
        /// Print the whole script object instead of a one-line summary.
        #[arg(long)]
        json: bool,
    },
    /// Fetch chapters into `data/chapters/` without a cluster.
    ///
    /// The worker's own crawl, run here: the workspace's crawler, the chapter
    /// index built the way a run builds it, each chapter written to
    /// `data/chapters/chNN.txt` — the file every later stage reads, so a
    /// chapter fetched this way is a chapter the next serve offers digest and
    /// render for. An EPUB crawl needs no network and no worker at all: the
    /// book is already on this machine, and the crawler is a lookup into it.
    Crawl {
        /// First chapter.
        #[arg(long, default_value_t = 1)]
        start: u32,
        /// How many chapters.
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// Rebuild the chapter index even when the fingerprint matches.
        #[arg(long)]
        force: bool,
    },
    /// Backup digestor: run the digest here, while the cluster's digest has no
    /// quota, and hand every accepted chapter to a running inductor.
    ///
    /// **The worker's own digest, not a second one.** Round 1 renders
    /// `build_attribution_prompt`, round 2 renders `build_staging_prompt`, and
    /// the answers are checked by the same validators the automatic path uses,
    /// with a model standing where the operator's pastes would be. Each
    /// finished chapter is reported to `POST /api/complete` under the reserved
    /// `operator` id, so the ledger, the bible and the cast move exactly as
    /// they do for a worker, and a race resolves the way the manual digest's
    /// does: the report wins. A running inductor is a **precondition**, as it
    /// is for `tui`.
    Backup {
        /// First chapter. Default: the one after the last digested chapter
        /// the only chapter a digest may start from, since each chapter's bible
        /// delta lands on its predecessor's.
        start: Option<u32>,
        /// Last chapter, inclusive. Default: the last chapter the ledger knows
        /// about, so a bare `backup` carries on to the end of the book.
        #[arg(long)]
        through: Option<u32>,
        /// Which service to call. Default: the active provider in
        /// `.bm/llm.json` (TUI: `L`), else read off `--api` (a URL containing
        /// `openrouter` or `googleapis`), else settings.
        #[arg(long)]
        analyzer: Option<String>,
        /// The model service's base URL, where the two digest calls go. This is
        /// **not** the inductor: the report target is this machine's own control
        /// API, `127.0.0.1:<control_port>`, unless `--inductor` says otherwise.
        ///
        /// Omitting it uses the chosen provider's own endpoint from
        /// `.bm/llm.json`. It is deliberately **not** defaulted to a service
        /// URL: a default would override whatever `L` was configured with,
        /// which is how `--analyzer tokenharbor` would end up calling
        /// OpenRouter's address with a TokenHarbor key.
        #[arg(long)]
        api: Option<String>,
        /// Where the finished chapters are reported. Default: this machine's
        /// inductor, on the port in settings. Rarely needs saying.
        #[arg(long)]
        inductor: Option<String>,
        /// The model to answer with, on whichever service the key names.
        #[arg(long)]
        model: Option<String>,
        /// How many times to re-ask a round the gate refused, with the
        /// validator's own complaint attached. The worker's path allows itself
        /// one repair; a backup digestor is a person-or-model with more
        /// patience and a chapter nobody else is racing, so it is worth more.
        /// 0 asks once and reports the refusal.
        #[arg(long, default_value_t = 3)]
        retries: u32,
        /// Analyze and print, but report nothing, a dry run of the prompts.
        #[arg(long)]
        dry_run: bool,
    },
    /// Recover the missing chapter excerpts, without re-digesting a chapter.
    ///
    /// A book digested before the excerpt field existed has scripts but no
    /// excerpts, and every chapter after it loses the one cross-chapter memory
    /// the analyzer gets. This asks the model the digest's own excerpt question
    /// — the same rule text, the same bible, the same `---PREVIOUSLY---` chain —
    /// and writes the answer back into each stored script as one field, leaving
    /// the segments, the cast and the speakers untouched.
    ///
    /// Chapters are filled **in order**, so each recovered excerpt feeds the
    /// next chapter's `---PREVIOUSLY---` block: the run repairs the chain and
    /// the backfill, not only the field. Nothing is reported to an inductor and
    /// no worker is touched, so it runs with the cluster down.
    Excerpts {
        /// First chapter.
        #[arg(long, default_value_t = 1)]
        start: u32,
        /// Last chapter, inclusive. Default: the highest chapter text on disk.
        #[arg(long)]
        through: Option<u32>,
        /// Which service to call. Default: the active provider in
        /// `.bm/llm.json` (TUI: `L`), else read off `--api`.
        #[arg(long)]
        analyzer: Option<String>,
        /// The model service's base URL. Omitting it uses the chosen provider's
        /// own endpoint from `.bm/llm.json`.
        #[arg(long)]
        api: Option<String>,
        /// The model to answer with, on whichever service the key names.
        #[arg(long)]
        model: Option<String>,
        /// Re-ask a chapter whose answer came back empty, with a complaint
        /// attached. Only an empty answer is repaired; a short or long one is
        /// squeezed into the field, as the digest does.
        #[arg(long, default_value_t = 1)]
        retries: u32,
        /// Re-ask every chapter in the range, including the ones that already
        /// hold an excerpt.
        #[arg(long)]
        force: bool,
        /// Ask and print, but write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check a link before you build a workspace around it: one request, and a
    /// verdict on whether a crawl of that page would produce a chapter.
    ///
    /// **Paste a URL and find out in a second**, rather than finding out from
    /// ten workers failing at once. A site behind a bot check costs a cluster an
    /// afternoon to discover; this costs one request.
    ///
    /// Reads the active workspace's `crawl.user_agent` and `crawl.headers` when
    /// they are set, so a check goes out exactly as a real crawl would, a
    /// session cookie you are relying on is part of what is being checked, and
    /// `bm-inductor check` is how you confirm the cookie still works. Writes
    /// nothing, and exits non-zero when the page is not a chapter, so a setup
    /// script can gate on it.
    Check {
        /// The URL to check. A chapter, not a book index: the question is whether
        /// a *chapter* comes back.
        url: String,
        /// Per-request timeout.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Link a machine by name: remembers how to reach it so `provision --box`
    /// needs no flags. Writes `.bm/machines.json`, which is ignored.
    Link {
        /// Short handle, e.g. `box-1`.
        #[arg(long)]
        name: String,
        /// Machine address (IP or hostname).
        #[arg(long)]
        addr: String,
        /// SSH user.
        #[arg(long, default_value = "thang")]
        user: String,
        /// SSH port.
        #[arg(long, default_value = "22")]
        port: u16,
        /// SSH key path.
        #[arg(long)]
        key: Option<String>,
    },
    /// Workspaces: one directory per book under `workspaces/`, holding its
    /// settings, ledger, data and output. Switching only moves the
    /// `.bm/active-workspace` pointer, nothing is wiped, nothing is mixed.
    /// Local only, touches no worker.
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// AWS worker pool: the IAM user this app runs as, the definition written
    /// once in `.bm/aws.json`, and what the account actually holds. Local only
    ///, starts nothing.
    Aws {
        #[command(subcommand)]
        cmd: AwsCmd,
    },
    /// Asset composition: fold the assets this one depends on into the live
    /// tree, and record what was inherited so a re-resolve can withdraw it.
    /// Local only, touches no worker.
    Asset {
        #[command(subcommand)]
        cmd: AssetCmd,
    },
    /// Profile releases: one bundle per piece, and the manifest that says what
    /// it was built against. Local only, touches no worker.
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum ProfileCmd {
    /// Print the manifest for one piece of the live tree, as JSON.
    ///
    /// `tools/profile.sh pack` stages the trees and writes this to
    /// `manifest.json`; the *staleness* gate lives here rather than there
    /// because it needs the composition record, which the assets own.
    Manifest {
        /// Release name (`xianxia`).
        name: String,
        /// Which piece: `pack` or `adapter`.
        #[arg(long, default_value = "pack")]
        piece: String,
        #[arg(long, default_value = "1")]
        version: String,
        /// Pack even when a dependency this asset was built on has moved.
        #[arg(long)]
        force: bool,
        /// Release a dependency of the live pack itself (`assets/_extends/<name>`
        /// unpacked to `assets/`) rather than the live composition: the
        /// sanitized, self-contained root pack.
        #[arg(long)]
        dep: bool,
    },
    /// What this checkout's adapter, the binding and the engine say about each
    /// other, and whether a run will cook this language at all.
    ///
    /// The same verdict the scheduler gates on (`Inner::voice_gate`) and the
    /// same one `serve` warns about — asked *before* a run, which is the whole
    /// point: the alternative today is inferring it from a chapter that has
    /// been sitting `Pending` for an hour with an idle cluster beside it.
    ///
    /// **The exit status is the answer**, so a script can gate on it: `ok`
    /// prints and exits 0, anything else prints every fact it read and then
    /// fails with the consequence. Read-only — nothing is re-stamped and no
    /// file is written, so it is safe to run with the cluster up.
    Check,
    /// Bring every dependency of the live composition up to its newest release.
    ///
    /// What it reads is the **closure**, not the list: the live
    /// `assets/pack.json` names the dependencies, and each dependency's own
    /// `pack.json` names its parents, which are walked too. Doing that by hand
    /// is what leaves a checkout running a parent nobody remembered to re-fetch,
    /// and a parent of a parent is one nobody notices at all.
    ///
    /// Two comparisons decide everything, and neither of them is a guess. A
    /// dependency already at the newest release is **not downloaded** — the
    /// composition record says which release each tree came from, and a version
    /// that has not changed is content that has not changed, because a tag is cut
    /// once. A dependency whose tree has been edited here since the last resolve
    /// is **refused** rather than overwritten: `--force` is the operator saying
    /// they meant it.
    ///
    /// Nothing is replaced until every fetch has verified, so a corrupt release
    /// leaves the tree exactly as it was rather than half-updated; a dependency
    /// with no release at all, or a loop in the graph, is a refusal for the same
    /// reason. `--dry-run` asks the release list and writes nothing.
    Update {
        /// Report the plan and write nothing. Reads the release list, so it
        /// needs the network; it downloads no bundle.
        #[arg(long)]
        dry_run: bool,
        /// Replace a dependency whose tree has been edited here since the last
        /// resolve, instead of refusing.
        #[arg(long)]
        force: bool,
        /// The repo hosting the releases (`owner/name`). Defaults to
        /// `settings.json`'s `packs_release`, the same setting a provisioned box
        /// resolves its URL from.
        #[arg(long)]
        repo: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum AssetCmd {
    /// Fold every dependency named in `assets/pack.json` into the live tree,
    /// weakest first: a key or a file the asset already ships wins, a missing
    /// one is filled in from the dependency, and everything the *last* resolve
    /// put there is withdrawn first — so a parent that has since dropped a
    /// sound does not leave it behind for good.
    ///
    /// Run it after editing a dependency, or after unpacking one under
    /// `assets/_extends/`. It writes only what changed (a registry is rewritten
    /// entry by entry, so a resolve touching one sound diffs as one sound), and
    /// it names any dependency that has moved since the last resolve — which is
    /// what makes a child visibly stale rather than quietly out of date.
    Resolve {
        /// Report what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum AwsCmd {
    /// Print the pool definition, what is still missing, and the one-off setup
    /// commands it needs. Reads no network.
    Show,
    /// Print the least-privilege policy for this app's IAM user, and the
    /// commands that create that user and attach it. Reads no network.
    ///
    /// The policy is the tracked `aws-policy.json`, one document, used by both
    /// this command and `aws iam put-user-policy`, so what you read and what
    /// you install cannot drift apart.
    Policy,
    /// Write a `.bm/aws.json` template to fill in. Refuses to overwrite one
    /// that exists unless --force.
    Init {
        /// Overwrite an existing pool definition.
        #[arg(long)]
        force: bool,
    },
    /// List the boxes this tool started, straight from the account.
    ///
    /// The credential and region check: if this answers, `up` can too.
    Ls,
    /// Store the credentials of the IAM user created for this app, then verify
    /// them against the account.
    ///
    /// **An IAM user, and nothing else.** The identity is checked with
    /// `sts get-caller-identity` and refused unless it is a `:user/` ARN, so a
    /// root key or an assumed role cannot be stored, those are the identities
    /// a dedicated user exists to replace.
    ///
    /// The secret is read from stdin and **never** taken as an argument:
    /// `argv` is visible in `ps` on every box it was typed on. Piped input
    /// works, so a script can do
    /// `printf '%s\\n' "$SECRET" | bm-inductor aws login --access-key-id AKIA…`.
    Login {
        #[command(flatten)]
        args: aws_ops::LoginArgs,
    },
    /// Fill in the pool fields a console page cannot hand you as a copy-paste:
    /// the AMI, the default network, and the instance profile.
    ///
    /// Everything it learns is written into `.bm/aws.json` **and printed**, so
    /// what was chosen stays visible and reviewable instead of being
    /// re-resolved on every launch. Read-only calls; writes nothing outside
    /// `.bm/`. Run it once, after `aws login`.
    Discover {
        #[command(flatten)]
        args: aws_ops::DiscoverArgs,
    },
    /// Launch boxes and leave them running, tagged, ready to provision.
    ///
    /// `--dry-run` prints the exact `aws ec2 run-instances` call and stops
    /// the review step before anything costs money.
    Up {
        /// How many. Refused if it would take the pool past `max_workers`.
        #[arg(long, default_value = "1")]
        count: u32,
        /// Print the call instead of making it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Terminate the boxes carrying our marker tag.
    ///
    /// Resolves the ids from the tag first and prints them, so the destructive
    /// step is always over a list someone could read.
    Down {
        /// Print what would be terminated instead of doing it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum WorkspaceCmd {
    /// Create `workspaces/<name>/` with default settings stamped to the
    /// loaded profile, and switch to it.
    New {
        /// Workspace name, e.g. `beyond-myriads`.
        name: String,
        /// A profile preset from `profiles/presets.json`, binding THIS
        /// workspace to its pack × adapter × engine triple instead of
        /// inheriting the checkout's loaded profile. A preset with `pack_deps`
        /// composes the workspace's own pack from those roots, so two books on
        /// one checkout do not share a score; the checkout's `.bm/profile` is
        /// never touched.
        #[arg(long)]
        profile: Option<String>,
        /// A crawler to seed the new workspace with. Not a CLI flag: the guided
        /// create flow (the dashboard's `:workspace` → `new`) builds this, and
        /// the CLI keeps the preset's own `crawler` as its whole story.
        #[arg(skip)]
        crawler: Option<bm_core::preset::CrawlerSetup>,
    },
    /// Switch the pointer to an existing workspace. Data follows the
    /// directory, so selecting never wipes.
    Use {
        /// Workspace name.
        name: String,
    },
    /// Give an existing workspace its own copy of the preset material it would
    /// otherwise borrow — the adapter's `prompts/` and `crawl/`. Idempotent and
    /// additive: it copies what the workspace does not already own and never
    /// overwrites what it has, so a book's edited prompts cannot be clobbered.
    /// Voices are not migrated: they are not a preset yet and are not shared.
    Migrate {
        /// Workspace name.
        name: String,
    },
    /// List workspaces, marking the active one.
    List,
}

#[derive(Subcommand)]
pub(crate) enum RosterCmd {
    /// Rewrite the cast files to store catalogue keys instead of display names,
    /// keeping a `.bak` of each.
    ///
    /// Not a prerequisite for anything: the reader accepts both forms and the
    /// writer keys the file on its next save. This does it now, and shows what
    /// changed.
    MigrateCast {
        /// Report what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Add a sample clip to the voice pool: copies it under `refs/`, takes its
    /// tags from the filename (`young-female-4.mp3` → young, female), registers
    /// it in `voice-pool.json`, maps it in `voices.json` so the next provision
    /// enrolls it on workers, and enrolls it into this machine's own store
    /// right away when it has one, so renders use it immediately.
    AddSample {
        /// Clip to add (mp3/wav/m4a/ogg/flac).
        path: std::path::PathBuf,
        /// Override the filename tags: `--tags young,female`.
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Voice name to register under (default: the file stem). Without
        /// `--tags` the voice stays private: assignable by hand, never
        /// auto-rolled. `refs/narrator.mp3 --name Narrator` voices as `Narrator`.
        #[arg(long)]
        name: Option<String>,
    },
}
