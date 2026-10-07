//! Running the parser mid-Lua for `frame:expandTemplate` and friends.
//!
//! Scribunto's frame methods call back into the parser *synchronously*, but
//! rustoid's pipeline is async and `mlua` cannot await. The call is therefore
//! deferred: Lua records what it wants (`title` + `args`), yields, the host
//! expands it with the ordinary async pipeline, and the module is re-run with
//! the answer available. A request is keyed by its content, so the *n*th call in
//! a run lands on the *n*th answer even if the module reaches its calls in a
//! different order.
//!
//! The two halves live here because they must agree on the shape of things:
//! [`frame_method`] (inside the engine, building a request) and
//! [`parse_expanded`](crate::pipeline::parser::Parser::expand_lua_result)
//! (outside it, turning the expansion back into a token).

use mlua::{Lua, Table, Value};

use crate::error::{Result, RustoidError};

use crate::wikitext::tokens_v2::Item;

/// `frame:expandTemplate` — `{ title = …, args = { … } }`.
pub const EXPAND_TEMPLATE: &str = "expandTemplate";
/// `frame:preprocess` — a wikitext string.
pub const PREPROCESS: &str = "preprocess";
/// `frame:callParserFunction` — `{ name, args = { … } }`.
pub const CALL_PARSER_FUNCTION: &str = "callParserFunction";
/// `frame:extensionTag` — `{ name, content, args }`.
pub const EXTENSION_TAG: &str = "extensionTag";

/// The out-of-band marker that carries a deferred answer back into Lua.
///
/// mlua can ferry any Lua value across invocations, so the results travel as
/// registry entries (`lua_deferred:N`) keyed by a reserved global name; this
/// table on the engine's `_G` holds the `expandTemplate` answers.
pub const RESULTS: &str = "__rustoid_frame_results";

/// A frame method that needs the parser, as data the host can expand.
///
/// Deliberately *not* the token the answer will become: the host must run the
/// real pipeline, so all it needs is the wikitext-equivalent description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameRequest {
    ExpandTemplate {
        title: String,
        args: Vec<FrameArg>,
    },
    Preprocess(String),
    CallParserFunction {
        name: String,
        args: Vec<FrameArg>,
    },
    /// One or more `frame.args` reads the host has not expanded yet.
    ///
    /// Scribunto expands an argument the *first time the module reads it*, and
    /// never before: the templates inside an argument spend their `about` ids
    /// at that moment, not when the frame is created. So a single `args[k]`
    /// read carries one slot, and the `pairs`/`argumentPairs` view carries
    /// every still-unexpanded slot at once, in the frame's own order.
    ExpandArgs {
        slots: Vec<ArgSlot>,
    },
}

/// Which raw argument list a [`FrameRequest::ExpandArgs`] slot indexes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgSource {
    /// The `#invoke` call's own arguments.
    Call,
    /// The arguments of the frame that made the call — `frame:getParent().args`.
    Parent,
}

impl ArgSource {
    fn tag(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Parent => "parent",
        }
    }
}

/// One `frame.args` argument the host has not expanded yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSlot {
    pub source: ArgSource,
    /// Position in the raw argument list it belongs to.
    pub index: usize,
    /// The name it is stored under in `frame.args`; `None` for a positional
    /// argument, whose key is its number.
    pub name: Option<String>,
    /// The argument as written. Kept so the call can be echoed when nothing can
    /// be expanded (no data source), which never reads an argument at all.
    pub raw: String,
}

impl ArgSlot {
    /// The key this argument's expanded text is cached under.
    ///
    /// A `HashMap` key rather than a positional identity, because the answers
    /// travel as strings: two slots never collide, since the source names the
    /// list and `index` its position.
    pub fn key(&self) -> String {
        format!("invoke-arg:{}:{}", self.source.tag(), self.index)
    }
}

/// One argument of a frame call: a value, and the name it was given if any.
///
/// Modelled as a struct rather than `(Option<String>, String)` so a reader does
/// not have to remember which side of the tuple is which at every use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameArg {
    /// `None` for a positional argument.
    pub name: Option<String>,
    pub value: String,
}

impl FrameArg {
    pub fn positional(value: impl Into<String>) -> Self {
        Self {
            name: None,
            value: value.into(),
        }
    }

    pub fn named(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            value: value.into(),
        }
    }
}

impl FrameRequest {
    /// The call's arguments, positional first then named.
    pub fn args(&self) -> &[FrameArg] {
        match self {
            Self::ExpandTemplate { args, .. } | Self::CallParserFunction { args, .. } => args,
            Self::Preprocess(_) | Self::ExpandArgs { .. } => &[],
        }
    }

    /// Identity of the request, for matching an answer to the call that asked.
    ///
    /// Named arguments are sorted: Lua table iteration order for
    /// `{ a = 1, b = 2 }` need not match the order the caller wrote them, and
    /// MediaWiki treats them as a set, so two spellings of the same call must
    /// land on the same answer.
    pub fn key(&self) -> String {
        let render = |args: &[FrameArg]| {
            let mut named: Vec<(String, String)> = args
                .iter()
                .filter_map(|a| a.name.clone().map(|k| (k, a.value.clone())))
                .collect();
            named.sort();
            let positional: Vec<&String> = args
                .iter()
                .filter(|a| a.name.is_none())
                .map(|a| &a.value)
                .collect();
            let named: Vec<String> = named.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!(
                "{}|{}",
                positional
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\u{1f}"),
                named.join("\u{1f}")
            )
        };
        match self {
            Self::ExpandTemplate { title, args } => {
                format!("{{{{{title}|{}}}}}", render(args))
            }
            Self::Preprocess(text) => format!("preprocess:{text}"),
            Self::CallParserFunction { name, args } => {
                format!("{{{{#{name}:{}}}}}", render(args))
            }
            // Unused: a slot's answer is keyed by the slot itself
            // ([`ArgSlot::key`]), so the batch request never needs a key of its
            // own. Kept total rather than panicking.
            Self::ExpandArgs { slots } => {
                let keys: Vec<String> = slots.iter().map(ArgSlot::key).collect();
                format!("expand-args:{}", keys.join("\u{1f}"))
            }
        }
    }
}

/// The cached answers of one `#invoke` run, keyed by [`FrameRequest::key`].
///
/// A string, because that is what every one of these frame methods returns: the
/// expanded result, which the module may then concatenate, compare or slice.
pub type DeferredAnswers = std::collections::HashMap<String, String>;

/// Read a Scribunto `args` table — `{"a", "b", name = "c"}` — into call order.
///
/// Integer keys are positional, in ascending order; everything else is named,
/// sorted for determinism because Lua table iteration order is unspecified. A
/// module that writes `args = { "x" }` and one that writes
/// `args = { [1] = "x" }` mean the same call, and both are positional — which is
/// why the test is the *key*, not how the value was reached.
pub fn args_from_table(table: &Table) -> mlua::Result<Vec<FrameArg>> {
    let mut positional: Vec<(i64, String)> = Vec::new();
    let mut named: Vec<FrameArg> = Vec::new();

    for pair in table.pairs::<Value, Value>() {
        let (k, v) = pair?;
        match integer_key(&k) {
            Some(i) => positional.push((i, stringify(&v)?)),
            None => named.push(FrameArg::named(stringify(&k)?, stringify(&v)?)),
        }
    }

    positional.sort_by_key(|(i, _)| *i);
    // A key that is not 1, 2, 3… (`{[0] = 'x'}`, `{[5] = 'x'}`) is not an
    // argument list, so it becomes a named argument with the number as its
    // name — which is what the wikitext preprocessor does with such a key.
    let keys: Vec<i64> = positional.iter().map(|(i, _)| *i).collect();
    let expected: Vec<i64> = (1..=positional.len() as i64).collect();
    if keys != expected {
        named.extend(
            positional
                .into_iter()
                .map(|(i, v)| FrameArg::named(i.to_string(), v)),
        );
    } else {
        named.extend(positional.into_iter().map(|(_, v)| FrameArg::positional(v)));
    }

    named.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(named)
}

/// The integer a Lua key denotes, for a key that denotes one at all.
fn integer_key(key: &Value) -> Option<i64> {
    match key {
        Value::Integer(i) => Some(*i),
        Value::Number(n) if n.fract() == 0.0 && n.is_finite() => Some(*n as i64),
        _ => None,
    }
}

/// Scribunto's argument values are strings; a number is a common accident.
fn stringify(value: &Value) -> mlua::Result<String> {
    match value {
        Value::Nil => Ok(String::new()),
        Value::String(s) => Ok(s.to_str()?.to_string()),
        Value::Integer(i) => Ok(i.to_string()),
        Value::Number(n) => Ok(crate::lua::engine::format_number(*n)),
        Value::Boolean(b) => Ok(b.to_string()),
        other => Ok(other.to_string()?),
    }
}

/// Build an `mlua` function implementing one [`FrameRequest`].
///
/// The returned function looks the call up in `answers`; on a miss it records
/// the request and raises Scribunto's "not cached" error, which the host
/// recognises by its text and turns into a re-run.
pub fn frame_method(
    lua: &Lua,
    method: &'static str,
    answers: DeferredAnswers,
    pending: std::rc::Rc<std::cell::RefCell<Vec<FrameRequest>>>,
) -> Result<mlua::Function> {
    lua.create_function(move |lua, args: mlua::MultiValue| {
        let request = build_request(method, args)?;
        let key = request.key();
        // Answers are strings, so nothing has to be re-created per run:
        // mlua clones them directly.
        if let Some(answer) = answers.get(&key) {
            return Ok(Value::String(lua.create_string(answer)?));
        }
        // Only the first miss of a round is recorded: the host expands what it
        // is told about, and the error unwinds the whole call anyway.
        if pending.borrow().is_empty() {
            pending.borrow_mut().push(request);
        }
        Err(mlua::Error::runtime(format!("{NOT_CACHED}{key}")))
    })
    .map_err(|e| RustoidError::Lua(e.to_string()))
}

/// Marker prefix of the "this call has not been expanded yet" error.
///
/// The key follows on the same line, so the host knows precisely which request
/// to serve instead of having to re-derive it from the error's shape.
pub const NOT_CACHED: &str = "rustoid: frame method deferred: ";

/// Parse the marker back into the request key.
pub fn pending_key(message: &str) -> Option<&str> {
    let start = message.find(NOT_CACHED)? + NOT_CACHED.len();
    let rest = &message[start..];
    Some(rest.lines().next().unwrap_or(rest).trim())
}

/// Split a `#tag:nowiki`-style function name into the function and its first
/// argument, per the manual. Any other name is returned unchanged.
fn split_colon_name(raw: &str) -> (String, Option<String>) {
    match raw.split_once(':') {
        Some((head, tail)) if head.starts_with('#') => (head.to_string(), Some(tail.to_string())),
        _ => (raw.to_string(), None),
    }
}

/// Whether a value is the frame table itself — recognised by the members every
/// frame has, so an ordinary options table is not mistaken for one.
fn is_frame(value: &Value) -> bool {
    matches!(value, Value::Table(t)
        if t.contains_key("args").unwrap_or(false)
            && t.contains_key("getTitle").unwrap_or(false))
}

/// The method arguments of a frame call, in the order the module wrote them.
///
/// `frame:expandTemplate{…}` takes exactly one table — Scribunto's named
/// argument — so the `self` parameter is simply not passed by the engine; see
/// [`crate::lua::engine::frame_methods`].
fn build_request(method: &str, args: mlua::MultiValue) -> mlua::Result<FrameRequest> {
    let mut args = args.into_iter();
    let first = args.next().unwrap_or(Value::Nil);
    // The methods are called with `:` (`frame:expandTemplate{…}`), so Lua passes
    // the frame itself as the first argument and the real one follows. Keying
    // off `args`+`getTitle` rather than the type keeps `frame:preprocess("…")`
    // on a *different* frame table working, which a bare `Table` test would
    // misread as the argument.
    let arg = if is_frame(&first) {
        args.next().unwrap_or(Value::Nil)
    } else {
        first
    };

    match method {
        EXPAND_TEMPLATE => {
            let Value::Table(opts) = arg else {
                return Err(mlua::Error::runtime(
                    "expandTemplate expects a table with a title",
                ));
            };
            let title = opts
                .get::<Option<String>>("title")?
                .unwrap_or_default()
                .trim()
                .to_string();
            if title.is_empty() {
                return Err(mlua::Error::runtime("expandTemplate expects a title"));
            }
            let args = match opts.get::<Value>("args")? {
                Value::Table(t) => args_from_table(&t)?,
                _ => Vec::new(),
            };
            Ok(FrameRequest::ExpandTemplate { title, args })
        }
        CALL_PARSER_FUNCTION => {
            // Three documented spellings, all in the corpus:
            //   frame:callParserFunction( name, args )
            //   frame:callParserFunction( name, ... )
            //   frame:callParserFunction{ name = name, args = args }
            // The named form arrives as the single table; the other two as
            // positional arguments, which are collected into the same shape.
            let (name, args) = match arg {
                Value::Table(opts) if opts.contains_key("name").unwrap_or(false) => {
                    let name = opts
                        .get::<Option<String>>("name")?
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    let args = match opts.get::<Value>("args")? {
                        Value::Table(t) => args_from_table(&t)?,
                        _ => Vec::new(),
                    };
                    (name, args)
                }
                Value::Table(table) => {
                    // `frame:callParserFunction( 'ns', { 0 } )` — the first
                    // positional is the name, an optional table the arguments.
                    let name = table
                        .get::<Option<String>>(1)?
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    let args = match table.get::<Value>(2)? {
                        Value::Table(t) => args_from_table(&t)?,
                        Value::Nil => Vec::new(),
                        other => vec![FrameArg::positional(stringify(&other)?)],
                    };
                    (name, args)
                }
                Value::String(s) => {
                    // `frame:callParserFunction( name, args )` and
                    // `frame:callParserFunction( name, ... )` — the name is the
                    // first positional and everything after it is the
                    // arguments, either as one sequence table or spread out.
                    let raw = s.to_str().map_err(mlua::Error::external)?.to_string();
                    let rest: Vec<Value> = args.collect();

                    // A `#tag:nowiki` name carries its first argument in the
                    // name, which the manual documents: the part after the
                    // first `:` is prepended to the arguments.
                    let (name, colon_arg) = split_colon_name(&raw);

                    let mut collected: Vec<FrameArg> = Vec::new();
                    if let Some(arg) = colon_arg {
                        collected.push(FrameArg::positional(arg));
                    }
                    match rest.as_slice() {
                        // A lone table is the argument list, not an argument.
                        [Value::Table(t)] => collected.extend(args_from_table(t)?),
                        _ => {
                            for value in rest {
                                collected.push(FrameArg::positional(stringify(&value)?));
                            }
                        }
                    }
                    (name, collected)
                }
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "callParserFunction expects a function name, got a {}",
                        other.type_name()
                    )));
                }
            };
            if name.is_empty() {
                return Err(mlua::Error::runtime(
                    "callParserFunction expects a function name",
                ));
            }
            Ok(FrameRequest::CallParserFunction { name, args })
        }
        EXTENSION_TAG => {
            // `frame:extensionTag` is `#tag`, so it is lowered to exactly that.
            // Two documented spellings, and the corpus uses the positional one:
            //   frame:extensionTag( name, content, args )
            //   frame:extensionTag{ name, content, args }
            // In both, the tag name becomes the first argument and the content
            // the second, with the rest as named parameters.
            //
            // A *positional* call arrives as several values, not as one table,
            // so the rest of the argument list is collected here rather than
            // read out of `arg`.
            let rest: Vec<Value> = args.collect();
            let (name, content, extra) = match arg {
                Value::Table(opts) if opts.contains_key("name").unwrap_or(false) => (
                    opts.get::<Option<String>>("name")?.unwrap_or_default(),
                    opts.get::<Value>("content")?,
                    opts.get::<Value>("args")?,
                ),
                first => (
                    stringify(&first)?,
                    rest.first().cloned().unwrap_or(Value::Nil),
                    rest.get(1).cloned().unwrap_or(Value::Nil),
                ),
            };
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(mlua::Error::runtime("extensionTag expects a tag name"));
            }
            let mut args = vec![FrameArg::positional(name)];
            // The content is optional, and a nil one must not become the string
            // "nil" — `frame:extensionTag{ name = 'ref' }` is a bare tag.
            if let Value::String(content) = content {
                args.push(FrameArg::positional(content.to_str()?.to_string()));
            }
            if let Value::Table(extra) = extra {
                args.extend(args_from_table(&extra)?);
            }
            Ok(FrameRequest::CallParserFunction {
                name: "#tag".to_string(),
                args,
            })
        }
        PREPROCESS => {
            let Value::String(s) = arg else {
                return Err(mlua::Error::runtime(format!(
                    "preprocess expects a string, got a {}",
                    arg.type_name()
                )));
            };
            Ok(FrameRequest::Preprocess(s.to_str()?.to_string()))
        }
        other => Err(mlua::Error::runtime(format!(
            "unknown frame method {other}"
        ))),
    }
}

/// The wikitext a request stands for.
///
/// This is the call as the module would have written it, which is what the
/// pipeline expands.
pub fn render_call(request: &FrameRequest) -> String {
    match request {
        FrameRequest::ExpandTemplate { title, args } => render_invocation(title, args),
        // `{{#name:first|rest}}`: a parser function's first argument is joined
        // by a *colon*, and the pipe separates the rest. Scribunto lets the
        // module pass an empty first argument precisely so the colon can be
        // written even when the function ignores it.
        FrameRequest::CallParserFunction { name, args } => {
            let mut args = args.iter();
            // The `#` is *syntax*, not part of the name: Scribunto names the
            // function `tag`, and `{{#tag:…}}` writes the hash once. A name that
            // arrives with its own leading `#` — which is how
            // `frame:extensionTag` is lowered, to `"#tag"` — would otherwise
            // render as `{{##tag:…}}`, which is not a call at all and silently
            // failed to expand.
            //
            // Whether the hash belongs there at all follows MediaWiki's
            // registration: a function in Parsoid's `noHashFunctions` set is
            // written bare (`{{DISPLAYTITLE:…}}`), every other one carries a
            // hash (`{{#if:…}}`). Rendering `{{#DISPLAYTITLE:…}}` would be a
            // *broken* parser function — the service renders it literally — so a
            // `frame:callParserFunction('DISPLAYTITLE', …)` has to come out bare.
            let bare = name.strip_prefix('#').unwrap_or(name);
            let hash = if is_no_hash_function(bare) { "" } else { "#" };
            let mut out = format!("{{{{{hash}{bare}");
            if let Some(first) = args.next() {
                out.push(':');
                out.push_str(&first.value);
            }
            for arg in args {
                out.push('|');
                if let Some(name) = &arg.name {
                    out.push_str(name);
                    out.push('=');
                }
                out.push_str(&arg.value);
            }
            out.push_str("}}");
            out
        }
        FrameRequest::Preprocess(text) => text.clone(),
        // Not a call at all: the host serves an `ExpandArgs` request by
        // expanding the slots directly, not by rendering wikitext.
        FrameRequest::ExpandArgs { .. } => String::new(),
    }
}

/// Whether a parser function is registered *without* a leading `#`.
///
/// MediaWiki's `Parser::setFunctionHook` prepends the hash unless the function
/// is listed in core's `$noHashFunctions`; Parsoid mirrors that in
/// `ApiSiteConfig::updateFunctionSynonym`. The set below is Parsoid's own
/// approximation list (it carries the same caveat in its source), kept in the
/// order and spelling it uses. It decides how a module's
/// `frame:callParserFunction` is written back to wikitext: `{{DISPLAYTITLE:…}}`
/// and `{{#if:…}}` are the two ends of it.
fn is_no_hash_function(name: &str) -> bool {
    let lower = name.to_lowercase();
    const NO_HASH_FUNCTIONS: &[&str] = &[
        "ns",
        "nse",
        "urlencode",
        "lcfirst",
        "ucfirst",
        "lc",
        "uc",
        "localurl",
        "localurle",
        "fullurl",
        "fullurle",
        "canonicalurl",
        "canonicalurle",
        "formatnum",
        "grammar",
        "gender",
        "plural",
        "formal",
        "bidi",
        "numberingroup",
        "language",
        "padleft",
        "padright",
        "anchorencode",
        "defaultsort",
        "filepath",
        "pagesincategory",
        "pagesize",
        "protectionlevel",
        "protectionexpiry",
        "pagename",
        "pagenamee",
        "fullpagename",
        "fullpagenamee",
        "subpagename",
        "subpagenamee",
        "rootpagename",
        "rootpagenamee",
        "basepagename",
        "basepagenamee",
        "talkpagename",
        "talkpagenamee",
        "subjectpagename",
        "subjectpagenamee",
        "pageid",
        "revisionid",
        "revisionday",
        "revisionday2",
        "revisionmonth",
        "revisionmonth1",
        "revisionyear",
        "revisiontimestamp",
        "revisionuser",
        "cascadingsources",
        "namespace",
        "namespacee",
        "namespacenumber",
        "talkspace",
        "talkspacee",
        "subjectspace",
        "subjectspacee",
        "numberofarticles",
        "numberoffiles",
        "numberofusers",
        "numberofactiveusers",
        "numberofpages",
        "numberofadmins",
        "numberofedits",
        "bcp47",
        "dir",
        "interwikilink",
        "interlanguagelink",
        "int",
        "displaytitle",
        "pagesinnamespace",
    ];
    NO_HASH_FUNCTIONS.contains(&lower.as_str())
}

/// The wikitext a template or parser-function call with these arguments is.
fn render_invocation(target: &str, args: &[FrameArg]) -> String {
    let mut out = format!("{{{{{target}");
    for arg in args {
        out.push('|');
        if let Some(name) = &arg.name {
            out.push_str(name);
            out.push('=');
        }
        out.push_str(&arg.value);
    }
    out.push_str("}}");
    out
}

/// The text a module receives from a frame call.
///
/// These frame methods return strings — the manual says so of each — and the
/// string is the expansion's **wikitext**, not its rendered HTML. That is easy
/// to get wrong and expensive to guess at, so it was measured against the live
/// service by asking a module for the answer's length and first bytes:
///
/// | call | answer | length |
/// |---|---|---|
/// | `{{1x|'''b'''}}` | `'''b'''` | 7 (HTML would be 8) |
/// | `{{1x|</b>}}` | `</b>` | 4 (HTML would be 0) |
/// | `{{1x|&amp;}}` | `&amp;` | 5 (HTML would be 1) |
/// | `{{Taxonomy/NoSuchTaxon|…}}` | `[[:Template:Taxonomy/NoSuchTaxon]]` | 37 |
///
/// So markup survives as *source*, which is what makes the fourth row work: a
/// missing template answers with the wikitext `[[:Title]]` (PHP's
/// `braceSubstitution`), and a module may feed that back in as a title or an
/// argument. Rendering it to an anchor instead changes what the module holds,
/// and `'Taxonomy/' .. <a …>Title</a>` is not a title the wiki would ever build.
///
/// Comments are the one thing removed, because the preprocessor strips them
/// before the module sees anything (`{{1x|<!--c-->t}}` is 1 character, not 9).
///
/// A `mw:Transclusion` marker is Parsoid bookkeeping rather than output and is
/// dropped.
pub fn render_answer(items: &[Item]) -> String {
    render_answer_with(items, MissingSrc::Drop)
}

/// How the answer renderer treats a token it cannot spell: what to do with a live
/// element that has no `src`, and whether a *stale* source may be reconstructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingSrc {
    /// Drop a source-less element, and write a recorded source verbatim.
    /// [`render_answer`]'s behaviour, for a token-list rebuilt into an attribute
    /// value (the recursive case inside `quote_token_wikitext` and
    /// `wikilink_token_wikitext`).
    Drop,
    /// An expansion's *text* handed across the Lua boundary — a `frame` call's
    /// answer or an `#invoke` argument value. A stale recorded source (one that
    /// still spells a `{{…}}` the frame expanded) is rebuilt from the token's live
    /// attributes, so the module receives the expansion, not the construct.
    Html,
}

/// [`render_answer`] with a policy for what it cannot spell as written.
pub fn render_answer_with(items: &[Item], missing_src: MissingSrc) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < items.len() {
        if let Some((text, next)) = answer_item_text(items, i, missing_src) {
            out.push_str(&text);
            i = next;
        } else {
            i += 1;
        }
    }
    out
}

/// The wikitext of the item at `i`, and the index to continue at — or `None` when
/// the item has no textual form at all.
///
/// [`render_answer`]'s arms, with the `missing_src` policy for the two cases its
/// doc block does not cover. An `mw:Entity` span consumes its decoded character
/// and its end tag with its source, which is why the index is returned.
pub fn answer_item_text(
    items: &[Item],
    i: usize,
    missing_src: MissingSrc,
) -> Option<(String, usize)> {
    use crate::wikitext::tokens_v2::ParsoidToken;
    match &items[i] {
        Item::Str(s) => Some((s.clone(), i + 1)),
        Item::Tok(tok) => {
            // Both the opening marker and its `mw:Transclusion/End` partner are
            // bookkeeping, not output, and a comment never reaches a module.
            if is_transclusion_marker(tok) {
                return Some((String::new(), i + 1));
            }
            match tok {
                // A comment never reaches a module; a newline is source the
                // module can see (and table syntax needs it on its own line).
                ParsoidToken::Comment(_) => return Some((String::new(), i + 1)),
                // A `Nl` whose source is recorded writes that source below;
                // emitting the bare `\n` here too doubled the newline
                // (`...>\n<div...` became `...>\n\n<div...`). Only a `Nl` that
                // carries no source needs the newline here.
                ParsoidToken::Nl(_)
                    if tok
                        .data_parsoid()
                        .and_then(|dp| dp.src.as_deref())
                        .is_none() =>
                {
                    return Some(("\n".to_string(), i + 1));
                }
                _ => {}
            }
            // The token's own source is the source *as written*; an attribute
            // substituted during expansion has to be written back so the module is
            // handed the expansion, not the template. See
            // [`crate::wikitext::token_utils::rewrite_expanded_attrs`].
            // Table-structure tokens have no `src` and are rebuilt instead.
            if let Some(src) = tok.data_parsoid().and_then(|dp| dp.src.as_deref()) {
                let rewritten =
                    crate::wikitext::token_utils::rewrite_expanded_attrs(src, tok.get_attribs())
                        .unwrap_or_else(|| src.to_string());
                let text = stale_tag_html(tok, &rewritten, missing_src).unwrap_or(rewritten);
                // An `mw:Entity` span's `src` already spells the entity, so its
                // decoded text child is skipped rather than emitted too.
                let next = if crate::wikitext::token_utils::skip_entity_decoded_text(
                    tok,
                    items.get(i + 1),
                ) {
                    i + 2
                } else {
                    i + 1
                };
                return Some((text, next));
            }
            if let Some(src) = wikilink_token_wikitext(tok) {
                // A `[[…]]` the tokenizer did not stamp with `src` — a link a
                // module returned, tokenized from the module's output — is
                // rebuilt from its parts, so a `frame:expandTemplate` answer keeps
                // it (`[[File:…]]` otherwise vanished and the taxobox image row
                // came back empty).
                return Some((src, i + 1));
            }
            if let Some(src) = crate::wikitext::token_utils::table_token_wikitext(tok) {
                return Some((src, i + 1));
            }
            if let Some(src) = quote_token_wikitext(tok) {
                // An `mw-quote` a module's output produced has no `src` (the quote
                // transformer has not run, nor can it — the answer is wikitext), so
                // it too is rebuilt: without this a link's italic markup was
                // dropped and `[[Equus (genus)|''Equus'']]` came back as
                // `[[Equus (genus)|Equus]]`.
                return Some((src, i + 1));
            }
            None
        }
    }
}

/// Rebuild an element from its *expanded* attributes when a construct survives the
/// source rewrite, or `None` to keep the rewritten source.
///
/// A template construct can sit in attribute position and expand into whole
/// attributes rather than a value. `Template:Plainlist` writes its body's open tag
/// as `<div class="plainlist {{{class|}}}" {{safesubst<noinclude />:#if:…}}>`, and
/// `Template:Div col` as `<div class="div-col {{#ifeq:…}}" {{#if:…}}>`; the frame
/// expands both, so the token's `attribs` hold the answer while its `src` still
/// spells the constructs. `rewrite_expanded_attrs` fixes the attribute (`class`),
/// but the construct itself is not an attribute, so it survives — and the module is
/// handed the raw call. Rebuilding the tag from the attributes resolves it. A
/// rewritten source that no longer spells a `{{…}}` is left alone, so the ordinary
/// case (a substituted attribute value) is unchanged.
fn stale_tag_html(
    tok: &crate::wikitext::tokens_v2::ParsoidToken,
    rewritten: &str,
    mode: MissingSrc,
) -> Option<String> {
    if mode != MissingSrc::Html || !rewritten.contains("{{") {
        return None;
    }
    element_html_from_attribs(tok)
}

/// The HTML of a live element token built from its attributes — an open tag with
/// its attributes, or a close tag.
///
/// Parsoid's own attributes are left out: `about`, a `typeof` in the `mw:`
/// namespaces (encapsulation and extension types), and the `data-mw` bookkeeping
/// keys are not HTML. A `mw:DOMFragment` placeholder declines — it is stashed
/// output, carried as a strip marker rather than spelled.
fn element_html_from_attribs(tok: &crate::wikitext::tokens_v2::ParsoidToken) -> Option<String> {
    use crate::wikitext::tokens_v2::ParsoidToken;
    match tok {
        ParsoidToken::EndTag(t) => Some(format!("</{}>", t.name)),
        ParsoidToken::Tag(t) if !is_dom_fragment(t) => {
            Some(format!("<{}{}>", t.name, html_attributes(&t.attribs)))
        }
        _ => None,
    }
}

/// Whether a tag token is a `mw:DOMFragment` placeholder — stashed output reached
/// through a strip marker, not markup.
fn is_dom_fragment(t: &crate::wikitext::tokens_v2::TagTk) -> bool {
    t.attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("typeof"))
        .and_then(|kv| kv.value.as_str())
        .is_some_and(|ty| ty.contains("mw:DOMFragment"))
}

/// The attribute list of a generated element, as HTML.
///
/// The value is read with [`crate::wikitext::token_utils::key_value_source_text`],
/// not `KeyValue::as_str`: `Template:Plain list` writes `class="plainlist
/// {{{class|}}}"`, so the `class` a live element must keep is held as `Tokens`,
/// and reading only `Str` dropped it (`<div>` where the service has
/// `<div class="plainlist ">`).
fn html_attributes(attribs: &[crate::wikitext::tokens_v2::KV]) -> String {
    let mut out = String::new();
    for kv in attribs {
        let Some(key) = kv.key.as_str() else {
            continue;
        };
        let value = crate::wikitext::token_utils::key_value_source_text(&kv.value);
        if key == "about" || key.starts_with("data-mw") {
            continue;
        }
        if key == "typeof" && value.split_whitespace().any(|t| t.starts_with("mw:")) {
            continue;
        }
        out.push_str(&format!(" {key}=\"{}\"", escape_attribute(&value)));
    }
    out
}

/// Escape an attribute value for a double-quoted HTML attribute, as the
/// serializer does (`&`, `<`, `"`; `'` is not the delimiter here).
fn escape_attribute(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

/// An `mw-quote` token's wikitext: the apostrophe run it stands for.
///
/// A quote token carries its run in the `value` attribute and no `src`, so the
/// only way to stringify it as wikitext is to spell the run back out.
fn quote_token_wikitext(token: &crate::wikitext::tokens_v2::ParsoidToken) -> Option<String> {
    use crate::wikitext::tokens_v2::{KeyValue, ParsoidToken};
    let ParsoidToken::SelfclosingTag(t) = token else {
        return None;
    };
    if t.name != "mw-quote" {
        return None;
    }
    let value = t
        .attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("value"))?;
    Some(match &value.value {
        KeyValue::Str(s) => s.clone(),
        KeyValue::Tokens(items) => render_answer(items),
    })
}

/// Whether a token is a `mw:Transclusion` marker or its `/End` partner.
fn is_transclusion_marker(token: &crate::wikitext::tokens_v2::ParsoidToken) -> bool {
    token
        .get_attribute_v("typeof")
        .is_some_and(|ty| ty.starts_with("mw:Transclusion"))
}

/// Rebuild a `wikilink` token's `[[target|…]]` wikitext from its attributes.
///
/// A link tokenized from a page or template body carries `data_parsoid.src`, but
/// one tokenized from a *module's output* does not, and `render_answer` would
/// drop it — so a `[[File:…]]` a module emitted through `frame:expandTemplate`
/// never reached the DOM. `href` is the target and each `mw:maybeContent` is a
/// `|`-separated part; a part this cannot spell declines the whole link.
fn wikilink_token_wikitext(token: &crate::wikitext::tokens_v2::ParsoidToken) -> Option<String> {
    use crate::wikitext::tokens_v2::KeyValue;
    let crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(t) = token else {
        return None;
    };
    if t.name != "wikilink" {
        return None;
    }
    // A part may be a `Tokens` list even when it holds no real token — a
    // substituted `{{{1}}}` leaves several `Str` runs (`link=Template:Taxonomy/`
    // + the value) — so render it rather than declining the whole link.
    let part_text = |v: &KeyValue| match v {
        KeyValue::Str(s) => s.clone(),
        KeyValue::Tokens(items) => render_answer(items),
    };
    let href = t
        .attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("href"))?;
    let mut out = String::from("[[");
    out.push_str(&part_text(&href.value));
    for part in t
        .attribs
        .iter()
        .filter(|kv| kv.key.as_str() == Some("mw:maybeContent"))
    {
        out.push('|');
        out.push_str(&part_text(&part.value));
    }
    out.push_str("]]");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `frame:callParserFunction` for a function MediaWiki registers without a
    /// hash must be written without one: `{{DISPLAYTITLE:…}}` resolves, while
    /// `{{#DISPLAYTITLE:…}}` is a broken parser function the service renders
    /// literally. `#tag` and `#if` keep their hash.
    #[test]
    fn render_call_writes_a_no_hash_function_without_the_hash() {
        let call = |name: &str| FrameRequest::CallParserFunction {
            name: name.to_string(),
            args: vec![FrameArg::positional("X")],
        };
        assert_eq!(render_call(&call("DISPLAYTITLE")), "{{DISPLAYTITLE:X}}");
        assert_eq!(
            render_call(&call("PROTECTIONEXPIRY")),
            "{{PROTECTIONEXPIRY:X}}"
        );
        // `frame:extensionTag` lowers to `#tag`; the hash must survive.
        assert_eq!(render_call(&call("#tag")), "{{#tag:X}}");
        assert_eq!(render_call(&call("if")), "{{#if:X}}");
    }

    /// A `[[File:…]]` a module emits through `frame:expandTemplate` is tokenized
    /// without `data_parsoid.src`, so `render_answer` must rebuild it from the
    /// token's parts instead of dropping it (the taxobox image row came back
    /// empty otherwise).
    #[test]
    fn render_answer_rebuilds_a_module_wikilink_without_src() {
        use crate::wikitext::tokens_v2::{DataParsoid, Item, ParsoidToken, SelfclosingTagTk};
        let mut link = SelfclosingTagTk::new("wikilink", vec![], DataParsoid::default());
        link.add_attribute_str("href", "File:Plains Zebra Equus quagga cropped.jpg");
        link.add_attribute_str("mw:maybeContent", "frameless");
        let items = vec![Item::Tok(ParsoidToken::SelfclosingTag(link))];
        assert_eq!(
            render_answer(&items),
            "[[File:Plains Zebra Equus quagga cropped.jpg|frameless]]"
        );
    }

    /// A module's `[[Genus|''Name'']]` tokenizes its italic run into `mw-quote`
    /// tokens that carry no `src`, so `render_answer` must spell the run back out:
    /// without it the rebuilt link lost its italics (`[[Equus (genus)|Equus]]`).
    #[test]
    fn render_answer_rebuilds_a_module_wikilinks_quote_markup() {
        use crate::wikitext::tokens_v2::{
            DataParsoid, Item, KeyValue, ParsoidToken, SelfclosingTagTk,
        };
        let quote = |run: &str| {
            let mut q = SelfclosingTagTk::new("mw-quote", vec![], DataParsoid::default());
            q.add_attribute_str("value", run);
            Item::Tok(ParsoidToken::SelfclosingTag(q))
        };
        let mut link = SelfclosingTagTk::new("wikilink", vec![], DataParsoid::default());
        link.add_attribute_str("href", "Equus (genus)");
        link.attribs.push(crate::wikitext::tokens_v2::KV {
            key: KeyValue::Str("mw:maybeContent".into()),
            value: KeyValue::Tokens(vec![quote("''"), Item::Str("Equus".into()), quote("''")]),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
        let items = vec![Item::Tok(ParsoidToken::SelfclosingTag(link))];
        assert_eq!(render_answer(&items), "[[Equus (genus)|''Equus'']]");
    }

    fn request(title: &str, args: &[(&str, &str)]) -> FrameRequest {
        FrameRequest::ExpandTemplate {
            title: title.to_string(),
            args: args
                .iter()
                .map(|(k, v)| {
                    if k.is_empty() {
                        FrameArg::positional(*v)
                    } else {
                        FrameArg::named(*k, *v)
                    }
                })
                .collect(),
        }
    }

    #[test]
    fn named_arguments_key_independently_of_order() {
        assert_eq!(
            request("T", &[("a", "1"), ("b", "2")]).key(),
            request("T", &[("b", "2"), ("a", "1")]).key()
        );
    }

    #[test]
    fn positional_order_matters() {
        assert_ne!(
            request("T", &[("", "1"), ("", "2")]).key(),
            request("T", &[("", "2"), ("", "1")]).key()
        );
    }

    #[test]
    fn different_titles_differ() {
        assert_ne!(request("T", &[]).key(), request("U", &[]).key());
    }

    #[test]
    fn render_answer_skips_an_entitys_decoded_text() {
        // `&#59;` is an `mw:Entity` span (src `&#59;`), the decoded `;`, and an
        // end tag. The `src` already spells the entity, so the decoded text must
        // be skipped or the answer carries `&#59;;` (which reparses to `;;`).
        use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, Item, ParsoidToken, TagTk};
        let dp = DataParsoid {
            src: Some("&#59;".to_string()),
            src_content: Some(";".to_string()),
            ..Default::default()
        };
        let mut span = TagTk::new("span", vec![], dp);
        span.add_attribute_str("typeof", "mw:Entity");
        let items = vec![
            Item::Tok(ParsoidToken::Tag(span)),
            Item::Str(";".to_string()),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "span",
                vec![],
                DataParsoid::default(),
            ))),
        ];
        assert_eq!(render_answer(&items), "&#59;");
    }

    #[test]
    fn render_answer_does_not_double_a_newline() {
        // A `Nl` token carries the source newline in `src`; emitting the bare
        // `\n` as well turned a source `...>\n<div...` into `...>\n\n<div...`
        // in every module answer that held one (the taxobox's timeline row).
        use crate::wikitext::tokens_v2::{Item, NlTk, ParsoidToken, SourceRange};
        let mut nl = NlTk::new(SourceRange::new(0, 1));
        nl.data_parsoid.src = Some("\n".to_string());
        let with_src = vec![Item::Tok(ParsoidToken::Nl(nl))];
        assert_eq!(render_answer(&with_src), "\n");

        // A `Nl` with no recorded source still renders as one newline.
        let bare = vec![Item::Tok(ParsoidToken::Nl(NlTk::new(SourceRange::new(
            0, 0,
        ))))];
        assert_eq!(render_answer(&bare), "\n");
    }

    #[test]
    fn preprocess_keys_by_text() {
        assert_ne!(
            FrameRequest::Preprocess("a".into()).key(),
            FrameRequest::Preprocess("b".into()).key()
        );
    }

    #[test]
    fn pending_key_round_trips() {
        let message = format!("{NOT_CACHED}{}", request("T", &[("", "x")]).key());
        assert_eq!(
            pending_key(&message),
            Some(request("T", &[("", "x")]).key().as_str())
        );
    }

    /// A `#tag:nowiki` name splits into the function and its first argument,
    /// which the manual documents; any other name is left alone.
    #[test]
    fn colon_names_split_into_a_function_and_an_argument() {
        assert_eq!(
            split_colon_name("#tag:nowiki"),
            ("#tag".to_string(), Some("nowiki".to_string()))
        );
        assert_eq!(
            split_colon_name("#tag"),
            ("#tag".to_string(), None),
            "a hash name without a colon is a plain function"
        );
        assert_eq!(
            split_colon_name("uc"),
            ("uc".to_string(), None),
            "a namespace-like name must not be split"
        );
        assert_eq!(
            split_colon_name("#tag:ref"),
            ("#tag".to_string(), Some("ref".to_string()))
        );
    }

    /// A live element rebuilt from its attributes must keep a value the expander
    /// left as `Tokens`: `Template:Plain list` writes `class="plainlist
    /// {{{class|}}}"`, so reading only `Str` dropped the class and the module's
    /// output came back `<div>` where the service has `<div class="plainlist ">`.
    #[test]
    fn element_html_keeps_an_expanded_attribute_value() {
        use crate::wikitext::tokens_v2::{DataParsoid, Item, KV, KeyValue, ParsoidToken, TagTk};
        let kv = |k: &str, v: KeyValue| KV {
            key: KeyValue::Str(k.into()),
            value: v,
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        let tag = TagTk::new(
            "div",
            vec![
                // Parsoid bookkeeping, not HTML: must be left out.
                kv("about", KeyValue::Str("#mwt1".into())),
                kv(
                    "class",
                    KeyValue::Tokens(vec![Item::Str("plainlist ".into())]),
                ),
            ],
            DataParsoid::default(),
        );
        assert_eq!(
            element_html_from_attribs(&ParsoidToken::Tag(tag)).as_deref(),
            Some(r#"<div class="plainlist ">"#)
        );
    }
}
