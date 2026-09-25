//! Lua binding for the crawl ABI — the engine the bundled crawlers are in.
//!
//! A fresh `Lua` per run: one chapter, one sandbox. Cheap (the script is a few
//! hundred lines), and it removes the whole class of bug where chapter 34's
//! globals leak into chapter 35's crawl.
//!
//! Lua is a good fit for this job and would be even if nobody had asked for it:
//! tables *are* the request/response objects, `for` loops over a listing read
//! like prose, and there is no event loop to reason about — a scripted fetch is
//! synchronous from the script's point of view while the host drives the async
//! request on a blocking thread.

use anyhow::{anyhow, Result};
use mlua::{HookTriggers, Lua, LuaSerdeExt, Value as LuaValue, VmState};
use serde_json::Value;

use super::{fns, Entry, Program, SharedHost};

/// How often the VM checks the clock. Low enough that a runaway loop cannot
/// hold a worker for more than a fraction of a second, high enough not to
/// dominate the interpreter's time on a long walk.
const CHECK_EVERY: u32 = 100_000;

pub fn run(
    program: &Program,
    entry: Entry,
    host: SharedHost,
    input: &Value,
) -> Result<Option<Value>> {
    let name = program.name.clone();
    let lua = Lua::new();
    sandbox(&lua)?;
    install_hook(&lua, host.clone())?;
    install_fns(&lua, host)
        .map_err(|e| anyhow!("{name}: binding the crawl host ABI failed: {e}"))?;
    lua.load(&program.source)
        .set_name(name.clone())
        .exec()
        .map_err(|e| anyhow!("{name}: {e}"))?;

    let globals = lua.globals();
    // A script without `discover` is the normal single-page case, not an
    // error: `FromLua` refuses a nil global and that refusal is the signal.
    let Ok(func) = globals.get::<mlua::Function>(entry.name()) else {
        return Ok(None);
    };
    let arg = request_arg(&lua, input)
        .map_err(|e| anyhow!("{name}: handing the request to {}(): {e}", entry.name()))?;
    let out: LuaValue = func
        .call(arg)
        .map_err(|e| anyhow!("{name}: {}() failed: {e}", entry.name()))?;
    match out {
        LuaValue::Nil => Ok(None),
        v => {
            let json: Value = lua.from_value(v).map_err(|e| {
                anyhow!(
                    "{name}: {}() returned a value that is not a response table: {e}",
                    entry.name()
                )
            })?;
            Ok(Some(json))
        }
    }
}

/// The request table, with **absent optionals as real `nil`**.
///
/// mlua's serde maps a JSON `null` to its own `NULL` *userdata* rather than to
/// `nil`, so that a `null` round-trips through the boundary intact. That is the
/// right default for data and the wrong one for control flow: `input.url` is
/// `null` on a chapter the index has no URL for, and a script written the
/// obvious way — `if not input.url then error("no URL for ch" …) end` — sees a
/// truthy userdata, sails past its own guard, and hands that userdata to
/// `fetch`.
///
/// So the nulls are walked out of the request before the script sees it. This
/// is the same trap as `challenge()` returning its `NULL` sentinel, and both
/// exist for the same reason: in Lua, `NULL` is truthy, and a contract that
/// documents "nil" while delivering "a truthy userdata" is a contract that
/// trains every reader to write the wrong thing.
fn request_arg(lua: &Lua, input: &Value) -> mlua::Result<LuaValue> {
    let arg = lua.to_value(input)?;
    // Only the fields the contract documents as nullable, so a `null` that means
    // something else is not silently swallowed.
    for field in NULLABLE_FIELDS {
        let Some(table) = arg.as_table() else {
            break;
        };
        let value: LuaValue = table.get(field)?;
        if matches!(value, LuaValue::LightUserData(_)) {
            table.set(field, LuaValue::Nil)?;
        }
    }
    Ok(arg)
}

/// The request fields a script may find absent, and must therefore be able to
/// test for: `crawl.url` is null when nothing computed one.
const NULLABLE_FIELDS: [&str; 2] = ["url", "params"];

/// Remove the standard library's reach outside the process.
///
/// `Lua::new()` opens everything, which would make the ABI a suggestion rather
/// than a boundary: `io.open` reads any file the worker can, `os.execute` runs
/// a shell, `os.exit` kills the box mid-task, and `require`/`package` load
/// native code. A crawl script needs none of them — the whole point of the host
/// functions is that `fetch` is the only way out — so they go, and the test
/// that a script sees `nil nil nil` is what keeps them gone.
///
/// This is a *boundary*, not an escape hatch for hostile code: a script still
/// runs in-process with the worker's privileges, and an operator-supplied
/// crawler is trusted the way any other configuration is. It exists so that the
/// documented ABI is the true one, and so a script that reaches for `io` fails
/// loudly instead of working by accident.
fn sandbox(lua: &Lua) -> Result<()> {
    let g = lua.globals();
    for name in [
        "io", "os", "package", "require", "dofile", "loadfile", "debug",
    ] {
        let _ = g.set(name, LuaValue::Nil);
    }
    Ok(())
}

/// The clock, checked from inside the VM.
///
/// Without this an infinite loop in a `next`-link walk is unbounded: the
/// budget check in each host function only fires when the script *calls* one,
/// and `while true do end` never does.
fn install_hook(lua: &Lua, host: SharedHost) -> Result<()> {
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(CHECK_EVERY),
        move |_lua, _debug| {
            // A `try_borrow` and not a `borrow`: the hook fires between VM
            // instructions, and if it ever fired while a host function held the
            // host it must not panic — continuing just defers to that
            // function's own check.
            match host.try_borrow() {
                Ok(h) if !h.budget_ok() => Err(mlua::Error::RuntimeError(
                    "crawl time budget exceeded".into(),
                )),
                _ => Ok(VmState::Continue),
            }
        },
    )
    .map_err(|e| anyhow!("installing the crawl instruction hook: {e}"))
}

/// `anyhow` into Lua, as a **message** rather than an `external` error.
///
/// `Error::external` wants a `Send + Sync` boxed error and `anyhow::Error`'s
/// own trait object is neither, so the chain is rendered here instead — which
/// is better anyway: the script's `pcall` and the ledger row both read a
/// sentence, not a type name.
fn ext(e: anyhow::Error) -> mlua::Error {
    let msg = format!("{e:#}").replace('\n', " ");
    mlua::Error::RuntimeError(msg)
}

/// The borrowed host, or an error saying why it was unavailable.
fn held(host: &SharedHost) -> mlua::Result<std::cell::RefMut<'_, super::super::host::Host>> {
    host.try_borrow_mut().map_err(|_| {
        mlua::Error::RuntimeError("the crawl host is already in use (re-entrant host call?)".into())
    })
}

fn install_fns(lua: &Lua, host: SharedHost) -> mlua::Result<()> {
    let g = lua.globals();

    let h = host.clone();
    g.set(
        "fetch",
        lua.create_function(move |lua, (url, opts): (String, Option<LuaValue>)| {
            let opts = match opts {
                Some(v) => lua.from_value(v)?,
                None => Value::Null,
            };
            let page = fns::fetch(&mut *held(&h)?, &url, opts).map_err(ext)?;
            lua.to_value(&page)
        })?,
    )?;

    let h = host.clone();
    g.set(
        "select",
        lua.create_function(move |lua, (html, sel): (String, String)| {
            let out = fns::select(&mut *held(&h)?, &html, &sel).map_err(ext)?;
            lua.to_value(&out)
        })?,
    )?;

    let h = host.clone();
    g.set(
        "select_all",
        lua.create_function(move |lua, (html, sel): (String, String)| {
            let out = fns::select_all(&mut *held(&h)?, &html, &sel).map_err(ext)?;
            lua.to_value(&out)
        })?,
    )?;

    let h = host.clone();
    g.set(
        "select_text",
        lua.create_function(move |lua, (html, sel): (String, String)| {
            let out = fns::select_text(&mut *held(&h)?, &html, &sel).map_err(ext)?;
            lua.to_value(&out)
        })?,
    )?;

    let h = host.clone();
    g.set(
        "readable",
        lua.create_function(move |lua, html: String| {
            let out = fns::readable(&mut *held(&h)?, &html).map_err(ext)?;
            lua.to_value(&out)
        })?,
    )?;

    g.set(
        "strip_tags",
        lua.create_function(move |lua, html: String| lua.to_value(&fns::strip_tags(&html)))?,
    )?;
    g.set(
        "decode_entities",
        lua.create_function(move |lua, s: String| lua.to_value(&fns::decode_entities(&s)))?,
    )?;
    g.set(
        "sanitize",
        lua.create_function(move |lua, s: String| lua.to_value(&fns::sanitize(&s)))?,
    )?;
    g.set(
        "abs_url",
        lua.create_function(move |lua, (base, href): (String, String)| {
            lua.to_value(&fns::abs_url(&base, &href))
        })?,
    )?;
    g.set(
        "chapter_url",
        lua.create_function(move |lua, (template, n): (String, u32)| {
            lua.to_value(&fns::chapter_url(&template, n))
        })?,
    )?;
    let h = host.clone();
    g.set(
        "challenge",
        lua.create_function(move |lua, page: LuaValue| {
            let page: Value = lua.from_value(page)?;
            let why = fns::challenge(&mut *held(&h)?, page).map_err(ext)?;
            // mlua's serde maps `None` to its own `NULL` sentinel rather than to
            // `nil`, so that a JSON `null` round-trips — and that sentinel is a
            // *userdata*, which is **truthy** in Lua. A script written the
            // obvious way, `if challenge(r) then return blocked end`, would
            // therefore refuse every page it was ever handed. Build the `nil`
            // by hand; this is the whole reason the function exists.
            match why {
                Some(why) => lua.to_value(&why),
                None => Ok(LuaValue::Nil),
            }
        })?,
    )?;

    let h = host.clone();
    g.set(
        "log",
        lua.create_function(move |_, msg: String| {
            fns::log(&mut *held(&h)?, &msg);
            Ok(())
        })?,
    )?;

    Ok(())
}
