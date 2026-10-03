use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextKind {
    AddMachine,
    /// Pooled sample: tags come from the filename, the voice auto-rolls.
    AddSample,
    /// Named voice (`path as Name`): manual assignment only, never rotates.
    AddNamed,
    /// Run-config editor (opened with `e` on the run screen): saves range,
    /// analyzer and model chain to the settings file. Launches nothing.
    RunConfig,
    /// App-wide ssh defaults (`:sshkey`, `:sshuser`, `:sshport`): save-only
    /// prompts in the `RunConfig` style — persist to settings.json, dispatch
    /// nothing.
    SshKey,
    SshUser,
    SshPort,
    /// The address workers should dial (`:advertise`): save-only, like the ssh
    /// defaults. Empty clears it back to the routing-table guess.
    Advertise,
    /// GitHub `owner/name` hosting the model artifact (`:release`): save-only.
    /// Empty means the weights are pushed to each box instead of fetched from
    /// a release, which is what every box did before the setting existed.
    ModelsRelease,
    /// GitHub `owner/name` hosting the profile pack (`:packrelease`): save-only.
    /// Empty means `assets/` is pushed to each box instead of fetched, which is
    /// what every box did before the setting existed. The *tag* is not asked for
    /// here — it comes from the loaded profile's version.
    PacksRelease,
    /// Render batch size (`:batch`): how many of one chapter's takes a single
    /// offer carries. Save-only, like the ssh defaults — it is read by the
    /// scheduler when it builds the next offer, so nothing is dispatched.
    RenderBatch,
    /// One box's TTS sidecar thread count (`:threads`): the ONNX threads its
    /// sidecar is launched with. A number sets it, empty clears the override
    /// back to the sidecar's own default. Dispatched to the API — config, so it
    /// persists in `machines.json` and converges on the box. One model behind a
    /// mutex, so more threads do not run two lines at once.
    TtsThreads,
    /// Mix levels (`:mix`): story speed plus the two layer volumes, saved to
    /// the settings file like the run config. Launches nothing.
    Mix,
    /// One sound-design pool entry (`:sound` → `a`): the whole entry as a
    /// `key=value` line, saved to that layer's registry. Launches nothing.
    SoundAdd(bm_core::audio_pool::PoolKind),
    /// The same line for an entry already in the pool, carrying its name: the
    /// name is the registry's key, so the prompt has to know which key it is
    /// rewriting rather than reading it out of the buffer.
    SoundEdit(bm_core::audio_pool::PoolKind, String),
    /// One pooled sound's own trim. Empty clears it.
    SoundLevel(bm_core::audio_pool::PoolKind, String),
    Translate,
    CrawlTemplate,
    /// `:import` — `<chapter> <path>`: text the operator supplies instead of a
    /// fetch. A **path**, not a paste: a terminal delivers a dropped file as its
    /// path, and a chapter pasted into a single-line prompt would submit on the
    /// first newline (so a whole chapter is `:import` over the API, or saved to
    /// a file first).
    Import,
    /// `:workspace` — list, switch or create. Switching only moves the
    /// `.bm/active-workspace` pointer, but the ledger, settings and data the
    /// running cluster reads all move with it, so the dispatch is gated on a
    /// quiet cluster.
    Workspace,
    /// `:profile` — list bundles, load one (unpack) or pack the live tree.
    /// Loading replaces `assets/` + `prompts/`, which workers are reading.
    Profile,
    /// `:login` — hand over the console's `accessKeys.csv`. The secret is
    /// never typed here, so the CSV is the whole prompt.
    AwsLogin,
    /// `:discover` — the flags for one account read, exactly as the CLI takes
    /// them (same clap definition), with any path tilde-expanded.
    AwsDiscover,
    /// `:` command line: the buffer names a key (`m`) or a word
    /// (`reconcile`) and Enter presses it for you. Never dispatched —
    /// handled inline so one keypress can open another prompt.
    Command,
    /// One LLM provider's secret key (`L` → `k`): typed, never displayed.
    LlmKey(String),
    /// One provider's base URL (`L` → `u`).
    LlmUrl(String),
    /// One provider's model name (`L` → `m`): typed, or picked from the
    /// fetched list with `f` then `Enter`.
    LlmModel(String),
}

/// A single-line editor with a real cursor. The old prompt could only append
/// and backspace; a mistyped address meant starting over.
#[derive(Debug, Clone)]
pub(crate) struct TextPrompt {
    pub(crate) kind: TextKind,
    pub(crate) title: String,
    pub(crate) hint: String,
    pub(crate) buf: String,
    /// Cursor position in *characters*, never bytes — the data is Vietnamese.
    pub(crate) cursor: usize,
}

impl TextPrompt {
    pub(crate) fn new(kind: TextKind, title: &str, hint: &str, initial: &str) -> Self {
        let buf = initial.to_string();
        let cursor = buf.chars().count();
        TextPrompt {
            kind,
            title: title.to_string(),
            hint: hint.to_string(),
            buf,
            cursor,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.chars().count()
    }

    /// The site the buffer names, if this prompt takes a URL and the buffer
    /// holds one we have a crawler for.
    ///
    /// Recomputed from the buffer on every keystroke rather than cached, because
    /// the answer is a function of what is on screen and a stale answer is the
    /// kind of lie this whole module exists to avoid. It is `None` for every
    /// other prompt and for a URL we have nothing on, so the caller can simply
    /// ask.
    pub(crate) fn known_site(&self) -> Option<&'static bm_core::crawl::KnownSite> {
        if self.kind != TextKind::CrawlTemplate {
            return None;
        }
        // A template with a `{n}` in it is not a site URL — it is already a
        // mapping, and matching one against the registry would only ever match a
        // registry entry that happens to contain the same host, which says
        // nothing about whether the template is the right one for the site.
        if self.buf.contains("{n}") {
            return None;
        }
        bm_core::crawl::for_url(&self.buf)
    }

    /// The note to show under the prompt for a recognised URL: which crawler,
    /// what shape it is written against, and the one thing to know first.
    ///
    /// The paste block is deliberately **not** included. This is an 88-column
    /// dialog, not a terminal, and ten lines of JSON in it would push the input
    /// line itself off the top of a laptop screen. `bm-inductor check` prints
    /// the block for the person who wants to copy it.
    pub(crate) fn known_note(&self) -> Option<String> {
        let site = self.known_site()?;
        let mut s = format!("known site · {} · ", site.host);
        if site.is_crawlable() {
            s.push_str(&format!("crawler {}", site.script));
        } else {
            s.push_str("no bundled crawler");
        }
        s.push_str(&format!(" · {}", site.language));
        if site.url_template.is_empty() && site.is_crawlable() {
            s.push_str(" · no {n} in its URLs: submit empty and set crawl.script");
        }
        if !site.language.starts_with("Vietnamese") {
            // Said here, and only here: a Vietnamese G2P applied to another
            // language does not fail, it mispronounces, and the person watching
            // a hundred renders is the only one who can tell.
            s.push_str(&format!(
                "\nheads up · the voices and the G2P are Vietnamese, so this {} text will \
                 be pronounced against Vietnamese syllable rules — expect it to sound wrong, \
                 not to error.",
                site.language
            ));
        }
        if let Some(c) = site.caveat {
            s.push('\n');
            s.push_str(c);
        }
        Some(s)
    }

    pub(crate) fn byte_at(&self, char_idx: usize) -> usize {
        self.buf
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.buf.len())
    }

    pub(crate) fn insert(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.buf.insert(b, c);
        self.cursor += 1;
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let (a, b) = (self.byte_at(self.cursor - 1), self.byte_at(self.cursor));
        self.buf.replace_range(a..b, "");
        self.cursor -= 1;
    }

    pub(crate) fn delete(&mut self) {
        if self.cursor >= self.len() {
            return;
        }
        let (a, b) = (self.byte_at(self.cursor), self.byte_at(self.cursor + 1));
        self.buf.replace_range(a..b, "");
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(crate) fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.len();
    }

    pub(crate) fn kill_to_start(&mut self) {
        let b = self.byte_at(self.cursor);
        self.buf.replace_range(..b, "");
        self.cursor = 0;
    }

    pub(crate) fn kill_word(&mut self) {
        while self.cursor > 0 {
            let prev = self.buf.chars().nth(self.cursor - 1).unwrap_or(' ');
            if prev.is_whitespace() {
                self.backspace();
            } else {
                break;
            }
        }
        while self.cursor > 0 {
            let prev = self.buf.chars().nth(self.cursor - 1).unwrap_or(' ');
            if prev.is_whitespace() {
                break;
            }
            self.backspace();
        }
    }

    /// The buffer split at the cursor, for rendering a visible caret.
    pub(crate) fn split(&self) -> (String, String) {
        let b = self.byte_at(self.cursor);
        (self.buf[..b].to_string(), self.buf[b..].to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickStage {
    Character,
    Voice,
}

#[derive(Debug, Clone)]
pub(crate) struct Picker {
    pub(crate) stage: PickStage,
    /// Chosen in step 1; empty until then.
    pub(crate) character: String,
    pub(crate) filter: String,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    /// Voices auditioned this session, so the operator can tell them apart
    /// from ones merely read about.
    pub(crate) previewed: Vec<String>,
    /// The real line the current and candidate voice are A/B'd on. Held here so
    /// both auditions speak the same sentence; re-picked when the character
    /// changes or the operator asks for another.
    pub(crate) line: Option<AuditionLine>,
    /// Filter focus on step 2: every letter types (t/T included) and the
    /// audition keys go quiet. Off by default — the screen opens in audition
    /// focus, where t/T/^T play and any other letter focuses the filter.
    /// Step 1 ignores it: picking a character needs every letter.
    pub(crate) filter_focus: bool,
}

impl Picker {
    pub(crate) fn new() -> Self {
        Picker {
            stage: PickStage::Character,
            character: String::new(),
            filter: String::new(),
            cursor: 0,
            scroll: 0,
            previewed: Vec::new(),
            line: None,
            filter_focus: false,
        }
    }
}

/// One entry's removal, and the screen it was asked from.
///
/// A named struct rather than three fields on the variant: the confirmation
/// carries where to *return to* as well as what to do, and spelling that inline
/// made the whole `ConfirmAction` enum too wide to stay on one line per variant.
#[derive(Debug, Clone)]
pub(crate) struct SoundRemoval {
    pub(crate) layer: bm_core::audio_pool::PoolKind,
    pub(crate) name: String,
    pub(crate) view: crate::tui::sound::SoundView,
}

#[derive(Debug, Clone)]
pub(crate) enum ConfirmAction {
    Quit,
    /// Terminate the named EC2 instance ids. Always explicit ids, never a
    /// filter: the destructive step is over a list the operator just read.
    AwsDown {
        ids: Vec<String>,
    },
    Provision {
        addr: String,
        force: bool,
    },
    DropMachine {
        addr: String,
    },
    /// Re-point a machine whose EC2 address drifted at the address the box
    /// carries now. Carries the old address; the instance id comes from the
    /// selected machine's note inside the job.
    RelinkMachine {
        old_addr: String,
    },
    SwapVoice {
        character: String,
        voice: String,
    },
    StopBackend,
    Reconcile,
    Rerender,
    /// Fold characters by hand: the first name survives, the rest are
    /// absorbed. Asked first like every other rewrite, then dispatched as
    /// one `merge` op.
    Merge {
        survivor: String,
        absorbed: Vec<String>,
    },
    /// Take one entry out of a sound-design pool. Carries the view it was
    /// asked from, so answering the dialog returns to the same tab and row
    /// instead of dumping the operator back on the dashboard.
    SoundRemove(SoundRemoval),
    /// Take every row one worker holds back off it. The only release that
    /// asks first: a single row is one keypress with nothing to lose, while
    /// this can be a whole box's afternoon and the operator cannot see the
    /// size of it from the row they pressed on. `list` is the ledger view it
    /// was asked from, so answering leaves the rows leaving in front of them.
    ReleaseWorker {
        worker: String,
        count: usize,
        beating: bool,
        list: TasksView,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Confirm {
    pub(crate) title: String,
    pub(crate) body: Vec<String>,
    pub(crate) action: ConfirmAction,
    pub(crate) danger: bool,
}

impl Confirm {
    /// Full re-speak behind one Enter: every render back to pending with
    /// its merge, caches deleted. Shared by `:rerender` and the Tasks
    /// screen's `E`, so the two paths cannot disagree about the cost.
    pub(crate) fn rerender() -> Self {
        Confirm {
            title: "Re-render everything?".into(),
            danger: true,
            body: vec![
                "Every render task goes back to pending, with its merge.".into(),
                "Cached segments and finished mp3s are deleted, so every".into(),
                "voice is re-synthesized from scratch — slow and costly.".into(),
                String::new(),
                "For mix-only changes (speed, volumes, effect clips) use".into(),
                ":mix instead: it requeues merges and keeps this cache.".into(),
            ],
            action: ConfirmAction::Rerender,
        }
    }

    /// Take every row one worker holds back off it — `W` on the ledger.
    ///
    /// One dialog for both cases, because the *answer* is the same; only the
    /// sentence changes, and `beating` decides it. A silent box is the ordinary
    /// case and nothing is at risk. A box that is still answering is the
    /// surprising one, and there the operator has to be told the price in the
    /// one place they can still say no: the work is not lost, it is spoken
    /// twice.
    pub(crate) fn release_worker(
        worker: String,
        count: usize,
        beating: bool,
        list: TasksView,
    ) -> Self {
        let mut body = vec![
            format!("{count} row(s) held by {worker} go back to the pool."),
            String::new(),
        ];
        if beating {
            body.push("It is still beating, so whatever it has in hand finishes and".into());
            body.push("its report lands stale — and the rows here are offered again,".into());
            body.push("so a take can be spoken twice before the ledger settles.".into());
        } else {
            body.push("It has stopped beating, so nothing is in flight: every row".into());
            body.push("returns exactly as it was — attempts kept, nothing deleted —".into());
            body.push("and is offered to the next box that asks.".into());
        }
        Confirm {
            title: format!("Release everything {worker} holds?"),
            danger: beating,
            body,
            action: ConfirmAction::ReleaseWorker {
                worker,
                count,
                beating,
                list,
            },
        }
    }
}
