//! Lua binding for the crawl ABI — the engine the bundled crawlers are in.

use anyhow::{anyhow, Result};
use mlua::{HookTriggers, Lua, LuaSerdeExt, Value as LuaValue, VmState};
use serde_json::Value;

use super::{fns, Entry, Program, SharedHost};

/// How often the VM checks the clock. Low enough that a runaway loop cannot
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
fn request_arg(lua: &Lua, input: &Value) -> mlua::Result<LuaValue> {
    let arg = lua.to_value(input)?;
    // Only the fields the contract documents as nullable, so a `null` that means
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
const NULLABLE_FIELDS: [&str; 2] = ["url", "params"];

/// Remove the standard library's reach outside the process.
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
fn install_hook(lua: &Lua, host: SharedHost) -> Result<()> {
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(CHECK_EVERY),
        move |_lua, _debug| {
            // A `try_borrow` and not a `borrow`: the hook fires between VM
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
            match why {
                Some(why) => lua.to_value(&why),
                None => Ok(LuaValue::Nil),
            }
        })?,
    )?;

    let h = host.clone();
    g.set(
        "epub_chapter",
        lua.create_function(move |lua, (path, n): (String, u32)| {
            let out = fns::epub_chapter(&mut *held(&h)?, &path, n).map_err(ext)?;
            // `nil` by hand, as with `challenge`: a JSON null would arrive as a
            match out {
                Some(v) => lua.to_value(&v),
                None => Ok(LuaValue::Nil),
            }
        })?,
    )?;

    let h = host.clone();
    g.set(
        "epub_total",
        lua.create_function(move |lua, path: String| {
            let out = fns::epub_total(&mut *held(&h)?, &path).map_err(ext)?;
            match out {
                Some(v) => lua.to_value(&v),
                None => Ok(LuaValue::Nil),
            }
        })?,
    )?;

    let h = host.clone();
    g.set(
        "epub_index",
        lua.create_function(move |lua, path: String| {
            let out = fns::epub_index(&mut *held(&h)?, &path).map_err(ext)?;
            match out {
                Some(v) => lua.to_value(&v),
                None => Ok(LuaValue::Nil),
            }
        })?,
    )?;

    let h = host.clone();
    g.set(
        "epub_text",
        lua.create_function(move |lua, (path, from, to): (String, u32, u32)| {
            let out = fns::epub_text(&mut *held(&h)?, &path, from, to).map_err(ext)?;
            lua.to_value(&out)
        })?,
    )?;

    let h = host.clone();
    g.set(
        "epub_books",
        lua.create_function(move |lua, dir: String| {
            let out = fns::epub_books(&mut *held(&h)?, &dir).map_err(ext)?;
            lua.to_value(&out)
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
