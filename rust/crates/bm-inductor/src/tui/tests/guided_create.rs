use super::*;

#[test]
fn the_workspace_picker_renders_the_books_and_why_one_is_not_one() {
    // Without this the painter never runs: a picker that draws nothing is
    // indistinguishable, from the dashboard, from a picker with nothing to
    // list — and this one exists precisely so there is never a name to type.
    let root = two_workspaces_one_bad("draw");
    let mut app = App::new("http://127.0.0.1:8901");
    app.layout = bm_core::Layout::new(&root);
    app.screen =
        super::screen::Screen::WorkspaceList(super::screen::WsList::read(&app.layout.root));

    let text = render_text(&mut app, 90, 24);
    assert!(text.contains("Workspace — switch"), "no title:\n{text}");
    assert!(text.contains("book-a"), "the book is not listed:\n{text}");
    assert!(
        hint_visible(&text, "active · 1 chapter · 1 script"),
        "the active book's row says where it stands:\n{text}"
    );
    assert!(
        hint_visible(&text, "no settings.json"),
        "the directory that is not a book says why:\n{text}"
    );
    assert!(
        hint_visible(&text, "Enter switches"),
        "the keys are on screen:\n{text}"
    );
}

/// The dashboard reads the **workspace's** binding, not the checkout's pointer:
/// a book created from a preset owns its pack and engine, and the footer must
/// name the book that will run rather than the root it sits on.
#[test]
fn the_dashboard_reads_the_workspace_binding_not_the_checkout_pointer() {
    let root = tempfile::tempdir().unwrap();
    // The checkout's pointer names another book's pack and language.
    std::fs::create_dir_all(root.path().join(".bm")).unwrap();
    std::fs::write(
        root.path().join(".bm/profile"),
        r#"{"pack":{"name":"xianxia","hash":"c"},"adapter":{"name":"vi-VN","hash":"a"},"engine":{"name":"vieneu","hash":""}}"#,
    )
    .unwrap();
    // The workspace names its own pack and engine, and leaves the language to
    // the checkout — the piece-by-piece merge `Layout::resolve` already makes.
    std::fs::create_dir_all(root.path().join("workspaces/book")).unwrap();
    std::fs::write(
        root.path().join("workspaces/book/settings.json"),
        r#"{"profile":{"pack":{"name":"apothecary","hash":"w","version":""},"engine":{"name":"pocket","hash":"","version":""}}}"#,
    )
    .unwrap();
    std::fs::write(root.path().join(".bm/active-workspace"), "book\n").unwrap();

    let mut app = App::new("http://127.0.0.1:9");
    app.layout = bm_core::Layout::resolve(root.path()).unwrap();
    let (dead_tx, _dead_rx) = tokio::sync::mpsc::unbounded_channel();
    app.relayout(&dead_tx, &reqwest::Client::new());

    let binding = app.profile.as_ref().expect("a binding in force");
    assert_eq!(binding.pack.name, "apothecary");
    assert_eq!(
        binding.adapter.name, "vi-VN",
        "the checkout's language fills the gap"
    );
    assert_eq!(binding.engine.name, "pocket");
    assert_eq!(
        super::model::profile_label(app.profile.as_ref()),
        "apothecary · vi-VN · pocket (w)",
        "the footer names the book's own binding"
    );
}

/// The guided create flow: name → profile → crawler, then one create job
/// carrying the chosen preset and the crawler it seeds the book with.
#[tokio::test]
async fn the_guided_create_picks_a_profile_and_a_crawler_then_creates() {
    let root = std::env::temp_dir().join(format!("bm-ws-guided-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(
        root.join("profiles/presets.json"),
        r#"{"xianxia-vi": {"label": "Xianxia (vi)", "pack": "xianxia", "adapter": "vi-VN", "engine": "vieneu"}}"#,
    )
    .unwrap();
    // A known site is only offered when its global script is on this checkout.
    std::fs::create_dir_all(root.join("crawlers/known")).unwrap();
    std::fs::write(root.join("crawlers/known/storya.lua"), "-- crawl").unwrap();

    let mut app = App::new("http://127.0.0.1:9");
    app.layout = bm_core::Layout::new(&root);
    let profiles: Vec<super::screen::WsItem> = bm_core::preset::read_presets(&root)
        .unwrap()
        .into_iter()
        .map(|(id, p)| super::screen::WsItem {
            label: p.label,
            note: id.clone(),
            value: id,
        })
        .collect();
    assert_eq!(profiles.len(), 1);
    app.screen = super::screen::Screen::WorkspaceNew(super::screen::WorkspaceNew::new(
        "book".into(),
        profiles,
    ));

    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();

    // name → profile
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, super::screen::Screen::WorkspaceNew(ref ws) if ws.step == super::screen::WsStep::Profile),
        "{:?}",
        app.screen
    );

    // profile → crawler
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    let crawlers = match &app.screen {
        super::screen::Screen::WorkspaceNew(ws) => {
            assert_eq!(ws.step, super::screen::WsStep::Crawler);
            ws.crawlers.clone()
        }
        other => panic!("{other:?}"),
    };
    assert!(
        crawlers.iter().any(|c| c.value == "site:storya.click"),
        "a known site whose script is on disk is offered: {crawlers:?}"
    );

    // Walk to the known site and create.
    let idx = crawlers
        .iter()
        .position(|c| c.value == "site:storya.click")
        .unwrap();
    for _ in 0..idx {
        handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;

    assert!(
        matches!(app.screen, super::screen::Screen::Normal),
        "the wizard closes on create: {:?}",
        app.screen
    );
    let Job::Tracked { job, .. } = job_rx.try_recv().expect("a create job") else {
        panic!("the create must be a tracked job")
    };
    match *job {
        Job::Workspace {
            req:
                WorkspaceReq::New {
                    name,
                    profile,
                    crawler,
                },
            ..
        } => {
            assert_eq!(name, "book");
            assert_eq!(profile.as_deref(), Some("xianxia-vi"));
            let c = crawler.expect("the chosen crawler travels with the job");
            assert_eq!(
                c.script, "crawlers/known/storya.lua",
                "a known site is referenced globally, not copied"
            );
            assert!(c.source.as_os_str().is_empty(), "nothing to copy");
            assert!(c.url_template.contains("{n}"), "{}", c.url_template);
        }
        other => panic!("expected a workspace create, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Choosing **Local file (EPUB)** asks for the book, and the create job carries
/// it (`book`) beside the global example script and the workspace-relative
/// `params.epub` — the TUI's "add epub", with no hand-copied file.
#[tokio::test]
async fn the_guided_create_takes_an_epub_path_and_hands_it_to_the_job() {
    let root = std::env::temp_dir().join(format!("bm-ws-guided-epub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(
        root.join("profiles/presets.json"),
        r#"{"jnovel-en": {"label": "JNovel", "pack": "", "adapter": "jnovel-en-US", "engine": "pocket"}}"#,
    )
    .unwrap();
    // The book the operator names is a real file, so the path step accepts it.
    let book = root.join("somewhere/apothecary.epub");
    std::fs::create_dir_all(book.parent().unwrap()).unwrap();
    std::fs::write(&book, b"PK\x03\x04 placeholder").unwrap();

    let mut app = App::new("http://127.0.0.1:9");
    app.layout = bm_core::Layout::new(&root);
    let profiles: Vec<WsItem> = bm_core::preset::read_presets(&root)
        .unwrap()
        .into_iter()
        .map(|(id, p)| WsItem {
            label: p.label,
            note: id.clone(),
            value: id,
        })
        .collect();
    app.screen = Screen::WorkspaceNew(WorkspaceNew::new("book".into(), profiles));

    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();

    // name → profile → crawler
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;

    let crawlers = match &app.screen {
        Screen::WorkspaceNew(ws) => ws.crawlers.clone(),
        other => panic!("{other:?}"),
    };
    let idx = crawlers
        .iter()
        .position(|c| c.value == "local-epub")
        .expect("the EPUB choice is always offered");
    for _ in 0..idx {
        handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::WorkspaceNew(ref ws) if ws.step == WsStep::Epub),
        "the EPUB choice asks for the book: {:?}",
        app.screen
    );

    if let Screen::WorkspaceNew(ws) = &mut app.screen {
        ws.epub = book.display().to_string();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;

    assert!(
        matches!(app.screen, Screen::Normal),
        "the wizard closes on create: {:?}",
        app.screen
    );
    let Job::Tracked { job, .. } = job_rx.try_recv().expect("a create job") else {
        panic!("the create must be a tracked job")
    };
    match *job {
        Job::Workspace {
            req: WorkspaceReq::New { crawler, .. },
            ..
        } => {
            let c = crawler.expect("the EPUB crawler travels with the job");
            assert_eq!(c.script, "crawlers/examples/epub.lua");
            assert_eq!(c.params["epub"], "tmp/book.epub");
            assert_eq!(c.book, book, "the named file is what gets copied in");
            assert!(c.source.as_os_str().is_empty(), "no script to copy");
        }
        other => panic!("expected a workspace create, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Choosing **Local file (EPUB)** and naming a **folder** is the multi-volume
/// shape: the create job carries the directory (`books`) and the
/// workspace-relative `params.books`, and no single book.
#[tokio::test]
async fn the_guided_create_takes_a_books_folder_and_hands_it_to_the_job() {
    let root = std::env::temp_dir().join(format!("bm-ws-guided-books-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(
        root.join("profiles/presets.json"),
        r#"{"jnovel-en": {"label": "JNovel", "pack": "", "adapter": "jnovel-en-US", "engine": "pocket"}}"#,
    )
    .unwrap();
    // The operator names a real directory, so the path step accepts it.
    let shelf = root.join("somewhere/volumes");
    std::fs::create_dir_all(&shelf).unwrap();
    std::fs::write(shelf.join("vol-01.epub"), b"PK\x03\x04 one").unwrap();
    std::fs::write(shelf.join("vol-02.epub"), b"PK\x03\x04 two").unwrap();

    let mut app = App::new("http://127.0.0.1:9");
    app.layout = bm_core::Layout::new(&root);
    let profiles: Vec<WsItem> = bm_core::preset::read_presets(&root)
        .unwrap()
        .into_iter()
        .map(|(id, p)| WsItem {
            label: p.label,
            note: id.clone(),
            value: id,
        })
        .collect();
    app.screen = Screen::WorkspaceNew(WorkspaceNew::new("book".into(), profiles));

    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();

    // name → profile → crawler
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    let crawlers = match &app.screen {
        Screen::WorkspaceNew(ws) => ws.crawlers.clone(),
        other => panic!("{other:?}"),
    };
    let idx = crawlers
        .iter()
        .position(|c| c.value == "local-epub")
        .expect("the EPUB choice is always offered");
    for _ in 0..idx {
        handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::WorkspaceNew(ref ws) if ws.step == WsStep::Epub));

    if let Screen::WorkspaceNew(ws) = &mut app.screen {
        ws.epub = shelf.display().to_string();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;

    let Job::Tracked { job, .. } = job_rx.try_recv().expect("a create job") else {
        panic!("the create must be a tracked job")
    };
    match *job {
        Job::Workspace {
            req: WorkspaceReq::New { crawler, .. },
            ..
        } => {
            let c = crawler.expect("the EPUB crawler travels with the job");
            assert_eq!(c.script, "crawlers/examples/epub.lua");
            assert_eq!(c.params["books"], "books");
            assert!(c.params.get("epub").is_none(), "a folder is not one book");
            assert_eq!(c.books, shelf, "the named folder is what gets copied in");
            assert!(c.book.as_os_str().is_empty(), "no single book");
        }
        other => panic!("expected a workspace create, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_guided_create_overlay_draws_the_step_and_the_presets() {
    let mut app = App::new("http://x");
    app.screen = super::screen::Screen::WorkspaceNew(super::screen::WorkspaceNew::new(
        "book".into(),
        vec![super::screen::WsItem {
            label: "Xianxia (vi)".into(),
            note: "xianxia-vi".into(),
            value: "xianxia-vi".into(),
        }],
    ));
    let name_step = render_text(&mut app, 100, 44);
    assert!(name_step.contains("Workspace — new"), "{name_step}");
    assert!(
        name_step.contains("book"),
        "the typed name is echoed: {name_step}"
    );

    if let super::screen::Screen::WorkspaceNew(mut ws) = app.screen.clone() {
        ws.step = super::screen::WsStep::Profile;
        app.screen = super::screen::Screen::WorkspaceNew(ws);
    }
    let profile_step = render_text(&mut app, 100, 44);
    assert!(
        profile_step.contains("Xianxia (vi)"),
        "the preset list draws: {profile_step}"
    );
}
