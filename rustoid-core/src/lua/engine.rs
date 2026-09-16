//! Lua/Scribunto engine.
//!
//! Wraps `mlua` to provide a sandboxed Lua runtime for Scribunto modules.
//! Implements the `mw` global table with MediaWiki API stubs.

use std::sync::Arc;

use mlua::{Function, Lua, Table, Value};

use crate::error::{Result, RustoidError};
use crate::traits::SiteConfig;

/// Configuration for the Lua engine.
#[derive(Debug, Clone)]
pub struct LuaEngineConfig {
    pub instruction_limit: u64,
    pub memory_limit: usize,
}

impl Default for LuaEngineConfig {
    fn default() -> Self {
        Self {
            instruction_limit: 10_000_000,
            memory_limit: 50 * 1024 * 1024,
        }
    }
}

/// The site facts a Lua module can observe, copied out of the `SiteConfig`.
///
/// Owned rather than borrowed: `mlua` requires the data its closures capture to
/// be `'static`, and a `Parser` only borrows its config. Copying the handful of
/// values Lua can actually see is cheaper than restructuring the parser to own
/// an `Arc`, and it makes the boundary between "what Lua may know" and "the
/// parser's config" explicit.
#[derive(Debug, Clone, Default)]
pub struct LuaSite {
    pub server: String,
    pub language_code: String,
    pub script_path: String,
    /// `(id, canonical name)` for every namespace, for `mw.title` resolution.
    pub namespaces: Vec<(i32, String)>,
}

impl LuaSite {
    pub fn from_config(config: &dyn SiteConfig) -> Self {
        let mut namespaces: Vec<(i32, String)> = config
            .namespaces()
            .iter()
            .map(|(id, ns)| (*id, ns.canonical.clone()))
            .filter(|(_, name)| !name.is_empty())
            .collect();
        namespaces.sort_by_key(|(id, _)| *id);
        Self {
            server: config.server_url().to_string(),
            language_code: config.language_code().to_string(),
            script_path: config.script_path().to_string(),
            namespaces,
        }
    }

    /// Canonical name of a namespace id, empty for the main namespace.
    pub fn namespace_name(&self, id: i32) -> String {
        self.namespaces
            .iter()
            .find(|(ns_id, _)| *ns_id == id)
            .map(|(_, name)| name.clone())
            .unwrap_or_default()
    }

    /// Namespace id for a canonical or localized name, or for a numeric string.
    pub fn namespace_id(&self, name: &str) -> Option<i32> {
        if let Ok(id) = name.trim().parse::<i32>() {
            return self.namespaces.iter().any(|(i, _)| *i == id).then_some(id);
        }
        self.namespaces
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(name.trim()))
            .map(|(id, _)| *id)
    }
}

pub struct LuaContext {
    pub site: LuaSite,
    pub page_title: String,
    /// Module sources available to `require`/`mw.loadData`, keyed by full title
    /// (`Module:Foo`). Scribunto's `require` is synchronous inside Lua, so
    /// modules are fetched *before* execution and looked up here; see
    /// [`crate::lua::invoke`].
    pub modules: std::collections::HashMap<String, String>,
}

impl LuaContext {
    pub fn new(site: LuaSite, page_title: impl Into<String>) -> Self {
        Self {
            site,
            page_title: page_title.into(),
            modules: std::collections::HashMap::new(),
        }
    }

    /// Same, with the preloaded module registry.
    pub fn with_modules(
        site: LuaSite,
        page_title: impl Into<String>,
        modules: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            site,
            page_title: page_title.into(),
            modules,
        }
    }
}

/// One argument to a module, as `frame.args` presents it.
///
/// MediaWiki's preprocessor keys positional arguments by number and named ones
/// by name, and modules rely on both spellings: `frame.args[1]` and
/// `frame.args[1]`-as-`"1"` are both written in the wild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Positional(String),
    Named(String, String),
}

pub struct LuaEngine {
    lua: Lua,
    _context: Arc<LuaContext>,
}

impl LuaEngine {
    pub fn new(engine_config: LuaEngineConfig, ctx: LuaContext) -> Result<Self> {
        let lua = Lua::new();

        for name in &["os", "io", "package", "require", "loadfile", "dofile"] {
            lua.globals()
                .set(*name, Value::Nil)
                .map_err(|e| RustoidError::Lua(e.to_string()))?;
        }

        lua.set_memory_limit(engine_config.memory_limit)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;

        let ctx = Arc::new(ctx);
        let mw = setup_mw_table(&lua, ctx.clone())?;
        lua.globals()
            .set("mw", mw)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
        install_module_loader(&lua, &ctx)?;

        Ok(Self { lua, _context: ctx })
    }

    pub fn execute(
        &self,
        module_source: &str,
        function_name: &str,
        args: &[Arg],
    ) -> Result<String> {
        let module = self.load_module_value(module_source, None)?;
        let func = self.module_function(&module, function_name)?;

        let frame = create_frame(&self.lua, args, &self._context.page_title)?;
        // `mw.getCurrentFrame()` reads this, so the frame must be installed
        // before the module runs.
        self.lua
            .set_named_registry_value("current_frame", frame.clone())
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
        let result: Value = func
            .call::<Value>(frame)
            .map_err(|e| RustoidError::Lua(format!("execution error: {e}")))?;

        Ok(lua_value_to_string(&result))
    }

    /// Run a module's source and return its value.
    ///
    /// A Scribunto module is a chunk that `return`s a table (`local p = {} …
    /// return p`), not a set of globals, so the *returned* value is what carries
    /// the entry points. `title` names it for error messages.
    pub fn load_module_value(&self, module_source: &str, title: Option<&str>) -> Result<Value> {
        self.lua
            .load(module_source)
            .set_name(title.unwrap_or("module"))
            .eval::<Value>()
            .map_err(|e| {
                RustoidError::Lua(format!(
                    "module load error in {}: {e}",
                    title.unwrap_or("module")
                ))
            })
    }

    /// Fetch a module's entry point: `module[function_name]`, falling back to a
    /// global of that name for the older style and for the engine's own tests.
    pub fn module_function(&self, module: &Value, function_name: &str) -> Result<Function> {
        if let Value::Table(t) = module
            && let Ok(f) = t.get::<Function>(function_name)
        {
            return Ok(f);
        }
        self.lua
            .globals()
            .get(function_name)
            .map_err(|e| RustoidError::Lua(format!("function not found: {function_name}: {e}")))
    }

    /// Run `code` in this engine's environment.
    pub fn eval(&self, code: &str) -> Result<String> {
        let result: Value = self
            .lua
            .load(code)
            .eval()
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
        Ok(lua_value_to_string(&result))
    }
}

// ---- mw table setup ----

// ---- module loader ----

/// Install `require`, `mw.loadData` and a `package` stub.
///
/// Scribunto's `require` is synchronous inside Lua, so it cannot fetch: the
/// modules a run needs are fetched *before* execution (see
/// [`crate::lua::invoke::preload`]) and looked up in `ctx.modules` here. A
/// `require` for something outside that registry is an error rather than a
/// silent nil, because a module that quietly receives nothing tends to produce
/// plausible-but-wrong output.
fn install_module_loader(lua: &Lua, ctx: &Arc<LuaContext>) -> Result<()> {
    let lua_err = |e: mlua::Error| RustoidError::Lua(e.to_string());

    // Loaded modules are cached, as Scribunto caches them: a module is executed
    // once per render however many times it is required.
    let cache = lua.create_table().map_err(lua_err)?;
    // Titles currently being loaded, so a `require` cycle reports instead of
    // recursing until the stack runs out.
    let loading = lua.create_table().map_err(lua_err)?;

    let modules = ctx.modules.clone();
    let require = lua
        .create_function(move |lua, name: Value| {
            let title = match name {
                Value::String(s) => s.to_str().map_err(mlua::Error::external)?.to_string(),
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "require expects a module name, got {} — dynamic requires cannot be \
                     resolved ahead of execution",
                        other.type_name()
                    )));
                }
            };

            if let Ok(cached) = cache.get::<Value>(title.clone())
                && !cached.is_nil()
            {
                return Ok(cached);
            }

            if loading.get::<bool>(title.clone()).unwrap_or(false) {
                return Err(mlua::Error::runtime(format!(
                    "circular dependency while loading {title}"
                )));
            }

            let Some(source) = lookup_module(&modules, &title) else {
                return Err(mlua::Error::runtime(format!(
                    "module {title} was not preloaded"
                )));
            };

            loading.set(title.clone(), true)?;
            let value = lua.load(source).set_name(title.clone()).eval::<Value>();
            loading.set(title.clone(), Value::Nil)?;

            let value = value?;
            cache.set(title, value.clone())?;
            Ok(value)
        })
        .map_err(lua_err)?;
    lua.globals().set("require", require).map_err(lua_err)?;

    // `mw.loadData(name)` — a data module's table, read once. Scribunto marks the
    // result read-only; sharing the same value as `require` is enough here.
    let mw: Table = lua.globals().get("mw").map_err(lua_err)?;
    let require_fn: Function = lua.globals().get("require").map_err(lua_err)?;
    mw.set(
        "loadData",
        lua.create_function(move |_, name: String| require_fn.call::<Value>(name))
            .map_err(lua_err)?,
    )
    .map_err(lua_err)?;

    // `mw.getCurrentFrame()` — the frame of the invocation being executed.
    mw.set(
        "getCurrentFrame",
        lua.create_function(|lua, ()| lua.named_registry_value::<Value>("current_frame"))
            .map_err(lua_err)?,
    )
    .map_err(lua_err)?;

    // Lure modules that probe it into the normal path: Scribunto provides
    // `package` with its own loaders rather than leaving it nil.
    let package = lua.create_table().map_err(lua_err)?;
    package
        .set("loaders", lua.create_table().map_err(lua_err)?)
        .map_err(lua_err)?;
    lua.globals().set("package", package).map_err(lua_err)?;

    Ok(())
}

/// Look a module up by title, tolerating case and underscore/space differences.
/// An unattempted lookup is the common failure, so it is worth being lenient.
fn lookup_module<'a>(
    modules: &'a std::collections::HashMap<String, String>,
    title: &str,
) -> Option<&'a String> {
    if let Some(src) = modules.get(title) {
        return Some(src);
    }
    let normalized = title.replace('_', " ");
    for (k, v) in modules {
        if k.eq_ignore_ascii_case(&normalized) {
            return Some(v);
        }
    }
    None
}

fn setup_mw_table(lua: &Lua, ctx: Arc<LuaContext>) -> Result<Table> {
    let mw = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    // mw.text
    let text = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    text.set("encode", lua.create_function(luafn_text_encode)?)?;
    text.set("decode", lua.create_function(luafn_text_decode)?)?;
    text.set("trim", lua.create_function(luafn_text_trim)?)?;
    text.set("split", lua.create_function(luafn_text_split)?)?;
    text.set("tag", lua.create_function(luafn_text_tag)?)?;
    mw.set("text", text)?;

    // mw.title
    let title = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    let ctx2 = ctx.clone();
    title.set(
        "new",
        lua.create_function(move |lua, (text, ns): (String, Option<i32>)| {
            luafn_title_new(lua, &ctx2, text, ns)
        })?,
    )?;
    let ctx3 = ctx.clone();
    title.set(
        "getCurrentTitle",
        lua.create_function(move |lua, ()| luafn_title_current(lua, &ctx3))?,
    )?;
    mw.set("title", title)?;

    // mw.site
    let site = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    site.set("siteName", "Wikipedia")?;
    site.set("server", ctx.site.server.clone())?;
    site.set("scriptPath", ctx.site.script_path.clone())?;
    site.set("languageCode", ctx.site.language_code.clone())?;
    mw.set("site", site)?;

    // mw.uri
    let uri = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    uri.set("encode", lua.create_function(luafn_uri_encode)?)?;
    uri.set("decode", lua.create_function(luafn_uri_decode)?)?;
    uri.set(
        "anchorEncode",
        lua.create_function(luafn_uri_anchor_encode)?,
    )?;
    mw.set("uri", uri)?;

    // mw.language
    let lang = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    lang.set("formatNum", lua.create_function(luafn_lang_format_num)?)?;
    lang.set(
        "getCode",
        lua.create_function(|_, ()| Ok("en".to_string()))?,
    )?;
    mw.set("language", lang)?;

    // mw.ustring
    let ustring = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ustring.set("len", lua.create_function(luafn_ustring_len)?)?;
    ustring.set("sub", lua.create_function(luafn_ustring_sub)?)?;
    ustring.set("upper", lua.create_function(luafn_ustring_upper)?)?;
    ustring.set("lower", lua.create_function(luafn_ustring_lower)?)?;
    mw.set("ustring", ustring)?;

    // mw.message
    let message = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    message.set("new", lua.create_function(luafn_message_new)?)?;
    mw.set("message", message)?;

    // mw.html (simplified)
    let html = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    html.set("create", lua.create_function(luafn_html_create)?)?;
    mw.set("html", html)?;

    Ok(mw)
}

// ---- Standalone Lua functions ----

fn luafn_text_encode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(html_escape(&s))
}

fn luafn_text_decode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(html_unescape(&s))
}

fn luafn_text_trim(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(s.trim().to_string())
}

fn luafn_text_split(_: &Lua, (s, sep): (String, String)) -> mlua::Result<Vec<String>> {
    Ok(s.split(&sep).map(|p| p.to_string()).collect())
}

fn luafn_text_tag(
    _: &Lua,
    (name, attrs, content): (String, Option<Table>, Option<String>),
) -> mlua::Result<String> {
    let mut result = format!("<{name}");
    if let Some(attr_table) = attrs {
        for (key, val) in attr_table.pairs::<String, String>().flatten() {
            result.push_str(&format!(" {key}=\"{val}\""));
        }
    }
    if let Some(content) = content {
        result.push_str(&format!(">{content}</{name}>"));
    } else {
        result.push_str("/>");
    }
    Ok(result)
}

fn luafn_title_new(
    lua: &Lua,
    ctx: &LuaContext,
    text: String,
    namespace: Option<i32>,
) -> mlua::Result<Table> {
    // Split `Ns:Title#frag` against the namespace snapshot. Full `TitleParser`
    // semantics (interwiki, language variants) are not observable here yet.
    let (prefix, rest) = match text.split_once(':') {
        Some((p, r)) => (ctx.site.namespace_id(p), r),
        None => (None, text.as_str()),
    };
    let (rest, fragment) = match rest.split_once('#') {
        Some((t, f)) => (t, f.to_string()),
        None => (rest, String::new()),
    };
    let ns_id = namespace.or(prefix).unwrap_or(0);
    let title_text = rest.trim().to_string();
    let ns_text = ctx.site.namespace_name(ns_id);

    let table = lua.create_table()?;
    table.set("text", title_text.clone())?;
    table.set("nsText", ns_text.clone())?;
    table.set("namespace", ns_id)?;
    let full = if ns_text.is_empty() {
        title_text.clone()
    } else {
        format!("{ns_text}:{title_text}")
    };
    table.set("fullText", full)?;
    table.set("prefixedText", title_text.clone())?;
    // Existence is not resolvable from here: the engine has no data source (see
    // `LuaSite`). Reporting `false` would make a module render red links for
    // pages that do exist, so it reports `true` and the gap is recorded in
    // ONLINE-PARITY.md.
    table.set("exists", true)?;
    table.set("isRedirect", false)?;
    table.set("fragment", fragment)?;
    table.set(
        "rootText",
        title_text.split('/').next().unwrap_or("").to_string(),
    )?;
    Ok(table)
}

fn luafn_title_current(lua: &Lua, ctx: &LuaContext) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("text", ctx.page_title.clone())?;
    table.set("prefixedText", ctx.page_title.clone())?;
    table.set("namespace", 0)?;
    Ok(table)
}

fn luafn_uri_encode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(url_encode(&s))
}

fn luafn_uri_decode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(url_decode(&s))
}

fn luafn_uri_anchor_encode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(s.replace(' ', "_").replace('?', "%3F").replace('#', "%23"))
}

fn luafn_lang_format_num(_: &Lua, n: f64) -> mlua::Result<String> {
    Ok(format_number(n))
}

fn luafn_ustring_len(_: &Lua, s: String) -> mlua::Result<usize> {
    Ok(s.chars().count())
}

fn luafn_ustring_sub(
    _: &Lua,
    (s, start, length): (String, i64, Option<i64>),
) -> mlua::Result<String> {
    let chars: Vec<char> = s.chars().collect();
    let start_idx = if start > 0 {
        ((start - 1) as usize).min(chars.len())
    } else {
        chars.len().saturating_sub((-start) as usize)
    };
    let end_idx = if let Some(len) = length {
        if len > 0 {
            (start_idx + len as usize).min(chars.len())
        } else {
            chars.len().saturating_sub((-len) as usize)
        }
    } else {
        chars.len()
    };
    Ok(chars[start_idx..end_idx].iter().collect())
}

fn luafn_ustring_upper(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(s.to_uppercase())
}

fn luafn_ustring_lower(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(s.to_lowercase())
}

fn luafn_message_new(lua: &Lua, (key, _args): (String, Option<Table>)) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let k = key.clone();
    table.set("key", key)?;
    table.set("plain", lua.create_function(move |_, ()| Ok(k.clone()))?)?;
    Ok(table)
}

fn luafn_html_create(lua: &Lua, (tag_name, _args): (String, Option<Table>)) -> mlua::Result<Table> {
    let builder = lua.create_table()?;
    let t1 = tag_name.clone();
    builder.set(
        "wikitext",
        lua.create_function(move |lua, text: String| {
            let b = lua.create_table()?;
            b.set("_text", text)?;
            b.set("_tag", t1.clone())?;
            Ok(b)
        })?,
    )?;
    let t2 = tag_name.clone();
    builder.set(
        "done",
        lua.create_function(move |_, ()| Ok(format!("<{t2}></{t2}>")))?,
    )?;
    let t3 = tag_name;
    builder.set(
        "allDone",
        lua.create_function(move |_, ()| Ok(format!("<{t3}></{t3}>")))?,
    )?;
    Ok(builder)
}

// ---- Frame ----

fn create_frame(lua: &Lua, args: &[Arg], page_title: &str) -> Result<Value> {
    let frame = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    let args_table = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    // Positional arguments get a *numeric* key, so `frame.args[1]` and `#args`
    // behave; named ones get their name. A metatable makes the string spelling
    // (`args["1"]`) resolve too, which modules also write, without duplicating
    // keys and so without confusing `pairs`.
    let mut next_positional = 0usize;
    for arg in args {
        match arg {
            Arg::Positional(v) => {
                next_positional += 1;
                args_table
                    .set(next_positional, v.clone())
                    .map_err(|e| RustoidError::Lua(e.to_string()))?;
            }
            Arg::Named(k, v) => {
                args_table
                    .set(k.clone(), v.clone())
                    .map_err(|e| RustoidError::Lua(e.to_string()))?;
            }
        }
    }
    let args_mt = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    args_mt
        .set(
            "__index",
            lua.create_function(|_, (t, k): (Table, Value)| {
                // Positional args are stored under an integer key, but both
                // spellings are written in the wild (`args[1]` and `args["1"]`),
                // so map either to the other. `raw_get` keeps this from
                // re-entering the metatable.
                let alternative = match k {
                    Value::Integer(i) => Some(Value::Integer(i)),
                    Value::Number(n) if n.fract() == 0.0 => Some(Value::Integer(n as i64)),
                    Value::String(ref s) => s
                        .to_str()
                        .ok()
                        .and_then(|s| s.trim().parse::<i64>().ok())
                        .map(Value::Integer),
                    _ => None,
                };
                match alternative {
                    Some(key) => Ok(t.raw_get::<Value>(key)?),
                    None => Ok(Value::Nil),
                }
            })
            .map_err(|e| RustoidError::Lua(e.to_string()))?,
        )
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    args_table.set_metatable(Some(args_mt));
    frame.set("args", args_table)?;

    // `frame:argumentPairs()` — the generic-for protocol: return (iterator,
    // state, control). Lua's own `next` is the iterator, so the args table is
    // the state and no snapshot has to be stored on the Lua side.
    frame.set(
        "argumentPairs",
        lua.create_function(|lua, this: Table| {
            let args: Table = this.get("args")?;
            let next: Function = lua.globals().get("next")?;
            Ok((next, args, Value::Nil))
        })
        .map_err(|e| RustoidError::Lua(e.to_string()))?,
    )?;

    // `frame:getTitle()` — the page the frame was invoked from.
    let title = page_title.to_string();
    let ctx_for_title = title.clone();
    frame.set(
        "getTitle",
        lua.create_function(move |_, ()| Ok(ctx_for_title.clone()))?,
    )?;
    frame.set("getParent", lua.create_function(|_, ()| Ok(Value::Nil))?)?;
    // `frame:preprocess(text)` — expand wikitext. It needs the parser, so it is
    // a later phase; returning the text unchanged would silently produce wrong
    // output, so it reports instead.
    frame.set(
        "preprocess",
        lua.create_function(|_, _text: String| -> mlua::Result<String> {
            Err(mlua::Error::runtime(
                "frame:preprocess is not implemented yet",
            ))
        })?,
    )?;
    frame.set(
        "extensionTag",
        lua.create_function(|_, opts: Table| {
            let name: String = opts.get("name").unwrap_or_default();
            let content: String = opts.get("content").unwrap_or_default();
            Ok(format!("<{name}>{content}</{name}>"))
        })?,
    )?;

    Ok(Value::Table(frame))
}

// ---- Utilities ----

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn html_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn format_number(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        let s = (n as i64).to_string();
        let mut result = String::new();
        for (i, ch) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                result.push(',');
            }
            result.push(ch);
        }
        result.chars().rev().collect()
    } else {
        n.to_string()
    }
}

fn url_encode(s: &str) -> String {
    let mut result = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push('+'),
            _ => result.push_str(&format!("%{byte:02X}")),
        }
    }
    result
}

fn url_decode(s: &str) -> String {
    let mut result = String::new();
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                result.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(hex) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    result.push(hex as char);
                    i += 3;
                } else {
                    result.push('%');
                    i += 1;
                }
            }
            _ => {
                result.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    result
}

fn lua_value_to_string(value: &Value) -> String {
    match value {
        Value::Nil => String::new(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Table(_) => String::new(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    fn make_engine() -> LuaEngine {
        let ctx = LuaContext::new(LuaSite::from_config(&MockSiteConfig::new()), "Test Page");
        LuaEngine::new(LuaEngineConfig::default(), ctx).unwrap()
    }

    #[test]
    fn test_create_engine() {
        make_engine();
    }

    #[test]
    fn test_basic_lua_execution() {
        let engine = make_engine();
        assert_eq!(engine.eval("return 1 + 1").unwrap(), "2");
    }

    #[test]
    fn test_mw_text_encode() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.text.encode('<>&\"')").unwrap(),
            "&lt;&gt;&amp;&quot;"
        );
    }

    #[test]
    fn test_mw_text_trim() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.text.trim('  hello  ')").unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_mw_text_split() {
        let engine = make_engine();
        let result = engine
            .eval("return table.concat(mw.text.split('a,b,c', ','), '|')")
            .unwrap();
        assert_eq!(result, "a|b|c");
    }

    #[test]
    fn test_mw_title_new() {
        let engine = make_engine();
        let result = engine
            .eval("return mw.title.new('Template:Foo').fullText")
            .unwrap();
        assert_eq!(result, "Template:Foo");
    }

    #[test]
    fn test_mw_title_current() {
        let engine = make_engine();
        let result = engine
            .eval("return mw.title.getCurrentTitle().text")
            .unwrap();
        assert_eq!(result, "Test Page");
    }

    #[test]
    fn test_mw_site() {
        let engine = make_engine();
        assert_eq!(engine.eval("return mw.site.siteName").unwrap(), "Wikipedia");
    }

    #[test]
    fn test_mw_uri_encode() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.uri.encode('hello world')").unwrap(),
            "hello+world"
        );
    }

    #[test]
    fn test_mw_uri_anchor_encode() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.uri.anchorEncode('Hello World?')")
                .unwrap(),
            "Hello_World%3F"
        );
    }

    #[test]
    fn test_mw_language_format_num() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.language.formatNum(1234567)")
                .unwrap(),
            "1,234,567"
        );
    }

    #[test]
    fn test_mw_ustring_len() {
        let engine = make_engine();
        assert_eq!(engine.eval("return mw.ustring.len('hello')").unwrap(), "5");
    }

    #[test]
    fn test_mw_ustring_sub() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.ustring.sub('hello world', 7, 5)")
                .unwrap(),
            "world"
        );
    }

    #[test]
    fn test_mw_ustring_upper() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.ustring.upper('hello')").unwrap(),
            "HELLO"
        );
    }

    #[test]
    fn test_frame_args() {
        let ctx = LuaContext::new(LuaSite::from_config(&MockSiteConfig::new()), "Test");
        let engine = LuaEngine::new(LuaEngineConfig::default(), ctx).unwrap();
        let result = engine
            .execute(
                "function myfn(frame) return frame.args[1] end",
                "myfn",
                &[Arg::Positional("hello".to_string())],
            )
            .unwrap();
        assert_eq!(result, "hello");
    }

    /// A module returns its table (`local p = {} … return p`), so entry points
    /// live in the returned value, not in globals.
    #[test]
    fn test_module_table_entry_point() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                return "from " .. frame.args["who"]
            end
            return p
        "#;
        let result = engine
            .execute(
                src,
                "main",
                &[Arg::Named("who".to_string(), "table".to_string())],
            )
            .unwrap();
        assert_eq!(result, "from table");
    }

    /// Both spellings of a positional argument are used in the wild.
    #[test]
    fn test_positional_args_by_number_and_string() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                return tostring(frame.args[1]) .. "/" .. tostring(frame.args["1"])
            end
            return p
        "#;
        let result = engine
            .execute(src, "main", &[Arg::Positional("x".to_string())])
            .unwrap();
        assert_eq!(result, "x/x");
    }

    /// `frame:argumentPairs()` must drive a generic `for` loop.
    #[test]
    fn test_argument_pairs_iterates() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                local out = {}
                for k, v in frame:argumentPairs() do
                    out[#out + 1] = tostring(k) .. "=" .. tostring(v)
                end
                table.sort(out)
                return table.concat(out, ",")
            end
            return p
        "#;
        let result = engine
            .execute(
                src,
                "main",
                &[
                    Arg::Positional("a".to_string()),
                    Arg::Named("n".to_string(), "v".to_string()),
                ],
            )
            .unwrap();
        assert_eq!(result, "1=a,n=v");
    }

    /// `require` resolves from the preloaded registry, executes once, and caches.
    #[test]
    fn test_require_uses_the_registry() {
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            "Module:Helper".to_string(),
            "local m = {} m.value = 'from helper' return m".to_string(),
        );
        let ctx = LuaContext::with_modules(
            LuaSite::from_config(&MockSiteConfig::new()),
            "Test",
            modules,
        );
        let engine = LuaEngine::new(LuaEngineConfig::default(), ctx).unwrap();
        let src = r#"
            local p = {}
            function p.main(frame)
                local helper = require('Module:Helper')
                return helper.value
            end
            return p
        "#;
        assert_eq!(engine.execute(src, "main", &[]).unwrap(), "from helper");
    }

    /// A `require` of something that was not preloaded must error, not return
    /// nil: a module quietly handed nothing produces plausible wrong output.
    #[test]
    fn test_require_of_unknown_module_errors() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                return require('Module:Absent').value
            end
            return p
        "#;
        let err = engine.execute(src, "main", &[]).unwrap_err();
        assert!(err.to_string().contains("Absent"), "{err}");
    }

    /// A `require` cycle reports rather than recursing until the stack dies.
    #[test]
    fn test_require_cycle_is_reported() {
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            "Module:A".to_string(),
            "return require('Module:B')".to_string(),
        );
        modules.insert(
            "Module:B".to_string(),
            "return require('Module:A')".to_string(),
        );
        let ctx = LuaContext::with_modules(
            LuaSite::from_config(&MockSiteConfig::new()),
            "Test",
            modules,
        );
        let engine = LuaEngine::new(LuaEngineConfig::default(), ctx).unwrap();
        let src = r#"
            local p = {}
            function p.main(frame)
                return require('Module:A')
            end
            return p
        "#;
        let err = engine.execute(src, "main", &[]).unwrap_err();
        assert!(err.to_string().contains("circular"), "{err}");
    }

    /// `mw.getCurrentFrame()` must return the frame of the running invocation,
    /// which modules use for `args` and for `expandTemplate`.
    #[test]
    fn test_current_frame_is_available() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                return mw.getCurrentFrame().args['k']
            end
            return p
        "#;
        let result = engine
            .execute(src, "main", &[Arg::Named("k".to_string(), "v".to_string())])
            .unwrap();
        assert_eq!(result, "v");
    }

    #[test]
    fn test_module_execution() {
        let engine = make_engine();
        let source = r#"
            local p = {}
            function p.test(frame)
                return "hello from module"
            end
            return p.test(nil)
        "#;
        let result = engine.eval(source).unwrap();
        assert_eq!(result, "hello from module");
    }
}
