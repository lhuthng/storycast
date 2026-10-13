use super::*;
use bm_core::audio_pool::PoolKind;

// The parsers below read text an operator types. `cargo test` is where a
// surprising shape should surface — a `:mix` line, a pasted range — not a
// worker mid-run. Each test pins one behaviour; the last one only insists that
// none of them panics on garbage.

#[test]
fn range_ignores_extra_tokens_and_refuses_non_numbers() {
    assert_eq!(parse_range("1 1").unwrap(), (1, 1));
    assert_eq!(
        parse_range(" 2\t3\n").unwrap(),
        (2, 3),
        "any whitespace splits"
    );
    assert_eq!(
        parse_range("5 3 9").unwrap(),
        (5, 3),
        "tokens past the count are ignored, not rejected"
    );
    assert_eq!(
        parse_range("+7 2").unwrap(),
        (7, 2),
        "a leading + is a sign"
    );
    assert_eq!(parse_range("0 1").unwrap(), (0, 1), "chapter 0 is a number");

    assert!(parse_range("").unwrap_err().contains("expected"));
    assert!(parse_range("5").unwrap_err().contains("expected"));
    assert!(parse_range("5 0").unwrap_err().contains("at least 1"));
    assert!(parse_range("-1 2")
        .unwrap_err()
        .contains("not a chapter number"));
    assert!(
        parse_range("1 4294967296")
            .unwrap_err()
            .contains("not a number"),
        "u32 overflow is refused, not truncated"
    );
    assert!(parse_range("1 1.5").unwrap_err().contains("not a number"));
}

#[test]
fn run_config_reads_aliases_and_folds_commas() {
    // Retired provider aliases still resolve to the live id.
    assert_eq!(parse_run_config("1 1 openrouter", "x").unwrap().2, "openai");
    assert_eq!(parse_run_config("1 1 local", "x").unwrap().2, "ollama");
    assert_eq!(
        parse_run_config("1 1 gemini", "x").unwrap().3,
        None,
        "no models is not an empty list"
    );
    // Empty pieces between commas drop rather than becoming a model named "".
    let (_, _, a, m) = parse_run_config("1 1 gemini a,,b", "x").unwrap();
    assert_eq!(a, "gemini");
    assert_eq!(m.unwrap(), vec!["a", "b"]);
    // Whitespace around a comma is trimmed.
    assert_eq!(
        parse_run_config("1 1 gemini  a , b ", "x")
            .unwrap()
            .3
            .unwrap(),
        vec!["a", "b"]
    );

    assert!(
        parse_run_config("1 1 GEMINI", "x")
            .unwrap_err()
            .contains("unknown"),
        "the slot names are case-sensitive"
    );
    assert!(parse_run_config("1 1 gemini ,,", "x")
        .unwrap_err()
        .contains("empty"));
    assert!(parse_run_config("1 1 gemini a a", "x")
        .unwrap_err()
        .contains("comma-separated"));
}

#[test]
fn mix_config_accepts_scientific_forms_and_refuses_the_rest() {
    assert_eq!(parse_mix_config("1e0 1 1 1").unwrap().0, 1.0);
    assert_eq!(parse_mix_config(".5 1 1 1").unwrap().0, 0.5);
    assert!(parse_mix_config("").unwrap_err().contains("expected"));
    assert!(parse_mix_config("1 1 1 1 1")
        .unwrap_err()
        .contains("expected"));
    for input in ["NaN 1 1 1", "inf 1 1 1", "-inf 1 1 1"] {
        assert!(
            parse_mix_config(input).unwrap_err().contains("speed"),
            "{input}"
        );
    }
}

#[test]
fn render_batch_is_a_bounded_whole_number() {
    assert_eq!(parse_render_batch(" 12 ").unwrap(), 12);
    assert_eq!(parse_render_batch("+7").unwrap(), 7);
    assert_eq!(
        parse_render_batch(&bm_core::config::MAX_RENDER_BATCH.to_string()).unwrap(),
        bm_core::config::MAX_RENDER_BATCH
    );
    assert!(parse_render_batch("0").unwrap_err().contains("at least 1"));
    assert!(parse_render_batch("-1")
        .unwrap_err()
        .contains("not a number of takes"));
    assert!(parse_render_batch("5.0")
        .unwrap_err()
        .contains("not a number of takes"));
    assert!(
        parse_render_batch(&(bm_core::config::MAX_RENDER_BATCH + 1).to_string())
            .unwrap_err()
            .contains("at most")
    );
}

#[test]
fn split_args_treats_only_double_quotes_as_syntax() {
    assert_eq!(split_args("a\tb\nc"), vec!["a", "b", "c"]);
    assert_eq!(
        split_args("\"a b\"c d"),
        vec!["a bc", "d"],
        "text after a closed quote continues the same argument"
    );
    assert_eq!(
        split_args("a\\\"b"),
        vec!["a\\b"],
        "a backslash is literal and does not escape the quote"
    );
    assert_eq!(
        split_args("'single'"),
        vec!["'single'"],
        "single quotes are ordinary characters"
    );
}

#[test]
fn command_key_refuses_out_of_range_arguments() {
    assert_eq!(command_key("up"), Some(Command::AwsUp { count: 1 }));
    assert_eq!(command_key("up 0"), None, "zero boxes is not a launch");
    assert_eq!(command_key("up abc"), None);
    assert_eq!(command_key("up 3"), Some(Command::AwsUp { count: 3 }));

    assert_eq!(
        command_key("threads 1"),
        Some(Command::TtsThreads {
            threads: Some(Some(1))
        })
    );
    assert_eq!(
        command_key("threads clear"),
        Some(Command::TtsThreads {
            threads: Some(None)
        })
    );
    assert_eq!(command_key("threads 65"), None, "the cap is 64");
    assert_eq!(command_key("threads 0"), None);

    assert_eq!(command_key("retry 0"), None, "chapter 0 names nothing");
    assert_eq!(
        command_key("retry crawl 3"),
        Some(Command::Retry {
            stage: Some(Stage::Crawl),
            chapter: Some(3)
        })
    );
    assert_eq!(command_key("retry bogus 3"), None);

    assert_eq!(
        command_key("speaker 1 2 a"),
        None,
        "a speaker line needs four"
    );
    assert_eq!(command_key(":"), None);
    assert_eq!(command_key(""), None);
    assert_eq!(command_key("go"), Some(Command::Dispatch { go: true }));
}

#[test]
fn sound_entry_refuses_the_shapes_that_would_break_a_pool() {
    let effect = PoolKind::Effect;
    let ok = sound::parse_entry(
        effect,
        "name=rain files=effects/rain-1.mp3 tags=storm",
        None,
    )
    .unwrap();
    assert_eq!(ok.0, "rain");
    assert_eq!(ok.1.files, vec!["effects/rain-1.mp3"]);
    assert_eq!(ok.1.tags, vec!["storm"]);

    assert!(sound::parse_entry(effect, "", None)
        .unwrap_err()
        .contains("name= is required"));
    assert!(sound::parse_entry(effect, "name= files=x tags=t", None)
        .unwrap_err()
        .contains("name is empty"));
    assert!(
        sound::parse_entry(effect, "name=_note files=x tags=t", None)
            .unwrap_err()
            .contains("start with")
    );
    assert!(sound::parse_entry(effect, "name=a/b files=x tags=t", None)
        .unwrap_err()
        .contains("path separator"));
    assert!(sound::parse_entry(effect, "oops", None)
        .unwrap_err()
        .contains("not key=value"));
    assert!(sound::parse_entry(effect, "foo=1", None)
        .unwrap_err()
        .contains("is not a field"));
    assert!(sound::parse_entry(effect, "name=a files= tags=t", None)
        .unwrap_err()
        .contains("files is empty"));
    assert!(sound::parse_entry(effect, "name=a files=x tags=", None)
        .unwrap_err()
        .contains("tags is empty"));
    assert!(
        sound::parse_entry(effect, "name=a files=x tags=t level=0", None)
            .unwrap_err()
            .contains("level must be")
    );
    assert!(
        sound::parse_entry(effect, "name=a files=x tags=t looped=maybe", None)
            .unwrap_err()
            .contains("not true or false")
    );
    // A field another layer owns is not silently accepted here.
    assert!(
        sound::parse_entry(PoolKind::Music, "name=a files=x tags=t mode=hit", None)
            .unwrap_err()
            .contains("is not a field")
    );
    assert!(
        sound::parse_entry(PoolKind::Inject, "name=a files=x tags=t mode=nope", None)
            .unwrap_err()
            .contains("mode")
    );

    // A repeated key is last-wins, not an error.
    assert_eq!(
        sound::parse_entry(effect, "name=a name=b files=x tags=t", None)
            .unwrap()
            .0,
        "b"
    );
    // Renaming in place may keep the name; changing it must not.
    assert!(sound::parse_entry(effect, "name=a files=x tags=t", Some("a")).is_ok());
    assert!(
        sound::parse_entry(effect, "name=b files=x tags=t", Some("a"))
            .unwrap_err()
            .contains("cannot become")
    );
}

#[test]
fn level_prompt_empty_clears_and_zero_is_refused() {
    assert_eq!(sound::parse_level_prompt("").unwrap(), None);
    assert_eq!(sound::parse_level_prompt("   ").unwrap(), None);
    assert_eq!(sound::parse_level_prompt("1").unwrap(), Some(1.0));
    assert!(sound::parse_level_prompt("0")
        .unwrap_err()
        .contains("level must be"));
    assert!(sound::parse_level_prompt("oops")
        .unwrap_err()
        .contains("is not a number"));
}

#[test]
fn no_parser_panics_on_arbitrary_text() {
    // The point of the file: every one of these takes bytes a person typed,
    // and none of them may index out of bounds, unwrap, or divide by zero on a
    // shape nobody expected. A panic fails the test, which is the assertion.
    let nasty = [
        "",
        " ",
        "\0",
        "\u{feff}",
        "0",
        "-1",
        "+",
        ".",
        "..",
        "e",
        "NaN",
        "inf",
        "-inf",
        "1e9999",
        "4294967296",
        "-4294967296",
        "99999999999999999999999999",
        ":",
        "\"",
        "\"\"",
        "\"unterminated",
        "\\",
        "\\\\",
        "a=b",
        "name=",
        "files=",
        "tags=",
        "=",
        "==",
        "name=a files=x tags=t",
        "🎬 4 🎬",
        "\t\n\r",
        "up -1",
        "threads -1",
        "retry",
        "speaker",
    ];
    for s in nasty {
        let _ = parse_range(s);
        let _ = parse_run_config(s, "opencode");
        let _ = parse_mix_config(s);
        let _ = parse_render_batch(s);
        let _ = sound::parse_level_prompt(s);
        let _ = split_args(s);
        let _ = command_key(s);
        for layer in [PoolKind::Effect, PoolKind::Music, PoolKind::Inject] {
            let _ = sound::parse_entry(layer, s, None);
            let _ = sound::parse_entry(layer, s, Some("existing"));
        }
    }
}

#[test]
fn range_reads_only_ascii_digits_and_treats_unicode_space_as_space() {
    // Arabic-Indic digits are digits to the eye, not to `u32::from_str`.
    assert!(parse_range("١٢ ٣").is_err());
    assert!(parse_range("1_000 2").is_err(), "no digit separators");
    assert_eq!(parse_range("4294967295 1").unwrap().0, u32::MAX);
    assert!(parse_range("4294967296 1").is_err(), "one past u32::MAX");
    // NBSP is Unicode whitespace, so `split_whitespace` takes it as a separator.
    assert_eq!(parse_range("\u{a0}1 2").unwrap(), (1, 2));
}

#[test]
fn run_config_rejects_a_non_breaking_space_inside_a_model() {
    // `char::is_whitespace` is Unicode, so a pasted NBSP counts as the space
    // that the comma rule forbids.
    assert!(parse_run_config("1 1 gemini a\u{a0}b", "x")
        .unwrap_err()
        .contains("comma-separated"));
    // Duplicates are kept: the chain is what the operator typed.
    assert_eq!(
        parse_run_config("1 1 gemini a,a", "x").unwrap().3.unwrap(),
        vec!["a", "a"]
    );
    // A trailing tab after the analyzer is just whitespace.
    assert_eq!(parse_run_config("1 1 gemini\t", "x").unwrap().3, None);
}

#[test]
fn mix_config_survives_every_way_to_spell_not_a_number() {
    for bad in ["nan", "NAN", "NaN", "inf", "Infinity", "-inf"] {
        let input = format!("{bad} 1 1 1");
        assert!(
            parse_mix_config(&input).unwrap_err().contains("speed"),
            "{input}"
        );
    }
    // -0.0 is below the floor, and the inclusive bounds are inclusive.
    assert!(parse_mix_config("-0.0 1 1 1")
        .unwrap_err()
        .contains("speed"));
    assert_eq!(parse_mix_config("2.0 1 1 1").unwrap().0, 2.0);
    assert!(parse_mix_config("2.0000001 1 1 1")
        .unwrap_err()
        .contains("speed"));
    // NBSP splits like any whitespace.
    assert_eq!(
        parse_mix_config("1\u{a0}1\u{a0}1").unwrap(),
        (1.0, 1.0, 1.0, None)
    );
}

#[test]
fn render_batch_needs_a_plain_whole_number() {
    assert!(parse_render_batch("  ")
        .unwrap_err()
        .contains("not a number"));
    assert!(parse_render_batch("+0").unwrap_err().contains("at least 1"));
    assert!(parse_render_batch("one")
        .unwrap_err()
        .contains("not a number"));
    assert!(parse_render_batch("١٢")
        .unwrap_err()
        .contains("not a number"));
}

#[test]
fn split_args_treats_every_unicode_space_as_a_separator() {
    assert_eq!(split_args("a\u{a0}b"), vec!["a", "b"]);
    assert_eq!(split_args("a\u{2028}b"), vec!["a", "b"], "a line separator");
    // Runs of quotes collapse; an empty run still yields one empty argument.
    assert_eq!(split_args("\"\"\"\""), vec![""]);
    assert_eq!(split_args("a\"\"b"), vec!["ab"]);
    assert_eq!(split_args("\"a b\" \"c d\" e"), vec!["a b", "c d", "e"]);
    // A quote that opens at the very first character takes the whole tail.
    assert_eq!(split_args("\"a b"), vec!["a b"]);
}

#[test]
fn command_key_reads_the_whole_argument_surface() {
    assert_eq!(command_key("UP"), Some(Command::AwsUp { count: 1 }));
    assert_eq!(command_key("down"), Some(Command::AwsDown { force: false }));
    assert_eq!(
        command_key("down force"),
        Some(Command::AwsDown { force: true })
    );
    assert_eq!(command_key("down nonsense"), None);

    assert_eq!(
        command_key("threads +5"),
        Some(Command::TtsThreads {
            threads: Some(Some(5))
        })
    );
    assert_eq!(
        command_key("threads 64"),
        Some(Command::TtsThreads {
            threads: Some(Some(64))
        }),
        "64 is the top of the range"
    );
    assert_eq!(command_key("threads 1.0"), None);

    assert_eq!(
        command_key("retry render 24"),
        Some(Command::Retry {
            stage: Some(Stage::Render),
            chapter: Some(24)
        })
    );
    assert_eq!(command_key("retry 24 extra"), None);
    assert_eq!(command_key("speaker 0 5 a b"), None, "chapter 0");
    assert_eq!(command_key("speaker 1 0 a b"), None, "segment 0");
    assert_eq!(command_key("merge A"), None, "one name absorbs nothing");
    assert_eq!(command_key("merge \"\" B"), None, "no survivor");
    assert_eq!(command_key("merge A \"\""), None, "a blank absorbed name");
    assert_eq!(command_key("ws"), Some(Command::WorkspacePick));
    assert_eq!(
        command_key("ws book two"),
        Some(Command::Workspace {
            prefill: "book two".into()
        })
    );
    assert_eq!(command_key("bogus"), None);
}

#[test]
fn sound_entry_covers_lists_bools_and_each_layers_fields() {
    let effect = PoolKind::Effect;
    assert_eq!(
        sound::parse_entry(effect, "name=a files=x,y tags=t", None)
            .unwrap()
            .1
            .files,
        vec!["x", "y"]
    );
    assert!(sound::parse_entry(effect, "name=a files=,, tags=t", None)
        .unwrap_err()
        .contains("files is empty"));
    assert_eq!(
        sound::parse_entry(effect, "name=a files=x tags=t,t", None)
            .unwrap()
            .1
            .tags,
        vec!["t", "t"],
        "tags are kept as typed, duplicates included"
    );
    // A quote or a backslash in a name is refused; the name is a key.
    assert!(sound::parse_entry(effect, "name=a\"b files=x tags=t", None).is_err());
    assert!(sound::parse_entry(effect, "name=a\\b files=x tags=t", None).is_err());

    // Booleans: the spellings, and only them.
    assert!(
        sound::parse_entry(effect, "name=a files=x tags=t looped=yes", None)
            .unwrap()
            .1
            .looped
    );
    assert!(
        !sound::parse_entry(effect, "name=a files=x tags=t looped=0", None)
            .unwrap()
            .1
            .looped
    );
    assert!(
        sound::parse_entry(effect, "name=a files=x tags=t looped=TRUE", None)
            .unwrap_err()
            .contains("not true or false")
    );

    // Levels are inclusive at both ends.
    assert!(sound::parse_entry(effect, "name=a files=x tags=t level=0.01", None).is_ok());
    assert!(sound::parse_entry(effect, "name=a files=x tags=t level=4.0", None).is_ok());
    assert!(
        sound::parse_entry(effect, "name=a files=x tags=t level=4.0001", None)
            .unwrap_err()
            .contains("level must be")
    );

    // The inject layer owns hold/dur_s/mode, and each must be positive/legal.
    let inject = PoolKind::Inject;
    assert!(
        sound::parse_entry(inject, "name=a files=x tags=t hold=0", None)
            .unwrap_err()
            .contains("greater than 0")
    );
    assert!(
        sound::parse_entry(inject, "name=a files=x tags=t dur_s=-1", None)
            .unwrap_err()
            .contains("greater than 0")
    );
    assert!(
        sound::parse_entry(inject, "name=a files=x tags=t mode=", None)
            .unwrap_err()
            .contains("mode")
    );
    assert!(
        sound::parse_entry(inject, "name=a files=x tags=t nope=1", None)
            .unwrap_err()
            .contains("is not a field")
    );

    // An edit keeps the existing name when the line names none.
    assert_eq!(
        sound::parse_entry(effect, "files=x tags=t", Some("old"))
            .unwrap()
            .0,
        "old"
    );
}
