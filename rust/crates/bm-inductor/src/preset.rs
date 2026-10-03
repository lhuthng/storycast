use super::cli::WorkspaceCmd;
use super::*;

/// Stamp a workspace's binding from a profile preset — the `--profile` half of
/// `workspace new`.
///
/// Three properties the checkout's own load path does not have, all because a
/// second book must be creatable beside a first one that is mid-run:
///
/// * **The checkout's `.bm/profile` is never written.** The binding lands in
///   the new workspace's `settings.json` alone; switching between books stays
///   `workspace use`, and re-loading the checkout stays an explicit
///   `profile load`.
/// * **A preset with `pack_deps` composes the workspace's own pack** (see
///   `preset::compose_workspace_pack`), so its score is its own — the
///   workspace-pack shape ROADMAP §3 defers to this. The binding's pack hash
///   is the resolved workspace tree's, the same fold a checkout's pack gets.
/// * **The adapter and engine are stamped by name and claim**, the adapter
///   hashed over its home the way `verify_binding` folds it. The engine is a
///   declaration and never a digest.
fn apply_preset(
    root: &std::path::Path,
    work: &std::path::Path,
    id: &str,
    settings: &mut Settings,
    out: &mut Vec<String>,
    crawler: Option<&bm_core::preset::CrawlerSetup>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let presets = bm_core::preset::read_presets(root)?;
    let preset = presets.get(id).ok_or_else(|| {
        anyhow::anyhow!(
            "no profile preset {id:?} in profiles/presets.json — available: {}",
            presets.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })?;
    let mut binding = bm_core::profile::Binding::default();

    // The pack. Composed into the workspace when the preset names roots.
    if !preset.pack_deps.is_empty() {
        bm_core::preset::compose_workspace_pack(
            work,
            &root.join("assets").join("_extends"),
            &preset.pack_deps,
        )?;
        let pack_name = if preset.pack.is_empty() {
            work.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| preset.pack.clone())
        } else {
            preset.pack.clone()
        };
        binding.pack = bm_core::profile::Pointer {
            hash: bm_core::preset::workspace_pack_hash(work)?,
            name: pack_name,
            version: String::new(),
        };
        out.push(format!(
            "pack     {} — this workspace's own composition of {}",
            binding.pack.name,
            preset.pack_deps.join(" + ")
        ));
    } else {
        // Shared pack: the checkout's tree is the one in force, so its hash
        // travels with the name when the two agree — and an empty hash with a
        // note when they do not, because a binding claiming the checkout's
        // hash under another name would be a lie about bytes nobody hashed.
        let checkout = bm_core::profile::read_binding(root).ok();
        binding.pack = bm_core::profile::Pointer {
            name: preset.pack.clone(),
            hash: String::new(),
            version: String::new(),
        };
        match &checkout {
            Some(b) if !preset.pack.is_empty() && b.pack.name != preset.pack => {
                out.push(format!(
                    "note: the checkout's live pack is '{}' — run `profile load {}` before this workspace runs, or it will bind against {}",
                    b.pack.name, preset.pack, preset.pack,
                ));
            }
            Some(b) if !preset.pack.is_empty() => binding.pack.hash = b.pack.hash.clone(),
            _ => {}
        }
        out.push(format!(
            "pack     {} — the checkout's live tree",
            if preset.pack.is_empty() {
                "(unnamed)"
            } else {
                &preset.pack
            }
        ));
    }

    // The adapter, hashed over its home the way the load gate folds it. No
    // `crawl` half: an adapter ships no crawlers — they are global (`crawlers/`)
    // or the book's own (`crawl/`), and hashing a directory that is not there
    // would report a language as incomplete for a reason that is not its own.
    let home_dirs = [format!("adapters/{}/prompts", preset.adapter)];
    let adapter = bm_core::profile::Pointer {
        hash: bm_core::profile::trees_hash(root, &home_dirs).unwrap_or_else(|_| {
            out.push(format!(
                "note: adapters/{}/ holds no prompts or crawlers yet — the binding stamps the name only",
                preset.adapter
            ));
            String::new()
        }),
        name: preset.adapter.clone(),
        version: String::new(),
    };
    binding.adapter = adapter;

    // The engine: `settings.engine` is the whole fact, the binding's claim
    // beside it for the label and the wire.
    binding.engine = bm_core::profile::Pointer {
        name: preset.engine.clone(),
        hash: String::new(),
        version: String::new(),
    };
    settings.engine = preset.engine.clone();

    // The crawler: a `{ type, file }` selection. A **global** crawler (a known
    // site, the EPUB example) is referenced in place — `resolve_script` finds it
    // under the root, and an edit reaches every book that selected it. A
    // **custom** one is the book's own, copied into `crawl/`, where it shadows
    // the global tree. The guided create flow hands one in; a preset's own
    // `crawler` is the fallback, and neither is required — a book that names no
    // source starts in `manual`.
    let crawler: Option<bm_core::preset::CrawlerSetup> = match crawler {
        Some(c) => Some(c.clone()),
        None if !preset.crawler.is_none() => Some(crawler_from_preset(work, &preset.crawler)?),
        None => None,
    };
    if let Some(c) = &crawler {
        if !c.source.as_os_str().is_empty() {
            let file_name = c
                .source
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("crawler {:?} has no file name", c.source))?
                .to_string_lossy()
                .into_owned();
            let dest = work.join("crawl").join(&file_name);
            std::fs::create_dir_all(dest.parent().expect("crawl/ has a parent"))?;
            std::fs::copy(&c.source, &dest)
                .with_context(|| format!("copying {} -> {}", c.source.display(), dest.display()))?;
            settings.crawl.mode = "script".into();
            settings.crawl.script = format!("crawl/{file_name}");
            out.push(format!(
                "crawler  crawl/{file_name} — this workspace's own, seeded from {}",
                c.source.display()
            ));
        } else if !c.script.trim().is_empty() {
            settings.crawl.mode = "script".into();
            settings.crawl.script = c.script.clone();
            out.push(format!(
                "crawler  {} — global, shared by every book",
                c.script
            ));
        }
        // The guided "Local file (EPUB)" choice: the operator named a book and
        // it is copied into the workspace's own `tmp/book.epub`, which is what
        // `crawl.params.epub` names. Copied, not referenced: the crawl's read
        // root is the workspace, and a path outside it is refused.
        if !c.book.as_os_str().is_empty() {
            let dest = work.join("tmp").join("book.epub");
            std::fs::create_dir_all(dest.parent().expect("tmp/ has a parent"))?;
            std::fs::copy(&c.book, &dest)
                .with_context(|| format!("copying {} -> {}", c.book.display(), dest.display()))?;
            out.push(format!(
                "book     tmp/book.epub — copied from {}",
                c.book.display()
            ));
        }
        // The multi-volume shape: a folder of `.epub`s, each one a volume, in
        // the workspace's own `books/`, which is what `crawl.params.books`
        // names. Only `.epub` files are copied — it is a person's own folder
        // and the rest of it is not the crawl's business.
        if !c.books.as_os_str().is_empty() {
            let dest = work.join("books");
            std::fs::create_dir_all(&dest)?;
            let mut copied = 0usize;
            for entry in std::fs::read_dir(&c.books)
                .with_context(|| format!("reading the books directory {}", c.books.display()))?
            {
                let path = entry?.path();
                let is_epub = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("epub"));
                if !is_epub || !path.is_file() {
                    continue;
                }
                let name = path.file_name().expect("an .epub has a file name");
                std::fs::copy(&path, dest.join(name)).with_context(|| {
                    format!(
                        "copying {} -> books/{}",
                        path.display(),
                        name.to_string_lossy()
                    )
                })?;
                copied += 1;
            }
            anyhow::ensure!(
                copied > 0,
                "no .epub in {} — a books directory needs at least one volume",
                c.books.display()
            );
            out.push(format!(
                "books    books/ — {copied} volume(s) copied from {}",
                c.books.display()
            ));
        }
        if !c.url_template.is_empty() {
            settings.url_template = c.url_template.clone();
            out.push(format!("url      {}", c.url_template));
        }
        if !c.params.is_empty() {
            settings.crawl.params = c.params.clone();
        }
        if c.max_fetches > 0 {
            settings.crawl.max_fetches = c.max_fetches;
        }
        if c.max_seconds > 0 {
            settings.crawl.max_seconds = c.max_seconds;
        }
    }

    out.push(format!("adapter  {}", preset.adapter));
    out.push(format!("engine   {}", preset.engine));
    settings.profile = binding;
    Ok(())
}

/// Turn a preset's `{ type, file }` crawler into the setup `apply_preset`
/// installs.
///
/// `known` and `example` are **global** and referenced in place, so the setup
/// carries the `crawlers/…` path and no `source`; `custom` is the book's own, so
/// it carries no path (the operator drops the file into `crawl/`) and just makes
/// the directory. A `known` host the registry does not have, or one it lists
/// without a crawler, is a preset error worth failing on: a workspace silently
/// born with no crawler is the failure this whole flow exists to prevent.
fn crawler_from_preset(
    work: &std::path::Path,
    c: &bm_core::preset::PresetCrawler,
) -> anyhow::Result<bm_core::preset::CrawlerSetup> {
    match c.kind.as_str() {
        "known" => {
            let site = bm_core::crawl::known_sites()
                .iter()
                .find(|s| s.host == c.file)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "preset crawler names known site {:?}, which crawlers/knownsites.json does not have",
                        c.file
                    )
                })?;
            anyhow::ensure!(
                site.is_crawlable(),
                "preset crawler names site {:?}, which has no bundled crawler — {}",
                site.host,
                site.caveat.unwrap_or("no reason recorded")
            );
            let mut params = serde_json::Map::new();
            for (k, v) in site.params {
                params.insert(
                    (*k).to_string(),
                    serde_json::Value::String((*v).to_string()),
                );
            }
            Ok(bm_core::preset::CrawlerSetup {
                script: site.script.to_string(),
                url_template: site.url_template.to_string(),
                params,
                max_fetches: site.max_fetches,
                max_seconds: site.max_seconds,
                source: std::path::PathBuf::new(),
                book: std::path::PathBuf::new(),
                books: std::path::PathBuf::new(),
            })
        }
        "example" => Ok(bm_core::preset::CrawlerSetup {
            script: format!("crawlers/examples/{}", c.file.trim()),
            ..Default::default()
        }),
        "custom" => {
            // The book's own crawler: make the directory ready, and name the
            // file only if the preset already knows it. Otherwise the operator
            // drops the script in and sets `crawl.script` when they do.
            std::fs::create_dir_all(work.join("crawl"))?;
            Ok(if c.file.trim().is_empty() {
                bm_core::preset::CrawlerSetup::default()
            } else {
                bm_core::preset::CrawlerSetup {
                    script: format!("crawl/{}", c.file.trim()),
                    ..Default::default()
                }
            })
        }
        other => anyhow::bail!(
            "preset crawler type {other:?} is not known — use known, example, custom or none"
        ),
    }
}

/// Give a workspace its own copy of the material the **profile preset**
/// declares: the adapter's trees — its `prompts/` and `crawl/`.
///
/// A preset names a pack × adapter × engine triple, and each piece is created
/// differently: the pack is **composed** (`compose_workspace_pack`, real files
/// resolved from the linked deps), the preset's crawler is **installed** by
/// `apply_preset` (referenced globally, or copied for a custom one), and the
/// adapter's home (its `prompts/`) is **copied here** — so a book's prompts
/// resolve from the workspace, which
/// [`bm_core::paths::Layout::adapter_home`] already searches first, instead of
/// from the checkout's shared home.
///
/// **Voices are deliberately not here.** They are not a preset yet — they
/// arrive as bundles later — so a new workspace gets none and reads none: it
/// must not reach back to the checkout's roster, which belongs to
/// beyond-myriads and to nobody else.
///
/// `only_missing` is the `migrate` case: a workspace that already owns the
/// adapter home keeps it, so a migration can never clobber edited prompts.
fn clone_book_material(
    root: &std::path::Path,
    work: &std::path::Path,
    adapter: &str,
    only_missing: bool,
    out: &mut Vec<String>,
) -> anyhow::Result<()> {
    if adapter.is_empty() {
        return Ok(());
    }
    let from = root.join(bm_core::paths::ADAPTERS_DIR).join(adapter);
    let to = work.join(bm_core::paths::ADAPTERS_DIR).join(adapter);
    if from.is_dir() && !(only_missing && to.is_dir()) {
        bm_core::util::copy_tree(&from, &to)?;
        out.push(format!(
            "book     adapters/{adapter}/ copied in (this workspace's own prompts and crawlers)"
        ));
    }
    Ok(())
}

/// `workspace`, one directory per book. Creating switches to it; selecting
/// only moves the pointer, so data is never wiped and ledgers never mix
/// (the serve gate still refuses a ledger bound to another profile).
///
/// Returns the lines to show rather than printing them: the CLI prints, the
/// dashboard logs them into its event pane, and both are the same operation.
pub(crate) fn workspace_cmd(
    root: &std::path::Path,
    cmd: WorkspaceCmd,
) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let dir = |name: &str| root.join("workspaces").join(name);
    let valid = |name: &str| {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\0')
    };
    match cmd {
        WorkspaceCmd::New {
            name,
            profile,
            crawler,
        } => {
            if !valid(&name) {
                anyhow::bail!("bad workspace name {name:?}");
            }
            if dir(&name).exists() {
                anyhow::bail!(
                    "workspace {name:?} already exists — `workspace use {name}` to select it"
                );
            }
            for d in ["data/chapters", "data/audio", "output"] {
                std::fs::create_dir_all(dir(&name).join(d))?;
            }
            // A workspace is born bound to a profile, so its first run cannot
            // mix genres. A `--profile` preset names its own triple; without
            // one the loaded profile is inherited, which is the shape every
            // workspace before presets had. No profile loaded yet is not an
            // error — the serve gate names it when it matters.
            let mut settings = Settings::default();
            match profile.as_deref() {
                Some(id) => {
                    apply_preset(root, &dir(&name), id, &mut settings, &mut out, crawler.as_ref())?
                }
                None => match bm_core::profile::read_binding(root) {
                    Ok(b) => settings.profile = b,
                    Err(_) => {
                        out.push(
                            "note: no profile loaded — `:profile` in the dashboard, or `tools/profile.sh fetch/unpack <name>`, first"
                                .into(),
                        )
                    }
                },
            }
            // …and its own copy of the adapter the profile names, so the first
            // run reads its own prompts and crawlers instead of the checkout's
            // shared home — which is how a second book used to inherit the
            // first one's language. Voices are not copied: they are not a preset
            // yet, and a new workspace must not read another book's roster.
            clone_book_material(
                root,
                &dir(&name),
                &settings.profile.adapter.name,
                false,
                &mut out,
            )?;
            settings.save(&dir(&name).join("settings.json"))?;
            std::fs::create_dir_all(root.join(".bm"))?;
            std::fs::write(Layout::active_workspace_file(root), format!("{name}\n"))?;
            out.push(format!("workspace {name} created and selected"));
            Ok(out)
        }
        WorkspaceCmd::Use { name } => {
            if !dir(&name).is_dir() {
                anyhow::bail!("no workspace {name:?} under workspaces/");
            }
            std::fs::create_dir_all(root.join(".bm"))?;
            std::fs::write(Layout::active_workspace_file(root), format!("{name}\n"))?;
            out.push(format!("workspace {name} selected"));
            Ok(out)
        }
        WorkspaceCmd::Migrate { name } => {
            if !dir(&name).is_dir() {
                anyhow::bail!("no workspace {name:?} under workspaces/");
            }
            let ws = dir(&name);
            let adapter = bm_core::config::Settings::load(&ws.join("settings.json"))
                .profile
                .adapter
                .name;
            clone_book_material(root, &ws, &adapter, true, &mut out)?;
            out.push(format!("workspace {name} now owns its book material"));
            Ok(out)
        }
        WorkspaceCmd::List => {
            // The same read the TUI picker draws from, so a name listed here is
            // a name offered there, and both say the same thing about a
            // directory that is not a book.
            let found = bm_core::paths::workspaces(root);
            if found.is_empty() {
                out.push("no workspaces (this root is the implicit default)".into());
            }
            for w in found {
                let mark = if w.active { "*" } else { " " };
                let note = match w.config {
                    bm_core::paths::WorkspaceConfig::Valid => String::new(),
                    bm_core::paths::WorkspaceConfig::Missing => {
                        "  — no settings.json, not a workspace".into()
                    }
                    bm_core::paths::WorkspaceConfig::Broken => {
                        "  — settings.json does not parse".into()
                    }
                };
                out.push(format!("{mark} {}{note}", w.name));
            }
            Ok(out)
        }
    }
}
