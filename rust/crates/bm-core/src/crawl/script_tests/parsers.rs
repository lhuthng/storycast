//! Cheap gates on the crawl inputs that are parsed rather than executed: which
//! engine a path selects, what a blocked-class name means, and how `{n}`
//! expands. None needs a page or an interpreter.

use super::*;
use crate::crawl::engine::EngineKind;

#[test]
fn an_engine_name_is_read_loosely_and_defaults_to_lua() {
    assert_eq!(EngineKind::parse("lua"), Some(EngineKind::Lua));
    assert_eq!(EngineKind::parse("LUA"), Some(EngineKind::Lua));
    assert_eq!(EngineKind::parse(" Lua5.4 "), Some(EngineKind::Lua));
    assert_eq!(EngineKind::parse("lua54"), Some(EngineKind::Lua));
    assert_eq!(EngineKind::parse("js"), Some(EngineKind::Js));
    assert_eq!(EngineKind::parse("MJS"), Some(EngineKind::Js));
    assert_eq!(EngineKind::parse("ecmascript"), Some(EngineKind::Js));
    for bad in ["", "python", "lua 5.4", "js5"] {
        assert_eq!(EngineKind::parse(bad), None, "{bad:?}");
    }

    // A path's extension decides, and anything unrecognised is Lua.
    assert_eq!(EngineKind::from_name("a/b.js"), EngineKind::Js);
    assert_eq!(EngineKind::from_name("a/b.mjs"), EngineKind::Js);
    assert_eq!(EngineKind::from_name("a/b.JavaScript"), EngineKind::Js);
    assert_eq!(EngineKind::from_name("a/b.lua"), EngineKind::Lua);
    for lua in ["a/b", "a.b.c", "trailing.", "", ".lua.tmp"] {
        assert_eq!(EngineKind::from_name(lua), EngineKind::Lua, "{lua:?}");
    }
}

#[test]
fn a_blocked_class_name_is_read_loosely() {
    for (name, want) in [
        ("challenge", BlockedClass::Challenge),
        ("  CAPTCHA ", BlockedClass::Challenge),
        ("cloudflare", BlockedClass::Challenge),
        ("rate-limit", BlockedClass::RateLimit),
        ("ratelimit", BlockedClass::RateLimit),
        ("429", BlockedClass::RateLimit),
        ("login", BlockedClass::LoginRequired),
        ("paywall", BlockedClass::LoginRequired),
        ("js", BlockedClass::JsRequired),
        ("404", BlockedClass::Gone),
        ("410", BlockedClass::Gone),
        ("empty", BlockedClass::Empty),
        ("nonsense", BlockedClass::Unknown),
        ("", BlockedClass::Unknown),
    ] {
        assert_eq!(BlockedClass::parse(name), want, "{name:?}");
    }
    // Retryability is the decision the class exists to make.
    assert!(BlockedClass::RateLimit.retryable());
    assert!(BlockedClass::Challenge.retryable());
    assert!(BlockedClass::Unknown.retryable());
    assert!(!BlockedClass::Gone.retryable());
    assert!(!BlockedClass::Empty.retryable());
    assert!(!BlockedClass::LoginRequired.retryable());
    assert!(!BlockedClass::JsRequired.retryable());
}

#[test]
fn a_url_template_expands_only_the_placeholders_it_understands() {
    assert_eq!(expand_template("ch-{n}.html", 7), "ch-7.html");
    assert_eq!(expand_template("{n:03}", 7), "007");
    assert_eq!(
        expand_template("{n:003}", 7),
        "007",
        "extra leading zeros are the width"
    );
    assert_eq!(
        expand_template("{n:00}", 7),
        "7",
        "width zero is just the number"
    );
    assert_eq!(expand_template("{n}-{n}", 12), "12-12");
    assert_eq!(
        expand_template("chương-{n}", 3),
        "chương-3",
        "text around it is literal"
    );
    assert_eq!(
        expand_template("{n:033}", 7),
        "{n:033}",
        "a width over 32 is not a placeholder"
    );
    assert_eq!(
        expand_template("{n:32}", 7),
        "{n:32}",
        "the width must be zero-padded to count"
    );
    assert_eq!(
        expand_template("{n", 7),
        "{n",
        "an unclosed brace is literal"
    );
    assert_eq!(
        expand_template("{x}", 7),
        "{x}",
        "an unknown name is literal"
    );
    assert_eq!(expand_template("no placeholder", 7), "no placeholder");
}

#[test]
fn the_crawl_parsers_never_panic_on_arbitrary_text() {
    let nasty = [
        "",
        " ",
        "{",
        "}",
        "{}",
        "{n",
        "n}",
        "{n:}",
        "{n:0}",
        "{n:00000000000000000000000000000000000}",
        "{n:99999999999999999999}",
        "{N}",
        "{n:-1}",
        "🎬{n}🎬",
        "\u{a0}",
        "lua\0",
        "ch-{n}-{n}-{n}",
    ];
    for s in nasty {
        let _ = expand_template(s, 0);
        let _ = expand_template(s, u32::MAX);
        let _ = EngineKind::parse(s);
        let _ = EngineKind::from_name(s);
        let _ = BlockedClass::parse(s);
    }
}
