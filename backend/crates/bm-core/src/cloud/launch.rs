//! The launch contract: what a provider is asked, what it answers, and the one
//! vocabulary shared code is allowed to branch on.
use crate::cloud::{Provider, ProviderView};
use bm_proto::{Machine, MachineState};

/// What *we* do with a refusal. A provider decides which of these its own code
/// means; shared code branches on it and never renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Spend nothing at all — bad credentials, a broken definition, a bad spec.
    Stop,
    /// Launch fewer than asked, and say both numbers.
    Clamp,
    /// Stop asking for this round: the provider has no room right now.
    Capacity,
    /// Not understood: show the provider's own code, words and docs untouched.
    Unknown,
}

impl Reason {
    /// A provider-free label for a status line.
    pub fn label(self) -> &'static str {
        match self {
            Reason::Stop => "refused",
            Reason::Clamp => "fewer",
            Reason::Capacity => "capacity",
            Reason::Unknown => "unknown",
        }
    }
}

/// One box (or a share of the asked-for boxes) a provider would not create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortfall {
    /// Boxes this refusal accounts for.
    pub want: u32,
    pub reason: Reason,
    /// The provider's own code, verbatim.
    pub code: String,
    /// The provider's own words, verbatim.
    pub message: String,
    /// Built by the provider from live facts; rendered untouched.
    pub fix: Option<String>,
    pub docs: Option<String>,
    pub retryable: bool,
    pub retry_after_s: Option<u64>,
}

impl Shortfall {
    pub fn new(reason: Reason, code: impl Into<String>, message: impl Into<String>) -> Self {
        Shortfall {
            want: 0,
            reason,
            code: code.into(),
            message: message.into(),
            fix: None,
            docs: None,
            retryable: false,
            retry_after_s: None,
        }
    }

    pub fn covering(mut self, want: u32) -> Self {
        self.want = want;
        self
    }

    pub fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    pub fn with_docs(mut self, docs: impl Into<String>) -> Self {
        self.docs = Some(docs.into());
        self
    }

    pub fn retry_in(mut self, secs: Option<u64>) -> Self {
        self.retryable = true;
        self.retry_after_s = secs;
        self
    }

    /// One line for the operator: the provider's code and words, then its fix.
    pub fn line(&self) -> String {
        let mut line = format!("{}: {}", self.code, self.message);
        if let Some(fix) = &self.fix {
            line.push_str(&format!(" — fix: {fix}"));
        }
        if let Some(docs) = &self.docs {
            line.push_str(&format!(" — {docs}"));
        }
        line
    }
}

/// The TTS sidecar's resident set with its weights loaded, measured on the
/// target platform (linux/x86_64, release): 2,918,708 kB `VmHWM`, plus ~30 MB
/// over a dozen renders. `models/` is 668 MB on disk, so `du` understates it.
pub const SIDECAR_RESIDENT_MIB: u32 = 2918;

/// The smallest box worth launching: the sidecar, the agent, the page cache a
/// push dirties and the OS. A 4 GiB machine leaves ~3.9 GiB usable, so it
/// cannot hold a 2.9 GiB sidecar and everything else.
pub const MIN_BOX_MIB: u32 = 8192;

/// A box too small to host the worker, refused before anything is created.
pub fn too_small(code: &str, mib: u32, fix: impl Into<String>) -> Shortfall {
    Shortfall::new(
        Reason::Stop,
        code,
        format!(
            "{mib} MiB of RAM — the worker's TTS sidecar holds {SIDECAR_RESIDENT_MIB} MiB \
             resident before its first render, and {MIN_BOX_MIB} MiB is the smallest box that \
             leaves it room for the agent and the OS"
        ),
    )
    .with_fix(fix)
}

/// What the provider checked before anything spends.
#[derive(Debug, Clone)]
pub struct Smoke {
    pub provider: Provider,
    /// Provider-built facts, printed verbatim.
    pub lines: Vec<String>,
}

/// How many boxes would fit, and why not more.
#[derive(Debug, Clone)]
pub struct Plan {
    pub want: u32,
    pub fit: u32,
    /// Boxes already live: the base the headroom was measured against.
    pub live: u32,
    /// Provider-built facts: the numbers the fit came from.
    pub notes: Vec<String>,
    pub shortfalls: Vec<Shortfall>,
}

impl Plan {
    pub fn clamped(&self) -> bool {
        self.fit < self.want
    }
}

/// A box the provider created, in terms the registry speaks.
#[derive(Debug, Clone)]
pub struct CloudBox {
    pub provider: Provider,
    pub provider_id: String,
    pub name: String,
    /// Dialable address; empty until the provider assigns one.
    pub addr: String,
    pub note: String,
    pub state: MachineState,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub key: Option<String>,
    pub expires_at: Option<u64>,
    /// A published worker URL, when the provider gives no dialable address.
    pub base_url: Option<String>,
}

impl CloudBox {
    /// What a launch line says: `instabox-1`, or `i-0abc → 18.1.2.3`.
    pub fn label(&self) -> String {
        let m = self.machine();
        if m.addr.contains("://") {
            m.handle()
        } else {
            format!("{} → {}", m.handle(), m.addr)
        }
    }

    /// The registry row this box becomes.
    pub fn machine(&self) -> Machine {
        let mut m = Machine::new(
            &self.addr,
            &self.ssh_user,
            self.ssh_port,
            self.key.clone(),
            "worker",
        );
        m.name = self.name.clone();
        m.state = self.state;
        m.note = self.note.clone();
        m.expires_at = self.expires_at;
        m.provider = Some(self.provider.as_str().to_string());
        m.provider_id = Some(self.provider_id.clone());
        m.base_url = self.base_url.clone();
        m
    }
}

/// What a launch produced. A refusal is data here, not an error: some boxes
/// exist and the rest have a reason.
#[derive(Debug, Clone)]
pub struct Launch {
    pub created: Vec<CloudBox>,
    pub skipped: Vec<Shortfall>,
}

impl Launch {
    pub fn summary(&self) -> String {
        if self.skipped.is_empty() {
            return format!("launched {}", self.created.len());
        }
        let reasons: Vec<&str> = self.skipped.iter().map(|s| s.reason.label()).collect();
        format!(
            "launched {} — {} not created ({})",
            self.created.len(),
            self.skipped.iter().map(|s| s.want).sum::<u32>(),
            reasons.join(", ")
        )
    }
}

/// One cloud, answering in the shared vocabulary.
pub trait Cloud: Send + Sync {
    fn provider(&self) -> Provider;

    /// Prove the credentials and the definition before any call that spends.
    fn smoke(&self) -> Result<Smoke, Shortfall>;

    /// What would fit, from live facts. No spend.
    fn plan(&self, want: u32) -> Result<Plan, Shortfall>;

    /// Create the boxes `plan` allows, one at a time.
    fn up(&self, plan: &Plan) -> Launch;

    /// What the provider says about these ids, one view per id it still knows.
    ///
    /// An id missing from a successful answer is `Gone`; a poll that failed is
    /// an error, never an empty list — the two mean different things and only
    /// one of them may end a box.
    fn list(&self, ids: &[String]) -> Result<Vec<ProviderView>, Shortfall>;

    /// Delete explicit provider ids. Gone already counts as success.
    fn down(&self, ids: &[String]) -> Result<Vec<String>, Shortfall>;
}
