use super::attribution::build_attribution_prompt;
use super::quotes::build_repair_prompt;
use super::*;
use crate::paths::Layout;

const BROKEN: &str = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n\"Ta sẽ đi.\nHắn quay lưng bước đi.";

/// A layout whose `prompts/` tree resolves to the shipped adapter templates,
fn workspace() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("bm-repair-prompt-{}", std::process::id()))
}

#[test]
fn the_repair_pass_renders_from_the_prompt_file_not_a_hardcoded_string() {
    // The template is a file an operator can edit and a language can
    for adapter in ["vi-VN", "xianxia-en-US"] {
        let root = workspace().join(adapter);
        // The flat `prompts/` tree, which `prompts_base` falls back to when a
        let prompts = root.join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../../adapters/{adapter}/prompts/repair.txt"));
        std::fs::copy(&source, prompts.join("repair.txt")).expect("the template ships");

        let layout = Layout::resolve(&root).unwrap();
        let findings = quote_findings(BROKEN);
        assert!(!findings.is_empty(), "the third paragraph never closes");
        let complaint = format!(
            "  - {}: paragraph {}: {}",
            findings[0].kind,
            findings[0].paragraph,
            head_chars(&findings[0].text, 120)
        );
        let prompt = build_repair_prompt(&layout, BROKEN, &complaint).unwrap();

        // The scan's own finding reaches the model, and the placeholders
        assert!(
            prompt.contains(&format!("paragraph {}", findings[0].paragraph)),
            "{prompt}"
        );
        assert!(prompt.contains("unclosed quote"), "{prompt}");
        assert!(
            !prompt.contains("{fault_line}"),
            "an unrendered placeholder"
        );
        assert!(
            !prompt.contains("{chapter_text}"),
            "an unrendered placeholder"
        );
        assert!(
            prompt.contains(BROKEN),
            "the whole chapter is proofread, not a window"
        );
        // What the file cannot be allowed to talk its way out of.
        assert!(prompt.contains("---REPAIR OUTPUT CONTRACT---"), "{prompt}");
        assert!(
            prompt.contains("REJECTED"),
            "the verifier's rule must be stated"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}

#[test]
fn the_attribution_contract_speaks_the_adapters_language() {
    use crate::adapter;
    // The output contract is code-side, so it is where a hardcoded
    // language could override every template — and did: "3-8 word
    // Vietnamese chapter title" reached an English book's prompt with its
    let root = workspace().join("contract-language");
    let home = root.join("adapters/jnovel-en-US");
    let prompts = home.join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../adapters/jnovel-en-US/prompts/analyze.txt");
    std::fs::copy(&src, prompts.join("analyze.txt")).expect("the shipped template");
    std::fs::write(
        adapter::path(&home),
        r#"{ "pack": "", "language": "en-US", "engine": "" }"#,
    )
    .unwrap();
    let layout = Layout {
        adapter: "jnovel-en-US".into(),
        ..Layout::new(&root)
    };

    let prepared = prepare_chapter("Chapter 1: Maomao\n\n\"Yes.\"");
    let prompt = build_attribution_prompt(&layout, &json!({}), &prepared, None, None).unwrap();
    assert!(prompt.contains("in en-US"), "{prompt}");
    assert!(
        !prompt.contains("the chapter's own language"),
        "a declared language answers, it does not fall back: {prompt}"
    );
    assert!(
        !prompt.contains("{content_language}"),
        "unrendered placeholder"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_missing_template_is_fatal_rather_than_a_silent_fallback() {
    // The failure this pass exists to prevent is a broken chapter digested
    let root = workspace().join("no-template");
    std::fs::create_dir_all(root.join("data")).unwrap();
    let layout = Layout::resolve(&root).unwrap();
    let err = build_repair_prompt(&layout, BROKEN, "any complaint").unwrap_err();
    assert!(err.to_string().contains("repair.txt"), "{err}");
    std::fs::remove_dir_all(&root).ok();
}
