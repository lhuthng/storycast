use super::*;
use crate::util::read_json;

#[test]
fn only_default_opts_out_of_the_digest_title() {
    // `auto` is the default, so every workspace written before this field
    // existed keeps the digest's title.
    let v: Settings = serde_json::from_str(r#"{"engine":"pocket"}"#).unwrap();
    assert!(
        v.auto_title(),
        "a settings file without the field reads as auto"
    );
    assert!(Settings::default().auto_title());
    let mut s = Settings {
        title_mode: "default".into(),
        ..Settings::default()
    };
    assert!(!s.auto_title());
    s.title_mode = "  DEFAULT  ".into();
    assert!(!s.auto_title(), "case and padding are not a different mode");
    s.title_mode = "auto".into();
    assert!(s.auto_title());
    // A typo must not silently pin a book to its crawled headline: the
    // safe direction is the behaviour every existing workspace already has.
    s.title_mode = "defualt".into();
    assert!(s.auto_title(), "an unrecognised mode is not an opt-out");
}

#[test]
fn settings_without_ssh_parses_as_defaults_and_roundtrips() {
    // A pre-ssh settings.json has no `ssh` key: it must load as defaults.
    let v: Settings = serde_json::from_str(r#"{"engine":"gemini"}"#).unwrap();
    assert_eq!(v.engine, "gemini");
    assert_eq!(v.ssh.user, "thang");
    assert_eq!(v.ssh.port, 22);
    assert_eq!(v.ssh.key, None);

    let dir = std::env::temp_dir().join("bm-settings-ssh");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("settings.json");
    let mut s = Settings::default();
    s.ssh.key = Some("~/.ssh/k".into());
    s.save(&p).unwrap();
    let back = Settings::load(&p);
    assert_eq!(back.ssh.key.as_deref(), Some("~/.ssh/k"));
    assert_eq!(back.ssh.user, "thang");
}

#[test]
fn an_unset_advertise_is_not_an_address_to_hand_out() {
    // The default is the sentinel for "unset". Read as a value it would
    // hand every worker `http://127.0.0.1:8901` — a URL that works on the
    // inductor and nowhere else, which is the silent failure this field
    // exists to prevent.
    let mut s = Settings::default();
    assert_eq!(s.advertise, "127.0.0.1");
    assert!(s.advertised_host().is_none());
    for unset in ["", "  ", "localhost", "::1", "127.0.0.1"] {
        s.advertise = unset.into();
        assert!(s.advertised_host().is_none(), "{unset:?} is not an address");
    }
    for set in ["box.example.com", "203.0.113.9", "box.example.com:8901"] {
        s.advertise = set.into();
        assert_eq!(s.advertised_host(), Some(set));
    }
}

#[test]
fn analyzer_models_default_only_when_omitted() {
    let omitted: Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(omitted.analyze_models, vec!["gemini-3.5-flash"]);
    let explicit_empty = bm_proto::AnalyzerSettings {
        analyze_models: Some(vec![]),
        ..Default::default()
    };
    let effective = omitted.with_analyzer_settings(&explicit_empty);
    assert!(effective.analyze_models.is_empty());
    assert_eq!(effective.analyzer_settings().analyze_models, Some(vec![]));
    for models in [vec![], vec!["first", "second"]] {
        let settings: Settings =
            serde_json::from_value(serde_json::json!({"analyze_models": models})).unwrap();
        assert_eq!(settings.analyze_models, models);
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["analyze_models"], serde_json::json!(models));
    }
}

#[test]
fn chapter_url_substitutes_every_n() {
    let s = Settings {
        url_template: "https://x/chuong-{n}?page={n}".into(),
        ..Default::default()
    };
    assert_eq!(s.chapter_url(12), "https://x/chuong-12?page=12");
    // The padded form, which is the whole reason this is not a bare
    // `replace`: a site numbering `chapter-001` needs no script.
    let padded = Settings {
        url_template: "https://x/chapter-{n:03}".into(),
        ..Default::default()
    };
    assert_eq!(padded.chapter_url(7), "https://x/chapter-007");
}

#[test]
fn a_workspace_written_before_scripted_crawls_still_crawls_the_same_way() {
    // The migration promise: a settings.json that only ever named a
    // url_template loads as `mode: script` with the bundled crawler, and
    // that crawler is handed the same URL the old Rust path expanded.
    // The absent `crawl` block means *old workspace*, so it keeps the
    // scripted default — the manual default is for workspaces created now.
    let old: Settings =
        serde_json::from_str(r#"{"url_template":"https://storya.click/truyen/x/chuong-{n}"}"#)
            .unwrap();
    assert_eq!(old.crawl.mode, "script");
    assert_eq!(old.crawl.script, crate::crawl::DEFAULT_SCRIPT);
    assert!(!old.crawl.is_manual(), "the old workspace still fetches");
    assert_eq!(
        old.chapter_url(34),
        "https://storya.click/truyen/x/chuong-34"
    );
    // A workspace created now defaults to manual: nothing fetches until
    // the operator says how chapters arrive.
    let fresh = Settings::default();
    assert!(fresh.crawl.is_manual(), "the fresh default does not fetch");
    assert!(fresh.crawl.script.is_empty());
    assert!(
        fresh.url_template.is_empty(),
        "a fresh workspace names no book — the old default pointed at beyond-myriads"
    );
    // …and an explicit value is honoured. A block that names no script is
    // the built-in fetcher (`script` fills from the per-field default, which
    // is empty — the operator named no crawler); a block naming one gets it.
    let plain: Settings = serde_json::from_str(r#"{"crawl":{"script":""}}"#).unwrap();
    assert_eq!(plain.crawl.script, "");
    assert!(plain.crawl.params.is_empty());
    let named: Settings =
        serde_json::from_str(r#"{"crawl":{"mode":"script","script":"assets/crawl/site.lua"}}"#)
            .unwrap();
    assert_eq!(named.crawl.script, "assets/crawl/site.lua");
    assert!(!named.crawl.is_manual());
}

#[test]
fn settings_roundtrip_and_default_on_missing() {
    let dir = std::env::temp_dir().join("bm-settings-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("settings.json");
    assert_eq!(Settings::load(&p).engine, "vieneu");

    let s = Settings {
        engine: "gemini".into(),
        count: 42,
        ..Default::default()
    };
    s.save(&p).unwrap();
    let back = Settings::load(&p);
    assert_eq!(back.engine, "gemini");
    assert_eq!(back.count, 42);
}

#[test]
fn a_settings_file_without_an_endpoint_still_names_one() {
    // The field is newer than every settings file in the wild. A missing
    // key must mean the public service, not an empty string: an empty base
    // builds a relative URL, which fails as a transport error naming
    // neither the provider nor the field.
    let parsed: Settings = serde_json::from_str(r#"{"engine":"vieneu"}"#).unwrap();
    assert_eq!(parsed.gemini_url, DEFAULT_GEMINI_URL);
    assert_eq!(Settings::default().gemini_url, DEFAULT_GEMINI_URL);
    // A file that names one keeps it, trailing slash and all — trimming is
    // the request builder's business, and this stays the operator's words.
    let named: Settings = serde_json::from_str(r#"{"gemini_url":"https://gw.example/"}"#).unwrap();
    assert_eq!(named.gemini_url, "https://gw.example/");
}

#[test]
fn a_gemini_providers_endpoint_travels_with_its_model() {
    // `kind: gemini` is a protocol slot, not a host. An operator pointing it
    // at a compatible gateway used to have the endpoint silently dropped —
    // the offer carried the model alone, so the box listed models off the
    // operator's endpoint and then generated against Google's. Both halves
    // are asserted here: the offer carries the base, and the overlay lands
    // it on the field the request builder reads.
    let mut cfg = llm_cfg(
        "google",
        &[("google", "gemini", "g-key", "gemini-3.5-flash")],
    );
    cfg.providers.get_mut("google").unwrap().base_url = "https://gw.example".into();
    let (analyzer, a) = cfg.offer_analyzer(&Settings::default());
    assert_eq!(analyzer, "google");
    assert_eq!(a.backend, "gemini");
    assert_eq!(
        a.analyze_models.as_deref(),
        Some(["gemini-3.5-flash".to_string()].as_slice())
    );
    assert_eq!(a.gemini_url, "https://gw.example");

    let boxed = Settings {
        gemini_url: "https://this-box.example".into(),
        ..Settings::default()
    };
    assert_eq!(
        boxed.with_analyzer_settings(&a).gemini_url,
        "https://gw.example"
    );

    // An older inductor says nothing about the endpoint: the box keeps its
    // own, exactly as it does for the model and the other two URLs.
    let older = bm_proto::AnalyzerSettings {
        analyze_models: Some(vec!["m".into()]),
        ..Default::default()
    };
    assert_eq!(
        boxed.with_analyzer_settings(&older).gemini_url,
        "https://this-box.example"
    );
}

#[test]
fn llm_config_defaults_to_nothing_at_all() {
    // Default is none twice over: no active provider AND no providers —
    // slots come from `llm.default.json` (or `.bm/llm.json`), never code.
    let cfg = LlmConfig::default();
    assert!(cfg.active.is_empty());
    assert!(cfg.providers.is_empty());
    assert!(cfg.resolve().is_none());
}

/// One test provider: `kind` is what routes, the id is just a label.
fn entry(kind: &str, key: &str, model: &str) -> ProviderEntry {
    ProviderEntry {
        kind: kind.into(),
        base_url: "https://example/v1".into(),
        api_key: key.into(),
        model: model.into(),
    }
}

fn llm_cfg(active: &str, providers: &[(&str, &str, &str, &str)]) -> LlmConfig {
    LlmConfig {
        active: active.into(),
        providers: providers
            .iter()
            .map(|(id, kind, key, model)| (id.to_string(), entry(kind, key, model)))
            .collect(),
    }
}

#[test]
fn llm_resolve_needs_a_key_and_a_model() {
    let mut cfg = llm_cfg("openrouter", &[("openrouter", "openai", "", "")]);
    assert!(cfg.resolve().is_none(), "no key, no model: not usable");
    cfg.providers.get_mut("openrouter").unwrap().api_key = "sk-or-x".into();
    assert!(cfg.resolve().is_none(), "key but no model: not usable");
    cfg.providers.get_mut("openrouter").unwrap().model = "x/y".into();
    let r = cfg.resolve().expect("key + model resolves");
    assert_eq!(r.analyzer, "openrouter");
    assert_eq!(r.key_var, "OPENROUTER_API_KEY");
    // An id the file invented still routes by its kind, not its name.
    cfg.providers
        .insert("my-gateway".into(), entry("openai", "k", "m"));
    cfg.providers.get_mut("my-gateway").unwrap().base_url = "https://gw.example/v1".into();
    cfg.active = "my-gateway".into();
    let r = cfg.resolve().expect("custom provider resolves");
    assert_eq!(r.analyzer, "my-gateway");
    assert_eq!(r.base_url, "https://gw.example/v1");
    // The ollama kind needs a model but no key.
    cfg.providers
        .insert("ollama".into(), entry("ollama", "", ""));
    cfg.active = "ollama".into();
    assert!(cfg.resolve().is_none(), "ollama still model-less");
    cfg.providers.get_mut("ollama").unwrap().model = "gemma-4-12b".into();
    let r = cfg.resolve().expect("ollama resolves keyless");
    assert_eq!(r.analyzer, "ollama");
}

#[test]
fn backend_slots_come_from_kinds_and_legacy_names() {
    // Labels travel, slots decide. Routing reads the entry's `kind`;
    // the retired wire values still map, so old offers keep working.
    let cfg = llm_cfg(
        "",
        &[
            ("google", "gemini", "", ""),
            ("tokenharbor", "openai", "", ""),
            ("my-gateway", "weird-kind", "", ""),
            ("ollama", "ollama", "", ""),
        ],
    );
    for (id, slot) in [
        ("google", "gemini"),
        ("tokenharbor", "openai"),
        ("my-gateway", "openai"),
        ("ollama", "ollama"),
        ("gemini", "gemini"),
        ("local", "ollama"),
        ("openrouter", "openai"),
    ] {
        assert_eq!(cfg.backend_for(id).as_deref(), Some(slot), "{id}");
    }
    for id in ["", "watson", "opencode"] {
        assert_eq!(cfg.backend_for(id), None, "{id} names nothing usable");
    }
}

#[test]
fn the_active_key_travels_never_a_neighbour() {
    // The outage: active TokenHarbor plus a stocked OpenRouter entry sent
    // OpenRouter's key to tokenharbor.ai — a 401 from the wrong issuer
    // that reads exactly like a revoked key.
    let cfg = llm_cfg(
        "tokenharbor",
        &[
            ("google", "gemini", "g-key", "gem"),
            ("openrouter", "openai", "o-key", "o-model"),
            ("tokenharbor", "openai", "t-key", "th-model"),
        ],
    );
    let creds = cfg.credentials().for_stage(
        bm_proto::Stage::Digest,
        &cfg.offer_analyzer(&Settings::default()).1.backend,
        "vieneu",
    );
    assert_eq!(creds.pairs(), vec![("OPENROUTER_API_KEY", "t-key")]);
    // …and the Gemini slot still finds its own key for a gemini render.
    let render = cfg
        .credentials()
        .for_stage(bm_proto::Stage::Render, "gemini", "gemini");
    assert_eq!(render.pairs(), vec![("GEMINI_API_KEY", "g-key")]);
}

#[test]
fn llm_offer_carries_only_the_active_provider() {
    let mut cfg = llm_cfg(
        "google",
        &[
            ("google", "gemini", "g-key", "gemini-3.5-flash"),
            ("tokenharbor", "openai", "", ""),
        ],
    );
    let (analyzer, a) = cfg.offer_analyzer(&Settings::default());
    assert_eq!(analyzer, "google");
    assert_eq!(a.analyze_models, Some(vec!["gemini-3.5-flash".into()]));
    assert_eq!(a.backend, "gemini");
    let creds = cfg
        .credentials()
        .for_stage(bm_proto::Stage::Digest, &a.backend, "vieneu");
    assert_eq!(
        creds.pairs(),
        vec![("GEMINI_API_KEY", "g-key")],
        "the digest offer carries its key and nothing else"
    );
    // Switching provider switches the next offer — that is the whole
    // sync: the id, key, model and slot travel per task.
    cfg.active = "tokenharbor".into();
    cfg.providers.get_mut("tokenharbor").unwrap().api_key = "t-key".into();
    cfg.providers.get_mut("tokenharbor").unwrap().model = "th-model".into();
    cfg.providers.get_mut("tokenharbor").unwrap().base_url = "https://th.example/v1".into();
    let (analyzer, a) = cfg.offer_analyzer(&Settings::default());
    assert_eq!(analyzer, "tokenharbor");
    assert_eq!(a.backend, "openai");
    assert_eq!(a.openrouter_model, "th-model");
    assert_eq!(a.openrouter_url, "https://th.example/v1");
    let creds = cfg
        .credentials()
        .for_stage(bm_proto::Stage::Digest, &a.backend, "vieneu");
    assert_eq!(creds.pairs(), vec![("OPENROUTER_API_KEY", "t-key")]);
}

#[test]
fn llm_seed_migrates_legacy_settings_once() {
    let _g = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("bm-llm-seed{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    let settings = Settings {
        analyzer: "gemini".into(),
        analyze_models: vec!["gemini-3.5-flash-lite".into()],
        ..Settings::default()
    };
    std::env::set_var("BM_LLM_SEED_TEST_G", "seed-key");
    let saved_g = std::env::var("GEMINI_API_KEY").ok();
    std::env::set_var("GEMINI_API_KEY", "seed-key");
    let cfg = LlmConfig::load_or_seed(&dir, &settings);
    assert_eq!(cfg.active, "gemini");
    assert_eq!(cfg.providers["gemini"].model, "gemini-3.5-flash-lite");
    assert!(LlmConfig::path(&dir).is_file(), "the seed is persisted");
    std::env::remove_var("BM_LLM_SEED_TEST_G");
    if let Some(k) = saved_g {
        std::env::set_var("GEMINI_API_KEY", k);
    } else {
        std::env::remove_var("GEMINI_API_KEY");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn llm_seed_reads_the_retired_env_file_once() {
    // The file is retired — nothing loads it at startup — but its keys
    // are still the operator's, so the one-time seed carries them over.
    // Process env wins over the file.
    let dir = std::env::temp_dir().join(format!("bm-llm-env{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::write(
        dir.join(".env"),
        "# legacy\nGEMINI_API_KEY=\"file-key\"\nOPENROUTER_API_KEY=file-or-key\n",
    )
    .unwrap();
    let _g = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved_g = std::env::var("GEMINI_API_KEY").ok();
    let saved_or = std::env::var("OPENROUTER_API_KEY").ok();
    std::env::remove_var("GEMINI_API_KEY");
    std::env::remove_var("OPENROUTER_API_KEY");
    let cfg = LlmConfig::load_or_seed(&dir, &Settings::default());
    assert_eq!(cfg.providers["gemini"].api_key, "file-key");
    assert_eq!(cfg.providers["openai"].api_key, "file-or-key");
    // Second load reads the seeded file, not the legacy one.
    std::fs::remove_file(dir.join(".env")).unwrap();
    let again = LlmConfig::load_or_seed(&dir, &Settings::default());
    assert_eq!(again.providers["gemini"].api_key, "file-key");
    if let Some(k) = saved_g {
        std::env::set_var("GEMINI_API_KEY", k);
    }
    if let Some(k) = saved_or {
        std::env::set_var("OPENROUTER_API_KEY", k);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_shipped_default_names_no_key_and_no_model() {
    // The template a fresh clone copies: endpoints only. A default key
    // would be a leaked secret and a default model a choice the operator
    // never made — both are set with `L`, never shipped.
    let root =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../llm.default.json");
    let cfg: LlmConfig =
        read_json(&root).expect("llm.default.json parses — if you moved it, move this test");
    assert!(cfg.active.is_empty());
    for (id, e) in &cfg.providers {
        assert!(e.api_key.is_empty(), "{id} ships a key");
        assert!(e.model.is_empty(), "{id} ships a model");
        assert!(!e.base_url.trim().is_empty(), "{id} has no endpoint");
        assert!(
            ["gemini", "openai", "ollama"].contains(&e.kind.as_str()),
            "{id} ships kind {:?}, which routes nowhere",
            e.kind
        );
    }
    assert_eq!(
        cfg.providers["tokenharbor"].base_url, "https://tokenharbor.ai/v1",
        "the OpenAI-compatible base, not the full /chat/completions path"
    );
}

#[test]
fn llm_load_backfills_kinds_from_the_shipped_file() {
    // Files written before `kind` existed carry keys and models but no
    // routing info. Loading restores it from the shipped data — same id,
    // else same endpoint — instead of stranding them on the default path.
    let dir = std::env::temp_dir().join(format!("bm-llm-kind{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::write(
        dir.join("llm.default.json"),
        r#"{"active":"","providers":{"google":{"kind":"gemini","base_url":"https://g.example","api_key":"","model":""}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join(".bm/llm.json"),
        r#"{"active":"google","providers":{"google":{"base_url":"https://g.example","api_key":"k","model":"m"}}}"#,
    )
    .unwrap();
    let cfg = LlmConfig::load(&dir);
    assert_eq!(cfg.kind_of("google"), LlmKind::Gemini);
    assert_eq!(cfg.backend_for("google").as_deref(), Some("gemini"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn llm_load_falls_back_to_the_shipped_default() {
    // No `.bm/llm.json` and no tracked file in this temp root: empty.
    // With a `llm.default.json` beside it: that file's content, and
    // nothing else — the file is the whole roster.
    let dir = std::env::temp_dir().join(format!("bm-llm-fallback{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bare = LlmConfig::load(&dir);
    assert!(bare.active.is_empty());
    assert!(bare.providers.is_empty());
    std::fs::write(
        dir.join("llm.default.json"),
        r#"{"active":"","providers":{"mybox":{"kind":"ollama","base_url":"http://x:11434","api_key":"","model":""}}}"#,
    )
    .unwrap();
    let cfg = LlmConfig::load(&dir);
    assert_eq!(cfg.providers["mybox"].base_url, "http://x:11434");
    assert_eq!(cfg.providers.len(), 1, "no compiled-in ids are merged in");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_inductors_analyzer_settings_win_over_the_boxes_own() {
    // The outage: a provisioned worker has no `.bm/settings.json` —
    // provisioning copies the sources bundle and never `.bm/` — so
    // `Settings::load` hands back the compiled default.
    let remote_box = Settings::default();
    let inductor = Settings {
        analyze_models: vec!["gemini-3.5-flash-lite".into()],
        openrouter_model: "someone/else".into(),
        ..Settings::default()
    };
    let effective = remote_box.with_analyzer_settings(&inductor.analyzer_settings());
    assert_eq!(effective.analyze_models, vec!["gemini-3.5-flash-lite"]);
    assert_eq!(effective.openrouter_model, "someone/else");
}

#[test]
fn an_inductor_with_no_opinion_leaves_the_boxes_own_analyzer_alone() {
    // An older inductor sends no block at all. Every local value survives,
    // which is what keeps either side upgradable on its own.
    let boxed = Settings {
        analyze_models: vec!["mine-1".into(), "mine-2".into()],
        ollama_url: "http://elsewhere:11434".into(),
        ..Settings::default()
    };
    let same = boxed.with_analyzer_settings(&bm_proto::AnalyzerSettings::default());
    assert_eq!(same.analyze_models, vec!["mine-1", "mine-2"]);
    assert_eq!(same.ollama_url, "http://elsewhere:11434");
}

#[test]
fn a_deliberately_empty_chain_clears_the_boxes_own() {
    let boxed = Settings {
        analyze_models: vec!["stale-1".into()],
        ..Settings::default()
    };
    let cleared = boxed.with_analyzer_settings(&bm_proto::AnalyzerSettings {
        analyze_models: Some(vec![]),
        ..Default::default()
    });
    assert!(
        cleared.analyze_models.is_empty(),
        "{:?}",
        cleared.analyze_models
    );
}

#[test]
fn the_render_batch_defaults_to_five_and_a_saved_value_wins() {
    // Three ways the setting can arrive, and the rule for each:
    //   * absent from settings.json  → five (the compiled default)
    //   * present                    → that value, not the default
    //   * nonsense                   → clamped, never obeyed and never fatal
    let omitted: Settings = serde_json::from_str(r#"{"engine":"vieneu"}"#).unwrap();
    assert_eq!(omitted.render_batch, DEFAULT_RENDER_BATCH);
    assert_eq!(omitted.render_batch(), 5, "and the scheduler sees five");

    let chosen: Settings = serde_json::from_str(r#"{"render_batch":3}"#).unwrap();
    assert_eq!(
        chosen.render_batch(),
        3,
        "a workspace value overrides the default"
    );

    // Zero is the deadlock the clamp exists for: an offer of no takes
    // assigns no row, so the chapter would never leave Pending and nothing
    // anywhere would say why.
    let zero: Settings = serde_json::from_str(r#"{"render_batch":0}"#).unwrap();
    assert_eq!(zero.render_batch(), 1, "zero would offer nothing at all");

    let absurd: Settings = serde_json::from_str(r#"{"render_batch":100000}"#).unwrap();
    assert_eq!(
        absurd.render_batch(),
        MAX_RENDER_BATCH as usize,
        "an absurd batch is a lease held on one box for hours"
    );
}

#[test]
fn the_sidecar_thread_override_is_opt_in_and_a_typo_never_fails_a_run() {
    let _g = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var("BM_TTS_THREADS").ok();
    std::env::remove_var("BM_TTS_THREADS");
    assert_eq!(tts_threads(), 0, "unset means the sidecar picks its own");
    std::env::set_var("BM_TTS_THREADS", " 8 ");
    assert_eq!(tts_threads(), 8, "surrounding space is forgiven");
    std::env::set_var("BM_TTS_THREADS", "half");
    assert_eq!(tts_threads(), 0, "a typo falls back, never fails a run");
    match saved {
        Some(v) => std::env::set_var("BM_TTS_THREADS", v),
        None => std::env::remove_var("BM_TTS_THREADS"),
    }
}

#[test]
fn a_saved_render_batch_round_trips_through_the_file() {
    // The value has to survive `save`/`load`, because that file is the
    // single source the run screen previews and the next backend boots with.
    let dir = std::env::temp_dir().join("bm-settings-batch");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("settings.json");
    let s = Settings {
        render_batch: 4,
        ..Default::default()
    };
    s.save(&p).unwrap();
    assert_eq!(Settings::load(&p).render_batch(), 4);
    // And a workspace that never mentions it still gets the default.
    std::fs::write(&p, r#"{"engine":"gemini"}"#).unwrap();
    assert_eq!(
        Settings::load(&p).render_batch(),
        DEFAULT_RENDER_BATCH as usize
    );
}

#[test]
fn the_overlay_carries_only_the_analyzer() {
    // `url_template`, the chapter range, the control port and the ssh
    // defaults are the inductor's business. A task offer is not a channel
    // for them, and this path must not become one.
    let boxed = Settings {
        url_template: "https://mine/{n}".into(),
        count: 7,
        ..Settings::default()
    };
    let inductor = Settings {
        url_template: "https://theirs/{n}".into(),
        count: 99,
        ..Settings::default()
    };
    let effective = boxed.with_analyzer_settings(&inductor.analyzer_settings());
    assert_eq!(effective.url_template, "https://mine/{n}");
    assert_eq!(effective.count, 7);
}
