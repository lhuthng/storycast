use super::*;

/// A checkout the screen can write `.bm/llm.json` and `settings.json` into.
fn llm_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    layout.ensure().unwrap();
    let mut app = App::new("http://unused");
    app.layout = layout;
    (dir, app)
}

/// One `n` submission: the line is the whole entry, so there is no id to pass.
fn add(app: &App, line: &str) -> Result<String, String> {
    input::llm::save_llm_field(app, "", &TextKind::LlmNew, line)
}

#[test]
fn an_operator_names_a_provider_and_its_url() {
    let (_dir, app) = llm_app();
    let msg = add(&app, "my-gateway https://gw.example/v1").expect("the line is taken");
    assert!(msg.contains("my-gateway added"), "{msg}");

    let cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    let e = &cfg.providers["my-gateway"];
    assert_eq!(e.kind, "openai", "an unnamed protocol is OpenAI-compatible");
    assert_eq!(e.base_url, "https://gw.example/v1");
    assert!(!e.has_key(), "the key follows with `k`");
    assert!(e.model.is_empty(), "the model follows with `f`/`m`");
    assert!(cfg.active.is_empty(), "adding it is not activating it");
    assert!(
        LlmView::ids(&cfg).contains(&"my-gateway".to_string()),
        "the row shows up on the screen"
    );
}

#[test]
fn the_line_names_the_protocol_and_refuses_what_it_cannot_use() {
    let (_dir, app) = llm_app();
    add(&app, "gemini-proxy https://proxy.example gemini").expect("a named protocol");
    assert_eq!(
        bm_core::config::LlmConfig::load(&app.layout.root).providers["gemini-proxy"].kind,
        "gemini"
    );
    // A pasted URL carries its trailing slash, which `u` strips too.
    add(&app, "local https://gw.example/v1/").unwrap();
    assert_eq!(
        bm_core::config::LlmConfig::load(&app.layout.root).providers["local"].base_url,
        "https://gw.example/v1"
    );

    for line in ["one-word", "a b c d", "x ftp://gw.example"] {
        assert!(add(&app, line).is_err(), "“{line}” must be refused");
    }
    let kind = add(&app, "x https://gw.example watson").unwrap_err();
    assert!(kind.contains("unknown kind"), "{kind}");
    let taken = add(&app, "gemini-proxy https://other.example/v1").unwrap_err();
    assert!(taken.contains("already configured"), "{taken}");

    let cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    assert!(
        !cfg.providers.contains_key("x"),
        "a refused line writes nothing"
    );
    assert_eq!(
        cfg.providers["gemini-proxy"].base_url, "https://proxy.example",
        "a refused line leaves the provider it named alone"
    );
}

#[test]
fn a_named_provider_carries_its_key_and_model_like_any_other() {
    let (_dir, app) = llm_app();
    add(&app, "gemini-proxy https://proxy.example gemini").unwrap();
    let set =
        |kind: TextKind, buf: &str| input::llm::save_llm_field(&app, "gemini-proxy", &kind, buf);
    set(TextKind::LlmKey("gemini-proxy".into()), "p-key").unwrap();
    set(
        TextKind::LlmModel("gemini-proxy".into()),
        "gemini-3.1-flash-lite",
    )
    .unwrap();
    input::llm::activate(&app, "gemini-proxy").unwrap();

    let r = bm_core::config::LlmConfig::load(&app.layout.root)
        .resolve()
        .expect("key + model + URL: usable");
    assert_eq!(r.analyzer, "gemini-proxy", "the id is the analyzer");
    assert_eq!(r.base_url, "https://proxy.example", "its own URL travels");
    assert_eq!(r.key_var, "GEMINI_API_KEY", "the protocol picks the slot");

    // And it reaches the settings the digest lane reads, the way the shipped
    // providers do — the point of adding one at all.
    let s = bm_core::config::Settings::load(&app.layout.settings());
    assert_eq!(s.analyzer, "gemini-proxy");
    assert_eq!(s.analyzer_backend, "gemini");
    assert_eq!(s.gemini_url, "https://proxy.example");
    assert_eq!(s.analyze_models, vec!["gemini-3.1-flash-lite".to_string()]);
}

#[tokio::test]
async fn the_llm_screen_opens_the_new_provider_prompt() {
    let (_dir, mut app) = llm_app();
    add(&app, "my-gateway https://gw.example/v1").unwrap();
    app.screen = Screen::Llm(LlmView::new());
    let text = render_text(&mut app, 120, 30);
    assert!(hint_visible(&text, "n new"), "{text}");

    handle_key(
        &mut app,
        key(KeyCode::Char('n')),
        &http_client(),
        &job_channel(),
    )
    .await;
    match app.screen.clone() {
        Screen::Text(p) => {
            assert_eq!(p.kind, TextKind::LlmNew);
            assert!(
                p.hint.contains("tokenharbor"),
                "the hint teaches by example: {}",
                p.hint
            );
        }
        other => panic!("`n` must open the new-provider prompt, got {other:?}"),
    }
}

#[tokio::test]
async fn typing_the_line_leaves_the_cursor_on_the_provider_it_made() {
    let (_dir, mut app) = llm_app();
    app.screen = Screen::Llm(LlmView::new());
    let http = http_client();
    let jobs = job_channel();
    handle_key(&mut app, key(KeyCode::Char('n')), &http, &jobs).await;
    for c in "proxy https://gw.example/v1".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &jobs).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &jobs).await;

    let cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    assert!(cfg.providers.contains_key("proxy"), "the line is the entry");
    let Screen::Llm(v) = app.screen.clone() else {
        panic!("a saved line returns to the screen, got {:?}", app.screen);
    };
    assert_eq!(
        LlmView::ids(&cfg)[v.cursor],
        "proxy",
        "the cursor lands on the row it made"
    );
}

#[tokio::test]
async fn the_screen_shows_a_row_for_your_own_provider() {
    let (_dir, mut app) = llm_app();
    add(&app, "my-gateway https://gw.example/v1").unwrap();
    app.screen = Screen::Llm(LlmView::new());
    let text = render_text(&mut app, 120, 30);
    assert!(hint_visible(&text, "+ name your own provider"), "{text}");
    assert!(
        hint_visible(&text, "a name and a URL, then `k` its key"),
        "the row says what to do with it:\n{text}"
    );
    assert!(hint_visible(&text, "n new"), "and the key is still listed");
}

#[tokio::test]
async fn the_add_row_is_reached_by_arrows_and_answers_enter_and_n() {
    let (_dir, mut app) = llm_app();
    add(&app, "my-gateway https://gw.example/v1").unwrap();
    let http = http_client();
    let jobs = job_channel();

    // Down from the only provider row lands on it, and Up comes back.
    app.screen = Screen::Llm(LlmView::new());
    handle_key(&mut app, key(KeyCode::Down), &http, &jobs).await;
    match app.screen.clone() {
        Screen::Llm(v) => assert_eq!(v.cursor, 1, "one provider, so the add row is next"),
        other => panic!("{other:?}"),
    }

    // No provider key acts on that row, and it says so rather than guessing.
    handle_key(&mut app, key(KeyCode::Char('a')), &http, &jobs).await;
    let cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    assert!(cfg.active.is_empty(), "the add row is not a provider to activate");
    match app.screen.clone() {
        Screen::Llm(v) => assert_eq!(v.cursor, 1, "the screen stays where it was"),
        other => panic!("a refused key must not leave the screen: {other:?}"),
    }

    // Enter is the same prompt `n` opens.
    handle_key(&mut app, key(KeyCode::Enter), &http, &jobs).await;
    match app.screen.clone() {
        Screen::Text(p) => assert_eq!(p.kind, TextKind::LlmNew),
        other => panic!("Enter on the add row opens the prompt, got {other:?}"),
    }
}

#[tokio::test]
async fn an_empty_provider_list_can_still_be_added_to() {
    let (_dir, mut fresh) = llm_app();
    fresh.screen = Screen::Llm(LlmView::new());
    let text = render_text(&mut fresh, 120, 30);
    assert!(hint_visible(&text, "no providers configured"), "{text}");

    handle_key(
        &mut fresh,
        key(KeyCode::Char('n')),
        &http_client(),
        &job_channel(),
    )
    .await;
    assert!(
        matches!(fresh.screen, Screen::Text(_)),
        "with nothing to point at, `n` is the one key that still acts"
    );

    let (_dir2, mut closing) = llm_app();
    closing.screen = Screen::Llm(LlmView::new());
    handle_key(
        &mut closing,
        key(KeyCode::Esc),
        &http_client(),
        &job_channel(),
    )
    .await;
    assert!(
        matches!(closing.screen, Screen::Normal),
        "Esc still closes an empty list"
    );
}
