use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextKind {
    AddMachine,
    /// Pooled sample: tags come from the filename, the voice auto-rolls.
    AddSample,
    /// Named voice (`path as Name`): manual assignment only, never rotates.
    AddNamed,
    /// Run-config editor (opened with `e` on the run screen): saves range,
    RunConfig,
    /// App-wide ssh defaults (`:sshkey`, `:sshuser`, `:sshport`): save-only
    SshKey,
    SshUser,
    SshPort,
    /// The address workers should dial (`:advertise`): save-only, like the ssh
    Advertise,
    /// GitHub `owner/name` hosting the model artifact (`:release`): save-only.
    ModelsRelease,
    /// GitHub `owner/name` hosting the profile pack (`:packrelease`): save-only.
    PacksRelease,
    /// Render batch size (`:batch`): how many of one chapter's takes a single
    RenderBatch,
    /// One box's TTS sidecar thread count (`:threads`): the ONNX threads its
    TtsThreads,
    /// Mix levels (`:mix`): story speed plus the two layer volumes, saved to
    Mix,
    /// One sound-design pool entry (`:sound` → `a`): the whole entry as a
    SoundAdd(bm_core::audio_pool::PoolKind),
    /// The same line for an entry already in the pool, carrying its name: the
    SoundEdit(bm_core::audio_pool::PoolKind, String),
    /// One pooled sound's own trim. Empty clears it.
    SoundLevel(bm_core::audio_pool::PoolKind, String),
    Translate,
    CrawlTemplate,
    /// `:import` — `<chapter> <path>`: text the operator supplies instead of a
    Import,
    /// `:workspace` — list, switch or create. Switching only moves the
    Workspace,
    /// `:profile` — list bundles, load one (unpack) or pack the live tree.
    Profile,
    /// `:login` — hand over the console's `accessKeys.csv`. The secret is
    AwsLogin,
    /// `:discover` — the flags for one account read, exactly as the CLI takes
    AwsDiscover,
    /// `:` command line: the buffer names a key (`m`) or a word
    Command,
    /// One LLM provider's secret key (`L` → `k`): typed, never displayed.
    LlmKey(String),
    /// One provider's base URL (`L` → `u`).
    LlmUrl(String),
    /// One provider's model name (`L` → `m`): typed, or picked from the
    LlmModel(String),
}

/// A single-line editor with a real cursor. The old prompt could only append
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
    pub(crate) fn known_site(&self) -> Option<&'static bm_core::crawl::KnownSite> {
        if self.kind != TextKind::CrawlTemplate {
            return None;
        }
        // A template with a `{n}` in it is not a site URL — it is already a
        if self.buf.contains("{n}") {
            return None;
        }
        bm_core::crawl::for_url(&self.buf)
    }

    /// The note to show under the prompt for a recognised URL: which crawler,
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
    pub(crate) previewed: Vec<String>,
    /// The real line the current and candidate voice are A/B'd on. Held here so
    pub(crate) line: Option<AuditionLine>,
    /// Filter focus on step 2: every letter types (t/T included) and the
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
    Merge {
        survivor: String,
        absorbed: Vec<String>,
    },
    /// Take one entry out of a sound-design pool. Carries the view it was
    SoundRemove(SoundRemoval),
    /// Take every row one worker holds back off it. The only release that
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
