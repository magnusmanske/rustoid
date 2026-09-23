//! Lua/Scribunto engine.
//!
//! Wraps `mlua` to provide a sandboxed Lua runtime for Scribunto modules.
//! Implements the `mw` global table with MediaWiki API stubs.

use std::sync::Arc;

use chrono::{Datelike, Timelike};
use mlua::{Function, Lua, Table, Value};
use unicode_normalization::UnicodeNormalization;

use crate::error::{Result, RustoidError};
use crate::lua::ustring;
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
    /// Article path with its `$1` placeholder, for `mw.title.fullUrl`.
    pub article_path: String,
    /// `(id, canonical name)` for every namespace, for `mw.title` resolution.
    /// `(id, (canonical name, aliases))` for every namespace, for `mw.title` and
    /// `mw.site`. The main namespace (id 0) is excluded: its name is empty.
    pub namespaces: Vec<(i32, String, Vec<String>)>,
    /// The interwiki prefixes, for `mw.site.interwikiMap`.
    pub interwikis: Vec<InterwikiFacts>,
    /// Interface messages (`MediaWiki:` namespace), for `mw.message`. Keyed by
    /// message name only, since one parse has one content language.
    ///
    /// Populated by the host before the engine runs: fetching a message is
    /// asynchronous and Lua is not, so a module cannot look one up on demand.
    /// An empty map is the honest representation of "no messages loaded", which
    /// makes `mw.message`'s existence checks report the message missing rather
    /// than inventing a value.
    pub messages: std::collections::HashMap<String, String>,
    /// `mw.site.stats` — the wiki-wide counters, for the modules that read them.
    pub stats: SiteStats,
}

/// The counters `mw.site.stats` exposes.
///
/// A struct rather than a map of Lua values, so a module reading a key that is
/// not here fails to compile rather than silently answering nil at runtime.
/// The names are Scribunto's, and match the siteinfo statistics keys.
#[derive(Debug, Clone, Default)]
pub struct SiteStats {
    pub pages: u64,
    pub articles: u64,
    pub edits: u64,
    pub images: u64,
    pub users: u64,
    pub active_users: u64,
    pub admins: u64,
}

/// One interwiki prefix as Lua sees it, in `mw.site.interwikiMap`'s terms.
///
/// The fields are the ones the manual documents; the names match what modules
/// index, so a module reading `v["prefix"]` or `v.isLocal` works unchanged.
#[derive(Debug, Clone)]
pub struct InterwikiFacts {
    pub prefix: String,
    pub url: String,
    pub is_local: bool,
    /// A local interwiki that resolves to *this* wiki (PHP's `localinterwiki`
    /// with an empty URL, or a URL matching the wiki's own server).
    pub is_current_wiki: bool,
    /// Whether the URL template has no scheme, so links built from it inherit
    /// the page's own protocol. Mirrors PHP's `protorel`.
    pub is_protocol_relative: bool,
    /// Whether transcluding across this prefix is allowed (`scary
    /// transclusion`), which MediaWiki disables on Wikimedia wikis.
    pub is_transcludable: bool,
    pub is_extra_language_link: bool,
}

impl LuaSite {
    pub fn from_config(config: &dyn SiteConfig) -> Self {
        let mut namespaces: Vec<(i32, String, Vec<String>)> = config
            .namespaces()
            .iter()
            .map(|(id, ns)| {
                // MediaWiki's main namespace has the *empty* name; a config that
                // labels it (the mock calls it "Main") must not leak into Lua,
                // where modules test `nsText == ''` and build titles from it.
                let name = if *id == 0 {
                    String::new()
                } else {
                    ns.canonical.clone()
                };
                (*id, name, ns.aliases.clone())
            })
            .filter(|(_, name, _)| !name.is_empty())
            .collect();
        namespaces.sort_by_key(|(id, _, _)| *id);
        Self {
            server: config.server_url().to_string(),
            language_code: config.language_code().to_string(),
            script_path: config.script_path().to_string(),
            article_path: config.article_path().to_string(),
            namespaces,
            interwikis: interwiki_facts(config),
            // No messages are loaded here: `from_config` has no async access to
            // the `MediaWiki:` namespace. A caller that needs `mw.message` sets
            // them with [`LuaSite::with_messages`].
            messages: std::collections::HashMap::new(),
            // Same reasoning as `messages`: the counters come from the siteinfo
            // API, which a bare config may not carry. Zeroes are the honest
            // "not loaded" answer, and they keep `mw.site.stats.edits` from
            // raising `attempt to index a nil value`.
            stats: config.site_stats(),
        }
    }

    /// Attach preloaded interface messages, for `mw.message`.
    #[must_use]
    pub fn with_messages(mut self, messages: std::collections::HashMap<String, String>) -> Self {
        self.messages = messages;
        self
    }

    /// Attach the wiki's statistics, for `mw.site.stats`.
    #[must_use]
    pub fn with_stats(mut self, stats: SiteStats) -> Self {
        self.stats = stats;
        self
    }

    /// Whether a namespace id exists on this wiki.
    ///
    /// Used for the two namespace facts a title cannot derive from its id: what
    /// its talk page's namespace is, and whether it can have one at all. The main
    /// namespace (0) is present even though its name is empty, since
    /// `namespaces` deliberately drops the empty name.
    pub fn namespace_id_exists(&self, id: i32) -> bool {
        id == 0 || self.namespaces.iter().any(|(ns_id, _, _)| *ns_id == id)
    }

    /// Canonical name of a namespace id, empty for the main namespace.
    pub fn namespace_name(&self, id: i32) -> String {
        if id == 0 {
            return String::new();
        }
        self.namespaces
            .iter()
            .find(|(ns_id, _, _)| *ns_id == id)
            .map(|(_, name, _)| name.clone())
            .unwrap_or_default()
    }

    /// Namespace id for a canonical or localized name, or for a numeric string.
    ///
    /// The main namespace is id 0 and has the empty name, so it is not in
    /// `namespaces` (an empty name carries no information there) and is matched
    /// here instead.
    pub fn namespace_id(&self, name: &str) -> Option<i32> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Some(0);
        }
        if let Ok(id) = trimmed.parse::<i32>() {
            return (id == 0 || self.namespaces.iter().any(|(i, _, _)| *i == id)).then_some(id);
        }
        self.namespaces
            .iter()
            .find(|(_, n, aliases)| {
                n.eq_ignore_ascii_case(trimmed)
                    || aliases.iter().any(|a| a.eq_ignore_ascii_case(trimmed))
            })
            .map(|(id, _, _)| *id)
    }
}

/// Read the wiki's interwiki map into the shape `mw.site.interwikiMap` reports.
///
/// Two of the flags MediaWiki computes cannot be read off the map entry alone,
/// so they are derived here:
///
/// - `isCurrentWiki` is true for a *local* interwiki pointing at this very wiki.
///   MediaWiki decides that by comparing the resolved URL host against
///   `$wgServer`, so the same comparison is made against the configured server.
/// - `isProtocolRelative` mirrors PHP's `protorel`, which is also implied by a
///   URL template that carries no scheme of its own (the usual `//host/$1`
///   form). Modules use it to decide whether to prepend `https:`.
fn interwiki_facts(config: &dyn SiteConfig) -> Vec<InterwikiFacts> {
    let server_host = host_of(config.server_url());
    let mut out: Vec<InterwikiFacts> = config
        .interwiki_map()
        .iter()
        .map(|(key, iw)| {
            // The map key is the prefix modules see; the entry's own `prefix` is
            // a normalized copy and is only a fallback. `Module:Citation/CS1`
            // keys its result by `v["prefix"]`, and both are the same string in
            // practice, but the key is the one that always exists.
            let prefix = iw.prefix.clone().unwrap_or_else(|| key.clone());
            let host = host_of(&iw.url);
            InterwikiFacts {
                is_current_wiki: iw.local
                    && server_host.is_some()
                    && host.as_deref() == server_host.as_deref(),
                is_protocol_relative: iw.protorel.unwrap_or(false) || is_protocol_relative(&iw.url),
                is_extra_language_link: iw.extralanglink.unwrap_or(false),
                is_transcludable: iw.transclusion_allowed,
                is_local: iw.local,
                url: iw.url.clone(),
                prefix,
            }
        })
        .collect();
    // MediaWiki returns the map in its configured order; a stable order here
    // keeps the comparison harness output reproducible.
    out.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    out
}

/// The host of an absolute or protocol-relative URL, lowercased.
///
/// A URL template such as `https://de.wikipedia.org/wiki/$1` yields the host;
/// anything without one (a relative or empty URL, e.g. a `localinterwiki`
/// shortcut) yields `None`.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("//")
        .or_else(|| url.split_once("://").map(|(_, rest)| rest))?;
    let host = rest.split(['/', '?', '#']).next()?;
    // Drop any userinfo and port, neither of which identifies the wiki.
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether a URL template has no scheme, so the link inherits the page's.
fn is_protocol_relative(url: &str) -> bool {
    url.starts_with("//")
}

/// Everything the engine needs about the invocation, supplied together.
#[derive(Debug, Clone, Default)]
pub struct FrameContext {
    /// Module sources available to `require`/`mw.loadData`.
    pub modules: std::collections::HashMap<String, String>,
    /// Arguments of the invoking template's frame.
    pub parent_args: Vec<Arg>,
    /// Its title.
    pub parent_title: Option<String>,
    /// Whether a parent frame exists at all. A direct `{{#invoke:…}}` has none,
    /// while one inside a template does even when that template took no args.
    pub has_parent: bool,
    /// Wikitext of the page being parsed.
    pub page_source: String,
    /// Title of the page being parsed.
    ///
    /// The *root* page, not the frame that made the `#invoke` call: a module
    /// invoked from inside a template asks `getEntityIdForCurrentPage` about the
    /// article, and the invoking frame's title is that template. `build_ast`
    /// fills this in, because it is the only place that knows the real title.
    pub page_title: Option<String>,
    /// Preloaded facts for other pages.
    pub titles: std::collections::HashMap<String, TitleFacts>,
    /// Preloaded Wikidata entities, for `mw.wikibase`.
    pub entities: crate::lua::wikibase::Entities,
}

/// What Lua can observe about a page other than the one being parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleFacts {
    /// The page exists.
    pub exists: bool,
    /// The page is a redirect.
    pub is_redirect: bool,
    /// Wikitext, when it was fetched. `None` means "not fetched", which is
    /// distinct from "empty": `getContent()` on an unfetched page returns nil,
    /// while a fetched empty page returns `""`.
    pub content: Option<String>,
    /// Protection levels per action (`"edit"`, `"move"`, …), as `title.protectionLevels`.
    ///
    /// Scribunto models this as a table of action to *array* whose first item is
    /// the level, so [`crate::traits::ProtectionEntry`] keeps it in that shape.
    /// Empty for a title that was never looked up, which reads as "unprotected" —
    /// the same conservative answer `exists` gives.
    pub protection: crate::traits::ProtectionEntry,
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
    /// Wikitext of the page under test, for `mw.title.getCurrentTitle():getContent()`.
    ///
    /// This is the one page whose content is always available without a fetch —
    /// rustoid is parsing it — and it is the dominant use of `getContent` in
    /// practice (`Module:Footnotes/anchor_id_list`, `Module:Engvar detect`).
    pub page_source: String,
    /// Page facts for other titles, fetched before the module runs.
    ///
    /// Existence, redirect status and content cannot be answered from inside
    /// synchronous Lua, so they are gathered beforehand, exactly as module
    /// sources are. See [`crate::lua::invoke::preload_titles`].
    pub titles: std::collections::HashMap<String, TitleFacts>,
    /// Module sources available to `require`/`mw.loadData`, keyed by full title
    /// (`Module:Foo`). Scribunto's `require` is synchronous inside Lua, so
    /// modules are fetched *before* execution and looked up here; see
    /// [`crate::lua::invoke`].
    pub modules: std::collections::HashMap<String, String>,
    /// Wikidata entities available to `mw.wikibase`, keyed by upper-cased id
    /// (`Q42`), with the page title each id is the sitelink of when known.
    ///
    /// Lua cannot fetch, so entities are gathered before execution exactly as
    /// module sources are; see [`crate::lua::wikibase`]. An id that was not
    /// fetched is simply absent, which is what makes `entityExists` answer false.
    pub entities: crate::lua::wikibase::Entities,
}

impl LuaContext {
    pub fn new(site: LuaSite, page_title: impl Into<String>) -> Self {
        Self {
            site,
            page_title: page_title.into(),
            parent_args: Vec::new(),
            parent_title: None,
            has_parent: false,
            page_source: String::new(),
            titles: std::collections::HashMap::new(),
            modules: std::collections::HashMap::new(),
            entities: crate::lua::wikibase::Entities::default(),
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
            page_source: String::new(),
            titles: std::collections::HashMap::new(),
            modules,
            entities: crate::lua::wikibase::Entities::default(),
        }
    }

    /// Same, with the invoking frame's arguments available as the parent frame.
    /// Everything about the invoking frame, supplied together.
    ///
    /// Grouped rather than passed as six positional arguments: several are
    /// `Option`s or collections of the same type, and a struct makes the call
    /// site say which is which.
    pub fn with_parent(site: LuaSite, page_title: impl Into<String>, frame: FrameContext) -> Self {
        Self {
            site,
            page_title: page_title.into(),
            modules: frame.modules,
            parent_args: frame.parent_args,
            parent_title: frame.parent_title,
            has_parent: frame.has_parent,
            page_source: frame.page_source,
            titles: frame.titles,
            entities: frame.entities,
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
    /// Deferred frame calls raised by the last execution. A `RefCell` because
    /// the Lua closures that fill it must be `'static` and `Fn`, not `FnMut`.
    pending: std::rc::Rc<std::cell::RefCell<Vec<crate::pipeline::lua_deferred::FrameRequest>>>,
}

impl LuaEngine {
    pub fn new(engine_config: LuaEngineConfig, ctx: LuaContext) -> Result<Self> {
        let lua = Lua::new();

        for name in &["io", "package", "require", "loadfile", "dofile"] {
            lua.globals()
                .set(*name, Value::Nil)
                .map_err(|e| RustoidError::Lua(e.to_string()))?;
        }
        // `os` is replaced rather than removed: Scribunto keeps the four
        // functions that cannot touch the filesystem, and modules call
        // `os.date('%Y')` and `os.date('!*t')` to learn the current time (both
        // `Module:Date` and `Module:Citation/CS1/Date_validation` do). It is
        // set here, after the nil-out above, so the order matters.
        lua.globals()
            .set("os", luafn_os_table(&lua)?)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;

        lua.set_memory_limit(engine_config.memory_limit)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;

        let ctx = Arc::new(ctx);
        let mw = setup_mw_table(&lua, ctx.clone())?;
        lua.globals()
            .set("mw", mw)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
        install_module_loader(&lua, &ctx)?;
        install_frame_results(&lua)?;

        Ok(Self {
            lua,
            _context: ctx,
            pending: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        })
    }

    /// Pre-fill a deferred frame call's answer, for the host's re-run.
    ///
    /// Keyed like [`crate::pipeline::lua_deferred::FrameRequest::key`], so the
    /// host need not know the call order: it expands whatever
    /// [`LuaEngine::take_pending`] reported and hands that key back.
    pub fn set_frame_result(&self, key: &str, text: String) -> Result<()> {
        let table: Table = self
            .lua
            .globals()
            .get(crate::pipeline::lua_deferred::RESULTS)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
        table
            .set(key, text)
            .map_err(|e| RustoidError::Lua(e.to_string()))
    }

    /// The deferred frame call the last execution raised, if any.
    ///
    /// The module is re-run from the start once the host has expanded
    /// everything it asked for, so at most one request is ever outstanding.
    pub fn take_pending(&self) -> Option<crate::pipeline::lua_deferred::FrameRequest> {
        self.pending.borrow_mut().pop()
    }

    /// Modules `require`/`mw.loadData` asked for and could not find, taken and
    /// cleared.
    ///
    /// Read after a run rather than inferred from the error, because a `pcall`
    /// around the call swallows the message — and a data module built by name at
    /// runtime (`"Module:Unicode data/" .. key`) is both unwrappable by the static
    /// scan and caught by the module itself.
    pub fn take_missing_modules(&self) -> Vec<String> {
        let Ok(t) = self
            .lua
            .named_registry_value::<mlua::Table>(MISSING_MODULES)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for i in 1..=t.raw_len() {
            if let Ok(v) = t.raw_get::<String>(i)
                && !out.contains(&v)
            {
                out.push(v);
            }
        }
        if let Ok(empty) = self.lua.create_table() {
            let _ = self.lua.set_named_registry_value(MISSING_MODULES, empty);
        }
        out
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
        answers: &crate::pipeline::lua_deferred::DeferredAnswers,
    ) -> Result<String> {
        // The frame must exist *before the module body runs*, not merely before
        // its entry point is called: a module may read `mw.getCurrentFrame()` at
        // module scope, and `Module:Lang` does exactly that on its line 13
        // (`mw.getCurrentFrame():getTitle():match('/sandbox')`). Installing it
        // after the load made that call index nil and failed the whole module,
        // which took `Module:lang`, `Module:Annotated link` and every page that
        // transcludes them with it.
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
            answers.clone(),
            // The engine's own collector: `take_pending` reads what this run
            // asked for, so a fresh one per run would lose it.
            self.pending.clone(),
        )?;
        self.pending.replace(Vec::new());
        // `mw.getCurrentFrame()` reads this.
        self.lua
            .set_named_registry_value("current_frame", frame.clone())
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
        // A fresh missing-module collector for this run. `require` appends to it
        // even when the caller's `pcall` swallows the error; see the comment at
        // the raise in `install_module_loader`.
        self.lua
            .set_named_registry_value(MISSING_MODULES, self.lua.create_table()?)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;

        let module = self.load_module_value(module_source, Some(title))?;
        let func = self.module_function(&module, function_name)?;

        let result: Value = func
            .call::<Value>(frame)
            .map_err(|e| RustoidError::Lua(format!("execution error: {e}")))?;

        // Scribunto runs the returned value through `tostring` before handing it
        // to the parser, so a value with a `__tostring` metamethod — an `mw.html`
        // node, for instance — renders rather than being dropped. Returning a
        // table used to yield the empty string, which lost a module's whole
        // output when it returned a builder.
        let rendered: Value = self
            .lua
            .globals()
            .get::<Function>("tostring")
            .and_then(|tostring| tostring.call(result))
            .unwrap_or(Value::Nil);
        Ok(lua_value_to_string(&rendered))
    }

    /// Run with no module title, for callers that have none (the engine's own
    /// tests, and `eval`).
    pub fn execute(
        &self,
        module_source: &str,
        function_name: &str,
        args: &[Arg],
    ) -> Result<String> {
        self.execute_in(
            module_source,
            "module",
            function_name,
            args,
            &Default::default(),
        )
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

/// Build the argument `WIKIBASE_LIB` expects: the parsed entities, keyed by id,
/// plus the id of the page being parsed.
///
/// Parsing happens here rather than in Lua so that malformed JSON is a build
/// error rather than a runtime one, and so the JSON reader is used exactly once
/// per entity. An entity that does not parse is skipped: a single bad entity
/// must not take the page down, and an absent one is already a state the API
/// handles.
fn entity_table(lua: &Lua, ctx: &LuaContext) -> Result<Table> {
    let lua_err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    let by_id = lua.create_table().map_err(lua_err)?;

    for (id, json) in ctx.entities.iter() {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        // `Special:EntityData/<id>.json` wraps the entity in an `entities` map;
        // accept a bare entity too.
        let entity = parsed
            .get("entities")
            .and_then(|e| e.as_object())
            .and_then(|o| o.values().next())
            .unwrap_or(&parsed)
            .clone();
        let value = json_to_lua(lua, &entity)?;
        by_id.set(id, value).map_err(lua_err)?;
    }

    let out = lua.create_table().map_err(lua_err)?;
    out.set("byId", by_id).map_err(lua_err)?;
    out.set("byTitle", by_title_arg(lua, ctx)?)
        .map_err(lua_err)?;
    out.set("current", ctx.entities.current().map(str::to_string))
        .map_err(lua_err)?;
    Ok(out)
}

/// The title→id index as a Lua table.
fn by_title_arg(lua: &Lua, ctx: &LuaContext) -> Result<Table> {
    let lua_err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    let table = lua.create_table().map_err(lua_err)?;
    for (id, json) in ctx.entities.iter() {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        let entity = parsed
            .get("entities")
            .and_then(|e| e.as_object())
            .and_then(|o| o.values().next())
            .unwrap_or(&parsed);
        if let Some(title) = entity
            .get("sitelinks")
            .and_then(|s| s.get("enwiki"))
            .and_then(|s| s.get("title"))
            .and_then(|t| t.as_str())
        {
            table.set(title.replace('_', " "), id).map_err(lua_err)?;
        }
    }
    Ok(table)
}

/// Convert a JSON value to its Lua equivalent.
///
/// Arrays become 1-based tables so `ipairs` works, which is how modules index
/// statements (`entity:getBestStatements('P31')[1]`). An empty JSON array
/// becomes an empty table, which is what `#statements == 0` expects.
///
/// This is deliberately a small conversion rather than a general JSON bridge:
/// only the shapes that appear in entity data are handled, so anything else
/// stands out rather than being silently coerced.
fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> Result<Value> {
    let lua_err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    match value {
        serde_json::Value::Null => Ok(Value::Nil),
        serde_json::Value::Bool(b) => Ok(Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            // A JSON number that has no fractional part stays an integer, so
            // `numeric-id` compares equal to a Lua integer literal.
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else {
                Ok(Value::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(lua.create_string(s).map_err(lua_err)?)),
        serde_json::Value::Array(items) => {
            let table = lua.create_table().map_err(lua_err)?;
            for (i, item) in items.iter().enumerate() {
                table.set(i + 1, json_to_lua(lua, item)?).map_err(lua_err)?;
            }
            Ok(Value::Table(table))
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table().map_err(lua_err)?;
            for (k, v) in map {
                table
                    .set(k.as_str(), json_to_lua(lua, v)?)
                    .map_err(lua_err)?;
            }
            Ok(Value::Table(table))
        }
    }
}

/// `mw.wikibase` — the entity API, over entity JSON parsed by the host.
///
/// `args` is `{ byId = { [id] = entity }, byTitle = { [title] = id }, current =
/// id }`. An entity is the raw JSON shape, so `statements`, `labels`,
/// `sitelinks` and `claims` are read as they appear in the entity, which is what
/// `Module:Wikidata` does when it walks a snak.
///
/// An unknown id is *absent* rather than a table of empty tables: that is what
/// `entityExists` reports, and the ~19 cached modules that guard on it then take
/// their own fallback — the behaviour of a wiki without Wikidata.
/// Named registry slot holding the titles `require` could not find.
///
/// A registry value rather than a field because the collector has to be reachable
/// from the `require` closure and survive a `pcall` in the module. See
/// [`LuaEngine::take_missing_modules`].
const MISSING_MODULES: &str = "rustoid_missing_modules";

const WIKIBASE_LIB: &str = r#"
return function(args)
    local wb = {}

    local byId = args.byId
    local byTitle = args.byTitle

    -- An entity object. The raw JSON is exposed directly, and the methods below
    -- are added to it, which is how Wikibase's own object behaves: it answers to
    -- both `entity.id` and `entity:getId()`.
    local Entity = {}
    -- The method table is consulted before the raw data, so `entity.getLabel`
    -- is the function and `entity.labels` is the JSON field. `rawget` is used
    -- for both lookups: going through `Entity` itself would re-enter this
    -- metamethod and recurse.
    Entity.__index = function(self, key)
        local method = rawget(Entity, key)
        if method ~= nil then return method end
        return rawget(self, '_data')[key]
    end

    local function wrap(data)
        return setmetatable({ _data = data }, Entity)
    end

    -- Statement lists are returned as fresh tables so a caller cannot mutate the
    -- cached entity by sorting or removing.
    local function copy_list(list)
        local out = {}
        for i = 1, #list do out[i] = list[i] end
        return out
    end

    function Entity:getId() return rawget(self, '_data').id end
    function Entity:getType() return rawget(self, '_data').type end

    -- `entity:getLabel( lang )` — the label is a map of language to `{value=…}`.
    function Entity:getLabel(lang)
        local labels = rawget(self, '_data').labels
        if not labels then return nil end
        local entry = labels[lang or 'en']
        return entry and entry.value
    end

    function Entity:getDescription(lang)
        local descs = rawget(self, '_data').descriptions
        if not descs then return nil end
        local entry = descs[lang or 'en']
        return entry and entry.value
    end

    function Entity:getSitelink(site)
        local links = rawget(self, '_data').sitelinks
        if not links then return nil end
        local entry = links[site or 'enwiki']
        return entry and entry.title
    end

    function Entity:getAllStatements(property)
        local claims = rawget(self, '_data').claims
        if not claims or not property then return {} end
        local list = claims[property]
        if not list then return {} end
        return copy_list(list)
    end

    -- `getBestStatements` filters to rank `preferred` when any exist, else
    -- `normal`. Deprecated ranks never win, which is the documented rule and
    -- what `Module:Sister project links` relies on for its P424 lookup.
    function Entity:getBestStatements(property)
        local all = self:getAllStatements(property)
        if #all == 0 then return {} end
        local best, normal = {}, {}
        for _, st in ipairs(all) do
            -- A statement with no rank defaults to `normal`.
            local rank = st.rank or 'normal'
            if rank == 'preferred' then
                best[#best + 1] = st
            elseif rank == 'normal' then
                normal[#normal + 1] = st
            end
        end
        if #best > 0 then return best end
        return normal
    end

    -- The id for a title, or for the current page when no title is given.
    -- Wikibase resolves this by sitelink search; here it is a lookup over the
    -- entities that were actually fetched.
    function wb.getEntityIdForTitle(title)
        if title == nil or title == '' then
            return args.current
        end
        local key = tostring(title):gsub('_', ' ')
        return byTitle[key]
    end

    function wb.getEntityIdForCurrentPage()
        return args.current
    end

    -- `mw.wikibase.getEntity( id )` — the entity object, or nil when it is not
    -- available. No argument means the current page, which is how real Wikibase
    -- behaves and how `Module:Location map` calls it.
    function wb.getEntity(id)
        local wanted = id or args.current
        if wanted == nil or wanted == '' then return nil end
        local data = byId[tostring(wanted):upper()]
        if not data then return nil end
        return wrap(data)
    end

    wb.getEntityObject = wb.getEntity

    function wb.entityExists(id)
        if id == nil then return false end
        return byId[tostring(id):upper()] ~= nil
    end

    -- An entity id is `Q`/`P` followed by digits, with no leading zero. Checked
    -- rather than trusted because modules pass arbitrary frame arguments here.
    function wb.isValidEntityId(id)
        if type(id) ~= 'string' then return false end
        return id:match('^[QqPp][1-9]%d*$') ~= nil
    end

    function wb.getLabel(id, lang)
        local entity = wb.getEntity(id)
        if not entity then return nil end
        return entity:getLabel(lang)
    end

    function wb.getLabelByLang(id, lang)
        return wb.getLabel(id, lang)
    end

    function wb.getDescription(id, lang)
        local entity = wb.getEntity(id)
        if not entity then return nil end
        return entity:getDescription(lang)
    end

    function wb.getDescriptionWithLang(id, lang)
        local entity = wb.getEntity(id)
        if not entity then return nil end
        local langcode = lang or 'en'
        return entity:getDescription(langcode), langcode
    end

    function wb.getSitelink(id, site)
        local entity = wb.getEntity(id)
        if not entity then return nil end
        return entity:getSitelink(site)
    end

    function wb.getBestStatements(id, property)
        local entity = wb.getEntity(id)
        if not entity then return {} end
        return entity:getBestStatements(property)
    end

    function wb.getAllStatements(id, property)
        local entity = wb.getEntity(id)
        if not entity then return {} end
        return entity:getAllStatements(property)
    end

    -- The wiki the entities come from. `getGlobalSiteId` is the *client* wiki's
    -- language, which is `enwiki` on the wiki under test.
    function wb.getGlobalSiteId() return 'enwiki' end

    return wb
end
"#;

/// Install the table deferred frame-call answers are parked in.
///
/// One table on `_G` rather than one registry key per call: the answers are
/// plain Lua values (strings, or tables carrying parser tokens) and the engine
/// hands them straight back to the module that asked for them.
fn install_frame_results(lua: &Lua) -> Result<()> {
    let table = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    lua.globals()
        .set(crate::pipeline::lua_deferred::RESULTS, table)
        .map_err(|e| RustoidError::Lua(e.to_string()))
}

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
    // A second handle on the module sources: `require` takes the first by move,
    // and `mw.loadJsonData` needs to look up data pages in the same map.
    let modules_for_json = modules.clone();
    let require = lua
        .create_function(move |lua, name: Value| {
            let title = match name {
                Value::String(s) => s.to_str().map_err(mlua::Error::external)?.to_string(),
                // A non-string name is reported by *type*, not by value: Lua's default
                // stringification is `table: 0x…`, which named nothing in the
                // "could not load … still missing: table: 0x78642c180" messages this
                // used to produce. The call site is then findable from the type.
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "require expects a module name string, got a {}",
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
                // Record the title before raising, so it survives a `pcall`.
                //
                // The error alone is not enough: `Module:Unicode data` builds its
                // data-module names at runtime (`"Module:Unicode data/" .. key`)
                // and calls `pcall(mw.loadData, …)`, so the message this raises is
                // caught and converted to `false` — and the caller then indexes
                // that boolean. The preload scan cannot see the name either, since
                // only the prefix is a literal.
                //
                // Putting the name somewhere the *parser* can read after the round
                // is what closes the gap: the deferred loop fetches it and re-runs,
                // exactly as it already does for an uncaught `require`.

                if let Ok(missing) = lua.named_registry_value::<Table>(MISSING_MODULES) {
                    let len = missing.raw_len();
                    missing.raw_set(len + 1, title.clone())?;
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
    // `mw.loadJsonData(page)` — a data page's JSON, parsed to a table.
    //
    // The manual: "the same as `mw.loadData()` above, except it loads data from
    // JSON pages rather than Lua tables", and "it throws an error if the page
    // does not exist or if it is empty". Because Lua cannot fetch, a page that
    // was not preloaded is reported the same way a missing module is, so the
    // caller's retry loop fetches it and re-runs — which is what lets
    // `Module:Music chart` reach `Module:Music chart/data`.
    mw.set(
        "loadJsonData",
        lua.create_function(move |lua, name: Value| {
            let title = match &name {
                Value::String(s) => s.to_str().map_err(mlua::Error::external)?.to_string(),
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "mw.loadJsonData expects a page name string, got a {}",
                        other.type_name()
                    )));
                }
            };
            let Some(source) = lookup_module(&modules_for_json, &title) else {
                // Same out-of-band record as `require`, so the name survives a
                // `pcall` and the retry loop can act on it.
                if let Ok(missing) = lua.named_registry_value::<Table>(MISSING_MODULES) {
                    let len = missing.raw_len();
                    missing.raw_set(len + 1, title.clone())?;
                }
                return Err(mlua::Error::runtime(format!(
                    "module {title} was not preloaded"
                )));
            };
            // An empty page is an error rather than an empty table, per the
            // manual, so a module that guards on the result sees the failure.
            if source.trim().is_empty() {
                return Err(mlua::Error::runtime(format!(
                    "mw.loadJsonData: page {title} is empty"
                )));
            }
            let parsed: serde_json::Value = serde_json::from_str(source).map_err(|e| {
                mlua::Error::runtime(format!("mw.loadJsonData: {title} is not valid JSON: {e}"))
            })?;
            // "The JSON content must be an array or object", so a bare scalar
            // is rejected rather than silently wrapped.
            if !parsed.is_array() && !parsed.is_object() {
                return Err(mlua::Error::runtime(format!(
                    "mw.loadJsonData: {title} must contain an array or object"
                )));
            }
            json_to_lua(lua, &parsed).map_err(|e| mlua::Error::runtime(e.to_string()))
        })
        .map_err(lua_err)?,
    )
    .map_err(lua_err)?;

    let require_fn: Function = lua.globals().get("require").map_err(lua_err)?;
    mw.set(
        "loadData",
        // A dynamic name (`mw.loadData(cfgModule)`) is not a string, and a
        // `String` parameter made that a bare "error converting Lua table to
        // String" naming no function. Reporting the type instead says which call
        // is at fault.
        lua.create_function(move |_, name: Value| {
            if !matches!(name, Value::String(_)) {
                return Err(mlua::Error::runtime(format!(
                    "mw.loadData expects a module name string, got a {}",
                    name.type_name()
                )));
            }
            require_fn.call::<Value>(name)
        })
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

        -- Returns a checker that raises when `self` is not the object the
        -- checker was made for. Scribunto also accepts a method *name* as the
        -- second argument to the returned function, and reports the object's
        -- description ("ItalicTitle object") when the identity check fails,
        -- which is what makes such a mistake readable.
        function libraryUtil.makeCheckSelfFunction(libraryName, varName, selfObj, selfObjDesc)
            selfObjDesc = selfObjDesc or (varName and (varName .. ' object') or 'self object')
            return function(self, method)
                if self ~= selfObj then
                    error(string.format(
                        "%s: '%s' is not a valid method",
                        libraryName, tostring(method)), 3)
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
    // `mw.text.jsonDecode(s, flags)` — the JSON bridge, over the host's parser.
    // The two documented flags are exposed as the constants modules pass, since
    // a module writes `mw.text.JSON_PRESERVE_KEYS` rather than a bare number.
    text.set("JSON_PRESERVE_KEYS", JSON_PRESERVE_KEYS)?;
    text.set("JSON_TRY_FIXING", JSON_TRY_FIXING)?;
    text.set("jsonDecode", lua.create_function(luafn_text_json_decode)?)?;
    // `mw.text.split` and `mw.text.gsplit` are written in `LUA_STDLIB_EXTRAS`,
    // over the literal-split primitive below, because the pattern form needs
    // `mw.ustring.find` and the pattern dialect belongs in one place.
    text.set("__splitPlain", lua.create_function(luafn_text_split_plain)?)?;
    text.set("tag", lua.create_function(luafn_text_tag)?)?;
    text.set("nowiki", lua.create_function(luafn_text_nowiki)?)?;
    text.set("listToText", lua.create_function(luafn_text_list_to_text)?)?;
    // The strip-marker functions operate on MediaWiki's "UNIQ…QINU" markers,
    // which rustoid has no equivalent of: a `nowiki`/`ref`/`gallery` tag is
    // parsed into the tree rather than carried through Lua as a marker. There is
    // therefore nothing to unstrip, and returning the input unchanged is the
    // correct answer rather than a stub — the text a module sees never held a
    // marker to begin with.
    for name in ["unstripNoWiki", "unstrip", "killMarkers"] {
        text.set(
            name,
            lua.create_function(|_, s: Value| coerce_string(&s, "unstrip"))?,
        )?;
    }
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
    // `mw.title.makeTitle( ns, text, fragment )` — always applies the given
    // namespace, unlike `new`, which lets a prefix in the text win. `Module:Listen`
    // calls `mw.title.makeTitle(-2, filename)`, and with `new` the text would win.
    let ctx_make = ctx.clone();
    title.set(
        "makeTitle",
        lua.create_function(
            move |lua, (ns, text, fragment): (Value, Value, Option<Value>)| {
                let ns = given_namespace_id(&ctx_make.site, &ns)
                    .ok_or_else(|| mlua::Error::runtime("makeTitle: unknown namespace"))?;
                let mut text = coerce_string(&text, "makeTitle")?;
                // A fragment is an argument here rather than part of the text.
                if let Some(v) = fragment.as_ref().filter(|v| !v.is_nil()) {
                    text.push('#');
                    text.push_str(&coerce_string(v, "makeTitle")?);
                }
                luafn_title_new(
                    lua,
                    &ctx_make,
                    Value::String(lua.create_string(text)?),
                    Some(&Value::Integer(ns as i64)),
                )
            },
        )?,
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
    // `mw.site.interwikiMap(filter)` — `Module:Citation/CS1/Configuration` spins
    // through the local entries to learn which prefixes are language codes, and
    // stops with "attempt to call a nil value (field 'interwikiMap')" without it.
    let iw_site = ctx.site.clone();
    site.set(
        "interwikiMap",
        lua.create_function(move |lua, filter: Option<Value>| {
            luafn_site_interwiki_map(lua, &iw_site, filter.as_ref())
                .map_err(|e| mlua::Error::runtime(e.to_string()))
        })?,
    )?;
    // `mw.site.stats` — the wiki-wide counters. `Module:Math` seeds its RNG from
    // `mw.site.stats.edits + mw.site.stats.pages`, so a missing table stops the
    // module with "attempt to index a nil value (field 'stats')".
    let stats = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    stats.set("pages", ctx.site.stats.pages)?;
    stats.set("articles", ctx.site.stats.articles)?;
    stats.set("edits", ctx.site.stats.edits)?;
    stats.set("images", ctx.site.stats.images)?;
    stats.set("users", ctx.site.stats.users)?;
    stats.set("activeUsers", ctx.site.stats.active_users)?;
    stats.set("admins", ctx.site.stats.admins)?;
    site.set("stats", stats)?;
    mw.set("site", site)?;

    // `mw.isSubsting()` reports whether the current parse is a `subst:`. rustoid
    // never substitutes, so it is always false — which is the answer modules
    // such as `Module:Unsubst` need in order to take their normal path.
    mw.set("isSubsting", lua.create_function(|_, ()| Ok(false))?)?;

    // `mw.log.*` and `mw.logObject` write to the debug console. rustoid has
    // none to write to, so these discard; the point is that a module calling them
    // does not stop, and `Module:Footnotes/anchor_id_list` calls `mw.logObject`
    // directly.
    let log = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    for name in ["log", "warn", "info", "error", "debug"] {
        log.set(
            name,
            lua.create_function(|_, _args: mlua::MultiValue| Ok(()))
                .map_err(|e| RustoidError::Lua(e.to_string()))?,
        )
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    }
    log.set(
        "logObject",
        lua.create_function(|_, (_v, _prefix): (Value, Option<Value>)| Ok(()))
            .map_err(|e| RustoidError::Lua(e.to_string()))?,
    )
    .map_err(|e| RustoidError::Lua(e.to_string()))?;
    mw.set("log", log.clone())
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    // `mw.log` is *also* callable directly (`mw.log('text')`), which is how
    // `Module:Footnotes` uses it, so the table needs a `__call` metamethod.
    let log_mt = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    log_mt
        .set(
            "__call",
            lua.create_function(|_, (_t, _args): (Value, mlua::MultiValue)| Ok(()))
                .map_err(|e| RustoidError::Lua(e.to_string()))?,
        )
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    log.set_metatable(Some(log_mt));
    // `mw.logObject(x)` is the module-level shorthand for `mw.log.logObject(x)`.
    let log_object: Function = log
        .get("logObject")
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    mw.set("logObject", log_object)
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    // `mw.clone(value)` — a deep copy, preserving metatables. Modules copy shared
    // configuration tables before modifying them, and a shallow copy would let
    // one invocation corrupt another's data.
    mw.set("clone", lua.create_function(luafn_clone)?)?;

    // `mw.addWarning(text)` — adds to the parser's warning list, which MediaWiki
    // shows above the edit box rather than in the page. Keeping a list nothing
    // reads is enough: the point is that the module does not stop.
    let warnings = lua.create_table()?;
    mw.set(
        "addWarning",
        lua.create_function(move |_, text: Value| {
            let text = coerce_string(&text, "addWarning")?;
            warnings.set(warnings.raw_len() + 1, text)?;
            Ok(())
        })?,
    )?;

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

    // mw.language — built by its own module, which also yields the content
    // language object the `mw.getContentLanguage` shorthand returns.
    let (lang, content_language) =
        crate::lua::language::build_language_library(lua, ctx.site.language_code.as_str())
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
    mw.set("language", lang)?;

    // `mw.getContentLanguage()` is the shorthand for
    // `mw.language.getContentLanguage()`; `Module:Citation/CS1/Configuration`
    // calls it on its first line, so without it the whole CS1 stack fails.
    mw.set(
        "getContentLanguage",
        lua.create_function(move |_, ()| Ok(content_language.clone()))?,
    )?;
    // `mw.getLanguage(code)` is the shorthand for `mw.language.new(code)`.
    let language_table: Table = mw.get("language")?;
    let language_new: Function = language_table.get("new")?;
    mw.set(
        "getLanguage",
        lua.create_function(move |_, code: Value| language_new.call::<Table>(code))?,
    )?;

    // mw.ustring
    //
    // Scribunto's `ustring` is a codepoint-aware version of Lua's `string`,
    // with its own pattern engine: the classes are Unicode properties (see
    // `crate::lua::ustring`) and every index is a codepoint. The pattern
    // functions below are therefore overridden rather than inherited, because
    // inheriting them is observable — it raised "malformed pattern (missing
    // ']')" on the C0-control set in `Module:Citation/CS1`, which the service
    // matches happily, and it made `%d` ASCII-only where the service matches
    // fullwidth digits.
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
    // `char` must be overridden rather than inherited through `__index`. Lua's
    // `string.char` takes bytes and rejects anything above 255, while
    // Scribunto's takes *codepoints*; `Module:Lang` calls
    // `mw.ustring.char(codepoint)` on whatever it parsed out of a language tag,
    // and the inherited byte version raised "value out of range" and took the
    // whole module down with it on seven corpus pages.
    ustring.set("char", lua.create_function(luafn_ustring_char)?)?;
    ustring.set("upper", lua.create_function(luafn_ustring_upper)?)?;
    ustring.set("lower", lua.create_function(luafn_ustring_lower)?)?;
    // `ucfirst`/`lcfirst` prefer the *first* character's case change, unlike
    // `upper`/`lower`. Scribunto's versions are codepoint-aware; these are close
    // enough for the ASCII case, and modules use them for identifiers.
    ustring.set(
        "ucfirst",
        lua.create_function(|_, s: Value| {
            let s = coerce_string(&s, "ucfirst")?;
            let mut chars = s.chars();
            Ok(match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            })
        })?,
    )?;
    ustring.set(
        "lcfirst",
        lua.create_function(|_, s: Value| {
            let s = coerce_string(&s, "lcfirst")?;
            let mut chars = s.chars();
            Ok(match chars.next() {
                Some(c) => c.to_lowercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            })
        })?,
    )?;
    // `gcodepoint` yields both a value and a new control, and mlua keeps only the
    // first of a closure's tuple return when the closure is used as a `for`
    // iterator. The codepoints are therefore collected in Rust and the iterator
    // itself is written in Lua, where a multi-value return is native.
    ustring.set("codepoints", lua.create_function(luafn_ustring_codepoints)?)?;
    // The string-shape predicates and the normalization family. These are
    // independent of the pattern dialect, so they do not wait on it.
    ustring.set("isutf8", lua.create_function(luafn_ustring_isutf8)?)?;
    ustring.set("toNFC", lua.create_function(luafn_ustring_to_nfc)?)?;
    ustring.set("toNFD", lua.create_function(luafn_ustring_to_nfd)?)?;
    ustring.set("toNFKC", lua.create_function(luafn_ustring_to_nfkc)?)?;
    ustring.set("toNFKD", lua.create_function(luafn_ustring_to_nfkd)?)?;
    ustring.set("byteoffset", lua.create_function(luafn_ustring_byteoffset)?)?;
    // The four pattern functions. `find` and `match` are the primitives; the
    // iterator and replacement forms are written in Lua over them, which keeps
    // the multi-value returns in Lua where they are native.
    ustring.set("find", lua.create_function(luafn_ustring_find)?)?;
    ustring.set("match", lua.create_function(luafn_ustring_match)?)?;
    // The anchored-captures primitive the Lua-side `gsub`/`gmatch` drive their
    // loops with. Double underscore marks it as internal, as elsewhere.
    ustring.set("__captures", lua.create_function(luafn_ustring_captures)?)?;
    ustring.set_metatable(Some(ustring_mt));
    mw.set("ustring", ustring)?;

    // `mw.ext.*` are extension-provided helpers; the one that matters in
    // practice is `mw.ext.data.get`, which loads a Wikidata tabular-data page.
    // rustoid does not implement the extension, so it returns an empty table:
    // a module reading statistics then renders nothing rather than aborting the
    // whole page. Making it *nil* is what fails, with an unattributed "attempt to
    // index a nil value (field 'ext')".
    let ext = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    let ext_data = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ext_data
        .set(
            "get",
            lua.create_function(|lua, _name: Value| {
                let empty = lua.create_table()?;
                empty.set("schema", lua.create_table()?)?;
                empty.set("data", lua.create_table()?)?;
                Ok(empty)
            })
            .map_err(|e| RustoidError::Lua(e.to_string()))?,
        )
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ext.set("data", ext_data)
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    // `mw.ext.TitleBlacklist.test` — whether the TitleBlacklist extension would
    // block an action on a page.
    //
    // Returns `nil`, not a table: a *hit* carries restriction parameters, and
    // rustoid cannot decide a hit. That asymmetry is deliberate rather than
    // lazy. `Module:Effective protection level` reads the result as
    // `if blacklistentry then <stricter level> elseif …`, so nil takes the
    // permissive branch, and a blacklist that raises protection cannot raise it
    // further by being ignored. Answering with a table would invent a hit and
    // *stricter* protection than the wiki applies. The most common corpus Lua
    // failure was this field being absent (34 of 48 pages).
    let ext_blacklist = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ext_blacklist
        .set(
            "test",
            lua.create_function(|_, (_action, _title): (Value, Value)| Ok(Value::Nil))
                .map_err(|e| RustoidError::Lua(e.to_string()))?,
        )
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    // The extension exposes `test` on the table itself; `test` is also reachable
    // as a method, so a module writing `mw.ext.TitleBlacklist:test(…)` must not
    // get "attempt to call a nil value".
    ext_blacklist
        .set(
            "check",
            lua.create_function(|_, _title: Value| Ok(Value::Nil))
                .map_err(|e| RustoidError::Lua(e.to_string()))?,
        )
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ext.set("TitleBlacklist", ext_blacklist)
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    // `mw.ext.ParserFunctions.expr` — ParserFunctions' `#expr` as a function.
    // `Module:Math` calls it when the `#expr` *parser function* is unavailable to
    // it; rustoid already evaluates `#expr`, so the same implementation answers
    // both, and the field is what stops "attempt to index a nil value
    // (field 'ParserFunctions')".
    let ext_pf = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ext_pf
        .set(
            "expr",
            lua.create_function(move |lua, expr: String| {
                let value = crate::pipeline::parser_functions::evaluate_expression(&expr);
                // `#expr` reports a malformed expression as an error span rather
                // than as a value, and ParserFunctions' Lua binding turns that
                // into a table with an `error` field. Detecting it the way
                // `pf_iferror` does keeps the two paths in agreement.
                if value.contains("class=\"error\"") {
                    let err = lua.create_table()?;
                    err.set("error", value)?;
                    Ok(Value::Table(err))
                } else {
                    Ok(Value::String(lua.create_string(&value)?))
                }
            })
            .map_err(|e| RustoidError::Lua(e.to_string()))?,
        )
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ext.set("ParserFunctions", ext_pf)
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    mw.set("ext", ext)
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    // mw.message — the object methods live in `MESSAGE_LIB`, which is given the
    // message store and the content language as its two arguments.
    let messages = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    for (key, text) in &ctx.site.messages {
        messages
            .set(key.as_str(), text.as_str())
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
    }
    let message: Table = lua
        .load(MESSAGE_LIB)
        .set_name("mw.message")
        .eval::<Function>()
        .and_then(|build| build.call(messages))
        .map_err(|e| RustoidError::Lua(format!("mw.message setup: {e}")))?;
    mw.set("message", message)?;

    // mw.wikibase — the entity JSON is parsed once here and the API is expressed
    // in Lua (`WIKIBASE_LIB`), because the entity object is a plain data wrapper
    // and its methods are far clearer as Lua than as a dozen registered
    // closures.
    let entities: Table = {
        let args =
            entity_table(lua, ctx.as_ref()).map_err(|e| mlua::Error::runtime(e.to_string()))?;
        lua.load(WIKIBASE_LIB)
            .set_name("mw.wikibase")
            .eval::<Function>()
            .and_then(|build| build.call(args))
            .map_err(|e| RustoidError::Lua(format!("mw.wikibase setup: {e}")))?
    };
    mw.set("wikibase", entities)?;

    // mw.html — built from Lua source (see `HTML_LIB`).
    let html: Table = lua
        .load(HTML_LIB)
        .set_name("mw.html")
        .eval()
        .map_err(|e| RustoidError::Lua(format!("mw.html setup: {e}")))?;
    mw.set("html", html)?;

    // Scribunto's standard-library additions. The chunk needs the `mw` table it
    // extends, and the global is not registered until `setup_mw_table` returns,
    // so the table is bound to a name for the duration of the call.
    lua.globals()
        .set("__rustoid_mw", mw.clone())
        .map_err(|e| RustoidError::Lua(format!("stdlib setup: {e}")))?;
    lua.load(LUA_STDLIB_EXTRAS)
        .set_name("lua stdlib extras")
        .exec()
        .map_err(|e| RustoidError::Lua(format!("stdlib setup: {e}")))?;
    lua.globals()
        .set("__rustoid_mw", Value::Nil)
        .map_err(|e| RustoidError::Lua(format!("stdlib setup: {e}")))?;
    // The comparison dispatcher is installed by the extras chunk, but only
    // `setup_mw_table`'s callers build the objects that use it, so leave the
    // entry point reachable rather than having each Rust constructor look it up.
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
pub(crate) fn coerce_string(value: &Value, function: &str) -> mlua::Result<String> {
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

/// `mw.text.nowiki( s )` — escape the characters that would otherwise be read as
/// wikitext, so the text renders literally.
///
/// The manual lists the cases; they are position-dependent, which is why this is
/// not a simple character map:
///
/// - a fixed set of characters is always escaped (`"`, `&`, `'`, `<`, `=`, `>`,
///   `[`, `]`, `{`, `|`, `}`);
/// - line-leading characters (`#`, `*`, `:`, `;`, space, tab) are escaped only at
///   the start of a line;
/// - a blank line has one of its newline characters escaped, and `----` has its
///   first `-` escaped;
/// - `__` loses one underscore, `://` loses its colon, and whitespace after
///   `ISBN`, `RFC` or `PMID` is escaped (those prefixes would otherwise become
///   magic links).
///
/// `Module:Citation/CS1` escapes an identifier with this so that a value such as
/// `ISBN 123` cannot turn into a link.
fn luafn_text_nowiki(_: &Lua, s: Value) -> mlua::Result<String> {
    let text = coerce_string(&s, "nowiki")?;
    let mut out = String::with_capacity(text.len());
    // Whether the next character starts a line, which the position-dependent
    // rules key off. The string starts at a line start, as MediaWiki treats it.
    let mut at_line_start = true;

    for (idx, ch) in text.char_indices() {
        // `ISBN`, `RFC`, `PMID` followed by whitespace become magic links.
        if ch.is_whitespace() && ends_with_magic_link_prefix(&text[..idx]) {
            out.push_str("&#");
            out.push_str(&(ch as u32).to_string());
            out.push(';');
            at_line_start = false;
            continue;
        }

        let escaped = match ch {
            '"' | '&' | '\'' | '<' | '=' | '>' | '[' | ']' | '{' | '|' | '}' => {
                Some(named_or_numeric(ch))
            }
            '#' | '*' | ':' | ';' | ' ' | '\t' if at_line_start => Some(named_or_numeric(ch)),
            _ => None,
        };
        if let Some(replacement) = escaped {
            out.push_str(&replacement);
            at_line_start = false;
            continue;
        }

        match ch {
            // A blank line: escape its newline so the blank line is preserved.
            '\n' => {
                if at_line_start {
                    out.push_str("&#10;");
                } else {
                    out.push('\n');
                }
                at_line_start = true;
                continue;
            }
            // `----` at a line start would be a horizontal rule.
            '-' if at_line_start && text[idx..].starts_with("----") => {
                out.push_str("&#45;");
                at_line_start = false;
                continue;
            }
            // `__` is a behavior-switch delimiter.
            '_' if text[idx..].starts_with("__") => {
                out.push_str("&#95;");
                at_line_start = false;
                continue;
            }
            // `://` would make the preceding text a protocol.
            ':' if text[idx..].starts_with("://") => {
                out.push_str("&#58;");
                at_line_start = false;
                continue;
            }
            _ => {}
        }
        out.push(ch);
        at_line_start = false;
    }
    Ok(out)
}

/// Whether `before` ends with one of the prefixes that form a magic link.
fn ends_with_magic_link_prefix(before: &str) -> bool {
    const PREFIXES: [&str; 3] = ["ISBN", "RFC", "PMID"];
    PREFIXES.iter().any(|p| before.ends_with(p))
}

/// The named entity for the characters the manual names, else a numeric one.
///
/// Only the five names the manual lists have short forms; the rest are emitted
/// numerically, as MediaWiki does.
fn named_or_numeric(ch: char) -> String {
    match ch {
        '<' => "&lt;".to_string(),
        '>' => "&gt;".to_string(),
        '&' => "&amp;".to_string(),
        '"' => "&quot;".to_string(),
        '\'' => "&#39;".to_string(),
        other => format!("&#{};", other as u32),
    }
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

/// `mw.text.split( s, pattern, plain )` — the pieces between separator matches.
///
/// Only the `plain` form is implemented in Rust: a literal separator is a
/// substring split, and the pattern form is handled in `LUA_STDLIB_EXTRAS`,
/// where the matching is done with `mw.ustring.find` — the same function
/// Scribunto's own library uses, so the pattern dialect is the Ustring one.
fn luafn_text_split_plain(_: &Lua, (s, sep): (Value, Value)) -> mlua::Result<Vec<String>> {
    let s = coerce_string(&s, "split")?;
    let sep = coerce_string(&sep, "split")?;
    if sep.is_empty() {
        // A separator that matches the empty string splits into characters, as
        // the manual says for the pattern case.
        return Ok(s.chars().map(|c| c.to_string()).collect());
    }
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
    // `prefixedText` is the title *including* its namespace, which is what
    // modules compare and display (`text` is the bare page name).
    let full = if ns_text.is_empty() {
        title_text.clone()
    } else {
        format!("{ns_text}:{title_text}")
    };
    table.set("prefixedText", full.clone())?;
    // `fullText` is `prefixedText` *plus* the fragment, which is the one way the
    // two differ.
    table.set(
        "fullText",
        if fragment.is_empty() {
            full.clone()
        } else {
            format!("{full}#{fragment}")
        },
    )?;
    // `exists` and `isRedirect` are *not* set here: they live on the metatable,
    // which consults the preloaded facts. An eager `table.set` would shadow it
    // and silently report every page as existing.
    table.set("fragment", fragment)?;

    // Scribunto derives these eagerly in `makeTitleObject`, and sets them on the
    // data table (so they are *fields*, not `__index` fallbacks). `isSubpage` in
    // particular is read directly by `Module:Flagg` and by sandbox detection in
    // a dozen modules.
    let ns = NamespaceFacts::of(&ctx.site, ns_id);
    let sub = SubpageFields::of(&title_text);
    table.set("isSubpage", sub.is_subpage)?;
    table.set("rootText", sub.root_text.clone())?;
    table.set("baseText", sub.base_text.clone())?;
    table.set("subpageText", sub.subpage_text.clone())?;
    table.set("subjectNsText", ns.subject_name.clone())?;
    table.set("isContentPage", ns.is_content)?;
    table.set("isSpecialPage", ns_id == ns.special_id)?;
    table.set("isTalkPage", ns_id % 2 == 1)?;
    table.set("isExternal", false)?;
    table.set("interwiki", String::new())?;
    // A namespace with no talk space (Special, Media) reports no `talkNsText` and
    // `canTalk = false`; every other namespace reports both.
    match &ns.talk_name {
        Some(talk) => {
            table.set("canTalk", true)?;
            table.set("talkNsText", talk.clone())?;
        }
        None => table.set("canTalk", false)?,
    }

    // Derived fields are computed on demand rather than eagerly: a module that
    // never looks at `talkPageTitle` should not pay for building it, and eager
    // construction would recurse (a title's talk page is itself a title).
    let facts = title_facts_for(ctx, &ns_id, &title_text);
    let is_current = {
        let current = ctx.page_title.replace('_', " ");
        full.eq_ignore_ascii_case(&current) || title_text.eq_ignore_ascii_case(&current)
    };
    let current_source = ctx.page_source.clone();

    // The shared metatable's `__index` is generic — it cannot capture this
    // title's facts — so the handful of per-title answers travel on the instance
    // as a lookup closure, under a key the metatable knows.
    let facts_for_lookup = facts.clone();
    let full_for_lookup = full.clone();
    table.raw_set(
        TITLE_FACTS_KEY,
        lua.create_function(move |lua, (_this, key): (Value, String)| {
            title_derived_field(lua, &key, &facts_for_lookup, is_current, &full_for_lookup)
        })?,
    )?;

    // `getContent()` — the page's wikitext. Only available for pages that were
    // fetched (or the page being parsed, whose source rustoid already has);
    // Scribunto returns nil for a page it has not loaded.
    table.set(
        "getContent",
        lua.create_function(move |lua, _this: Value| {
            if is_current {
                return Ok(Value::String(lua.create_string(&current_source)?));
            }
            Ok(match facts.as_ref().and_then(|f| f.content.as_ref()) {
                Some(c) => Value::String(lua.create_string(c)?),
                None => Value::Nil,
            })
        })?,
    )?;

    // `title.subPageTitle(text)` — `mw.title.makeTitle(ns, text .. '/' .. text)`.
    // `Module:Flagg` builds a sandbox module name with it, and its absence made
    // the call fail as "attempt to call a nil value (method 'subPageTitle')".
    let subpage_site = ctx.site.clone();
    table.set(
        "subPageTitle",
        lua.create_function(move |lua, (this, sub): (Table, String)| {
            let ns: i32 = this.get("namespace").unwrap_or(0);
            let text: String = this.get("text").unwrap_or_default();
            let full = prefix_title(&subpage_site, ns, &format!("{text}/{sub}"));
            title_from_full_text(lua, &subpage_site, &full)
        })?,
    )?;

    // `title:isSubpageOf(other)` — same interwiki and namespace, and `other`'s
    // text followed by a slash is a prefix of this title's.
    table.set(
        "isSubpageOf",
        lua.create_function(|_, (this, other): (Table, Table)| {
            let interwiki: String = this.get("interwiki").unwrap_or_default();
            let other_iw: String = other.get("interwiki").unwrap_or_default();
            let ns: i32 = this.get("namespace").unwrap_or(0);
            let other_ns: i32 = other.get("namespace").unwrap_or(0);
            let text: String = this.get("text").unwrap_or_default();
            let other_text: String = other.get("text").unwrap_or_default();
            Ok(interwiki == other_iw
                && ns == other_ns
                && text.starts_with(&format!("{other_text}/")))
        })?,
    )?;

    // `title:inNamespace(ns)` / `inNamespaces(...)` — the namespace checks
    // modules guard on before formatting a title.
    let ns_site = ctx.site.clone();
    let ns_for_in = ns_site.clone();
    table.set(
        "inNamespace",
        lua.create_function(move |_, (this, ns): (Table, Value)| {
            let this_ns: i32 = this.get("namespace").unwrap_or(0);
            Ok(given_namespace_id(&ns_for_in, &ns) == Some(this_ns))
        })?,
    )?;
    let ns_for_ins = ns_site.clone();
    table.set(
        "inNamespaces",
        // The frame is passed first (`title:inNamespaces(…)`), so the namespaces
        // are a *multivalue tail*: taking `(Table, MultiValue)` made mlua bind
        // only the first, so `inNamespaces(0, 6)` tested just `0`.
        lua.create_function(move |_, args: mlua::MultiValue| {
            let mut args = args.into_iter();
            let Some(Value::Table(this)) = args.next() else {
                return Ok(false);
            };
            let this_ns: i32 = this.get("namespace").unwrap_or(0);
            Ok(args.any(|ns| given_namespace_id(&ns_for_ins, &ns) == Some(this_ns)))
        })?,
    )?;
    let ns_for_subject = ctx.site.clone();
    table.set(
        "hasSubjectNamespace",
        lua.create_function(move |_, (this, ns): (Table, Value)| {
            let this_ns: i32 = this.get("namespace").unwrap_or(0);
            Ok(given_namespace_id(&ns_for_subject, &ns) == Some(this_ns - (this_ns % 2)))
        })?,
    )?;

    let title_mt = title_metatable(lua, &ctx.site)?;
    mark_instance(&title_mt, &table)?;
    table.set_metatable(Some(title_mt));
    Ok(table)
}

/// The `(interwiki, namespace, text)` triple Scribunto compares titles by.
fn title_identity(t: &Table) -> (String, i32, String) {
    (
        t.get::<String>("interwiki").unwrap_or_default(),
        t.get::<i32>("namespace").unwrap_or(0),
        t.get::<String>("text").unwrap_or_default(),
    )
}

/// The instance key holding a title's per-title field closure.
const TITLE_FACTS_KEY: &str = "__rustoid_title_facts";

/// Answer a field that needs the page facts, for a title built by
/// [`luafn_title_new`].
///
/// The fields here depend on whether the page exists and what was preloaded,
/// which is why they cannot live on the shared metatable.
fn title_derived_field(
    lua: &Lua,
    key: &str,
    facts: &Option<TitleFacts>,
    is_current: bool,
    full: &str,
) -> mlua::Result<Value> {
    match key {
        "exists" => Ok(Value::Boolean(
            is_current || facts.as_ref().is_some_and(|f| f.exists),
        )),
        "isRedirect" => Ok(Value::Boolean(
            facts.as_ref().is_some_and(|f| f.is_redirect),
        )),
        // `protectionLevels` is a table keyed by action, each value an *array*
        // whose first item is the level string: `{ edit = { 'sysop' } }`.
        // `Module:Effective protection level` reads
        // `title.protectionLevels[action][1]`, and a module that found a bare
        // string there would index a character instead.
        "protectionLevels" => {
            let levels = lua.create_table()?;
            if let Some(entry) = facts.as_ref().map(|f| &f.protection) {
                for (action, value) in &entry.levels {
                    let arr = lua.create_table()?;
                    arr.set(1, value.first().map(String::as_str).unwrap_or(""))?;
                    levels.set(action.as_str(), arr)?;
                }
            }
            Ok(Value::Table(levels))
        }
        // No cascading restrictions can be observed, and the documented shape is
        // a table with empty `restrictions` and `sources` rather than nil — a
        // module indexing `cascadingProtection.restrictions` on the live wiki
        // finds a table, so returning nil here would turn a correct lookup into
        // an error.
        "cascadingProtection" => {
            let cp = lua.create_table()?;
            cp.set("restrictions", lua.create_table()?)?;
            cp.set("sources", lua.create_table()?)?;
            Ok(Value::Table(cp))
        }
        "fullUrl" => Ok(full_url_closure(lua, full)?),
        _ => Ok(Value::Nil),
    }
}

/// The fields a title can answer from its namespace and text alone.
///
/// These are the ones a derived title (`subPageTitle`, `*PageTitle`) answers
/// too, since they never depend on loaded page data.
fn title_derived_without_facts(lua: &Lua, t: &Table, key: &str) -> mlua::Result<Value> {
    let ns_id: i32 = t.get("namespace").unwrap_or(0);
    let text: String = t.get("text").unwrap_or_default();
    let site = site_for_titles(lua)?;
    match key {
        "isTalkPage" => Ok(Value::Boolean(ns_id % 2 == 1)),
        "isContentPage" => Ok(Value::Boolean(ns_id == 0 || ns_id == 828)),
        "talkPageTitle" => {
            let talk = ns_id + if ns_id % 2 == 1 { -1 } else { 1 };
            lua_str(lua, prefix_title(&site, talk, &text))
        }
        "subjectPageTitle" => {
            let subject = ns_id - (ns_id % 2);
            lua_str(lua, prefix_title(&site, subject, &text))
        }
        "fullUrl" => {
            let full: String = t.get("fullText").unwrap_or_default();
            full_url_closure(lua, &full)
        }
        // `title:canonicalUrl{action = 'edit', preload = …}` — the `index.php`
        // form with the title and each supplied parameter as query arguments, in
        // the order the table gave them. It is what `Module:Documentation` builds
        // its "create this page" links from, and its absence was the first thing
        // to fail once the module ran: `attempt to call a nil value (method
        // 'canonicalUrl')`.
        //
        // The shape is read off rendered pages rather than guessed —
        // `//en.wikipedia.org/w/index.php?title=Module_talk%3AMath&preload=…&action=edit`
        // — so the server-relative `//` prefix, the `%3A`/`+` encoding and the
        // `title` first are all as the service serves them.
        "canonicalUrl" => {
            let full: String = t.get("fullText").unwrap_or_default();
            let server = site.server.trim_start_matches("//").to_string();
            Ok(Value::Function(lua.create_function(
                move |lua, (_this, opts): (Value, Value)| {
                    // Scribunto also accepts a query-string form
                    // (`canonicalUrl('action=edit')`), appended verbatim.
                    let mut query = format!("title={}", url_encode(&full));
                    match &opts {
                        Value::Table(t) => {
                            for pair in t.clone().pairs::<Value, Value>() {
                                let (k, v) = pair.map_err(mlua::Error::external)?;
                                let (Ok(k), Ok(v)) = (
                                    coerce_string(&k, "canonicalUrl"),
                                    coerce_string(&v, "canonicalUrl"),
                                ) else {
                                    continue;
                                };
                                query.push('&');
                                query.push_str(&url_encode(&k));
                                query.push('=');
                                query.push_str(&url_encode(&v));
                            }
                        }
                        Value::String(s) => {
                            let extra = s.to_str().map_err(mlua::Error::external)?;
                            if !extra.is_empty() {
                                query.push('&');
                                query.push_str(&extra);
                            }
                        }
                        _ => {}
                    }
                    let url = format!("//{server}/w/index.php?{query}");
                    Ok(Value::String(lua.create_string(&url)?))
                },
            )?))
        }
        // `title:newline()` and friends come from `mw.html`; a title has no such
        // method, so a miss must stay a miss rather than pretend.
        _ => Ok(Value::Nil),
    }
}

/// `title:fullUrl()` — a *method*, not a field.
///
/// Modules call `title:fullUrl()`, so this returns a function. Returning the URL
/// string made the call fail with "attempt to call a string value".
fn full_url_closure(lua: &Lua, full: &str) -> mlua::Result<Value> {
    let site = site_for_titles(lua)?;
    let path = site.article_path.replace("$1", &url_encode(full));
    let url = format!("{}{path}", site.server);
    Ok(Value::Function(
        lua.create_function(move |_, _this: Value| Ok(url.clone()))?,
    ))
}

/// The site snapshot the shared metatable's `__index` needs.
///
/// The metatable is engine-wide, so the site has to be reachable from it too. It
/// is a Rust snapshot rather than a Lua value, so it is held in the Lua app data
/// rather than the registry.
fn site_for_titles(lua: &Lua) -> mlua::Result<LuaSite> {
    lua.app_data_ref::<TitleSite>()
        .map(|s| s.0.clone())
        .ok_or_else(|| mlua::Error::runtime("title site not registered"))
}

/// Wrapper making the site snapshot storable in Lua app data.
struct TitleSite(LuaSite);

/// Resolve a namespace argument that may be an id or a name.
fn given_namespace_id(site: &LuaSite, ns: &Value) -> Option<i32> {
    match ns {
        Value::Integer(i) => Some(*i as i32),
        Value::Number(n) => Some(*n as i32),
        Value::String(s) => s.to_str().ok().and_then(|s| site.namespace_id(&s)),
        _ => None,
    }
}

/// Build the title object for a full `Ns:Text` string, with no external data.
///
/// `subPageTitle` and the `*PageTitle` accessors need to *construct* a title,
/// not merely describe one, and that construction is the same work
/// [`luafn_title_new`] does — minus the `LuaContext`, since these titles are
/// derived rather than looked up, so none of the preloaded page facts apply.
fn title_from_full_text(lua: &Lua, site: &LuaSite, full: &str) -> mlua::Result<Value> {
    let (ns_id, title_text) = split_title(site, full);
    let ns = NamespaceFacts::of(site, ns_id);
    let sub = SubpageFields::of(&title_text);
    let table = lua.create_table()?;
    table.set("text", title_text)?;
    table.set("nsText", site.namespace_name(ns_id))?;
    table.set("namespace", ns_id)?;
    table.set("fullText", full.to_string())?;
    table.set("prefixedText", full.to_string())?;
    table.set("fragment", String::new())?;
    table.set("isSubpage", sub.is_subpage)?;
    table.set("rootText", sub.root_text)?;
    table.set("baseText", sub.base_text)?;
    table.set("subpageText", sub.subpage_text)?;
    table.set("subjectNsText", ns.subject_name)?;
    table.set("isContentPage", ns.is_content)?;
    table.set("isSpecialPage", ns_id == ns.special_id)?;
    table.set("isTalkPage", ns_id % 2 == 1)?;
    table.set("isExternal", false)?;
    table.set("interwiki", String::new())?;
    match ns.talk_name {
        Some(talk) => {
            table.set("canTalk", true)?;
            table.set("talkNsText", talk)?;
        }
        None => table.set("canTalk", false)?,
    }
    let mt = title_metatable(lua, site)?;
    mark_instance(&mt, &table)?;
    table.set_metatable(Some(mt));
    Ok(Value::Table(table))
}

/// The metatable every title object carries.
///
/// Every construction path — `mw.title.new`, `subPageTitle`, the `*PageTitle`
/// accessors — must go through this, or comparisons silently fall back to table
/// identity for titles built by the paths that missed it. Lua 5.1 only honours a
/// relational metamethod when *both* operands carry it, so a title compared
/// against one built by another path raises "attempt to compare two table
/// values". One shared table for the whole engine is what makes the operands
/// match an ordinary `==` and any `~=`, `<`, `<=`, `>`, `>=`.
fn title_metatable(lua: &Lua, site: &LuaSite) -> mlua::Result<Table> {
    if let Ok(Some(mt)) = lua.named_registry_value(TITLE_METATABLE_KEY) {
        return Ok(mt);
    }
    // Every object of a class shares one metatable *and* one `__eq` function.
    // Lua 5.1 only dispatches `__eq` when both operands carry the very same
    // function — distinct closures with identical bodies do not qualify — so the
    // class's dispatcher closure has to be created once and reused.
    let mt = lua.create_table()?;
    // `__index` answers the fields Scribunto derives rather than stores. It is
    // generic — the per-title answers arrive through the `TITLE_FACTS_KEY`
    // closure on the instance — so one metatable can serve every title.
    mt.set(
        "__index",
        lua.create_function(|lua, (t, key): (Table, Value)| {
            let Value::String(key) = &key else {
                return Ok(Value::Nil);
            };
            let key = key.to_str().map_err(mlua::Error::external)?.to_string();
            // Fields a title derives from its id and text alone are answered by
            // the generic path. Only titles built by `mw.title.new` also carry a
            // closure of facts-dependent answers, and it is consulted *second*,
            // so a field it does not know about falls through rather than
            // vanishing.
            match title_derived_without_facts(lua, &t, &key)? {
                Value::Nil => match t.raw_get::<Option<Function>>(TITLE_FACTS_KEY)? {
                    Some(derived) => derived.call((t, key)),
                    None => Ok(Value::Nil),
                },
                found => Ok(found),
            }
        })?,
    )?;
    let Some(eq) = identity_eq(lua, &mt, |a, b| title_identity(a) == title_identity(b))? else {
        // The dispatcher is installed by the Lua extras, which run before any
        // object is built; if it is missing, the engine was set up wrong. Fail
        // loudly rather than silently falling back to comparing table identity.
        return Err(mlua::Error::runtime(
            "title metatable built before the comparison dispatcher",
        ));
    };
    // `__tostring` is what `require(tostring(mw.title.new('Module:X')))` relies
    // on: without it, `tostring` yields `table: 0x…`, and `Module:Flagg` asked
    // for a module of that name — a request that could never be satisfied, so
    // the preload loop retried it until it gave up.
    mt.set(
        "__tostring",
        lua.create_function(|_, t: Table| t.get::<String>("prefixedText"))?,
    )?;
    // `__lt` and `__le` compare the three identifying fields, in Scribunto's
    // order (interwiki, namespace, text). Titles are ordered with `<` and `<=`
    // in modules constantly, and without these Lua will not compare them at all.
    mt.set(
        "__lt",
        lua.create_function(|_, (a, b): (Table, Table)| {
            Ok(title_identity(&a) < title_identity(&b))
        })?,
    )?;
    mt.set(
        "__le",
        lua.create_function(|_, (a, b): (Table, Table)| {
            Ok(title_identity(&a) <= title_identity(&b))
        })?,
    )?;
    // `__eq` compares the same three fields. The dispatcher in the Lua extras
    // supplies the identity rule around it; see `__rustoid_mark_class`.
    mt.set("__eq", eq)?;
    // The shared metatable's `__index` consults the site, so it is stashed once
    // for the whole engine; only one wiki is ever parsed per engine.
    lua.set_app_data(TitleSite(site.clone()));
    lua.set_named_registry_value(TITLE_METATABLE_KEY, &mt)?;
    Ok(mt)
}

/// Prepare `mt` for `__eq` comparisons and return the metamethod to install.
///
/// Scribunto's titles and mw.html nodes are objects with value semantics, so
/// `==` is answered from their fields rather than from table identity. Lua 5.1
/// additionally requires both operands' `__eq` to be the *same function* — a
/// rule mlua's own table comparison does not enforce, which would let a table
/// with any `__eq` at all decide the result.
///
/// `mt` is therefore tagged with `class`, and the returned closure defers to the
/// registered predicate only after checking that both operands carry that same
/// tag. Instances are tagged individually by [`mark_instance`].
fn identity_eq(
    lua: &Lua,
    mt: &Table,
    eq: impl Fn(&Table, &Table) -> bool + 'static,
) -> mlua::Result<Option<Function>> {
    let Ok(register) = lua
        .globals()
        .get::<Option<Function>>("__rustoid_register_eq")
    else {
        return Ok(None);
    };
    let Some(register) = register else {
        return Ok(None);
    };
    let Some(class) = register.call::<Option<Value>>((
        mt.clone(),
        lua.create_function(move |_, (a, b): (Table, Table)| Ok(eq(&a, &b)))?,
    ))?
    else {
        return Ok(None);
    };
    mt.raw_set("__rustoid_class", class.clone())?;
    let mark: Function = lua.globals().get("__rustoid_mark_class")?;
    mark.call::<()>((mt.clone(), class))?;
    let dispatcher: Function = lua.globals().get("__rustoid_identity_eq")?;
    Ok(Some(dispatcher))
}

/// Tag an object as a member of `mt`'s comparison class.
///
/// Without the tag the dispatcher refuses the comparison, exactly as Lua 5.1
/// refuses a metamethod that only one operand carries.
fn mark_instance(mt: &Table, instance: &Table) -> mlua::Result<()> {
    if let Some(class) = mt.raw_get::<Option<Value>>("__rustoid_class")? {
        instance.raw_set("__rustoid_class", class)?;
    }
    Ok(())
}

/// Registry slot holding the shared title metatable.
const TITLE_METATABLE_KEY: &str = "rustoid.title_metatable";

/// Split `Ns:Text#frag` into a namespace id and the bare title text.
fn split_title(site: &LuaSite, full: &str) -> (i32, String) {
    let (prefix, rest) = match full.split_once(':') {
        Some((p, r)) => (site.namespace_id(p), r),
        None => (None, full),
    };
    let rest = rest.split_once('#').map(|(t, _)| t).unwrap_or(rest);
    (prefix.unwrap_or(0), rest.trim().to_string())
}

/// The subpage fields of a title, as `mw.title.lua` computes them.
///
/// Scribunto matches `'^[^/]*().*()/[^/]*$'` against the title text and uses the
/// two capture *positions* as boundaries:
///
/// - `rootText` is everything before the first slash;
/// - `baseText` is everything before the last slash;
/// - `subpageText` is everything after the last slash.
///
/// Checked against Lua rather than reasoned about, because the pattern is
/// subtler than it looks: any slash at all matches, including a leading `/Foo`
/// (root and base both empty) and a trailing `Foo/` (subpage empty), and the
/// first and last slash may be the same character. Only a title with no slash is
/// not a subpage, in which case all three fields are the whole text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubpageFields {
    is_subpage: bool,
    root_text: String,
    base_text: String,
    subpage_text: String,
}

impl SubpageFields {
    fn of(text: &str) -> Self {
        let (Some(first), Some(last)) = (text.find('/'), text.rfind('/')) else {
            return Self {
                is_subpage: false,
                root_text: text.to_string(),
                base_text: text.to_string(),
                subpage_text: text.to_string(),
            };
        };
        Self {
            is_subpage: true,
            root_text: text[..first].to_string(),
            base_text: text[..last].to_string(),
            subpage_text: text[last + 1..].to_string(),
        }
    }
}

/// What a title needs to know about its namespace.
///
/// `mw.title.lua` reads these off `mw.site.namespaces`, and two of them are not
/// derivable from the id alone:
///
/// - `canTalk` is false for the namespaces that have no talk space (Special,
///   Media), not merely for the odd ones;
/// - `subjectNsText` is the *subject* namespace's name, which for a subject
///   namespace is itself.
///
/// `isContent` follows MediaWiki's `$wgContentNamespaces`, in which only the
/// main namespace is content on most wikis; Module (828) is not.
struct NamespaceFacts {
    subject_name: String,
    talk_name: Option<String>,
    is_content: bool,
    special_id: i32,
}

impl NamespaceFacts {
    fn of(site: &LuaSite, ns_id: i32) -> Self {
        let special_id = site.namespace_id("Special").unwrap_or(-1);
        let media_id = site.namespace_id("Media").unwrap_or(-2);
        // Media and Special have no talk space; a namespace with no canonical
        // name at all is also treated as having none, which keeps an unknown id
        // from inventing one.
        let has_talk = ns_id != special_id && ns_id != media_id && site.namespace_id_exists(ns_id);
        let talk_id = ns_id + if ns_id % 2 == 1 { -1 } else { 1 };
        let talk_name = if has_talk && site.namespace_id_exists(talk_id) {
            Some(site.namespace_name(talk_id))
        } else {
            None
        };
        // The subject namespace of a talk page is the even id below it; of a
        // subject namespace, itself.
        let subject_id = ns_id - (ns_id % 2);
        Self {
            subject_name: site.namespace_name(subject_id),
            talk_name,
            is_content: ns_id == 0,
            special_id,
        }
    }
}

/// Look up the preloaded facts for a title, by full text and by bare text.
fn title_facts_for(ctx: &LuaContext, ns_id: &i32, title_text: &str) -> Option<TitleFacts> {
    let key = prefix_title(&ctx.site, *ns_id, title_text);
    if let Some(facts) = ctx.titles.get(&key) {
        return Some(facts.clone());
    }
    // Titles are keyed as written in `mw.title.new`, which may omit the
    // namespace when the module passed one separately.
    ctx.titles
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(title_text))
        .map(|(_, v)| v.clone())
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

/// `mw.title.getCurrentTitle()` — the page being parsed.
///
/// Built as a full title object (rather than a bare table) so it carries the
/// metatable, `getContent()` and the derived fields. The *page frame's* title is
/// the page itself, which `create_frame` passes in as `page_title`.
fn luafn_title_current(lua: &Lua, ctx: &LuaContext) -> mlua::Result<Table> {
    luafn_title_new(
        lua,
        ctx,
        Value::String(lua.create_string(&ctx.page_title)?),
        None,
    )
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
    -- The table form is documented — `html:attr{ id = 'x', class = 'y' }` — and
    -- a module passing one had its table grown as the *key*, which then reached
    -- `_render` and raised "attempt to concatenate a table value (local 'key')".
    -- `pairs` is order-unspecified, as Scribunto's is.
    if type(key) == 'table' then
        for k, v in pairs(key) do self:attr(k, v) end
        return self
    end
    if key == nil then return self end
    -- A nil value *unsets* the attribute, which is the documented behaviour.
    -- Removing it from `_order` as well is what makes the unset take effect:
    -- `_render` walks `_order` and would otherwise emit the stale key with
    -- `tostring(nil)` for a value.
    if value == nil then
        self._attrs[key] = nil
        for i, k in ipairs(self._order) do
            if k == key then
                table.remove(self._order, i)
                break
            end
        end
        return self
    end
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
    child._parent = self
    table.insert(self._children, child)
    return child
end

-- `node(builder)` inserts an already-built node (or any value) as a child.
-- Re-parenting matters: the child is rendered once, in its new position.
function Node:node(child)
    if child == nil then return self end
    if type(child) == 'table' then child._parent = self end
    table.insert(self._children, child)
    return self
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

function Node:done() return self._parent or self end
function Node:allDone()
    -- `allDone` traverses to the *root node*, not to its rendered string: the
    -- manual says it is `done()` all the way up. Returning the render made
    -- `res:...:done():newline()` call a method on a string, which is how
    -- Module:Spoken Wikipedia stopped.
    if self._parent then return self._parent:allDone() end
    return self
end

-- `tostring(node)` renders the subtree, which is how a node used as a value
-- behaves in Scribunto.
Node.__tostring = function(self) return self:_render() end

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
do
local mw = __rustoid_mw

-- `__pairs` and `__ipairs` are Lua 5.2 metamethods that Scribunto back-ports to
-- its 5.1 runtime: the manual's own list of differences says "Support for the
-- __pairs and __ipairs metamethods (added in Lua 5.2) has been added." They are
-- not optional, because `Module:Arguments` — which most templates route through
-- — returns an *empty* proxy table whose contents exist only behind these
-- metamethods. Without them `pairs(args)` yields nothing, so
-- `Module:TableTools.compressSparseArray` returns an empty array, so
-- `Module:Hatnote list.andList` returns nil, so
-- `string.format('%s', nil)` raises "bad argument #2 to 'format'" — which is
-- exactly the failure `Module:Main list:28` reported on seven pages.
--
-- Both are added by wrapping the globals rather than by touching the runtime:
-- the raw iterators stay available for the wrapper's own use (calling the
-- wrapped `pairs` from inside would recurse), and a table without the
-- metamethod behaves exactly as before.
local raw_pairs, raw_ipairs = pairs, ipairs

-- `pairs(t)`: when `t` has `__pairs`, its three returned values are used
-- directly, as the manual specifies. `__pairs` is read through the metatable,
-- so a table that merely *inherits* one is also covered.
local function with_pairs(t)
    local mt = getmetatable(t)
    local h = mt and rawget(mt, '__pairs')
    if h then return h(t) end
    return raw_pairs(t)
end

-- `ipairs(t)` likewise, with the default being the `1, 2, 3, …` walk that stops
-- at the first nil.
local function with_ipairs(t)
    local mt = getmetatable(t)
    local h = mt and rawget(mt, '__ipairs')
    if h then return h(t) end
    return raw_ipairs(t)
end

pairs, ipairs = with_pairs, with_ipairs

-- Lua 5.1 only honours `__eq` when the operands carry the *same* metamethod,
-- and mlua's `Table::equals` loosens that to "either operand has one". The
-- loosening is observable and wrong: two tables with distinct `__eq` closures
-- would compare by the first one found, where Lua 5.1 reports them unequal. It
-- also makes `__eq` fire for the title and reference objects below, whose
-- equality is decided by their fields rather than by a metamethod at all.
--
-- So `__eq` is not installed on those objects. Instead `__rustoid_register_eq`
-- hooks the class's *fields* through this dispatcher, which reproduces the
-- identity rule by requiring both operands to be instances of the same class.
local eq_hooks = {}
local class_marker = '__rustoid_class'

-- Register `class`'s comparison predicate. The class's *identity* is the
-- predicate function itself, which is what Lua 5.1 compares, and the tag handed
-- back is that same function — so the tag is unique per class and no counter is
-- needed.
function __rustoid_register_eq(class, fn)
    eq_hooks[fn] = fn
    rawset(class, class_marker, fn)
    return fn
end

-- The one `__eq` every comparison class installs. Lua 5.1 only dispatches when
-- both operands carry this same function, so they are refused by the runtime
-- unless they are of the same class — which is the intended behaviour, since an
-- untagged value is not an instance of the class at all.
function __rustoid_identity_eq(a, b)
    local fa = rawget(a, class_marker)
    local fb = rawget(b, class_marker)
    -- Null means one of these is a plain table: tables compare by identity, and
    -- `a` and `b` are two distinct objects by the time we get here.
    if fa == nil or fb == nil then return false end
    -- Different classes never compare; the identity requirement makes that so.
    if fa ~= fb then return false end
    return eq_hooks[fa](a, b)
end

function __rustoid_mark_class(class, tag)
    rawset(class, class_marker, tag)
    return class
end

if table.clone == nil then
    function table.clone(t)
        local copy = {}
        for key, value in pairs(t) do copy[key] = value end
        return copy
    end
end

-- `string.gfind` is Lua 5.0's name for `gmatch`, kept in Lua 5.1 as a
-- deprecated alias. Scribunto is built on 5.1, so the alias exists there and
-- published modules call it: Module:Navbox uses it to collect TemplateStyles
-- strip markers. It is supplied through `mw.ustring` below rather than by the
-- 5.1 core library this build links, so restore it here.
if string.gfind == nil then
    string.gfind = string.gmatch
end

-- `unpack` is a global in Lua 5.1, which Scribunto runs on; Lua 5.2 moved it
-- under `table`. Modules call the global spelling — `Module:Pagetype` unpacks
-- every option row with it — so without the alias the module stops at
-- "attempt to call a nil value (global 'unpack')".
if unpack == nil then
    unpack = table.unpack
end

-- `mw.ustring.gcodepoint( s, i, j )` — an iterator over the codepoints. The
-- codepoints come from `mw.ustring.codepoints` (Rust), but the iterator is
-- built here: a `for` iterator must return a value *and* the next control, and
-- a Rust closure's second return value does not survive that call.
-- `i` and `j` are codepoint offsets and default to the whole string.
function mw.ustring.gcodepoint(s, i, j)
    local points = mw.ustring.codepoints(s, i, j)
    local pos = 0
    return function()
        pos = pos + 1
        return points[pos]
    end
end

-- `mw.ustring.gmatch( s, pattern )` — an iterator over every match.
--
-- Anchors are suppressed in the loop, because a pattern applied repeatedly must
-- not keep re-anchoring at position 1: Lua's own `gmatch` strips a leading `^`
-- for exactly that reason, and a pattern like `^%a+` would otherwise loop
-- forever. A failed match advances by one *character* rather than one byte, so
-- the iteration cannot stop inside a multibyte sequence.
--
-- The iterator is written here rather than in Rust because a `for` iterator must
-- return a value and the next control, and a Rust closure's second return value
-- does not survive that call (the same reason as `gcodepoint` above).
function mw.ustring.gmatch(s, pattern)
    local pos = 1
    local len = mw.ustring.len(s)
    if type(pattern) == 'string' and pattern:sub(1, 1) == '^' then
        pattern = pattern:sub(2)
    end
    return function()
        while pos <= len + 1 do
            local a, b = mw.ustring.find(s, pattern, pos)
            if a then
                -- An empty match must still advance, or the loop never ends.
                pos = (b >= a) and (b + 1) or (a + 1)
                local caps = { mw.ustring.__captures(s, pattern, a) }
                if #caps > 0 then return unpack(caps) end
                return mw.ustring.sub(s, a, b)
            end
            pos = pos + 1
        end
        return nil
    end
end

-- `mw.ustring.gsub( s, pattern, repl, n )` — replace every match.
--
-- `repl` may be a string (with `%0`..`%9` referring to captures), a table
-- (indexed by the first capture, or by the whole match when there are none), or
-- a function (called with the captures). All three forms are used by modules, so
-- all three are supported; a `false` or `nil` replacement keeps the original
-- text, which is how `gsub` is used as a filter.
function mw.ustring.gsub(s, pattern, repl, n)
    local out = {}
    local pos = 1
    local len = mw.ustring.len(s)
    local count = 0
    local max = n or math.huge

    local function append_replacement(whole, ...)
        local caps = { ... }
        if type(repl) == 'function' then
            -- Lua passes the captures, or the whole match when the pattern has
            -- none; the `nil` argument that would otherwise arrive is what made
            -- a `function(c)` replacement see `c == nil`.
            local r
            if #caps > 0 then r = repl(unpack(caps)) else r = repl(whole) end
            if r == false or r == nil then return whole end
            return tostring(r)
        elseif type(repl) == 'table' then
            local key
            if #caps > 0 then key = caps[1] else key = whole end
            local r = repl[key]
            if r == false or r == nil then return whole end
            return tostring(r)
        else
            -- A string replacement: `%0` is the whole match, `%1`.. `%9` the
            -- captures, and `%%` a literal percent. When the pattern has no
            -- captures, `%1` refers to the whole match rather than raising —
            -- verified against the service, where `gsub('abc', '%a', '%1')`
            -- gives `abc` and `gsub('abc', '%a', '%0%0')` gives `aabbcc`. The
            -- error is therefore reserved for an index that is out of range
            -- *given* that a captureless pattern has one implicit capture.
            local r = repl:gsub('%%([%%0-9])', function(d)
                if d == '%' then return '%' end
                if d == '0' then return whole end
                local i = tonumber(d)
                local v
                if #caps == 0 then
                    -- The whole match is capture 1 when there are none.
                    if i ~= 1 then
                        error('invalid capture index %' .. d .. ' in replacement string', 2)
                    end
                    v = whole
                else
                    v = caps[i]
                    if v == nil then
                        error('invalid capture index %' .. d .. ' in replacement string', 2)
                    end
                end
                return tostring(v)
            end)
            return r
        end
    end

    while pos <= len + 1 and count < max do
        local a, b = mw.ustring.find(s, pattern, pos)
        if not a then break end
        out[#out + 1] = mw.ustring.sub(s, pos, a - 1)
        local caps = { mw.ustring.__captures(s, pattern, a) }
        if #caps > 0 then
            out[#out + 1] = append_replacement(mw.ustring.sub(s, a, b), unpack(caps))
        else
            out[#out + 1] = append_replacement(mw.ustring.sub(s, a, b))
        end
        count = count + 1
        if b >= a then pos = b + 1 else pos = a + 1 end
    end
    out[#out + 1] = mw.ustring.sub(s, pos)
    return table.concat(out), count
end

-- `mw.text.split( s, pattern, plain )` and its iterator form
-- `mw.text.gsplit`. The Rust side supplies only the literal split; the pattern
-- form is the reference implementation from the manual, walking the string with
-- `mw.ustring.find` so the pattern dialect is the Ustring one.
local function gsplit_pattern(text, pattern, plain)
    local s, l = 1, mw.ustring.len(text)
    return function()
        if not s then return nil end
        local e, n = mw.ustring.find(text, pattern, s, plain)
        local ret
        if not e then
            ret = mw.ustring.sub(text, s)
            s = nil
        elseif n < e then
            -- Empty separator: emit one character.
            ret = mw.ustring.sub(text, s, e)
            s = e < l and e + 1 or nil
        else
            ret = e > s and mw.ustring.sub(text, s, e - 1) or ''
            s = n + 1
        end
        return ret
    end
end

function mw.text.gsplit(text, pattern, plain)
    if plain then
        local parts = mw.text.__splitPlain(text, pattern)
        local i = 0
        return function()
            i = i + 1
            return parts[i]
        end
    end
    return gsplit_pattern(text, pattern)
end

function mw.text.split(text, pattern, plain)
    if plain then return mw.text.__splitPlain(text, pattern) end
    local out = {}
    for piece in gsplit_pattern(text, pattern) do
        out[#out + 1] = piece
    end
    return out
end
end
"#;

/// Scribunto's `mw.message`, with the message store passed in.
///
/// `Message::plain()` is the method modules actually call: it substitutes the
/// parameters marked `$1`, `$2`, … into the message text and returns the result
/// as wikitext. `newRawMessage` is the same thing with the text supplied
/// directly rather than looked up — `Module:Citation/CS1` and `Module:Lang` both
/// use it as their string-interpolation primitive.
///
/// The message store is a plain table of the interface messages the host
/// preloaded. `exists` and `isBlank` answer from that table, so a message that
/// was not preloaded reports as missing and a module falls back to its own
/// built-in copy — which is the behaviour `Module:TemplatePar` depends on.
const MESSAGE_LIB: &str = r#"
return function(messages)
    local object = {}

    -- Substitute `$1`, `$2`, … from `params`. A numbered parameter replaces
    -- every occurrence, and one that has no value is left standing, which is
    -- what MediaWiki does (an unfilled `$2` shows up literally rather than
    -- vanishing).
    local function substitute(text, params)
        if not params then return text end
        return (text:gsub('%$(%d+)', function(n)
            local i = tonumber(n)
            -- Parameters are one-based here, but Scribunto follows PHP's
            -- convention that `$1` is the first parameter, so no shift is
            -- needed; the raw text is used so a number formats as written.
            local v = params[i]
            if v == nil then return '$' .. n end
            return tostring(v)
        end))
    end

    -- `mw.message.newRawMessage( msg, ... )` — the text is used directly, with
    -- no lookup. This is the common case in the corpus.
    function object.newRawMessage(msg, ...)
        return object.new(msg, ...)
    end

    -- `mw.message.new( key, ... )` — look the key up. A key that is missing
    -- is kept as the object's raw text, so `plain()` still returns something
    -- and `exists()` reports the truth. The metatable is installed before
    -- `:params` is called, since that method is found through it.
    --
    -- The stored fields are prefixed with `_` so they cannot shadow the methods
    -- of the same name (`text`, `params`): an instance field is found first, and
    -- a nil `text` would otherwise resolve to the `text()` method.
    function object.new(key, ...)
        local msg = setmetatable(
            { _key = key, _text = messages[key], _params = {} },
            { __index = object }
        )
        msg:params(...)
        return msg
    end

    -- `mw.message.newFallbackSequence( ... )` — the first key that exists.
    function object.newFallbackSequence(...)
        for _, key in ipairs({ ... }) do
            if messages[key] then return object.new(key) end
        end
        return object.new('')
    end

    -- `mw.message.rawParam` / `numParam` wrap a value so the substitution knows
    -- not to treat it as wikitext (raw) or to format it (num). Neither affects
    -- `plain()`, which emits parameters verbatim, so a pair of markers is
    -- enough to keep the value and record the intent.
    function object.rawParam(value) return { raw = value } end
    function object.numParam(value) return { num = value } end

    -- `mw.message.getDefaultLanguage()` needs a language object, which is built
    -- by `mw.language` — reaching for it here would make the two libraries
    -- mutually dependent. `mw.message` is a stub for this one accessor.

    -- `msg:params( ... )` and `msg:params( table )`. Kept in the order PHP
    -- numbers them, so `$1` is the first value either way.
    --
    -- A lone table argument is a sequence of parameters to copy — unless it is
    -- one of the wrapper tables from `rawParam`/`numParam`, which are a single
    -- parameter that happens to be a table. The wrappers are recognised by
    -- their marker field, so the distinction is the same one a caller makes.
    local function is_wrapper(v)
        return type(v) == 'table' and (v.raw ~= nil or v.num ~= nil)
    end

    function object:params(...)
        local args = { ... }
        local given = args[1]
        if #args == 1 and type(given) == 'table' and not is_wrapper(given) then
            args = given
        end
        for i, v in ipairs(args) do self._params[i] = v end
        return self
    end

    function object:rawParams(...)
        self:params(...)
        return self
    end

    function object:numParams(...)
        self:params(...)
        return self
    end

    function object:inLanguage(_) return self end
    function object:useDatabase(_) return self end

    -- `msg:plain()` — the message with its parameters substituted, as wikitext.
    -- A parameter may be one of the wrapper tables above, which yields its
    -- inner value.
    function object:plain()
        local text = self._text or self._key or ''
        if #self._params == 0 then return text end
        local values = {}
        for i, v in ipairs(self._params) do
            if type(v) == 'table' then values[i] = v.raw or v.num or ''
            else values[i] = v end
        end
        return substitute(text, values)
    end

    -- `msg:text()` is `plain()`; `msg:parse()` differs only in that MediaWiki
    -- parses it, which the caller here would do anyway.
    function object:text() return self:plain() end
    function object:parse() return self:plain() end

    function object:exists()
        return self._text ~= nil
    end

    function object:isBlank()
        return self._text == nil or self._text == '' or self._text == '-'
    end

    function object:isDisabled() return self:isBlank() end
    function object:numParamsEqual(_) return false end

    return object
end
"#;

/// Scribunto's `os` library: the four functions that cannot touch the system.
///
/// Lua's full `os` is removed for the same reason Scribunto removes it —
/// `os.execute` and `os.remove` are filesystem and shell access. What remains is
/// `os.time`, `os.date`, `os.difftime` and `os.clock`, which `Module:Date` and
/// `Module:Citation/CS1/Date_validation` call to learn the current time and to
/// bound an access date against "tomorrow".
fn luafn_os_table(lua: &Lua) -> Result<Table> {
    let err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    let os = lua.create_table().map_err(err)?;
    os.set("time", lua.create_function(luafn_os_time)?)
        .map_err(err)?;
    os.set("date", lua.create_function(luafn_os_date)?)
        .map_err(err)?;
    os.set("difftime", lua.create_function(luafn_os_difftime)?)
        .map_err(err)?;
    // `os.clock` measures CPU time, which a page parse neither has a use for nor
    // a meaningful value of; the elapsed time is the honest approximation.
    os.set("clock", lua.create_function(|_, ()| Ok(0.0_f64))?)
        .map_err(err)?;
    Ok(os)
}

/// Now, as a Unix timestamp. This is the only clock the engine reads, so every
/// time-dependent module sees one consistent instant per call.
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `os.time( table )` — the current time, or the time a field table encodes.
///
/// A table is interpreted as local time by C's `mktime`; rustoid has no
/// timezone database to consult, so UTC is used, which is also the wiki's
/// reference clock. The fields `year`, `month` and `day` are required, and the
/// rest default as the manual documents (`hour` 12, `min`/`sec` 0).
fn luafn_os_time(_: &Lua, table: Option<Table>) -> mlua::Result<i64> {
    let Some(table) = table else {
        return Ok(now_unix());
    };
    let year: i32 = table
        .get("year")
        .map_err(|_| mlua::Error::runtime("os.time: field 'year' missing in date table"))?;
    let month: u32 = table
        .get("month")
        .map_err(|_| mlua::Error::runtime("os.time: field 'month' missing in date table"))?;
    let day: u32 = table
        .get("day")
        .map_err(|_| mlua::Error::runtime("os.time: field 'day' missing in date table"))?;
    let hour: u32 = table.get("hour").unwrap_or(12);
    let min: u32 = table.get("min").unwrap_or(0);
    let sec: u32 = table.get("sec").unwrap_or(0);
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(hour, min, sec))
        .map(|dt| dt.and_utc().timestamp())
        .ok_or_else(|| mlua::Error::runtime("os.time: invalid date"))
}

/// `os.difftime( t2, t1 )` — the number of seconds from `t1` to `t2`.
fn luafn_os_difftime(_: &Lua, (t2, t1): (i64, i64)) -> mlua::Result<i64> {
    Ok(t2 - t1)
}

/// `os.date( format, time )` — `strftime` formatting, or a field table.
///
/// The format is C's, not MediaWiki's: `%Y` is the year and `!` selects UTC.
/// A leading `!` makes the time UTC and is not passed to `strftime`; anything
/// else is formatted in the wiki's own zone, which for rustoid is UTC too, so
/// the two agree on every input.
///
/// The special format `"*t"` returns a table of fields instead of a string, and
/// is how `Module:Date` reads the current year, month and day.
fn luafn_os_date(lua: &Lua, (format, time): (Option<String>, Option<i64>)) -> mlua::Result<Value> {
    let unix = time.unwrap_or_else(now_unix);
    let utc = chrono::DateTime::from_timestamp(unix, 0)
        .ok_or_else(|| mlua::Error::runtime("os.date: time out of range"))?;
    let format = format.unwrap_or_else(|| "%c".to_string());
    let spec = format.strip_prefix('!').unwrap_or(&format);
    if spec == "*t" {
        // Fields as C's `struct tm` exposes them: `wday` and `yday` are
        // 1-based, with Sunday as 1. `isdst` is always false under UTC.
        let table = lua.create_table()?;
        table.set("year", utc.year() as i64)?;
        table.set("month", utc.month() as i64)?;
        table.set("day", utc.day() as i64)?;
        table.set("hour", utc.hour() as i64)?;
        table.set("min", utc.minute() as i64)?;
        table.set("sec", utc.second() as i64)?;
        table.set("wday", utc.weekday().num_days_from_sunday() as i64 + 1)?;
        table.set("yday", utc.ordinal() as i64)?;
        table.set("isdst", false)?;
        return Ok(Value::Table(table));
    }
    Ok(Value::String(
        lua.create_string(format_strftime(spec, utc))?,
    ))
}

/// Apply a C `strftime` format string.
///
/// `chrono`'s `format` handles the common conversions, but panics on an unknown
/// one rather than leaving it alone, so the format is checked first: a specifier
/// outside the supported set is passed through literally, which is what makes an
/// unsupported format visible instead of fatal.
fn format_strftime(spec: &str, utc: chrono::DateTime<chrono::Utc>) -> String {
    const SUPPORTED: &[char] = &[
        '%', 'a', 'A', 'b', 'B', 'c', 'C', 'd', 'D', 'e', 'F', 'g', 'G', 'h', 'H', 'I', 'j', 'k',
        'l', 'm', 'M', 'n', 'p', 'P', 'r', 'R', 's', 'S', 't', 'T', 'u', 'U', 'V', 'w', 'W', 'x',
        'y', 'Y', 'z', 'Z',
    ];
    let mut out = String::with_capacity(spec.len());
    let mut chars = spec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some(next) if SUPPORTED.contains(&next) => {
                chars.next();
                out.push_str(&utc.format(&format!("%{next}")).to_string());
            }
            // A trailing `%`, or an unknown conversion passes through as-is.
            _ => out.push('%'),
        }
    }
    out
}

/// A small subset of MediaWiki's `Language::sprintfDate`.
///
/// Covers the formats modules actually ask for when reading month names out of a
/// date (`Module:Citation/CS1` iterates `F` and `M` over a year). An unknown
/// letter is passed through as-is, so unsupported formatting shows up in the
/// output rather than silently producing an empty string.
pub(crate) fn format_date(format: &str, stamp: &str) -> std::result::Result<String, String> {
    // A relative expression (`today + 2 days`, `now`) is a documented `#time`
    // input, and `Module:Citation/CS1` relies on it: it bounds an access date
    // against `today + 2 days`. The resulting instant then formats like any
    // other, so it is resolved to a date here and the rest is unchanged.
    let resolved = resolve_relative(stamp);
    // An omitted or empty stamp means *now*, which the manual states explicitly
    // and `Module:Citation/CS1` relies on: `mw.getLanguage('en'):formatDate('U')`
    // seeds its random id.
    let now = resolve_relative("now");
    let stamp = resolved
        .as_deref()
        .or_else(|| {
            stamp
                .trim()
                .is_empty()
                .then_some(now.as_deref().unwrap_or(""))
        })
        .unwrap_or(stamp);

    // MediaWiki accepts several spellings of the date, and a caller that passes
    // one it does not recognise gets an *error* rather than an empty string:
    // `{{#time:U|nonsense}}` renders `Error: Invalid time.`. `Module:Time ago`
    // wraps its call in `pcall` for exactly that reason and returns its own
    // message — so returning `""` here turned a guarded error into an
    // `attempt to sub a 'string' with a 'string'` further down.
    let Some((year, month, day, hour, minute, second)) = parse_date(stamp) else {
        return Err("Error: Invalid time.".to_string());
    };
    let (year, month, day, hour, minute, second) = (
        Some(year),
        Some(month),
        Some(day),
        Some(hour),
        Some(minute),
        Some(second),
    );

    const LONG: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const SHORT: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const DAYS: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];

    // Day of week needs a real date calculation, which this does not attempt:
    // the letters below are the ones the corpus asks for.
    //
    // `x` is MediaWiki's "raw" prefix: it makes the next code literal (`xU`
    // renders a literal `U`). `n` is the exception, and the reason this is not a
    // two-line special case: on its own `n` is the *number format* modifier, not
    // a field, so `xn` emits nothing and `xnU` is simply the Unix timestamp.
    // `Module:Time ago` calls `formatDate('xnU')` and then subtracts a timestamp
    // from the result, so emitting a month number there made the whole module fail
    // with "attempt to sub a 'string' with a 'string'" — `11577836800` is not a
    // number it can use.
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c == 'x' {
            match chars.next() {
                // The modifier with no format code after it contributes nothing.
                Some('n') | None => {}
                Some(literal) => out.push(literal),
            }
            continue;
        }
        match c {
            'Y' => out.push_str(&year.unwrap_or(0).to_string()),
            'y' => out.push_str(&format!("{:02}", year.unwrap_or(0).rem_euclid(100))),
            'n' => out.push_str(&month.unwrap_or(0).to_string()),
            'm' => out.push_str(&format!("{:02}", month.unwrap_or(0))),
            'F' => out.push_str(
                month
                    .and_then(|m| (1..=12).contains(&m).then(|| LONG[m - 1]))
                    .unwrap_or(""),
            ),
            'M' => out.push_str(
                month
                    .and_then(|m| (1..=12).contains(&m).then(|| SHORT[m - 1]))
                    .unwrap_or(""),
            ),
            'j' => out.push_str(&day.unwrap_or(0).to_string()),
            'd' => out.push_str(&format!("{:02}", day.unwrap_or(0))),
            'G' => out.push_str(&hour.unwrap_or(0).to_string()),
            'H' => out.push_str(&format!("{:02}", hour.unwrap_or(0))),
            'i' => out.push_str(&format!("{:02}", minute.unwrap_or(0))),
            's' => out.push_str(&format!("{:02}", second.unwrap_or(0))),
            // `U` is the Unix timestamp, which is what `Module:Citation/CS1`
            // compares against when bounding an access date. An unparseable
            // date yields nothing rather than a misleading epoch.
            'U' => out.push_str(&unix_timestamp(year, month, day, hour, minute, second)),
            _ => out.push(c),
        }
    }
    let _ = DAYS;
    Ok(out)
}

/// Parse a `#time` date stamp into `(year, month, day, hour, minute, second)`.
///
/// The shapes here are the ones the corpus asks for, each checked against the
/// service:
///
/// - `2020-01-01`, optionally with a time (`2020-01-01 12:30`);
/// - `2020-1-1`, single-digit parts;
/// - `January 2020` and `2020 January`;
/// - `20200101`, the eight-digit form;
/// - a bare year (`2020`), which MediaWiki fills in with the current month and
///   day — `{{#time:U|2020}}` on 2026-09-22 gave `1600732800`, i.e. 2020-09-22;
/// - empty, meaning *now*, which the manual states explicitly and
///   `Module:Citation/CS1` relies on for its random id.
///
/// `None` is not "the epoch": it means MediaWiki would have raised, and the
/// caller turns it into the documented error.
fn parse_date(stamp: &str) -> Option<(i32, usize, u32, u32, u32, u32)> {
    let stamp = stamp.trim();
    if stamp.is_empty() {
        // `now` — the caller formats the current instant.
        return None;
    }

    // The time of day, when the stamp carries one, is split off first so the
    // date half can be matched without it. The test is whether the second half
    // *looks like* a clock time, not merely whether a space is present:
    // `January 2020` has a space but its second half is the year, and splitting
    // on it put the year in the time slot and lost the date entirely.
    let (date_part, time_part) = match stamp.split_once(['T', ' ']) {
        Some((d, t)) if t.trim().contains(':') => (d.trim(), Some(t.trim())),
        _ => (stamp, None),
    };
    let (hour, minute, second) = match time_part {
        Some(t) => {
            let mut it = t.split(':');
            let h = it.next().and_then(|s| s.trim().parse::<u32>().ok())?;
            let m = it
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            let s = it
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            (h, m, s)
        }
        None => (0, 0, 0),
    };

    // `YYYY-MM-DD`, also accepting single digits as MediaWiki does.
    let dashed: Vec<&str> = date_part.split('-').collect();
    if dashed.len() == 3 {
        return Some((
            dashed[0].parse().ok()?,
            dashed[1].parse().ok()?,
            dashed[2].parse().ok()?,
            hour,
            minute,
            second,
        ));
    }

    // A bare year: MediaWiki completes it with the current month and day.
    if dashed.len() == 1
        && date_part.len() == 4
        && let Ok(year) = date_part.parse::<i32>()
    {
        let (m, d) = today_month_day();
        return Some((year, m, d, hour, minute, second));
    }

    // The eight-digit `YYYYMMDD` form.
    if date_part.len() == 8 && date_part.bytes().all(|b| b.is_ascii_digit()) {
        return Some((
            date_part[..4].parse().ok()?,
            date_part[4..6].parse().ok()?,
            date_part[6..8].parse().ok()?,
            hour,
            minute,
            second,
        ));
    }

    // `<month name> <year>` and `<year> <month name>`, the two orders that occur.
    let words: Vec<&str> = date_part.split_whitespace().collect();
    if words.len() == 2 {
        let (name, year_str) = if words[0].parse::<i32>().is_ok() {
            (words[1], words[0])
        } else {
            (words[0], words[1])
        };
        if let (Some(month), Ok(year)) = (month_number(name), year_str.parse::<i32>()) {
            return Some((year, month, 1, hour, minute, second));
        }
    }

    None
}

/// The month number for an English month name, full or three-letter.
fn month_number(name: &str) -> Option<usize> {
    const NAMES: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let lower = name.to_lowercase();
    NAMES
        .iter()
        .position(|n| {
            *n == lower || n.starts_with(&lower[..lower.len().min(3)]) && lower.len() >= 3
        })
        .map(|i| i + 1)
}

/// The current month and day, for a bare-year stamp.
fn today_month_day() -> (usize, u32) {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    (now.month() as usize, now.day())
}

/// The Unix timestamp of a date and time, or an empty string if it is not a
/// real date.
///
/// An unparseable date must not silently become the epoch: `Module:Citation/CS1`
/// compares the result against a cutoff, and `0` would read as a valid — and
/// very old — date.
fn unix_timestamp(
    year: Option<i32>,
    month: Option<usize>,
    day: Option<u32>,
    hour: Option<u32>,
    minute: Option<u32>,
    second: Option<u32>,
) -> String {
    let (Some(y), Some(m), Some(d)) = (year, month, day) else {
        return String::new();
    };
    chrono::NaiveDate::from_ymd_opt(y, m as u32, d)
        .and_then(|date| {
            date.and_hms_opt(hour.unwrap_or(0), minute.unwrap_or(0), second.unwrap_or(0))
        })
        .map(|dt| dt.and_utc().timestamp().to_string())
        .unwrap_or_default()
}

/// Resolve the relative date expressions `#time` accepts to an absolute date.
///
/// Only the forms the corpus uses are handled — `today`, `now`, `yesterday`,
/// `tomorrow`, each optionally followed by ` +/- N unit(s)`. Anything else
/// returns `None`, so the caller falls back to parsing the text as a date.
///
/// The result is `YYYY-MM-DD HH:MM:SS` in UTC, which is what the caller's
/// parser expects.
fn resolve_relative(stamp: &str) -> Option<String> {
    const UNITS: [(&str, i64); 6] = [
        ("second", 1),
        ("minute", 60),
        ("hour", 3_600),
        ("day", 86_400),
        ("week", 604_800),
        ("month", 2_592_000),
    ];

    let trimmed = stamp.trim();
    let (base, rest) = trimmed
        .split_once(char::is_whitespace)
        .map_or((trimmed, ""), |(a, b)| (a, b.trim()));

    let mut when = chrono::Utc::now();
    match base.to_ascii_lowercase().as_str() {
        "today" | "now" => {}
        "yesterday" => when -= chrono::Duration::seconds(86_400),
        "tomorrow" => when += chrono::Duration::seconds(86_400),
        _ => return None,
    }
    // "today" means midnight, so a bare `today + 2 days` is two whole days on.
    if matches!(
        base.to_ascii_lowercase().as_str(),
        "today" | "yesterday" | "tomorrow"
    ) {
        when = when.date_naive().and_hms_opt(0, 0, 0)?.and_utc();
    }

    if !rest.is_empty() {
        // `+ N days` / `- N days`, with an optional plural `s`.
        let rest = rest.trim_start_matches(['+', ' ']);
        let (sign, rest) = match rest.strip_prefix('-') {
            Some(r) => (-1, r.trim_start()),
            None => (1, rest.trim_start()),
        };
        let mut fields = rest.split_whitespace();
        let amount: i64 = fields.next()?.parse().ok()?;
        let unit = fields.next()?.trim_end_matches('s');
        let seconds = UNITS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(unit))
            .map(|(_, secs)| *secs)?;
        when += chrono::Duration::seconds(sign * amount * seconds);
    }

    Some(when.format("%Y-%m-%d %H:%M:%S").to_string())
}

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

fn luafn_ustring_len(_: &Lua, s: Value) -> mlua::Result<usize> {
    Ok(coerce_string(&s, "len")?.chars().count())
}

/// `mw.ustring.char( … )` — the characters for the given *codepoints*.
///
/// Codepoints, not bytes: the inherited `string.char` takes 0-255 and raises
/// "value out of range" above that, which is what `Module:Lang` hit when it
/// asked for the character of a codepoint it had parsed from a tag.
///
/// Lua numbers are doubles, so the arguments arrive as such and must be
/// integral. A surrogate or a value past `char::MAX` has no character; skipping
/// it matches the intent of a caller that is probing whether a codepoint is
/// printable, and returning an error would abort a module over one bad value.
fn luafn_ustring_char(_: &Lua, args: mlua::MultiValue) -> mlua::Result<String> {
    let mut out = String::new();
    for arg in args {
        let n = match arg {
            Value::Integer(i) => i,
            Value::Number(f) if f.fract() == 0.0 => f as i64,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "bad argument to 'char' (number expected, got {})",
                    other.type_name()
                )));
            }
        };
        let Ok(n) = u32::try_from(n) else { continue };
        if let Some(c) = char::from_u32(n) {
            out.push(c);
        }
    }
    Ok(out)
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
fn luafn_ustring_sub(
    _: &Lua,
    (s, i, j): (Value, Option<i64>, Option<i64>),
) -> mlua::Result<String> {
    let s = coerce_string(&s, "sub")?;
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;

    // Lua's `posrelat`: a negative index counts back from the end. An omitted
    // start is 1, the whole string, as in `string.sub` — `Module:IPA` calls
    // `mw.ustring.sub(s, nil)` with an uninitialised offset.
    let i = i.unwrap_or(1);
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

/// The codepoints of `s[i..=j]`, as a table, for `mw.ustring.gcodepoint`.
///
/// The iterator itself is written in Lua (`LUA_STDLIB_EXTRAS`), because a `for`
/// iterator must return both a value and the next control, and mlua keeps only
/// the first of a closure's tuple return in that position.
///
/// `i` and `j` index codepoints (not bytes) and default to the whole string, as
/// in `ustring.sub`. The bounds are clamped here, so the Lua side never sees an
/// out-of-range index and a start past the end simply yields an empty table.
fn luafn_ustring_codepoints(
    _: &Lua,
    (s, i, j): (Value, Option<i64>, Option<i64>),
) -> mlua::Result<Vec<i64>> {
    let s = coerce_string(&s, "gcodepoint")?;
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;

    // Lua's `posrelat`: a negative index counts back from the end.
    let start = i.unwrap_or(1);
    let start = if start < 0 { n + start + 1 } else { start }.max(1);
    let end = j.unwrap_or(-1);
    let end = (if end < 0 { n + end + 1 } else { end }).min(n);

    if start > end {
        return Ok(Vec::new());
    }
    Ok(chars[(start - 1) as usize..end as usize]
        .iter()
        .map(|c| *c as i64)
        .collect())
}

fn luafn_ustring_lower(_: &Lua, s: Value) -> mlua::Result<String> {
    Ok(coerce_string(&s, "lower")?.to_lowercase())
}

/// `mw.ustring.isutf8(s)` — whether the string is valid UTF-8.
///
/// Rust strings are always valid UTF-8, but the *argument* arrives as raw bytes
/// from wikitext, where invalid sequences are possible. Decoding the lossy way
/// and comparing is what detects them: a replacement character in the result
/// means the input had a byte sequence that did not decode.
fn luafn_ustring_isutf8(_: &Lua, s: Value) -> mlua::Result<bool> {
    let bytes = match s {
        Value::String(s) => s.as_bytes().to_vec(),
        // Scribunto's `isutf8` takes a string, and a number is stringified the
        // same way the other string functions do it.
        Value::Integer(_) | Value::Number(_) => coerce_string(&s, "isutf8")?.into_bytes(),
        _ => return Ok(false),
    };
    // `from_utf8` is the exact predicate: it rejects overlong encodings,
    // surrogates and truncated sequences, which a lossy round-trip would too.
    Ok(std::str::from_utf8(&bytes).is_ok())
}

/// The normalization family. Each returns `nil` for invalid UTF-8 rather than
/// raising, which is what the manual documents and what lets a module probe a
/// value without wrapping it in `pcall`.
///
/// Shared because the four differ only in the form they produce.
fn normalize(s: Value, form: NormalizationForm) -> mlua::Result<Option<String>> {
    let bytes = coerce_string(&s, "normalize")?;
    Ok(Some(match form {
        NormalizationForm::Nfc => bytes.nfc().collect(),
        NormalizationForm::Nfd => bytes.nfd().collect(),
        NormalizationForm::Nfkc => bytes.nfkc().collect(),
        NormalizationForm::Nfkd => bytes.nfkd().collect(),
    }))
}

/// Which of the four Unicode normalization forms to apply.
#[derive(Clone, Copy)]
enum NormalizationForm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

fn luafn_ustring_to_nfc(_: &Lua, s: Value) -> mlua::Result<Option<String>> {
    normalize(s, NormalizationForm::Nfc)
}

fn luafn_ustring_to_nfd(_: &Lua, s: Value) -> mlua::Result<Option<String>> {
    normalize(s, NormalizationForm::Nfd)
}

fn luafn_ustring_to_nfkc(_: &Lua, s: Value) -> mlua::Result<Option<String>> {
    normalize(s, NormalizationForm::Nfkc)
}

fn luafn_ustring_to_nfkd(_: &Lua, s: Value) -> mlua::Result<Option<String>> {
    normalize(s, NormalizationForm::Nfkd)
}

/// `mw.ustring.byteoffset(s, l, i)` — the byte offset of the `l`-th character
/// relative to byte `i`.
///
/// The manual defines `l == 1` as "the first character starting at or after byte
/// `i`" and `l == 0` as "the first character starting at or before byte `i`",
/// with other values relative to those. So this is *not* a plain index lookup:
/// it is a search for the character boundary nearest `i`, in one direction or the
/// other. Both `l` and `i` default to 1, and `i` may be negative to count from
/// the end.
///
/// The result is a byte offset, 1-based, and it is clamped into `1..=len+1` the
/// way `string.find`'s indices are — a position past the end addresses the empty
/// string after the last character, which callers use as a terminator.
fn luafn_ustring_byteoffset(
    _: &Lua,
    (s, l, i): (Value, Option<i64>, Option<i64>),
) -> mlua::Result<i64> {
    let s = coerce_string(&s, "byteoffset")?;
    let len = s.len() as i64;

    // `i` is a *byte* index, so it is clamped against the byte length, not the
    // character count. Negative counts back from the end, as elsewhere.
    let i = i.unwrap_or(1);
    let i = if i < 0 { len + i + 1 } else { i }.clamp(1, len + 1);

    // Byte offsets of every character start, plus the one-past-the-end offset.
    // `char_indices` yields exactly the boundaries, and the extra `len + 1`
    // sentinel is what makes "past the last character" addressable.
    let boundaries: Vec<i64> = s
        .char_indices()
        .map(|(b, _)| b as i64 + 1)
        .chain(std::iter::once(len + 1))
        .collect();

    // `l == 1` is the first boundary at or after `i`; `l == 0` is the first at or
    // before it, *inclusive* of `i` itself. `above_i` counts boundaries strictly
    // below `i`, so the first boundary >= i sits at that index; `at_or_below`
    // counts boundaries <= i, so the last such boundary is one before it.
    let below_i = boundaries.partition_point(|&b| b < i);
    let at_or_below = boundaries.partition_point(|&b| b <= i);
    let l = l.unwrap_or(1);
    // `l == 1` -> the first boundary >= i. `l == 0` -> the last boundary <= i.
    // Other values shift by the same 1-offset from the `l == 1` anchor.
    let anchor = if l >= 1 { below_i } else { at_or_below };
    let index = anchor as i64 - 1 + l;
    let index = index.clamp(0, boundaries.len() as i64 - 1);
    Ok(boundaries[index as usize])
}

/// `mw.ustring.find(s, pattern, init, plain)` — codepoint indices.
///
/// Returns the 1-based start and end positions of the match followed by its
/// captures, or just the start when the pattern has none — the same shape Lua's
/// `string.find` returns, but measured in codepoints rather than bytes. That
/// difference is the point: `Module:Citation/CS1` compares a capture against
/// `'nowiki'` and uses the positions as string indices.
///
/// `init` is a codepoint offset, may be negative to count from the end, and
/// defaults to 1.
fn luafn_ustring_find(
    lua: &Lua,
    (s, pattern, init, plain): (Value, Value, Option<i64>, Option<bool>),
) -> mlua::Result<mlua::MultiValue> {
    let s = coerce_string(&s, "find")?;
    let pattern = coerce_string(&pattern, "find")?;
    let plain = plain.unwrap_or(false);
    let start = resolve_init(&s, init);
    let found = ustring::pattern::find_match(&s, &pattern, start, plain)
        .map_err(|e| mlua::Error::runtime(e.message()))?;
    let Some(m) = found else {
        return Ok(single_nil());
    };
    let mut out = mlua::MultiValue::new();
    // Positions are 1-based codepoints; the end is inclusive.
    let start_pos = m.start_index + 1;
    let end_pos = m.start_index + m.whole.chars().count();
    out.push_back(Value::Integer(start_pos as i64));
    out.push_back(Value::Integer(end_pos as i64));
    for cap in &m.captures {
        push_capture(lua, &mut out, cap)?;
    }
    Ok(out)
}

/// `mw.ustring.match(s, pattern, init)` — the captures of the first match.
///
/// With no captures this returns the whole match, so the arity depends on the
/// pattern, exactly as in Lua. `Module:Citation/CS1` assigns three values from
/// one call, which is why the returns are a multivalue rather than a table.
fn luafn_ustring_match(
    lua: &Lua,
    (s, pattern, init): (Value, Value, Option<i64>),
) -> mlua::Result<mlua::MultiValue> {
    let s = coerce_string(&s, "match")?;
    let pattern = coerce_string(&pattern, "match")?;
    let start = resolve_init(&s, init);
    let found = ustring::pattern::find_match(&s, &pattern, start, false)
        .map_err(|e| mlua::Error::runtime(e.message()))?;
    let Some(m) = found else {
        return Ok(single_nil());
    };
    let mut out = mlua::MultiValue::new();
    if m.captures.is_empty() {
        out.push_back(Value::String(lua.create_string(m.whole)?));
    } else {
        for cap in &m.captures {
            push_capture(lua, &mut out, cap)?;
        }
    }
    Ok(out)
}

/// `mw.ustring.__captures(s, pattern, at)` — the captures of a match anchored
/// at codepoint `at`.
///
/// `gsub` and `gmatch` need the captures of the match *at* a position, not of
/// the first match at or after it: `find` slides forward, so using it to drive a
/// replacement loop would skip the unmatched text between the position and the
/// match and duplicate it. Anchoring with `^` is not enough either, because the
/// pattern may itself start with one.
///
/// Returns the capture list, or nothing when the pattern does not match there.
fn luafn_ustring_captures(
    lua: &Lua,
    (s, pattern, at): (Value, Value, i64),
) -> mlua::Result<mlua::MultiValue> {
    let s = coerce_string(&s, "match")?;
    let pattern = coerce_string(&pattern, "match")?;
    let start = resolve_init(&s, Some(at));
    let anchored = format!("^{pattern}");
    let found = ustring::pattern::find_match(&s, &anchored, start, false)
        .map_err(|e| mlua::Error::runtime(e.message()))?;
    let Some(m) = found else {
        return Ok(single_nil());
    };
    let mut out = mlua::MultiValue::new();
    for cap in &m.captures {
        push_capture(lua, &mut out, cap)?;
    }
    Ok(out)
}

/// A return of exactly one `nil`.
///
/// Several `mw.ustring` functions report "no match" this way rather than by
/// returning no values at all, which is what Lua's own pattern functions do
/// (`lstrlib.c`: `lua_pushnil(L); return 1;`). The distinction is observable,
/// because a function called with *no* argument behaves differently from one
/// called with an explicit nil — `tonumber()` raises where `tonumber(nil)`
/// returns nil — so a caller that forwards the result hits an error.
fn single_nil() -> mlua::MultiValue {
    let mut out = mlua::MultiValue::new();
    out.push_back(Value::Nil);
    out
}

/// Resolve a Lua start offset to a 0-based codepoint index.
///
/// Negative counts back from the end and an omitted (`nil`) offset is 1; either
/// way the result is clamped into the string, so a caller may pass an
/// uninitialised local — which `Module:IPA` does.
fn resolve_init(s: &str, init: Option<i64>) -> usize {
    let n = s.chars().count() as i64;
    let init = init.unwrap_or(1);
    let resolved = if init < 0 { n + init + 1 } else { init };
    resolved.clamp(1, n + 1) as usize - 1
}

/// Append a capture's value in the form Lua returns it.
fn push_capture(
    lua: &Lua,
    out: &mut mlua::MultiValue,
    cap: &Option<ustring::Capture<'_>>,
) -> mlua::Result<()> {
    match cap {
        Some(ustring::Capture::Text(t)) => out.push_back(Value::String(lua.create_string(*t)?)),
        // A position capture is a number, not a string — `tostring` differs and
        // modules compare it with `#` or arithmetic.
        Some(ustring::Capture::Position(p)) => out.push_back(Value::Integer(*p as i64)),
        None => out.push_back(Value::Nil),
    }
    Ok(())
}

/// `mw.text.JSON_PRESERVE_KEYS` — keep JSON's zero-based array indices.
///
/// The value is Scribunto's own bit flag; modules pass the constant rather than a
/// literal, so the number only has to round-trip through `+`.
const JSON_PRESERVE_KEYS: i64 = 1;

/// `mw.text.JSON_TRY_FIXING` — permit a terminal comma in arrays and objects.
const JSON_TRY_FIXING: i64 = 2;

/// `mw.text.jsonDecode(s, flags)` — decode a JSON string to a table.
///
/// Two details are observable and are reproduced rather than approximated:
///
/// - **Array indices.** JSON arrays are zero-based and Lua's are one-based, so
///   indices shift by one by default; `JSON_PRESERVE_KEYS` suppresses that.
/// - **Null values.** A JSON object drops keys whose value is null, and an array
///   containing null is not a Lua sequence. Both fall out of mapping null to
///   nil, which is why no special case is needed for the object case — a `nil`
///   value assigned to a table key simply leaves the key absent.
///
/// `JSON_TRY_FIXING` relaxes the terminal-comma rule. Rather than a lenient
/// parser, a trailing comma before a closing bracket is removed textually, which
/// is the one relaxation the flag documents.
fn luafn_text_json_decode(lua: &Lua, (s, flags): (Value, Option<i64>)) -> mlua::Result<Value> {
    let s = coerce_string(&s, "jsonDecode")?;
    let flags = flags.unwrap_or(0);
    let preserve_keys = flags & JSON_PRESERVE_KEYS != 0;
    let text = if flags & JSON_TRY_FIXING != 0 {
        strip_trailing_commas(&s)
    } else {
        s
    };
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| mlua::Error::runtime(format!("mw.text.jsonDecode: {e}")))?;
    json_to_lua_flagged(lua, &parsed, preserve_keys)
        .map_err(|e| mlua::Error::runtime(e.to_string()))
}

/// Remove commas that sit immediately before a closing `]` or `}`.
///
/// This is what `JSON_TRY_FIXING` documents ("no terminal comma in arrays or
/// objects"), and doing it textually keeps it honest: it cannot accidentally
/// accept anything else that strict JSON rejects. Quotes are tracked so a comma
/// inside a string is left alone.
fn strip_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut pending_comma = false;
    for c in s.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                if pending_comma {
                    out.push(',');
                    pending_comma = false;
                }
                in_string = true;
                out.push(c);
            }
            ',' => pending_comma = true,
            ']' | '}' => {
                // The pending comma is dropped; this is the fix-up.
                pending_comma = false;
                out.push(c);
            }
            _ => {
                if pending_comma {
                    out.push(',');
                    pending_comma = false;
                }
                out.push(c);
            }
        }
    }
    if pending_comma {
        out.push(',');
    }
    out
}

/// `json_to_lua` with the optional key-preserving array rule.
///
/// Kept separate from the Wikidata path rather than adding a flag there: entity
/// data is always read with one-based arrays, so threading a switch through it
/// would invite the two callers to diverge for no reason.
fn json_to_lua_flagged(lua: &Lua, value: &serde_json::Value, preserve_keys: bool) -> Result<Value> {
    let lua_err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    match value {
        serde_json::Value::Array(items) => {
            let table = lua.create_table().map_err(lua_err)?;
            for (i, item) in items.iter().enumerate() {
                // JSON is zero-based; Lua's default is one-based.
                let index = if preserve_keys { i } else { i + 1 };
                table
                    .set(index, json_to_lua_flagged(lua, item, preserve_keys)?)
                    .map_err(lua_err)?;
            }
            Ok(Value::Table(table))
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table().map_err(lua_err)?;
            for (k, v) in map {
                table
                    .set(k.as_str(), json_to_lua_flagged(lua, v, preserve_keys)?)
                    .map_err(lua_err)?;
            }
            Ok(Value::Table(table))
        }
        other => json_to_lua(lua, other),
    }
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

/// The members every frame has, whichever frame it is: `args`,
/// `argumentPairs`, the four parser-calling methods, and `extensionTag`.
///
/// A `frame:getParent()` result is a *frame*, not a stub with `args` on it, so
/// both are built from here. Building the parent by hand gave it only `args`,
/// `getTitle` and `getParent`, and `Module:Noinclude` — which writes
/// `frame:getParent():preprocess(...)` — stopped with "attempt to call a nil
/// value (method 'preprocess')".
fn frame_common(
    lua: &Lua,
    args: &[Arg],
    answers: &crate::pipeline::lua_deferred::DeferredAnswers,
    pending: &std::rc::Rc<std::cell::RefCell<Vec<crate::pipeline::lua_deferred::FrameRequest>>>,
) -> Result<Table> {
    let err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    let frame = lua.create_table().map_err(err)?;
    frame
        .set("args", build_args_table(lua, args)?)
        .map_err(err)?;

    // `frame:argumentPairs()` — the generic-for protocol: return (iterator,
    // state, control). Lua's own `next` is the iterator, so the args table is
    // the state and no snapshot has to be stored on the Lua side.
    frame
        .set(
            "argumentPairs",
            lua.create_function(|lua, this: Table| {
                let args: Table = this.get("args")?;
                let next: Function = lua.globals().get("next")?;
                Ok((next, args, Value::Nil))
            })
            .map_err(err)?,
        )
        .map_err(err)?;

    // `frame:preprocess(text)` / `frame:expandTemplate{…}` /
    // `frame:callParserFunction{…}` / `frame:extensionTag{…}` — all of these
    // need the parser, which only the host has, so the calls are deferred and
    // answered on the re-run. See [`crate::pipeline::lua_deferred`].
    for method in [
        crate::pipeline::lua_deferred::PREPROCESS,
        crate::pipeline::lua_deferred::EXPAND_TEMPLATE,
        crate::pipeline::lua_deferred::CALL_PARSER_FUNCTION,
        // `frame:extensionTag{ name, content, args }` is the `#tag` parser
        // function, which is how Scribunto itself lowers it.
        crate::pipeline::lua_deferred::EXTENSION_TAG,
    ] {
        frame
            .set(
                method,
                crate::pipeline::lua_deferred::frame_method(
                    lua,
                    method,
                    answers.clone(),
                    pending.clone(),
                )?,
            )
            .map_err(err)?;
    }

    // `frame:newChild{ title = …, args = … }` — a child frame whose *parent is
    // this frame*. It exists so a module can hand another module a frame that
    // looks like it came from a template, and `Module:Hatnote inline` uses it to
    // call `Module:Hatnote` with its own arguments: without it the call fails as
    // "attempt to call method 'newChild' (a nil value)".
    //
    // The child is a frame like any other, so it is built from this same
    // function; what makes it a *child* is that its `getParent` returns this
    // frame. Both the title and the args are optional: a missing title falls back
    // to this frame's, and missing args to none, which is what the manual's
    // "optional arguments and title" means.
    let parent_answers = answers.clone();
    let parent_pending = pending.clone();
    frame
        .set(
            "newChild",
            lua.create_function(move |lua, (this, spec): (Table, Option<Table>)| {
                let title = spec
                    .as_ref()
                    .and_then(|s| s.get::<Option<String>>("title").ok().flatten());
                let child_args = match spec.as_ref().map(|s| s.get::<Value>("args")) {
                    Some(Ok(Value::Table(t))) => args_from_table(&t)?,
                    // A caller may pass `args = nil`, or omit it entirely; both
                    // mean "no arguments".
                    _ => Vec::new(),
                };
                let child = frame_common(lua, &child_args, &parent_answers, &parent_pending)
                    .map_err(|e| mlua::Error::runtime(e.to_string()))?;
                let child_title = title.unwrap_or_else(|| {
                    // The creating frame's own title, which `create_frame` stores
                    // alongside the method for exactly this purpose.
                    this.raw_get::<Option<String>>("__title")
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                });
                child.set("__title", child_title.clone())?;
                child.set(
                    "getTitle",
                    lua.create_function(move |_, ()| Ok(child_title.clone()))?,
                )?;
                // The defining property: this frame is the child's parent.
                let frame = this.clone();
                child.set(
                    "getParent",
                    lua.create_function(move |_, ()| Ok(frame.clone()))?,
                )?;
                Ok(child)
            })?,
        )
        .map_err(err)?;

    Ok(frame)
}

/// Convert a Lua table of arguments into the host's [`Arg`] form.
///
/// `frame:newChild{ args = … }` receives whatever table the caller built, whose
/// keys may be numbers or strings and whose values may be numbers. The args table
/// takes both key spellings, so the numeric ones are kept numeric here and the
/// rest become named.
fn args_from_table(t: &Table) -> mlua::Result<Vec<Arg>> {
    let mut positional: Vec<(i64, String)> = Vec::new();
    let mut named: Vec<(String, String)> = Vec::new();
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        // Values are stringified the way Lua's own argument passing does.
        let s = match v {
            Value::String(s) => s.to_str()?.to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Boolean(b) => b.to_string(),
            Value::Nil => continue,
            other => other.type_name().to_string(),
        };
        match k {
            Value::Integer(i) => positional.push((i, s)),
            Value::Number(n) if n.fract() == 0.0 => positional.push((n as i64, s)),
            Value::String(k) => named.push((k.to_str()?.to_string(), s)),
            _ => {}
        }
    }
    positional.sort_by_key(|(i, _)| *i);
    let mut out: Vec<Arg> = positional
        .into_iter()
        .map(|(_, v)| Arg::Positional(v))
        .collect();
    out.extend(named.into_iter().map(|(k, v)| Arg::Named(k, v)));
    Ok(out)
}

fn create_frame(
    lua: &Lua,
    args: &[Arg],
    page_title: &str,
    parent_args: Option<&[Arg]>,
    parent_title: Option<&str>,
    answers: crate::pipeline::lua_deferred::DeferredAnswers,
    pending: std::rc::Rc<std::cell::RefCell<Vec<crate::pipeline::lua_deferred::FrameRequest>>>,
) -> Result<Value> {
    let frame = frame_common(lua, args, &answers, &pending)?;

    // `frame:getTitle()` — the page the frame was invoked from.
    let title = page_title.to_string();
    let ctx_for_title = title.clone();
    // Also kept as a plain field, so `newChild` can inherit it: a child with no
    // title given takes the creating frame's, and reading it back off the method
    // would be a Lua call from Rust.
    frame.set("__title", title.clone())?;
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
            // The parent is a frame like any other, so it is built by the same
            // code; only its title and its own (absent) parent differ.
            let p = frame_common(lua, parent_args, &answers, &pending)?;
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
    // (an empty name carries no information); add it back.
    let mut all: Vec<(i32, String, Vec<String>)> = site.namespaces.clone();
    if !all.iter().any(|(id, _, _)| *id == 0) {
        all.push((0, String::new(), Vec::new()));
    }
    all.sort_by_key(|(id, _, _)| *id);

    for (id, canonical, aliases) in all {
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
        // `subject` and `talk` are *sub-tables*, not namespace numbers. Scribunto
        // documents them as "a namespace object for the subject/talk namespace",
        // and modules read `ns.subject.id` and `ns.talk.name`. A bare number here
        // made `.subject` non-nil but `.subject.id` nil, so
        // `mw.site.namespaces[10].subject.id` threw — and because
        // `Module:Documentation` reaches it inside a `pcall`, the throw surfaced
        // only as `subjectSpace = nil`, which cascaded to a nil `docTitle` and an
        // empty documentation body. The whole 200KB of `Template:Infobox/doc`
        // hung on this one field.
        let subject_id = id - (id % 2);
        let talk_id = subject_id + 1;
        entry
            .set(
                "subject",
                namespace_ref(lua, site, subject_id).map_err(err)?,
            )
            .map_err(err)?;
        entry
            .set("talk", namespace_ref(lua, site, talk_id).map_err(err)?)
            .map_err(err)?;
        // The localized names, as a table. It must exist even when empty:
        // `Module:Namespace detect/data` iterates `ipairs(ns.aliases)` for every
        // namespace, and a missing field was an unattributed "attempt to index a
        // nil value" raised inside a for iterator.
        let alias_table = lua.create_table().map_err(err)?;
        for (i, alias) in aliases.iter().enumerate() {
            alias_table.set(i + 1, alias.clone()).map_err(err)?;
        }
        entry.set("aliases", alias_table).map_err(err)?;
        namespaces.set(id, entry.clone()).map_err(err)?;
        if !canonical.is_empty() {
            namespaces.set(canonical, entry.clone()).map_err(err)?;
        }
    }
    Ok(namespaces)
}

/// A minimal namespace object for the `subject`/`talk` fields.
///
/// Deliberately shallow: it carries the fields a module reads off
/// `ns.subject`/`ns.talk` (`id`, `name`, `canonicalName`), and no further
/// `subject`/`talk` of its own. Scribunto's are full objects, but the shape here
/// exists to answer the reads that occur, and recursing would build a table per
/// namespace per level for fields nothing asks for.
fn namespace_ref(lua: &Lua, site: &LuaSite, id: i32) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let name = if id == 0 {
        String::new()
    } else {
        site.namespace_name(id)
    };
    t.set("id", id)?;
    t.set("name", name.clone())?;
    t.set("canonicalName", name)?;
    Ok(t)
}

/// `mw.site.interwikiMap( filter )` — the interwiki prefixes, keyed by prefix.
///
/// The filter selects by locality: `"local"` keeps the local prefixes, `"!local"`
/// the rest, and nil keeps everything. Each value is a table whose fields are the
/// ones the manual documents, so a module reading `v["prefix"]` or `v.isLocal`
/// works; `displayText` and `tooltip` are omitted, since they only apply to
/// extra-language links configured with them and rustoid does not read those.
fn luafn_site_interwiki_map(lua: &Lua, site: &LuaSite, filter: Option<&Value>) -> Result<Table> {
    let err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    // The filter is a tri-state: select local only, select non-local only, or
    // select everything. An unrecognized filter matches nothing, which is what
    // MediaWiki does with an unknown string rather than erroring.
    let keep: Box<dyn Fn(bool) -> bool> = match filter {
        None | Some(Value::Nil) => Box::new(|_| true),
        Some(v) => match coerce_string(v, "interwikiMap")?.as_str() {
            "local" => Box::new(|is_local| is_local),
            "!local" => Box::new(|is_local| !is_local),
            _ => Box::new(|_| false),
        },
    };

    let map = lua.create_table().map_err(err)?;
    for iw in &site.interwikis {
        if !keep(iw.is_local) {
            continue;
        }
        let entry = lua.create_table().map_err(err)?;
        entry.set("prefix", iw.prefix.clone()).map_err(err)?;
        entry.set("url", iw.url.clone()).map_err(err)?;
        entry
            .set("isProtocolRelative", iw.is_protocol_relative)
            .map_err(err)?;
        entry.set("isLocal", iw.is_local).map_err(err)?;
        entry
            .set("isCurrentWiki", iw.is_current_wiki)
            .map_err(err)?;
        entry
            .set("isTranscludable", iw.is_transcludable)
            .map_err(err)?;
        entry
            .set("isExtraLanguageLink", iw.is_extra_language_link)
            .map_err(err)?;
        map.set(iw.prefix.clone(), entry).map_err(err)?;
    }
    Ok(map)
}

pub fn format_number(n: f64) -> String {
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

pub(crate) fn lua_value_to_string(value: &Value) -> String {
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

    /// A site with the interface messages `mw.message` looks up loaded.
    fn engine_site() -> LuaSite {
        let messages = [
            ("comma-separator".to_string(), ", ".to_string()),
            ("word-separator".to_string(), " ".to_string()),
            ("and".to_string(), " and ".to_string()),
            // The "disabled" spelling, which `isBlank` must report as blank.
            ("disabled-message".to_string(), "-".to_string()),
        ]
        .into_iter()
        .collect();
        LuaSite::from_config(&MockSiteConfig::new()).with_messages(messages)
    }

    /// `(local, non-local, total)` prefix counts, as Lua sees them.
    ///
    /// Counting from inside Lua rather than from the config asserts on the map
    /// the engine actually builds, which is the thing modules index.
    fn interwiki_counts(engine: &LuaEngine) -> (u32, u32, u32) {
        let counts = engine
            .eval(
                "local l, r, n = 0, 0, 0 \
                 for _ in pairs(mw.site.interwikiMap('local')) do l = l + 1 end \
                 for _ in pairs(mw.site.interwikiMap('!local')) do r = r + 1 end \
                 for _ in pairs(mw.site.interwikiMap()) do n = n + 1 end \
                 return l .. ',' .. r .. ',' .. n",
            )
            .unwrap();
        let mut parts = counts.split(',').map(|p| p.parse().unwrap());
        let (l, r, n) = (parts.next(), parts.next(), parts.next());
        match (l, r, n) {
            (Some(l), Some(r), Some(n)) => (l, r, n),
            _ => panic!("unexpected count string: {counts}"),
        }
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

    /// Scribunto keeps the four `os` functions that cannot touch the system.
    /// `Module:Date` reads the current time through `os.date`, so its absence
    /// stopped 13 corpus pages with "attempt to index a nil value (global 'os')".
    #[test]
    fn test_os_library() {
        let engine = make_engine();
        // `%Y` is the year, which `Module:Citation/CS1` uses as a bound.
        assert_eq!(
            engine
                .eval("return #os.date('%Y') == 4 and tonumber(os.date('%Y')) > 2000")
                .unwrap(),
            "true"
        );
        // `os.date('!*t')` is a field table, the form `Module:Date` reads.
        assert_eq!(
            engine
                .eval(
                    "local t = os.date('!*t') \
                     return type(t.year) .. ',' .. type(t.month) .. ',' .. type(t.day) .. ',' \
                     .. type(t.hour) .. ',' .. type(t.min) .. ',' .. type(t.sec)"
                )
                .unwrap(),
            "number,number,number,number,number,number"
        );
        // `wday` is 1-based with Sunday as 1, and `yday` is the day of the year.
        assert_eq!(
            engine
                .eval(
                    "local t = os.date('!*t', 0) \
                     return t.year .. '-' .. t.month .. '-' .. t.day .. ',' .. t.wday .. ',' .. t.yday"
                )
                .unwrap(),
            "1970-1-1,5,1"
        );
        // A `!` prefix is UTC and is not itself formatted.
        assert_eq!(engine.eval("return os.date('!%Y', 0)").unwrap(), "1970");
        // An unsupported conversion passes through rather than raising.
        assert_eq!(engine.eval("return os.date('100%%', 0)").unwrap(), "100%");
    }

    /// `os.time` reads the clock, and encodes a field table; `os.difftime` is
    /// the difference the manual documents.
    #[test]
    fn test_os_time_and_difftime() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return tonumber(os.time()) > 1700000000")
                .unwrap(),
            "true"
        );
        // A field table round-trips through the epoch.
        assert_eq!(
            engine
                .eval("return os.time{year = 1970, month = 1, day = 1, hour = 0}")
                .unwrap(),
            "0"
        );
        assert_eq!(
            engine
                .eval("return os.time{year = 1970, month = 1, day = 2, hour = 0}")
                .unwrap(),
            "86400"
        );
        assert_eq!(engine.eval("return os.difftime(100, 40)").unwrap(), "60");
        assert_eq!(engine.eval("return os.difftime(40, 100)").unwrap(), "-60");
        assert!(engine.eval("return os.clock()").is_ok());
    }

    /// The `os` functions Scribunto removes must stay removed — this is a
    /// sandbox, and `os.execute`/`os.remove` are shell and filesystem access.
    #[test]
    fn test_os_is_sandboxed() {
        let engine = make_engine();
        for gone in [
            "execute",
            "remove",
            "rename",
            "tmpname",
            "getenv",
            "exit",
            "setlocale",
        ] {
            assert_eq!(
                engine.eval(&format!("return tostring(os.{gone})")).unwrap(),
                "nil",
                "os.{gone} must not be exposed"
            );
        }
        // `io` has no safe subset at all and stays removed.
        assert_eq!(engine.eval("return tostring(io)").unwrap(), "nil");
    }

    #[test]
    fn test_mw_text_encode() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.text.encode('<>&\"')").unwrap(),
            "&lt;&gt;&amp;&quot;"
        );
    }

    /// `newRawMessage(text, args):plain()` is the string-interpolation primitive
    /// `Module:Citation/CS1`, `Module:Lang` and `Module:age` all use, and its
    /// absence stopped 9 corpus pages at the first call.
    #[test]
    fn test_mw_message_new_raw_message() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$1 on $2', 'a', 'b'):plain()")
                .unwrap(),
            "a on b"
        );
        // A parameter used twice is substituted both times.
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$1/$1', 'x'):plain()")
                .unwrap(),
            "x/x"
        );
        // With no parameters the text is returned unchanged.
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('no params'):plain()")
                .unwrap(),
            "no params"
        );
        // A parameter with no value is left standing, as MediaWiki leaves it.
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$1-$3', 'a'):plain()")
                .unwrap(),
            "a-$3"
        );
        // `Module:Location_map` passes its parameters pre-collected as a table,
        // which must be numbered from one.
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$1,$2', {'a', 'b'}):plain()")
                .unwrap(),
            "a,b"
        );
        // A number substitutes as written, not as a float.
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$1 px', 300):plain()")
                .unwrap(),
            "300 px"
        );
    }

    /// `libraryUtil.makeCheckSelfFunction` must reject a `self` that is not the
    /// object it was made for. The stub used to accept anything, so a module
    /// that mixed up its object silently read another object's fields; the real
    /// check is what makes the mistake a diagnosable error.
    #[test]
    fn test_library_util_check_self() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval(
                    "local u = require('libraryUtil') \
                     local obj = {} \
                     local checkSelf = u.makeCheckSelfFunction('MyLib', 'obj', obj, 'MyLib object') \
                     return tostring(checkSelf(obj, 'method') == obj)"
                )
                .unwrap(),
            "true"
        );
        // A different object is rejected, and the message names the library.
        assert_eq!(
            engine
                .eval(
                    "local u = require('libraryUtil') \
                     local obj = {} \
                     local checkSelf = u.makeCheckSelfFunction('MyLib', 'obj', obj, 'MyLib object') \
                     local ok, err = pcall(checkSelf, {}, 'method') \
                     return tostring(ok) .. '|' .. tostring(err:match('MyLib:') ~= nil)"
                )
                .unwrap(),
            "false|true"
        );
    }

    /// A looked-up message answers from the preloaded store, and a key that was
    /// not preloaded reports as missing — which is how `Module:TemplatePar`
    /// decides to use its own built-in localisation.
    #[test]
    fn test_mw_message_lookup() {
        let ctx = LuaContext::new(engine_site(), "Test Page");
        let engine = LuaEngine::new(LuaEngineConfig::default(), ctx).unwrap();
        assert_eq!(
            engine
                .eval("return mw.message.new('comma-separator'):plain()")
                .unwrap(),
            ", "
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.message.new('comma-separator'):exists())")
                .unwrap(),
            "true"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.message.new('comma-separator'):isBlank())")
                .unwrap(),
            "false"
        );
        // A message with parameters substitutes them.
        assert_eq!(
            engine
                .eval("return mw.message.new('word-separator'):plain()")
                .unwrap(),
            " "
        );
        // An unloaded key reports missing, so a module takes its fallback path.
        assert_eq!(
            engine
                .eval("return tostring(mw.message.new('not-a-real-message'):exists())")
                .unwrap(),
            "false"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.message.new('not-a-real-message'):isBlank())")
                .unwrap(),
            "true"
        );
        // `newFallbackSequence` picks the first key that exists.
        assert_eq!(
            engine
                .eval("return mw.message.newFallbackSequence('nope', 'comma-separator'):plain()")
                .unwrap(),
            ", "
        );
    }

    /// `Module:Citation/CS1/Utilities`'s `substitute` guards on the args being
    /// present, and `Module:age` spreads the arguments in — both forms are in
    /// the corpus and must substitute identically.
    #[test]
    fn test_mw_message_params_forms() {
        let engine = make_engine();
        // A table is queued with `:params(table)`, then a spread with `:params(...)`.
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$1'):params('a'):plain()")
                .unwrap(),
            "a"
        );
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$1'):params({'a'}):plain()")
                .unwrap(),
            "a"
        );
        // `rawParams`/`numParams` queue the values too, and `plain()` emits them.
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$2'):numParams(1, 2):plain()")
                .unwrap(),
            "2"
        );
        // `rawParam` wraps a single value, which `plain()` unwraps.
        assert_eq!(
            engine
                .eval(
                    "return mw.message.newRawMessage('$1'):params(mw.message.rawParam('[[x]]')):plain()"
                )
                .unwrap(),
            "[[x]]"
        );
        // `text()` and `parse()` are the plain text as far as rustoid is concerned.
        assert_eq!(
            engine
                .eval("return mw.message.newRawMessage('$1', 'v'):text()")
                .unwrap(),
            "v"
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

    /// `mw.text.nowiki` escapes what would otherwise be read as wikitext.
    /// `Module:Citation/CS1` escapes an identifier with it so that a value such
    /// as `ISBN 123` cannot become a magic link.
    #[test]
    fn test_mw_text_nowiki() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.text.nowiki('a&b')").unwrap(),
            "a&amp;b"
        );
        assert_eq!(
            engine.eval("return mw.text.nowiki('[[x]]')").unwrap(),
            "&#91;&#91;x&#93;&#93;"
        );
        assert_eq!(
            engine.eval("return mw.text.nowiki(\"it's\")").unwrap(),
            "it&#39;s"
        );
        // A line-leading marker is escaped; the same character mid-line is not.
        assert_eq!(
            engine.eval("return mw.text.nowiki('* item')").unwrap(),
            "&#42; item"
        );
        assert_eq!(
            engine.eval("return mw.text.nowiki('a * b')").unwrap(),
            "a * b"
        );
        // A magic-link prefix followed by a space is broken up.
        assert_eq!(
            engine.eval("return mw.text.nowiki('ISBN 123')").unwrap(),
            "ISBN&#32;123"
        );
        // ...but a prefix that is not one of the three is left alone.
        assert_eq!(
            engine.eval("return mw.text.nowiki('FOO 123')").unwrap(),
            "FOO 123"
        );
        // `----` at a line start would be a horizontal rule.
        assert_eq!(
            engine.eval("return mw.text.nowiki('----')").unwrap(),
            "&#45;---"
        );
        // `__` is a behavior-switch delimiter, and `://` a protocol.
        assert_eq!(
            engine.eval("return mw.text.nowiki('__NOTOC__')").unwrap(),
            "&#95;_NOTOC&#95;_"
        );
        assert_eq!(
            engine.eval("return mw.text.nowiki('http://x')").unwrap(),
            "http&#58;//x"
        );
        // Text with nothing to escape is returned unchanged.
        assert_eq!(
            engine.eval("return mw.text.nowiki('plain text')").unwrap(),
            "plain text"
        );
    }

    /// rustoid has no strip markers to remove, so these return their input —
    /// which is the right answer, not a stub: the text a module sees never
    /// carried a marker. They must exist so a module that calls them does not
    /// stop.
    #[test]
    fn test_mw_text_strip_markers_are_identity() {
        let engine = make_engine();
        for call in ["unstripNoWiki", "unstrip", "killMarkers"] {
            assert_eq!(
                engine
                    .eval(&format!("return mw.text.{call}('plain')"))
                    .unwrap(),
                "plain",
                "mw.text.{call}"
            );
        }
    }

    #[test]
    fn test_mw_text_split() {
        let engine = make_engine();
        let result = engine
            .eval("return table.concat(mw.text.split('a,b,c', ','), '|')")
            .unwrap();
        assert_eq!(result, "a|b|c");
        // The separator is a *pattern*, not a literal, so a character class
        // works and a run of separators does not produce empty pieces.
        assert_eq!(
            engine
                .eval("return table.concat(mw.text.split('a b  c', '%s+'), '|')")
                .unwrap(),
            "a|b|c"
        );
        assert_eq!(
            engine
                .eval("return table.concat(mw.text.split('a1b22c', '%d+'), '|')")
                .unwrap(),
            "a|b|c"
        );
        // `plain` turns the separator back into a literal.
        assert_eq!(
            engine
                .eval("return table.concat(mw.text.split('a.s.b', '.', true), '|')")
                .unwrap(),
            "a|s|b"
        );
        // A pattern matching the empty string splits into characters.
        assert_eq!(
            engine
                .eval("return table.concat(mw.text.split('abc', ''), '|')")
                .unwrap(),
            "a|b|c"
        );
    }

    #[test]
    fn test_mw_title_new() {
        let engine = make_engine();
        let result = engine
            .eval("return mw.title.new('Template:Foo').fullText")
            .unwrap();
        assert_eq!(result, "Template:Foo");
    }

    /// `mw.title.makeTitle` always applies its namespace argument, where `new`
    /// would find the prefix in the text first. `Module:Listen` builds a Media
    /// title with it.
    #[test]
    fn test_mw_title_make_title() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.title.makeTitle(-2, 'Foo.ogg').fullText")
                .unwrap(),
            "Media:Foo.ogg"
        );
        assert_eq!(
            engine
                .eval("return mw.title.makeTitle(-2, 'Foo.ogg').namespace")
                .unwrap(),
            "-2"
        );
        // The namespace name works as well as the id.
        assert_eq!(
            engine
                .eval("return mw.title.makeTitle('Template', 'Foo').fullText")
                .unwrap(),
            "Template:Foo"
        );
        // A fragment is a separate argument, and lands on `fragment`.
        assert_eq!(
            engine
                .eval("return mw.title.makeTitle(0, 'Foo', 'Bar').fragment")
                .unwrap(),
            "Bar"
        );
        assert_eq!(
            engine
                .eval("return mw.title.makeTitle(0, 'Foo', 'Bar').fullText")
                .unwrap(),
            "Foo#Bar"
        );
        // `isSubpage` and the subpage fields follow from the text.
        assert_eq!(
            engine
                .eval("return mw.title.makeTitle(0, 'A/B').subpageText")
                .unwrap(),
            "B"
        );
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

    /// `mw.site.interwikiMap` is called by `Module:Citation/CS1/Configuration`,
    /// which stops the whole CS1 stack with "attempt to call a nil value (field
    /// 'interwikiMap')" without it.
    #[test]
    fn test_mw_site_interwiki_map() {
        let engine = make_engine();

        // The entry fields a module reads, keyed by prefix. The mock mirrors the
        // parser-test runner's interwiki set: `wikipedia` is local and points at
        // the wiki's own server, `meatball` is a remote wiki, and the language
        // links are protocol-relative (`//…`, added by `add_language_interwiki`).
        assert_eq!(
            engine
                .eval("return mw.site.interwikiMap('local').wikipedia.prefix")
                .unwrap(),
            "wikipedia"
        );
        assert_eq!(
            engine
                .eval("return mw.site.interwikiMap().wikipedia.url")
                .unwrap(),
            "http://en.wikipedia.org/wiki/$1"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.site.interwikiMap().wikipedia.isLocal)")
                .unwrap(),
            "true"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.site.interwikiMap().meatball.isLocal)")
                .unwrap(),
            "false"
        );
        // Only a local prefix on this wiki's own server is the current wiki.
        assert_eq!(
            engine
                .eval("return tostring(mw.site.interwikiMap().wikipedia.isCurrentWiki)")
                .unwrap(),
            "true"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.site.interwikiMap().gerrit.isCurrentWiki)")
                .unwrap(),
            "false"
        );

        // The filter splits the map, and `!local` plus `local` covers all of it.
        // The mock holds 8 prefixes: `wikipedia`, `gerrit` and `stats` are local,
        // `meatball` and `memoryalpha` are remote, and the three language links
        // (`en`, `de`, `fr`) count as local — a language link to another
        // Wikipedia is "the same project".
        assert_eq!(interwiki_counts(&engine), (6, 2, 8));

        // `nil` and an absent argument select everything; an unknown filter
        // selects nothing, which is what MediaWiki does rather than erroring.
        for call in ["mw.site.interwikiMap(nil)", "mw.site.interwikiMap()"] {
            let count = engine
                .eval(&format!(
                    "local n = 0 for _ in pairs({call}) do n = n + 1 end return n"
                ))
                .unwrap();
            assert_eq!(count, "8", "{call}");
        }
        assert_eq!(
            engine
                .eval(
                    "local n = 0 \
                     for _ in pairs(mw.site.interwikiMap('nonsense')) do n = n + 1 end \
                     return n"
                )
                .unwrap(),
            "0"
        );
    }

    /// `Module:Citation/CS1/Configuration` builds its language-prefix set by
    /// intersecting the interwiki map's prefixes with its own language table, so
    /// the prefix must be reachable both as the key and as the entry's field.
    #[test]
    fn test_mw_site_interwiki_map_prefix_field() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval(
                    "for k, v in pairs(mw.site.interwikiMap('local')) do \
                     if k == v.prefix then return 'ok' end end return 'mismatch'"
                )
                .unwrap(),
            "ok"
        );
        // The language links are protocol-relative, which is how a module tells
        // it must supply the scheme itself.
        assert_eq!(
            engine
                .eval("return tostring(mw.site.interwikiMap('local').en.isProtocolRelative)")
                .unwrap(),
            "true"
        );
    }

    /// With no entities loaded, `mw.wikibase` reports "nothing found" rather than
    /// being nil, which is what a wiki without Wikidata looks like.
    /// `Module:Coordinates` guards on the table's existence and on
    /// `entityExists`, so both must be present; indexing nil failed the page
    /// outright.
    #[test]
    fn test_mw_wikibase_without_entities() {
        let engine = make_engine();
        // The table exists, so a guard passes.
        assert_eq!(engine.eval("return type(mw.wikibase)").unwrap(), "table");
        // ...and every lookup reports a miss, so the module falls back.
        assert_eq!(
            engine
                .eval("return tostring(mw.wikibase.getEntityIdForCurrentPage())")
                .unwrap(),
            "nil"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.wikibase.entityExists('Q42'))")
                .unwrap(),
            "false"
        );
        assert_eq!(
            engine
                .eval("return #mw.wikibase.getAllStatements('Q42', 'P625')")
                .unwrap(),
            "0"
        );
        // The guard `Module:Coordinates` writes now takes its fallback path
        // instead of erroring.
        assert_eq!(
            engine
                .eval(
                    "local qid = mw.wikibase.getEntityIdForCurrentPage() \
                     if mw.wikibase and mw.wikibase.entityExists(qid) then return 'entity' end \
                     return 'fallback'"
                )
                .unwrap(),
            "fallback"
        );
    }

    /// An engine with one real entity loaded, which is what a wiki with Wikidata
    /// access looks like. The entity is the shape `Special:EntityData` returns,
    /// trimmed to the parts the API reads.
    fn make_engine_with_q42() -> LuaEngine {
        let mut ctx = LuaContext::new(LuaSite::default(), "Douglas Adams");
        ctx.entities.insert("Q42", ENTITY_Q42.to_string());
        ctx.entities.set_current(Some("Q42".to_string()));
        LuaEngine::new(LuaEngineConfig::default(), ctx).unwrap()
    }

    const ENTITY_Q42: &str = r#"{
        "entities": {
            "Q42": {
                "id": "Q42",
                "type": "item",
                "labels": {"en": {"language": "en", "value": "Douglas Adams"}},
                "descriptions": {"en": {"language": "en", "value": "English writer"}},
                "sitelinks": {"enwiki": {"site": "enwiki", "title": "Douglas Adams"}},
                "claims": {
                    "P31": [
                        {"id": "Q42$1", "rank": "normal", "mainsnak": {"snaktype": "value"}},
                        {"id": "Q42$2", "rank": "deprecated", "mainsnak": {"snaktype": "value"}}
                    ],
                    "P856": [
                        {"id": "Q42$3", "rank": "preferred", "mainsnak": {"snaktype": "value"}},
                        {"id": "Q42$4", "rank": "normal", "mainsnak": {"snaktype": "value"}}
                    ]
                }
            }
        }
    }"#;

    #[test]
    fn test_mw_wikibase_resolves_the_current_page() {
        let engine = make_engine_with_q42();
        assert_eq!(
            engine
                .eval("return mw.wikibase.getEntityIdForCurrentPage()")
                .unwrap(),
            "Q42"
        );
        assert_eq!(
            engine
                .eval("return mw.wikibase.getEntityIdForTitle('') ")
                .unwrap(),
            "Q42"
        );
        // The page's own title resolves through its sitelink.
        assert_eq!(
            engine
                .eval("return mw.wikibase.getEntityIdForTitle('Douglas_Adams')")
                .unwrap(),
            "Q42"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.wikibase.getEntityIdForTitle('Nobody'))")
                .unwrap(),
            "nil"
        );
    }

    #[test]
    fn test_mw_wikibase_reads_labels_and_sitelinks() {
        let engine = make_engine_with_q42();
        assert_eq!(
            engine.eval("return mw.wikibase.getLabel('Q42')").unwrap(),
            "Douglas Adams"
        );
        assert_eq!(
            engine
                .eval("return mw.wikibase.getDescription('Q42')")
                .unwrap(),
            "English writer"
        );
        assert_eq!(
            engine
                .eval("return mw.wikibase.getSitelink('Q42')")
                .unwrap(),
            "Douglas Adams"
        );
        // A lower-case id is the same entity, as modules write both spellings.
        assert_eq!(
            engine.eval("return mw.wikibase.getLabel('q42')").unwrap(),
            "Douglas Adams"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.wikibase.getLabel('Q99'))")
                .unwrap(),
            "nil"
        );
    }

    #[test]
    fn test_mw_wikibase_reads_statements() {
        let engine = make_engine_with_q42();
        assert!(
            engine
                .eval("return mw.wikibase.entityExists('Q42')")
                .unwrap()
                == "true"
        );
        assert!(
            engine
                .eval("return mw.wikibase.entityExists('Q99')")
                .unwrap()
                == "false"
        );
        // `getAllStatements` returns every rank, including deprecated ones.
        assert_eq!(
            engine
                .eval("return #mw.wikibase.getAllStatements('Q42', 'P31')")
                .unwrap(),
            "2"
        );
        // `getBestStatements` prefers `preferred` over `normal`, and never
        // returns a deprecated statement when a better rank exists.
        assert_eq!(
            engine
                .eval("return #mw.wikibase.getBestStatements('Q42', 'P856')")
                .unwrap(),
            "1"
        );
        // With only `normal` and `deprecated`, the normal one wins.
        assert_eq!(
            engine
                .eval("return #mw.wikibase.getBestStatements('Q42', 'P31')")
                .unwrap(),
            "1"
        );
        // A statement's fields are readable, which is how a snak is walked.
        assert_eq!(
            engine
                .eval("return mw.wikibase.getBestStatements('Q42', 'P31')[1].id")
                .unwrap(),
            "Q42$1"
        );
        assert_eq!(
            engine
                .eval("return #mw.wikibase.getAllStatements('Q42', 'P999')")
                .unwrap(),
            "0"
        );
    }

    #[test]
    fn test_mw_wikibase_entity_object() {
        let engine = make_engine_with_q42();
        assert_eq!(
            engine
                .eval("return mw.wikibase.getEntity('Q42').id")
                .unwrap(),
            "Q42"
        );
        assert_eq!(
            engine
                .eval("return mw.wikibase.getEntity('Q42'):getLabel()")
                .unwrap(),
            "Douglas Adams"
        );
        assert_eq!(
            engine
                .eval("return mw.wikibase.getEntity('Q42'):getId()")
                .unwrap(),
            "Q42"
        );
        assert_eq!(
            engine
                .eval("return mw.wikibase.getEntity('Q42'):getSitelink('enwiki')")
                .unwrap(),
            "Douglas Adams"
        );
        // Called with no argument, it is the current page's entity.
        assert_eq!(
            engine.eval("return mw.wikibase.getEntity().id").unwrap(),
            "Q42"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.wikibase.getEntity('Q99'))")
                .unwrap(),
            "nil"
        );
    }

    #[test]
    fn test_mw_wikibase_validates_ids() {
        let engine = make_engine();
        for good in ["Q42", "q42", "P31", "Q1"] {
            assert_eq!(
                engine
                    .eval(&format!(
                        "return tostring(mw.wikibase.isValidEntityId('{good}'))"
                    ))
                    .unwrap(),
                "true",
                "{good}"
            );
        }
        // A leading zero, a bare letter and a non-id are all invalid.
        for bad in ["Q0", "Q", "Foo", "Q42x"] {
            assert_eq!(
                engine
                    .eval(&format!(
                        "return tostring(mw.wikibase.isValidEntityId('{bad}'))"
                    ))
                    .unwrap(),
                "false",
                "{bad}"
            );
        }
        assert_eq!(
            engine
                .eval("return tostring(mw.wikibase.isValidEntityId(nil))")
                .unwrap(),
            "false"
        );
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

    /// `formatDate('U', …)` yields a Unix timestamp, which
    /// `Module:Citation/CS1/Date_validation` compares against a cutoff. It also
    /// passes a *relative* date (`'today + 2 days'`), a documented `#time`
    /// input, so both the `U` letter and relative resolution are needed.
    #[test]
    fn test_mw_language_format_date_unix() {
        let engine = make_engine();
        // An absolute date: 2020-01-01T00:00:00Z.
        assert_eq!(
            engine
                .eval("return mw.getContentLanguage():formatDate('U', '2020-01-01')")
                .unwrap(),
            "1577836800"
        );
        // A relative date resolves to a real timestamp, and `today` means
        // midnight, so adding two days lands on a whole day.
        assert_eq!(
            engine
                .eval(
                    "local ts = mw.getContentLanguage():formatDate('U', 'today + 2 days') \
                     if not tonumber(ts) then return 'not a number' end \
                     if tonumber(ts) % 86400 ~= 0 then return 'not midnight' end \
                     return 'ok'"
                )
                .unwrap(),
            "ok"
        );
        // Tomorrow is a day later than today.
        assert_eq!(
            engine
                .eval(
                    "local l = mw.getContentLanguage() \
                     return tonumber(l:formatDate('U', 'tomorrow')) \
                          - tonumber(l:formatDate('U', 'today'))"
                )
                .unwrap(),
            "86400"
        );
        // An omitted timestamp means *now*, not the empty string.
        // `Module:Citation/CS1` seeds a random id from `formatDate('U')` with no
        // stamp, and an empty result made `tonumber` nil — reported as
        // arithmetic on a nil value, the most common corpus failure, on 13
        // pages.
        assert_eq!(
            engine
                .eval(
                    "local n = tonumber(mw.getContentLanguage():formatDate('U')) \
                     if not n then return 'nil' end \
                     if n < 1600000000 then return 'too old' end \
                     return 'ok'"
                )
                .unwrap(),
            "ok"
        );
        // An unparseable date **raises**, as MediaWiki's does, so a caller's
        // `pcall` sees it. Returning an empty string instead let the `pcall`
        // succeed and pushed the failure into whatever the caller did next —
        // `Module:Time ago` turned it into `attempt to sub a 'string' with a
        // 'string'` rather than its own error message.
        assert_eq!(
            engine
                .eval(
                    "local ok = pcall(mw.getContentLanguage().formatDate, \
                     mw.getContentLanguage(), 'U', 'nonsense') \
                     return tostring(ok)"
                )
                .unwrap(),
            "false"
        );
        // The other supported letters keep working alongside `U`.
        assert_eq!(
            engine
                .eval("return mw.getContentLanguage():formatDate('Y-m-d', '2020-01-02')")
                .unwrap(),
            "2020-01-02"
        );
    }

    #[test]
    fn test_mw_language_format_num() {
        let engine = make_engine();
        // `formatNum` lives on a language *object*, not on `mw.language` —
        // Scribunto's `mw.language.lua` installs it per instance. The shorthand
        // path modules actually use is `mw.getContentLanguage()`.
        assert_eq!(
            engine
                .eval("return mw.getContentLanguage():formatNum(1234567)")
                .unwrap(),
            "1,234,567"
        );
        assert_eq!(
            engine
                .eval("return mw.language.getContentLanguage():formatNum(1234)")
                .unwrap(),
            "1,234"
        );
        // And a freshly constructed object for another code.
        assert_eq!(
            engine
                .eval("return mw.language.new('de'):formatNum(1234)")
                .unwrap(),
            "1,234"
        );
    }

    /// `mw.language` surface the corpus relies on, exercised through the same
    /// entry points the modules use.
    #[test]
    fn test_mw_language_surface() {
        let engine = make_engine();
        // The content language reports its code and direction.
        assert_eq!(
            engine
                .eval("return mw.getContentLanguage():getCode()")
                .unwrap(),
            "en"
        );
        assert_eq!(
            engine
                .eval("return mw.getContentLanguage():getDir()")
                .unwrap(),
            "ltr"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.getContentLanguage():isRTL())")
                .unwrap(),
            "false"
        );
        // An Arabic object is right-to-left, which is what `Module:Lang` branches on.
        assert_eq!(
            engine
                .eval("return mw.language.new('ar'):getDir()")
                .unwrap(),
            "rtl"
        );
        // `mw.getLanguage` is the documented shorthand for `new`.
        assert_eq!(
            engine
                .eval("return mw.getLanguage('fr'):getCode()")
                .unwrap(),
            "fr"
        );
        // The `.code` property is read directly by `Module:Lang`.
        assert_eq!(
            engine.eval("return mw.getContentLanguage().code").unwrap(),
            "en"
        );
    }

    /// `getDurationIntervals` feeds `string.format('%02d', …)`, so its values
    /// must be integers rather than floats.
    #[test]
    fn test_mw_language_duration_intervals() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval(
                    "local t = mw.getContentLanguage():getDurationIntervals(3725, {'hours','minutes','seconds'}) \
                     return t.hours .. '|' .. t.minutes .. '|' .. t.seconds"
                )
                .unwrap(),
            "1|2|5"
        );
    }

    /// `fetchLanguageNames` must return a table whose keys and values are all
    /// strings, because `Module:Citation/CS1` inverts it with `#k` on the key
    /// and `mw.ustring.lower` on the value.
    #[test]
    fn test_mw_language_fetch_language_names() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.language.fetchLanguageNames('en', 'all').de")
                .unwrap(),
            "German"
        );
        // The corpus checks `#k`, which a numeric key would break on.
        assert_eq!(
            engine
                .eval(
                    "for k, v in pairs(mw.language.fetchLanguageNames('en', 'all')) do \
                     if type(k) ~= 'string' or type(v) ~= 'string' then return 'bad' end end \
                     return 'ok'"
                )
                .unwrap(),
            "ok"
        );
    }

    #[test]
    #[ignore]
    fn tmp_probe_mark() {
        let engine = make_engine();
        for code in [
            "local a = mw.title.new('Foo') return tostring(rawget(a, '__rustoid_class'))",
            "local a = mw.title.new('Foo') local mt = getmetatable(a) \
             return tostring(rawget(mt, '__rustoid_class'))",
            "local a = mw.title.new('Foo') return tostring(a.fullUrl) .. '|' .. tostring(a.exists)",
        ] {
            println!("{:?}\n   => {:?}", code, engine.eval(code));
        }
    }

    #[test]
    fn test_mw_ustring_len() {
        let engine = make_engine();
        assert_eq!(engine.eval("return mw.ustring.len('hello')").unwrap(), "5");
    }

    /// A failed match returns exactly one `nil`, not zero values.
    ///
    /// Lua's own pattern functions do `lua_pushnil(L); return 1;` on a miss
    /// (`lstrlib.c`), and the arity is observable because a function called with
    /// no argument behaves differently from one called with an explicit nil:
    /// `tonumber()` raises "value expected" where `tonumber(nil)` returns nil.
    ///
    /// This is not hypothetical. `Module:Subject bar:18` reads
    /// `tonumber(mw.ustring.match(k, pattern))`, so returning no values made
    /// every non-matching key raise and took out six corpus pages.
    #[test]
    fn a_failed_match_returns_one_nil() {
        let engine = make_engine();
        // `select('#', ...)` separates "one nil" from "no values".
        assert_eq!(
            engine
                .eval("return select('#', (function() return mw.ustring.match('z','(%d+)') end)())")
                .unwrap(),
            "1"
        );
        assert_eq!(
            engine
                .eval("return select('#', (function() return mw.ustring.find('z','(%d+)') end)())")
                .unwrap(),
            "1"
        );
        // `select(2, x)` on a single value consumes it, leaving no values at
        // all — stock Lua, verified against the `lua` binary, where
        // `select('#', select(2, one_nil()))` is 0. So `select` is not a way to
        // observe the arity; `select('#', ...)` applied to the call directly is.
        // What matters for a caller is that the value can be held and forwarded,
        // which is the `v` case below.
        assert_eq!(
            engine
                .eval("local v = mw.ustring.match('z','(%d+)'); return tostring(v)")
                .unwrap(),
            "nil"
        );
        // And the arity survives being passed on: `tonumber` accepts one nil.
        assert_eq!(
            engine
                .eval("return tostring(tonumber(mw.ustring.match('z','(%d+)')))")
                .unwrap(),
            "nil"
        );
    }

    /// The shape of `Module:Subject bar`'s `findNumericArgs`, which is what
    /// exposed the arity bug: a non-matching key must be skipped, not fatal.
    #[test]
    fn a_non_matching_key_is_skipped_not_fatal() {
        let engine = make_engine();
        let result = engine
            .eval(
                "local args = { commons = 'Bitcoin', portal1 = 'Numismatics' } \
                 local values = {} \
                 for k, v in pairs(args) do \
                     local ord = tonumber(mw.ustring.match(k, '^portal_?(%d+)$')) \
                     if ord then values[ord] = v end \
                 end \
                 return tostring(values[1])",
            )
            .unwrap();
        assert_eq!(result, "Numismatics");
    }

    #[test]
    fn test_mw_ustring_sub() {
        let engine = make_engine();
        // Expected values verified against Lua 5.1's `string.sub`, which
        // `ustring.sub` mirrors for codepoint-indexed strings. Scribunto runs
        // on 5.1, so that is the reference. A previous
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
            // An omitted or nil start means "from the beginning", which
            // `Module:IPA` relies on: it passes an uninitialised offset.
            ("mw.ustring.sub('abc', nil)", "abc"),
            ("mw.ustring.sub('abc', nil, 2)", "ab"),
        ] {
            assert_eq!(
                engine.eval(&format!("return {expr}")).unwrap(),
                want,
                "{expr}"
            );
        }
        // An uninitialised local is nil, which is how `Module:IPA` reaches the
        // omitted-start case.
        assert_eq!(
            engine
                .eval("local i return mw.ustring.sub('abc', i)")
                .unwrap(),
            "abc"
        );
    }

    #[test]
    fn test_mw_ustring_isutf8() {
        let engine = make_engine();
        // A valid two-byte sequence, and the same bytes with the lead byte
        // dropped — the truncated form is the one wikitext actually produces.
        assert_eq!(
            engine
                .eval(r"return tostring(mw.ustring.isutf8('h\195\169llo'))")
                .unwrap(),
            "true"
        );
        assert_eq!(
            engine
                .eval(r"return tostring(mw.ustring.isutf8('h\195llo'))")
                .unwrap(),
            "false"
        );
        // A lone continuation byte and a surrogate encoded as CESU-8 are both
        // invalid UTF-8, and both are things a module can be handed.
        assert_eq!(
            engine
                .eval(r"return tostring(mw.ustring.isutf8('\169'))")
                .unwrap(),
            "false"
        );
        assert_eq!(
            engine
                .eval(r"return tostring(mw.ustring.isutf8('\237\160\128'))")
                .unwrap(),
            "false"
        );
        assert_eq!(
            engine
                .eval("return tostring(mw.ustring.isutf8(''))")
                .unwrap(),
            "true"
        );
    }

    /// `%s` in a ustring pattern is a Unicode category, not ASCII, so `é` is not
    /// a space while U+00A0 is. Covered in the pattern tests; here the shape
    /// predicates are checked.
    #[test]
    fn test_mw_ustring_normalization() {
        let engine = make_engine();
        // `é` as one composed codepoint, and as `e` + combining acute (U+0301).
        // NFD decomposes the first, NFC composes the second, and the round trip
        // returns the original bytes — which is the property that matters, since
        // a module compares the result against a literal.
        let composed = r"'\195\169'"; // é as U+00E9
        let decomposed = r"'e\204\129'"; // e + U+0301
        assert_eq!(
            engine
                .eval(&format!("return mw.ustring.toNFD({composed})"))
                .unwrap(),
            "e\u{301}"
        );
        assert_eq!(
            engine
                .eval(&format!("return mw.ustring.toNFC({decomposed})"))
                .unwrap(),
            "é"
        );
        // Compatibility forms fold things the canonical forms keep: the ﬁ
        // ligature (U+FB01) decomposes under NFKC/NFKD but not under NFC/NFD.
        let ligature = r"'\239\172\129'"; // U+FB01
        assert_eq!(
            engine
                .eval(&format!("return mw.ustring.toNFC({ligature})"))
                .unwrap(),
            "ﬁ"
        );
        assert_eq!(
            engine
                .eval(&format!("return mw.ustring.toNFKC({ligature})"))
                .unwrap(),
            "fi"
        );
    }

    /// `mw.ustring.byteoffset(s, l, i)` — the byte offset of the `l`-th character
    /// relative to byte `i`.
    ///
    /// These expectations are derived from the manual's text rather than taken
    /// from the service: the function is not exposed through `Module:String`, and
    /// the API rate-limited the differential run. The manual is unusually precise
    /// here, so the cases below pin each clause of it separately:
    /// `l == 1` is the character starting at or after `i`, `l == 0` the one
    /// starting at or before it, and other `l` relative to those.
    #[test]
    fn test_mw_ustring_byteoffset() {
        let engine = make_engine();
        // `héllo` is 6 bytes: h(1) é(2-3) l(4) l(5) o(6).
        for (expr, want) in [
            // `l == 1` from byte 1 is the first character: h, at byte 1.
            ("mw.ustring.byteoffset('héllo', 1, 1)", 1),
            // From byte 2 (the lead byte of é) forward: that is é itself.
            ("mw.ustring.byteoffset('héllo', 1, 2)", 2),
            // From byte 3 (the *continuation* byte of é) forward: the next
            // character starts at 4, which is the first `l`.
            ("mw.ustring.byteoffset('héllo', 1, 3)", 4),
            // `l == 0` from byte 3 back: é starts at 2, the last start <= 3.
            ("mw.ustring.byteoffset('héllo', 0, 3)", 2),
            // `l == 0` from byte 4 is l itself, which starts exactly there.
            ("mw.ustring.byteoffset('héllo', 0, 4)", 4),
            // Negative `i` counts from the end: -1 is the last byte, so `l == 0`
            // gives the character starting at or before it, which is `o` at 6.
            ("mw.ustring.byteoffset('héllo', 0, -1)", 6),
            // One past the last character addresses the empty string after it.
            ("mw.ustring.byteoffset('héllo', 1, 7)", 7),
            // Defaults: both `l` and `i` are 1, so this is byte 1.
            ("mw.ustring.byteoffset('héllo')", 1),
            // `l == -1` is one character before the `l == 0` anchor, and
            // `l == 2` one after the `l == 1` anchor. Pinning these catches an
            // off-by-one in the anchor choice, which is invisible when only
            // `l` in 0..=1 is tested.
            ("mw.ustring.byteoffset('héllo', 1, 4)", 4),
            ("mw.ustring.byteoffset('héllo', 2, 4)", 5),
            ("mw.ustring.byteoffset('héllo', 0, 5)", 5),
            ("mw.ustring.byteoffset('héllo', -1, 5)", 4),
            // An empty string has only the one-past-the-end boundary.
            ("mw.ustring.byteoffset('', 1, 1)", 1),
        ] {
            assert_eq!(
                engine.eval(&format!("return {expr}")).unwrap(),
                want.to_string(),
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

    /// `mw.ustring.gcodepoint` iterates codepoints. `Module:Lang` walks a string
    /// with it, so the values must be numeric codepoints and the iterator must
    /// terminate.
    #[test]
    fn test_mw_ustring_gcodepoint() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval(
                    "local t = {} \
                     for cp in mw.ustring.gcodepoint('ab') do t[#t + 1] = cp end \
                     return table.concat(t, ',')"
                )
                .unwrap(),
            "97,98"
        );
        // Multibyte characters count once, as codepoints rather than bytes.
        assert_eq!(
            engine
                .eval(
                    "local n = 0 \
                     for _ in mw.ustring.gcodepoint('héllo') do n = n + 1 end \
                     return n"
                )
                .unwrap(),
            "5"
        );
        // The bounds are codepoint offsets, and negatives count from the end.
        assert_eq!(
            engine
                .eval(
                    "local t = {} \
                     for cp in mw.ustring.gcodepoint('abc', 2) do t[#t + 1] = cp end \
                     return table.concat(t, ',')"
                )
                .unwrap(),
            "98,99"
        );
        assert_eq!(
            engine
                .eval(
                    "local t = {} \
                     for cp in mw.ustring.gcodepoint('abc', -2) do t[#t + 1] = cp end \
                     return table.concat(t, ',')"
                )
                .unwrap(),
            "98,99"
        );
        assert_eq!(
            engine
                .eval(
                    "local t = {} \
                     for cp in mw.ustring.gcodepoint('abc', 1, 2) do t[#t + 1] = cp end \
                     return table.concat(t, ',')"
                )
                .unwrap(),
            "97,98"
        );
        // An out-of-range start yields nothing rather than erroring.
        assert_eq!(
            engine
                .eval(
                    "local n = 0 \
                     for _ in mw.ustring.gcodepoint('abc', 9) do n = n + 1 end \
                     return n"
                )
                .unwrap(),
            "0"
        );
        assert_eq!(
            engine
                .eval(
                    "local n = 0 \
                     for _ in mw.ustring.gcodepoint('') do n = n + 1 end \
                     return n"
                )
                .unwrap(),
            "0"
        );
    }

    /// `mw.text.gsplit` is the iterator form of `mw.text.split`, which
    /// `Module:Annotated link` uses to walk a hash-delimited list.
    #[test]
    fn test_mw_text_gsplit() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval(
                    "local t = {} \
                     for part in mw.text.gsplit('a b  c', '%s+') do t[#t + 1] = part end \
                     return table.concat(t, '|')"
                )
                .unwrap(),
            "a|b|c"
        );
        // A plain separator is a literal, not a pattern.
        assert_eq!(
            engine
                .eval(
                    "local t = {} \
                     for part in mw.text.gsplit('a#b#c', '#', true) do t[#t + 1] = part end \
                     return table.concat(t, '|')"
                )
                .unwrap(),
            "a|b|c"
        );
        // The pieces match what `mw.text.split` returns, in order.
        assert_eq!(
            engine
                .eval(
                    "local a = {} \
                     for part in mw.text.gsplit('a b c', ' ') do a[#a + 1] = part end \
                     return table.concat(a, '|') == table.concat(mw.text.split('a b c', ' '), '|')"
                )
                .unwrap(),
            "true"
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

    /// `mw.ustring.char` takes *codepoints*, not bytes. Lua's `string.char`
    /// rejects anything above 255, and the inherited version raised
    /// "value out of range" on `Module:Lang`, taking the module down on seven
    /// corpus pages.
    #[test]
    fn test_ustring_char_takes_codepoints() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                -- Above the byte range, so `string.char` would raise.
                local u = mw.ustring.char(0x4E2D)
                -- Mixed, and a codepoint with no character is skipped.
                local m = mw.ustring.char(65, 0x4E2D, 0x110000, 66)
                return u .. '/' .. m .. '/' .. mw.ustring.len(u)
            end
            return p
        "#;
        assert_eq!(engine.execute(src, "main", &[]).unwrap(), "中/A中B/1");
    }

    /// `mw.html:attr` accepts a table of attributes as well as a name/value
    /// pair, and a nil value *unsets*. A module passing a table had it used as
    /// the attribute *key*, which reached `_render` and raised "attempt to
    /// concatenate a table value (local 'key')" — four pages in the corpus.
    #[test]
    fn test_mw_html_attr_table_form() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                local d = mw.html.create('div')
                d:attr{ id = 'x', class = 'y' }
                d:attr('data-a', 1)
                -- A nil value unsets rather than writing "nil".
                d:attr('class', nil)
                return tostring(d)
            end
            return p
        "#;
        assert_eq!(
            engine.execute(src, "main", &[]).unwrap(),
            r#"<div id="x" data-a="1"></div>"#
        );
    }

    /// `lang:formatDate` treats `x` as MediaWiki's "raw" prefix, and `n` as the
    /// number-format modifier rather than a field.
    ///
    /// Checked against the service: `U` and `xnU` both give `1577836800` for
    /// `2020-01-01`, `nU` gives `11577836800` (month then stamp), `xU` gives a
    /// literal `U`, and `xx` a literal `x`. `xn` emits *nothing*.
    ///
    /// `Module:Time ago` subtracts a timestamp from `formatDate('xnU')`, so the
    /// month number made it fail with "attempt to sub a 'string' with a 'string'"
    /// and took `Module:Citation/CS1`'s date handling with it.
    #[test]
    fn format_date_handles_the_raw_prefix() {
        let at = |f: &str| format_date(f, "2020-01-01").unwrap();
        assert_eq!(at("U"), "1577836800");
        assert_eq!(at("xnU"), "1577836800");
        assert_eq!(at("nU"), "11577836800");
        // `x` makes the next code literal, and `n` contributes nothing at all.
        assert_eq!(at("xU"), "U");
        assert_eq!(at("xx"), "x");
        assert_eq!(at("xn"), "");
        // A field may still follow the modifier: `H` is `00` here.
        assert_eq!(at("HxnU"), "001577836800");
        // A trailing `x` has nothing to make literal and adds nothing.
        assert_eq!(at("Ux"), "1577836800");
    }

    /// The input shapes MediaWiki accepts, each checked against the service.
    #[test]
    fn format_date_parses_the_documented_stamp_shapes() {
        let ts = |s: &str| format_date("U", s).unwrap();
        assert_eq!(ts("2020-01-01"), "1577836800");
        // Single-digit parts.
        assert_eq!(ts("2020-1-1"), "1577836800");
        // Eight digits, same instant.
        assert_eq!(ts("2020-01-01"), ts("20200101"));
        // A time of day is accepted, and the date half still matches.
        assert_eq!(format_date("H:i", "2020-01-01 07:30").unwrap(), "07:30");
        // A month name in either order; the service gives 2020-01-01 for both.
        assert_eq!(ts("January 2020"), "1577836800");
        assert_eq!(ts("2020 January"), "1577836800");
        // A bare year keeps the current month and day, so it is pinned to a real
        // instant inside that year rather than to an exact value. The service
        // returned 1600732800 for `{{#time:U|2020}}` on 2026-09-22.
        let year_only = ts("2020").parse::<i64>().unwrap();
        assert!(
            (1_577_836_800..1_609_459_200).contains(&year_only),
            "a bare year must land inside 2020, got {year_only}"
        );
    }

    /// An unparseable stamp is an *error*, as MediaWiki's is: `pcall` is what
    /// callers use to catch it. Returning a string made `Module:Time ago`'s
    /// `pcall` succeed and fed error markup into arithmetic.
    #[test]
    fn format_date_rejects_a_stamp_it_cannot_parse() {
        assert!(format_date("U", "nonsense").is_err());
        assert!(format_date("U", "not-a-date").is_err());
        // Empty is *now*, not an error, which the manual states explicitly.
        assert!(format_date("U", "").is_ok());
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

    /// A module a run could not find is reported even when the caller's `pcall`
    /// swallows the error.
    ///
    /// `Module:Unicode data` builds its data-module names at runtime
    /// (`"Module:Unicode data/" .. key`) and wraps the load:
    ///
    /// ```lua
    /// local success, data = pcall(mw.loadData, "Module:Unicode data/" .. key)
    /// if not success then data = false end
    /// ```
    ///
    /// The static preload scan cannot see the name — only the prefix is a literal —
    /// and the `pcall` means the `"module … was not preloaded"` error never reaches
    /// the retry loop. Before the collector existed the module silently got
    /// `false` and failed later with "attempt to index a boolean value".
    ///
    /// This asserts the invariant rather than the end-to-end page: the name
    /// survives the `pcall`, which is what the loop needs in order to fetch it.
    #[test]
    fn a_pcall_swallowed_missing_module_is_still_reported() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                -- The error is caught here, exactly as Module:Unicode data does it.
                local ok = pcall(require, "Module:Nowhere/scripts")
                return tostring(ok)
            end
            return p
        "#;
        // `execute` runs `main`; the missing module must be visible afterwards.
        let out = engine.execute(src, "main", &[]).unwrap();
        assert_eq!(out, "false", "the pcall must have caught the failure");
        assert_eq!(
            engine.take_missing_modules(),
            vec!["Module:Nowhere/scripts".to_string()],
            "the name must survive the pcall for the retry loop to fetch it"
        );
    }

    /// The collector is per run, so one run's miss is not reported against the
    /// next. Without the reset a module fetched on run 1 would look missing on
    /// run 2 and the loop would never settle.
    #[test]
    fn the_missing_module_collector_is_reset_each_run() {
        let engine = make_engine();
        let src = r#"
            local p = {}
            function p.main(frame)
                pcall(require, "Module:Nowhere/scripts")
                return "ok"
            end
            return p
        "#;
        engine.execute(src, "main", &[]).unwrap();
        assert_eq!(engine.take_missing_modules().len(), 1);
        // `take` cleared it, and a second run with no miss reports nothing.
        engine.execute(src, "main", &[]).unwrap();
        assert_eq!(
            engine.take_missing_modules().len(),
            1,
            "each run collects its own"
        );
    }
}
