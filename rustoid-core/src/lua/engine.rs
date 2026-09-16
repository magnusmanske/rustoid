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
    /// Arguments of the frame that *invoked* the module — `frame:getParent().args`.
    /// Empty when there is no parent (a direct `{{#invoke:…}}` from the page).
    pub parent_args: Vec<Arg>,
    /// Title of that parent frame, for `frame:getParent():getTitle()`.
    pub parent_title: Option<String>,
    /// Whether a parent frame exists at all. Distinct from "the parent had no
    /// arguments": a direct `{{#invoke:…}}` has no parent, while one made from
    /// inside a template does, even when that template took no arguments.
    pub has_parent: bool,
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
            parent_args: Vec::new(),
            parent_title: None,
            has_parent: false,
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
            parent_args: Vec::new(),
            parent_title: None,
            has_parent: false,
            modules,
        }
    }

    /// Same, with the invoking frame's arguments available as the parent frame.
    pub fn with_parent(
        site: LuaSite,
        page_title: impl Into<String>,
        modules: std::collections::HashMap<String, String>,
        parent_args: Vec<Arg>,
        parent_title: Option<String>,
        has_parent: bool,
    ) -> Self {
        Self {
            site,
            page_title: page_title.into(),
            parent_args,
            parent_title,
            has_parent,
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

    /// Run `module_source`'s function `function_name`.
    ///
    /// `title` names the module in error messages. Without it Lua reports
    /// `[string "module"]:13`, which says a line number in *something* and made
    /// several corpus failures impossible to attribute to a module.
    pub fn execute_in(
        &self,
        module_source: &str,
        title: &str,
        function_name: &str,
        args: &[Arg],
    ) -> Result<String> {
        let module = self.load_module_value(module_source, Some(title))?;
        let func = self.module_function(&module, function_name)?;

        let frame = create_frame(
            &self.lua,
            args,
            &self._context.page_title,
            // `Option`: a `#invoke` made outside any template has no parent
            // frame, and Scribunto returns nil for `getParent()` there.
            self._context
                .has_parent
                .then_some(self._context.parent_args.as_slice()),
            self._context.parent_title.as_deref(),
        )?;
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

    /// Run with no module title, for callers that have none (the engine's own
    /// tests, and `eval`).
    pub fn execute(
        &self,
        module_source: &str,
        function_name: &str,
        args: &[Arg],
    ) -> Result<String> {
        self.execute_in(module_source, "module", function_name, args)
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
                // Scribunto also exposes its *own* libraries through `require` —
                // `strict`, `libraryUtil`, `ustring` — which are not `Module:` pages.
                // Rejecting those was the single most common failure in the corpus
                // (36 pages for `strict`, 26 for `libraryUtil`).
                if let Some(lib) = builtin_library(lua, &title) {
                    cache.set(title, lib.clone())?;
                    return Ok(lib);
                }
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

/// Scribunto's built-in libraries, as Lua source, loadable by `require`.
///
/// These are part of Scribunto rather than of a wiki, so they can never be
/// fetched and must be supplied. They are Lua source rather than Rust because
/// they *are* Lua: reimplementing `libraryUtil` in the host language would make
/// it diverge for no gain.
///
/// `strict` is the module's metatable factory; `libraryUtil` is the argument
/// checker Scribunto's own modules use, which wiki modules also require
/// directly.
const BUILTIN_LIBRARIES: &[(&str, &str)] = &[
    (
        "strict",
        r#"
        local mt = {}
        mt.__index = function(t, k)
            error("strict mode: undefined global '" .. tostring(k) .. "'", 2)
        end
        mt.__newindex = function(t, k, v)
            error("strict mode: assignment to undeclared global '" .. tostring(k) .. "'", 2)
        end
        return mt
        "#,
    ),
    (
        "libraryUtil",
        r#"
        local libraryUtil = {}

        function libraryUtil.checkType(name, argIdx, arg, expectType, nilOk)
            if arg == nil and nilOk then return arg end
            local actual = type(arg)
            if actual ~= expectType then
                error(string.format(
                    "%s: bad argument #%d (type %s expected, got %s)",
                    name, argIdx, expectType, actual), 3)
            end
            return arg
        end

        function libraryUtil.checkTypeMulti(name, argIdx, arg, expectTypes)
            if arg == nil then return arg end
            local actual = type(arg)
            for _, t in ipairs(expectTypes) do
                if actual == t then return arg end
            end
            error(string.format(
                "%s: bad argument #%d (type %s expected, got %s)",
                name, argIdx, table.concat(expectTypes, " or "), actual), 3)
        end

        function libraryUtil.checkTypeForNamedArg(name, argName, arg, expectType, nilOk)
            if arg == nil and nilOk then return arg end
            local actual = type(arg)
            if actual ~= expectType then
                error(string.format(
                    "%s: bad argument '%s' (type %s expected, got %s)",
                    name, argName, expectType, actual), 3)
            end
            return arg
        end

        function libraryUtil.makeCheckSelfFunction(libraryName, varName, self, method)
            if type(self) ~= 'table' then
                error(string.format(
                    "%s: bad self argument (table expected, got %s)",
                    libraryName, type(self)), 3)
            end
            if method and self[method] == nil then
                error(string.format(
                    "%s: '%s' is not a valid method", libraryName, tostring(method)), 3)
            end
            return function(self, method)
                if type(self) ~= 'table' then
                    error(string.format(
                        "%s: bad self argument (table expected, got %s)",
                        libraryName, type(self)), 3)
                end
                return self
            end
        end

        return libraryUtil
        "#,
    ),
];

/// Resolve `require(name)` for a Scribunto built-in library.
///
/// `ustring` is not Lua source: it is the same table `mw.ustring` exposes, so it
/// is handed back directly.
fn builtin_library(lua: &Lua, name: &str) -> Option<Value> {
    if name == "ustring" {
        let mw: Table = lua.globals().get("mw").ok()?;
        return mw.get::<Value>("ustring").ok();
    }
    let (_, source) = BUILTIN_LIBRARIES.iter().find(|(n, _)| *n == name)?;
    lua.load(*source)
        .set_name((*name).to_string())
        .eval::<Value>()
        .ok()
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
    text.set("listToText", lua.create_function(luafn_text_list_to_text)?)?;
    mw.set("text", text)?;

    // mw.title
    let title = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    let ctx2 = ctx.clone();
    title.set(
        "new",
        lua.create_function(move |lua, (text, ns): (Value, Option<Value>)| {
            luafn_title_new(lua, &ctx2, text, ns.as_ref())
        })?,
    )?;
    // `mw.title.equals(a, b)` — whether two titles name the same page. Titles are
    // compared through their `prefixedText`, which is how the objects are built
    // here; a missing argument is unequal rather than an error, matching what
    // modules such as `Module:Message box` guard against.
    title.set(
        "equals",
        lua.create_function(|_, (a, b): (Value, Value)| {
            let key = |v: &Value| -> Option<String> {
                let t = match v {
                    Value::Table(t) => t,
                    _ => return None,
                };
                t.get::<Option<String>>("prefixedText")
                    .ok()
                    .flatten()
                    .or_else(|| t.get::<Option<String>>("fullText").ok().flatten())
            };
            Ok(match (key(&a), key(&b)) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            })
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
    // `mw.site.namespaces` is indexed by id and by name, and modules read
    // properties off the entries (Hatnote indexes it directly, which is why a
    // missing table stops the module with "attempt to index a nil value").
    site.set("namespaces", luafn_site_namespaces(lua, &ctx.site)?)?;
    // `subjectNamespaces`/`talkNamespaces` are the same entries under a second
    // index; modules cross-reference them (Namespace detect reads
    // `subjectNamespaces`).
    site.set("subjectNamespaces", luafn_site_namespaces(lua, &ctx.site)?)?;
    site.set("talkNamespaces", luafn_site_namespaces(lua, &ctx.site)?)?;
    mw.set("site", site)?;

    // `mw.isSubsting()` reports whether the current parse is a `subst:`. rustoid
    // never substitutes, so it is always false — which is the answer modules
    // such as `Module:Unsubst` need in order to take their normal path.
    mw.set("isSubsting", lua.create_function(|_, ()| Ok(false))?)?;

    // `mw.clone(value)` — a deep copy, preserving metatables. Modules copy shared
    // configuration tables before modifying them, and a shallow copy would let
    // one invocation corrupt another's data.
    mw.set("clone", lua.create_function(luafn_clone)?)?;

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
    lang.set("formatNum", lua.create_function(luafn_lang_format_num_any)?)?;
    lang.set(
        "getCode",
        lua.create_function(|_, ()| Ok("en".to_string()))?,
    )?;
    // `mw.language.getContentLanguage()` returns the wiki's content language
    // object; modules call through it to `formatNum`/`getCode` rather than using
    // `mw.language` directly.
    let content_language = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    content_language.set("formatNum", lua.create_function(luafn_lang_format_num_any)?)?;
    content_language.set(
        "getCode",
        lua.create_function(|_, ()| Ok("en".to_string()))?,
    )?;
    content_language.set("code", "en")?;
    lang.set(
        "getContentLanguage",
        lua.create_function(move |_, ()| Ok(content_language.clone()))?,
    )?;
    mw.set("language", lang)?;

    // `mw.getContentLanguage()` is the shorthand for
    // `mw.language.getContentLanguage()`; `Module:Citation/CS1/Configuration`
    // calls it on its first line, so without it the whole CS1 stack fails.
    let language_table: Table = mw.get("language")?;
    let get_content_language: Function = language_table.get("getContentLanguage")?;
    let content_language: Table = get_content_language.call(())?;
    mw.set(
        "getContentLanguage",
        lua.create_function(move |_, ()| Ok(content_language.clone()))?,
    )?;

    // mw.ustring
    //
    // Scribunto's `ustring` is a codepoint-aware version of Lua's `string`. The
    // pattern-matching functions (`match`/`gmatch`/`gsub`/`find`) are the ones
    // modules actually lean on, and implementing them by hand would be a project
    // of its own, so they forward to Lua's `string` library. The divergence is
    // real and worth stating: Lua's indices are *bytes* while Scribunto's are
    // *codepoints*, so a module matching a pattern against a non-ASCII string
    // and then slicing by index gets different offsets. `len` is overridden
    // below, so the common counting case is correct.
    let ustring = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    let ustring_mt = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ustring_mt
        .set(
            "__index",
            lua.create_function(|lua, (_t, key): (Table, Value)| {
                let name = match &key {
                    Value::String(s) => s.to_str().map_err(mlua::Error::external)?.to_string(),
                    _ => return Ok(Value::Nil),
                };
                let string: Table = lua.globals().get("string")?;
                string.get::<Value>(name)
            })
            .map_err(|e| RustoidError::Lua(e.to_string()))?,
        )
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ustring.set("len", lua.create_function(luafn_ustring_len)?)?;
    ustring.set("sub", lua.create_function(luafn_ustring_sub)?)?;
    ustring.set("upper", lua.create_function(luafn_ustring_upper)?)?;
    ustring.set("lower", lua.create_function(luafn_ustring_lower)?)?;
    ustring.set_metatable(Some(ustring_mt));
    mw.set("ustring", ustring)?;

    // mw.message
    let message = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    message.set("new", lua.create_function(luafn_message_new)?)?;
    mw.set("message", message)?;

    // mw.html — built from Lua source (see `HTML_LIB`).
    let html: Table = lua
        .load(HTML_LIB)
        .set_name("mw.html")
        .eval()
        .map_err(|e| RustoidError::Lua(format!("mw.html setup: {e}")))?;
    mw.set("html", html)?;

    // Scribunto's standard-library additions.
    lua.load(LUA_STDLIB_EXTRAS)
        .set_name("lua stdlib extras")
        .exec()
        .map_err(|e| RustoidError::Lua(format!("stdlib setup: {e}")))?;

    Ok(mw)
}

// ---- Standalone Lua functions ----

/// Coerce a Lua value to a string the way Lua's own string functions do.
///
/// Lua's C string library accepts numbers and coerces them, and modules rely on
/// that (`mw.text.trim(frame.args[1])` where the argument is numeric). It also
/// means a `nil` — an absent argument — reaches the function, so the error must
/// *name the function*: mlua's own conversion message
/// ("expected string or number") left 37 corpus pages unattributable.
fn coerce_string(value: &Value, function: &str) -> mlua::Result<String> {
    match value {
        Value::String(s) => s
            .to_str()
            .map(|s| s.to_string())
            .map_err(mlua::Error::external),
        Value::Integer(i) => Ok(i.to_string()),
        Value::Number(n) => Ok(format_number(*n)),
        other => Err(mlua::Error::runtime(format!(
            "bad argument #1 to '{function}' (string expected, got {})",
            other.type_name()
        ))),
    }
}

fn luafn_text_encode(_: &Lua, s: Value) -> mlua::Result<String> {
    Ok(html_escape(&coerce_string(&s, "encode")?))
}

fn luafn_text_decode(_: &Lua, s: Value) -> mlua::Result<String> {
    Ok(html_unescape(&coerce_string(&s, "decode")?))
}

fn luafn_text_trim(_: &Lua, s: Value) -> mlua::Result<String> {
    Ok(coerce_string(&s, "trim")?.trim().to_string())
}

/// `mw.text.listToText` — joins items with `sep` and a `conjunction` before the
/// last, e.g. `a, b and c`. MediaWiki's default conjunction for English is
/// "and".
fn luafn_text_list_to_text(
    _: &Lua,
    (items, sep, conj): (Table, Option<Value>, Option<Value>),
) -> mlua::Result<String> {
    let sep = match &sep {
        Some(v) => coerce_string(v, "listToText")?,
        None => ", ".to_string(),
    };
    let conj = match &conj {
        Some(v) => coerce_string(v, "listToText")?,
        None => "and".to_string(),
    };
    let mut parts: Vec<String> = Vec::new();
    for item in items.sequence_values::<Value>() {
        let item = item?;
        parts.push(coerce_string(&item, "listToText")?);
    }
    Ok(match parts.len() {
        0 => String::new(),
        1 => parts.remove(0),
        _ => {
            let last = parts.pop().unwrap_or_default();
            format!("{} {} {last}", parts.join(&sep), conj)
        }
    })
}

fn luafn_text_split(_: &Lua, (s, sep): (Value, Value)) -> mlua::Result<Vec<String>> {
    let s = coerce_string(&s, "split")?;
    let sep = coerce_string(&sep, "split")?;
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
    text: Value,
    namespace: Option<&Value>,
) -> mlua::Result<Table> {
    let text = coerce_string(&text, "title.new")?;
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
    let ns_id = namespace
        // Scribunto accepts a namespace *name* here as well as an id:
        // `mw.title.new('Foo', 'Template')` is common in modules.
        .and_then(|ns| match ns {
            Value::Integer(i) => Some(*i as i32),
            Value::Number(n) => Some(*n as i32),
            Value::String(s) => s.to_str().ok().and_then(|s| ctx.site.namespace_id(&s)),
            _ => None,
        })
        .or(prefix)
        .unwrap_or(0);
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
    table.set("fullText", full.clone())?;
    // `prefixedText` is the title *including* its namespace, which is what
    // modules compare and display (`text` is the bare page name).
    table.set("prefixedText", full)?;
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

    // Derived fields are computed on demand rather than eagerly: a module that
    // never looks at `talkPageTitle` should not pay for building it, and eager
    // construction would recurse (a title's talk page is itself a title).
    let site = ctx.site.clone();
    let mt = lua.create_table()?;
    mt.set(
        "__index",
        lua.create_function(move |lua, (t, key): (Table, Value)| {
            let key = match &key {
                Value::String(s) => s.to_str().map_err(mlua::Error::external)?.to_string(),
                _ => return Ok(Value::Nil),
            };
            let ns_id: i32 = t.get("namespace").unwrap_or(0);
            let text: String = t.get("text").unwrap_or_default();
            match key.as_str() {
                "isTalkPage" => Ok(Value::Boolean(ns_id % 2 == 1)),
                "isContentPage" => Ok(Value::Boolean(ns_id == 0 || ns_id == 828)),
                "subjectNsText" => lua_str(lua, site.namespace_name(ns_id - (ns_id % 2))),
                "nsText" => lua_str(lua, site.namespace_name(ns_id)),
                "talkPageTitle" => {
                    let talk = ns_id + if ns_id % 2 == 1 { -1 } else { 1 };
                    lua_str(lua, prefix_title(&site, talk, &text))
                }
                "subjectPageTitle" => {
                    let subject = ns_id - (ns_id % 2);
                    lua_str(lua, prefix_title(&site, subject, &text))
                }
                "baseText" => lua_str(
                    lua,
                    text.rsplit_once('/')
                        .map(|(b, _)| b)
                        .unwrap_or("")
                        .to_string(),
                ),
                "subpageText" => lua_str(
                    lua,
                    text.rsplit_once('/')
                        .map(|(_, s)| s)
                        .unwrap_or(&text)
                        .to_string(),
                ),
                _ => Ok(Value::Nil),
            }
        })?,
    )?;
    table.set_metatable(Some(mt));
    Ok(table)
}

/// A `Value::String` from an owned Rust string.
fn lua_str(lua: &Lua, s: String) -> mlua::Result<Value> {
    Ok(Value::String(lua.create_string(&s)?))
}

/// `Namespace:Text`, or just `Text` for the main namespace.
fn prefix_title(site: &LuaSite, ns_id: i32, text: &str) -> String {
    let prefix = site.namespace_name(ns_id);
    if prefix.is_empty() {
        text.to_string()
    } else {
        format!("{prefix}:{text}")
    }
}

fn luafn_title_current(lua: &Lua, ctx: &LuaContext) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("text", ctx.page_title.clone())?;
    table.set("prefixedText", ctx.page_title.clone())?;
    table.set("namespace", 0)?;
    Ok(table)
}

fn luafn_uri_encode(_: &Lua, s: Value) -> mlua::Result<String> {
    Ok(url_encode(&coerce_string(&s, "encode")?))
}

fn luafn_uri_decode(_: &Lua, s: Value) -> mlua::Result<String> {
    Ok(url_decode(&coerce_string(&s, "decode")?))
}

fn luafn_uri_anchor_encode(_: &Lua, s: Value) -> mlua::Result<String> {
    let s = coerce_string(&s, "anchorEncode")?;
    Ok(s.replace(' ', "_").replace('?', "%3F").replace('#', "%23"))
}

/// `formatNum` that accepts both call styles.
///
/// Scribunto's language objects are used as method tables, so
/// `lang:formatNum(x)` passes the language as the first argument while
/// `mw.language.formatNum(x)` does not. Taking `Value`s and using whichever one
/// is a number covers both, which matters because the colon form is the common
/// one in modules.
/// `mw.html`, as Lua source.
///
/// Scribunto implements this builder in Lua, and so does rustoid: it is a tree of
/// nodes with ordered attributes, which is far clearer in Lua than as a pile of
/// host-language closures. The previous Rust stub ignored its content and rejected
/// `mw.html.create(nil)` — but a *tagless* builder is documented and common
/// (`mw.html.create():wikitext(...)`), and wrongly refusing it stopped 37 corpus
/// pages with a message that did not even say which function was at fault.
const HTML_LIB: &str = r#"
local html = {}

local Node = {}
Node.__index = Node

local function new_node(tag)
    return setmetatable({ _tag = tag, _attrs = {}, _order = {}, _children = {} }, Node)
end

function Node:attr(key, value)
    if key == nil then return self end
    if self._attrs[key] == nil then table.insert(self._order, key) end
    self._attrs[key] = value
    return self
end

function Node:addClass(...)
    local list = {}
    if self._attrs['class'] then table.insert(list, self._attrs['class']) end
    for _, class in ipairs({ ... }) do
        if class ~= nil then table.insert(list, tostring(class)) end
    end
    return self:attr('class', table.concat(list, ' '))
end

local function add_style(self, name, value)
    local existing = self._attrs['style']
    local prefix = existing and (existing .. ' ') or ''
    return self:attr('style', prefix .. tostring(name) .. ': ' .. tostring(value) .. ';')
end

function Node:css(name, value)
    if type(name) == 'table' then
        for key, val in pairs(name) do add_style(self, key, val) end
        return self
    end
    return add_style(self, name, value)
end

function Node:cssText(text)
    if text == nil then return self end
    local existing = self._attrs['style']
    local prefix = existing and (existing .. ' ') or ''
    return self:attr('style', prefix .. tostring(text))
end

function Node:tag(tag)
    local child = new_node(tag)
    table.insert(self._children, child)
    return child
end

function Node:wikitext(...)
    for _, part in ipairs({ ... }) do
        if part ~= nil then table.insert(self._children, tostring(part)) end
    end
    return self
end

function Node:newline()
    table.insert(self._children, '\n')
    return self
end

function Node:_render()
    local out = {}
    if self._tag ~= nil then
        table.insert(out, '<' .. self._tag)
        for _, key in ipairs(self._order) do
            table.insert(out, ' ' .. key .. '="' .. tostring(self._attrs[key]) .. '"')
        end
        table.insert(out, '>')
    end
    for _, child in ipairs(self._children) do
        if type(child) == 'table' then
            table.insert(out, child:_render())
        else
            table.insert(out, child)
        end
    end
    if self._tag ~= nil then table.insert(out, '</' .. self._tag .. '>') end
    return table.concat(out)
end

function Node:done() return self:_render() end
function Node:allDone() return self:_render() end

function html.create(tag)
    -- The table form is accepted too: `mw.html.create{ 'div', selfClosing = true }`.
    if type(tag) == 'table' then tag = tag[1] end
    return new_node(tag)
end

return html
"#;

/// Small additions to the standard library that Scribunto provides.
///
/// `table.clone` is Scribunto's, not Lua's, and modules call it freely. Without
/// it a module stops at the call rather than at something diagnosable.
const LUA_STDLIB_EXTRAS: &str = r#"
if table.clone == nil then
    function table.clone(t)
        local copy = {}
        for key, value in pairs(t) do copy[key] = value end
        return copy
    end
end
"#;

/// `mw.clone(value)` — a deep copy that keeps metatables.
///
/// Only tables need copying; every other value is immutable in Lua, so it is
/// returned as-is. Cycles are handled through a memo table, because a module's
/// configuration often refers to itself.
fn luafn_clone(lua: &Lua, value: Value) -> mlua::Result<Value> {
    let memo = lua.create_table()?;
    clone_into(lua, &memo, &value)
}

fn clone_into(lua: &Lua, memo: &Table, value: &Value) -> mlua::Result<Value> {
    let source = match value {
        Value::Table(t) => t,
        other => return Ok(other.clone()),
    };
    // A value already copied (or being copied) is returned as its copy, which is
    // what makes a self-referential table terminate.
    if let Ok(Some(existing)) = memo.raw_get::<Option<Value>>(source.clone()) {
        return Ok(existing);
    }

    let copy = lua.create_table()?;
    memo.raw_set(source.clone(), copy.clone())?;
    for pair in source.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        // Keys are copied too: a table used as a key must be the same object
        // afterwards for lookups to still work.
        let key = clone_into(lua, memo, &k)?;
        let val = clone_into(lua, memo, &v)?;
        copy.raw_set(key, val)?;
    }
    if let Some(mt) = source.metatable() {
        copy.set_metatable(Some(mt));
    }
    Ok(Value::Table(copy))
}

fn luafn_lang_format_num_any(
    _: &Lua,
    (first, second): (Value, Option<Value>),
) -> mlua::Result<String> {
    let arg = match (&second, &first) {
        (Some(v), _) => v,
        (None, v) => v,
    };
    let n = match arg {
        Value::Number(n) => *n,
        Value::Integer(i) => *i as f64,
        Value::String(s) => s
            .to_str()
            .ok()
            .and_then(|s| s.trim().replace(',', "").parse::<f64>().ok())
            .ok_or_else(|| mlua::Error::runtime("formatNum expects a number"))?,
        other => {
            return Err(mlua::Error::runtime(format!(
                "formatNum expects a number, got {}",
                other.type_name()
            )));
        }
    };
    Ok(format_number(n))
}

fn luafn_ustring_len(_: &Lua, s: Value) -> mlua::Result<usize> {
    Ok(coerce_string(&s, "len")?.chars().count())
}

/// `mw.ustring.sub(s, i, j)` — codepoint-indexed, Lua's `string.sub` semantics.
///
/// The third argument is the **end index**, not a length. `Module:String` calls
/// `mw.ustring.sub(s, i, j)` with `j` defaulted to `-1` and range-checked as an
/// index, which is the evidence; treating it as a length produced `a` where Lua
/// produces `ab`.
///
/// Both indices follow Lua's rules: 1-based, negative counts back from the end,
/// and everything is clamped into range. Clamping independently is *not* enough —
/// `sub('abc', 5, -1)` clamps to `start > end` and panics on the slice, which is
/// what `Help:Introduction` triggered. An inverted range is empty.
fn luafn_ustring_sub(_: &Lua, (s, i, j): (Value, i64, Option<i64>)) -> mlua::Result<String> {
    let s = coerce_string(&s, "sub")?;
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;

    // Lua's `posrelat`: a negative index counts back from the end.
    let from = if i < 0 { n + i + 1 } else { i };
    let to = match j {
        None | Some(-1) => n,
        Some(j) if j < 0 => n + j + 1,
        Some(j) => j,
    };

    // `from` clamps to `n + 1`, not `n`: Lua allows a start one past the end and
    // then yields the empty string. Clamping to `n` instead made
    // `sub('abc', 4, -1)` return `c` where Lua returns `""`.
    let from = from.max(1).min(n + 1);
    let to = to.min(n);
    if from > to {
        return Ok(String::new());
    }
    // Both are 1-based; `from <= to <= n` here, so the range is valid.
    Ok(chars[(from - 1) as usize..to as usize].iter().collect())
}

fn luafn_ustring_upper(_: &Lua, s: Value) -> mlua::Result<String> {
    Ok(coerce_string(&s, "upper")?.to_uppercase())
}

fn luafn_ustring_lower(_: &Lua, s: Value) -> mlua::Result<String> {
    Ok(coerce_string(&s, "lower")?.to_lowercase())
}

fn luafn_message_new(lua: &Lua, (key, _args): (Value, Option<Table>)) -> mlua::Result<Table> {
    let key = coerce_string(&key, "message.new")?;
    let table = lua.create_table()?;
    let k = key.clone();
    table.set("key", key)?;
    table.set("plain", lua.create_function(move |_, ()| Ok(k.clone()))?)?;
    Ok(table)
}

/// Build a Scribunto `args` table from a frame's arguments.
///
/// Positional arguments get a *numeric* key, so `args[1]` and `#args` behave;
/// named ones get their name. A metatable resolves the string spelling
/// (`args["1"]`) as well, which modules also write, without duplicating keys and
/// so without upsetting `pairs`.
///
/// Shared with the parent frame, because both are read the same way.
fn build_args_table(lua: &Lua, args: &[Arg]) -> Result<Table> {
    let err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    let args_table = lua.create_table().map_err(err)?;
    let mut next_positional = 0usize;
    for arg in args {
        match arg {
            Arg::Positional(v) => {
                next_positional += 1;
                args_table.set(next_positional, v.clone()).map_err(err)?;
            }
            Arg::Named(k, v) => {
                args_table.set(k.clone(), v.clone()).map_err(err)?;
            }
        }
    }
    let args_mt = lua.create_table().map_err(err)?;
    args_mt
        .set(
            "__index",
            lua.create_function(|_, (t, k): (Table, Value)| {
                // Both spellings are written in the wild (`args[1]` and
                // `args["1"]`), so map either to the other. `raw_get` keeps this
                // from re-entering the metatable.
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
            .map_err(err)?,
        )
        .map_err(err)?;
    args_table.set_metatable(Some(args_mt));
    Ok(args_table)
}

// ---- Frame ----

fn create_frame(
    lua: &Lua,
    args: &[Arg],
    page_title: &str,
    parent_args: Option<&[Arg]>,
    parent_title: Option<&str>,
) -> Result<Value> {
    let frame = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    let args_table = build_args_table(lua, args)?;
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
    // `frame:getParent()` — the calling template's frame. Modules read its
    // `args` *and* its `getTitle()` constantly: `Module:Infobox` asks for args,
    // `Module:Labelled list hatnote` and `Module:Arguments` ask for the title,
    // and returning nil stopped a dozen corpus pages.
    let parent_frame = match parent_args {
        // A direct `{{#invoke:…}}` from the page has no *calling template*.
        // The parser passes the page frame in that case, whose arguments are
        // empty and whose title is the page itself — so key the decision on
        // whether a parent was supplied at all, not on whether it had args.
        Some(parent_args) => {
            let p = lua
                .create_table()
                .map_err(|e| RustoidError::Lua(e.to_string()))?;
            p.set("args", build_args_table(lua, parent_args)?)
                .map_err(|e| RustoidError::Lua(e.to_string()))?;
            let parent_name = parent_title.unwrap_or(page_title).to_string();
            p.set(
                "getTitle",
                lua.create_function(move |_, ()| Ok(parent_name.clone()))?,
            )
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
            // A parent has a parent of its own, but rustoid does not track the
            // chain past one level; nil is what Scribunto gives for the root.
            p.set("getParent", lua.create_function(|_, ()| Ok(Value::Nil))?)
                .map_err(|e| RustoidError::Lua(e.to_string()))?;
            Value::Table(p)
        }
        None => Value::Nil,
    };
    frame.set(
        "getParent",
        lua.create_function(move |_, ()| Ok(parent_frame.clone()))?,
    )?;
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

/// Build `mw.site.namespaces`, indexed by id and by canonical name.
///
/// Modules read properties (`id`, `name`, `canonicalName`, `isContent`, …) off
/// the entries, so each is a table rather than a bare string.
fn luafn_site_namespaces(lua: &Lua, site: &LuaSite) -> Result<Table> {
    let err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    let namespaces = lua.create_table().map_err(err)?;
    // The main namespace is id 0 with the empty name, which the snapshot drops
    // (an empty canonical name carries no information); add it back.
    let mut all: Vec<(i32, String)> = site.namespaces.clone();
    if !all.iter().any(|(id, _)| *id == 0) {
        all.push((0, String::new()));
    }
    all.sort_by_key(|(id, _)| *id);

    for (id, canonical) in all {
        let entry = lua.create_table().map_err(err)?;
        entry.set("id", id).map_err(err)?;
        entry.set("name", canonical.clone()).map_err(err)?;
        entry.set("canonicalName", canonical.clone()).map_err(err)?;
        entry.set("displayName", canonical.clone()).map_err(err)?;
        // `isContent` is true for the namespaces that hold articles: the main
        // namespace plus Module, and false for talk/user/file templates. The
        // exact set is a site detail, so this is the conservative reading.
        entry.set("isContent", id == 0 || id == 828).map_err(err)?;
        entry.set("isTalk", id % 2 == 1).map_err(err)?;
        entry.set("subject", id - (id % 2)).map_err(err)?;
        namespaces.set(id, entry.clone()).map_err(err)?;
        if !canonical.is_empty() {
            namespaces.set(canonical, entry.clone()).map_err(err)?;
        }
    }
    Ok(namespaces)
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
        // Expected values verified against Lua 5.4's `string.sub`, which
        // `ustring.sub` mirrors for codepoint-indexed strings. A previous
        // expectation here was `sub('hello world', 7, 5) == "world"`, which
        // encoded the bug: the third argument is an *end index*, so an inverted
        // range is empty, not a length.
        //
        // The `héllo` cases deliberately differ from byte-indexed
        // `string.sub`: `ustring.sub('héllo', 2, 3)` is `él` (two codepoints),
        // where byte-indexed `string.sub` returns `é` alone because byte 3 is the
        // second byte of that character.
        for (expr, want) in [
            ("mw.ustring.sub('abc', 5, -1)", ""),
            ("mw.ustring.sub('abc', 2)", "bc"),
            ("mw.ustring.sub('abc', -2)", "bc"),
            ("mw.ustring.sub('abc', 1, -2)", "ab"),
            ("mw.ustring.sub('abc', 2, 1)", ""),
            ("mw.ustring.sub('abc', 0, 99)", "abc"),
            ("mw.ustring.sub('abc', -99)", "abc"),
            ("mw.ustring.sub('abc', 1, -99)", ""),
            ("mw.ustring.sub('abc', 4, -1)", ""),
            ("mw.ustring.sub('abc', 3, -1)", "c"),
            ("mw.ustring.sub('abc', -1, -1)", "c"),
            ("mw.ustring.sub('abc', 2, -3)", ""),
            ("mw.ustring.sub('abc', 6, 6)", ""),
            ("mw.ustring.sub('héllo', 1, 1)", "h"),
            ("mw.ustring.sub('héllo', -1, -1)", "o"),
            ("mw.ustring.sub('héllo', 2, 3)", "él"),
            // `héllo` is 5 codepoints, so index 5 is its last one. (Byte-indexed
            // `string.sub` gives `lo` here, which is exactly the divergence.)
            ("mw.ustring.sub('héllo', 5, -1)", "o"),
            ("mw.ustring.sub('héllo', 4, 5)", "lo"),
            ("mw.ustring.sub('', 1, 1)", ""),
        ] {
            assert_eq!(
                engine.eval(&format!("return {expr}")).unwrap(),
                want,
                "{expr}"
            );
        }
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

    /// `require('strict')` and `require('libraryUtil')` are Scribunto's own
    /// libraries, not wiki modules, so they can never be fetched and must be
    /// supplied. Rejecting them was the most common corpus failure.
    #[test]
    fn test_scribunto_builtin_libraries_are_requirable() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                local libraryUtil = require('libraryUtil')
                local ok = pcall(libraryUtil.checkType, 'fn', 1, 'x', 'number')
                local strict = require('strict')
                return type(libraryUtil) .. '/' .. type(strict) .. '/' .. tostring(ok)
            end
            return p
        "#;
        assert_eq!(
            engine.execute(src, "main", &[]).unwrap(),
            "table/table/false"
        );
    }

    /// `require('ustring')` is the same table `mw.ustring` exposes, and the
    /// pattern functions must be there — modules call `ustring.match`
    /// constantly and a missing one stops the module dead.
    #[test]
    fn test_ustring_require_and_pattern_functions() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                local ustring = require('ustring')
                local m = ustring.match('abc123', '%d+')
                local g = mw.ustring.gsub('a-b-c', '-', '+')
                return m .. '/' .. g .. '/' .. ustring.len('héllo')
            end
            return p
        "#;
        assert_eq!(engine.execute(src, "main", &[]).unwrap(), "123/a+b+c/5");
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
