//! What a provider says a box is doing, normalised, and the one pure verdict
//! the reaper builds from it.

/// The provider's answer, in words every provider shares.
///
/// A provider maps its own states here and keeps the original word in
/// [`ProviderView::raw`]; nothing outside a provider matches on that word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The box exists and whatever was started on it runs.
    Running,
    /// The box exists and nothing of ours runs on it: `stopped`, `paused`.
    Stopped,
    /// On its way out. `reclaim_at` says when, when the provider says.
    Stopping,
    /// Winding down for good.
    Dead,
    /// The provider no longer has it.
    Gone,
    /// The poll failed: no opinion, which is never a verdict.
    Unknown,
}

/// One box as its provider described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderView {
    pub provider_id: String,
    pub state: Liveness,
    /// The provider's own state word, verbatim.
    pub raw: String,
    /// The provider's own explanation when it gives one (`idle-paused`,
    /// `max-run`, a spot interruption notice).
    pub detail: Option<String>,
    /// When the provider says the box goes, when it says.
    pub reclaim_at: Option<u64>,
}

impl ProviderView {
    pub fn new(provider_id: impl Into<String>, state: Liveness, raw: impl Into<String>) -> Self {
        ProviderView {
            provider_id: provider_id.into(),
            state,
            raw: raw.into(),
            detail: None,
            reclaim_at: None,
        }
    }

    pub fn saying(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn reclaim_at(mut self, at: Option<u64>) -> Self {
        self.reclaim_at = at;
        self
    }

    /// The shortest phrase that says why this box is not usable.
    pub fn why(&self) -> String {
        if let Some(detail) = &self.detail {
            return detail.clone();
        }
        if !self.raw.is_empty() {
            return self.raw.clone();
        }
        "gone".to_string()
    }
}

/// How long a beat stays evidence of life, the same window the lease reaper
/// uses before it calls a worker silent.
pub const BEAT_WINDOW_SECS: u64 = 90;

/// What to do about one box, built only from what was observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fate {
    /// Leave it alone.
    Keep,
    /// Alive as far as the provider knows, silent to us: mark `Offline`, never
    /// delete. `since` is when the last beat stopped counting.
    Suspect { since: u64 },
    /// Its processes are gone and it is not coming back.
    Dead { why: String },
}

/// The verdict: the provider's word, our last beat, and the box's own deadline.
///
/// Order matters. The deadline is ours and is conclusive, so it is read first;
/// a failed poll is only ever `Suspect`, because a poll that did not answer is
/// not evidence that anything died.
pub fn fate(
    view: &ProviderView,
    last_beat: Option<u64>,
    expires_at: Option<u64>,
    now: u64,
) -> Fate {
    if expires_at.is_some_and(|at| at <= now) {
        return Fate::Dead {
            why: "past its deadline".to_string(),
        };
    }
    let beat = |since: u64| Fate::Suspect { since };
    match view.state {
        Liveness::Running => match last_beat {
            Some(b) if now.saturating_sub(b) < BEAT_WINDOW_SECS => Fate::Keep,
            // Stale by `since`, which is one window after the last beat.
            Some(b) => beat(b.saturating_add(BEAT_WINDOW_SECS)),
            None => beat(now),
        },
        Liveness::Stopping => match view.reclaim_at {
            // Still inside the window the provider gave: it may yet report.
            Some(at) if at > now => Fate::Keep,
            Some(_) => Fate::Dead { why: view.why() },
            None => Fate::Dead { why: view.why() },
        },
        Liveness::Stopped | Liveness::Dead | Liveness::Gone => Fate::Dead { why: view.why() },
        Liveness::Unknown => beat(now),
    }
}
