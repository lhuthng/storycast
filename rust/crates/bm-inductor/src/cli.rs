use super::*;

#[derive(Parser)]
#[command(
    name = "bm-inductor",
    about = "Cluster orchestrator for the novel pipeline"
)]
pub(crate) struct Cli {
    /// Repo root (discovered when omitted: `rust/Cargo.toml` in a checkout,
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
        #[arg(long)]
        release_repo: Option<String>,
    },
    /// Live cluster dashboard (talks to a running inductor API).
    Tui {
        /// Inductor API base URL.
        #[arg(long, default_value = "http://127.0.0.1:8901")]
        api: String,
        /// Print one plain-text snapshot and exit instead of drawing the
        #[arg(long)]
        once: bool,
    },
    /// Voice roster maintenance. Local only, touches no worker.
    Roster {
        #[command(subcommand)]
        cmd: RosterCmd,
    },
    /// Segment inventory across the cluster: per chapter per machine, diffed
    Segments {
        /// Only these machines (address or link name), instead of all linked.
        #[arg(long)]
        from: Vec<String>,
        /// Pull remote-held segments the inductor lacks into `data/audio/`.
        #[arg(long)]
        collect: bool,
        /// Delete local files no chapter expects (stale voices). DESTRUCTIVE:
        #[arg(long)]
        prune: bool,
        /// Report what `--collect` would pull, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rewrite written-out non-verbal sounds into engine tags across every
    Retag {
        /// Inductor API base URL.
        #[arg(long, default_value = "http://127.0.0.1:8901")]
        api: String,
        /// Show every change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Ask the analyzer for ONE chapter and print what it said.
    Digest {
        /// Chapter number (needs `data/chapters/chNN.txt`).
        chapter: u32,
        /// Analyzer to use. Default: the active provider in `.bm/llm.json` (TUI: `L`).
        #[arg(long)]
        analyzer: Option<String>,
        /// Also write `data/script-NN.json`. Nothing else happens: caches are
        #[arg(long)]
        write: bool,
        /// Print the whole script object instead of a one-line summary.
        #[arg(long)]
        json: bool,
    },
    /// Fetch chapters into `data/chapters/` without a cluster.
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
    Backup {
        /// First chapter. Default: the one after the last digested chapter
        start: Option<u32>,
        /// Last chapter, inclusive. Default: the last chapter the ledger knows
        #[arg(long)]
        through: Option<u32>,
        /// Which service to call. Default: the active provider in
        #[arg(long)]
        analyzer: Option<String>,
        /// The model service's base URL, where the two digest calls go. This is
        #[arg(long)]
        api: Option<String>,
        /// Where the finished chapters are reported. Default: this machine's
        #[arg(long)]
        inductor: Option<String>,
        /// The model to answer with, on whichever service the key names.
        #[arg(long)]
        model: Option<String>,
        /// How many times to re-ask a round the gate refused, with the
        #[arg(long, default_value_t = 3)]
        retries: u32,
        /// Analyze and print, but report nothing, a dry run of the prompts.
        #[arg(long)]
        dry_run: bool,
    },
    /// Recover the missing chapter excerpts, without re-digesting a chapter.
    Excerpts {
        /// First chapter.
        #[arg(long, default_value_t = 1)]
        start: u32,
        /// Last chapter, inclusive. Default: the highest chapter text on disk.
        #[arg(long)]
        through: Option<u32>,
        /// Which service to call. Default: the active provider in
        #[arg(long)]
        analyzer: Option<String>,
        /// The model service's base URL. Omitting it uses the chosen provider's
        #[arg(long)]
        api: Option<String>,
        /// The model to answer with, on whichever service the key names.
        #[arg(long)]
        model: Option<String>,
        /// Re-ask a chapter whose answer came back empty, with a complaint
        #[arg(long, default_value_t = 1)]
        retries: u32,
        /// Re-ask every chapter in the range, including the ones that already
        #[arg(long)]
        force: bool,
        /// Ask and print, but write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check a link before you build a workspace around it: one request, and a
    Check {
        /// The URL to check. A chapter, not a book index: the question is whether
        url: String,
        /// Per-request timeout.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Link a machine by name: remembers how to reach it so `provision --box`
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
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// AWS worker pool: the IAM user this app runs as, the definition written
    Aws {
        #[command(subcommand)]
        cmd: AwsCmd,
    },
    /// Asset composition: fold the assets this one depends on into the live
    Asset {
        #[command(subcommand)]
        cmd: AssetCmd,
    },
    /// Profile releases: one bundle per piece, and the manifest that says what
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum ProfileCmd {
    /// Print the manifest for one piece of the live tree, as JSON.
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
        #[arg(long)]
        dep: bool,
    },
    /// What this checkout's adapter, the binding and the engine say about each
    Check,
    /// Bring every dependency of the live composition up to its newest release.
    Update {
        /// Report the plan and write nothing. Reads the release list, so it
        #[arg(long)]
        dry_run: bool,
        /// Replace a dependency whose tree has been edited here since the last
        #[arg(long)]
        force: bool,
        /// The repo hosting the releases (`owner/name`). Defaults to
        #[arg(long)]
        repo: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum AssetCmd {
    /// Fold every dependency named in `assets/pack.json` into the live tree,
    Resolve {
        /// Report what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum AwsCmd {
    /// Print the pool definition, what is still missing, and the one-off setup
    Show,
    /// Print the least-privilege policy for this app's IAM user, and the
    Policy,
    /// Write a `.bm/aws.json` template to fill in. Refuses to overwrite one
    Init {
        /// Overwrite an existing pool definition.
        #[arg(long)]
        force: bool,
    },
    /// List the boxes this tool started, straight from the account.
    Ls,
    /// Store the credentials of the IAM user created for this app, then verify
    Login {
        #[command(flatten)]
        args: aws_ops::LoginArgs,
    },
    /// Fill in the pool fields a console page cannot hand you as a copy-paste:
    Discover {
        #[command(flatten)]
        args: aws_ops::DiscoverArgs,
    },
    /// Launch boxes and leave them running, tagged, ready to provision.
    Up {
        /// How many. Refused if it would take the pool past `max_workers`.
        #[arg(long, default_value = "1")]
        count: u32,
        /// Print the call instead of making it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Terminate the boxes carrying our marker tag.
    Down {
        /// Print what would be terminated instead of doing it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum WorkspaceCmd {
    /// Create `workspaces/<name>/` with default settings stamped to the
    New {
        /// Workspace name, e.g. `beyond-myriads`.
        name: String,
        /// A profile preset from `profiles/presets.json`, binding THIS
        #[arg(long)]
        profile: Option<String>,
        /// A crawler to seed the new workspace with. Not a CLI flag: the guided
        #[arg(skip)]
        crawler: Option<bm_core::preset::CrawlerSetup>,
    },
    /// Switch the pointer to an existing workspace. Data follows the
    Use {
        /// Workspace name.
        name: String,
    },
    /// Give an existing workspace its own copy of the preset material it would
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
    MigrateCast {
        /// Report what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Add a sample clip to the voice pool: copies it under `refs/`, takes its
    AddSample {
        /// Clip to add (mp3/wav/m4a/ogg/flac).
        path: std::path::PathBuf,
        /// Override the filename tags: `--tags young,female`.
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Voice name to register under (default: the file stem). Without
        #[arg(long)]
        name: Option<String>,
    },
}
