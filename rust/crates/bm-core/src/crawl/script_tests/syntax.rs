//! The cheap gate: a bundled crawler that does not compile, or that defines no
//! `crawl(input)`, fails on a worker mid-range. `make test` is where it should
//! surface instead, for every script the registry names.

use super::*;

/// Read a registry script (`crawlers/known/…`) from the checkout.
fn registered_source(script: &str) -> String {
    let root = format!("{}/../../..", env!("CARGO_MANIFEST_DIR"));
    let path = format!("{root}/{script}");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

#[test]
fn every_registered_crawler_compiles_and_defines_crawl() {
    let mut checked = 0;
    for site in known_sites().iter().filter(|s| s.is_crawlable()) {
        let name = site.script;
        let source = registered_source(name);
        if name.ends_with(".js") {
            let rt = rquickjs::Runtime::new().expect("a QuickJS runtime");
            let ctx = rquickjs::Context::full(&rt).expect("a QuickJS context");
            let ok = ctx.with(|ctx| -> Result<bool, String> {
                ctx.eval::<(), _>(source.clone())
                    .map_err(|e| e.to_string())?;
                ctx.eval::<bool, _>("typeof crawl === 'function'")
                    .map_err(|e| e.to_string())
            });
            assert!(
                ok.unwrap_or_else(|e| panic!("{name} is not valid JavaScript: {e}")),
                "{name} defines no crawl(input)"
            );
        } else {
            let lua = mlua::Lua::new();
            lua.load(&source)
                .set_name(name)
                .exec()
                .unwrap_or_else(|e| panic!("{name} is not valid Lua: {e}"));
            let defined = lua
                .load("return type(crawl) == 'function'")
                .eval::<bool>()
                .expect("probing crawl()");
            assert!(defined, "{name} defines no crawl(input)");
        }
        checked += 1;
    }
    assert!(
        checked > 0,
        "no crawlable site in the registry — the list shrank"
    );
}
