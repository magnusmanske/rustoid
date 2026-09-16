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
    ExpandTemplate { title: String, args: Vec<FrameArg> },
    Preprocess(String),
    CallParserFunction { name: String, args: Vec<FrameArg> },
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
            Self::Preprocess(_) => &[],
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
            let Value::Table(opts) = arg else {
                return Err(mlua::Error::runtime(
                    "callParserFunction expects a table with a name",
                ));
            };
            let name = opts
                .get::<Option<String>>("name")?
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                return Err(mlua::Error::runtime(
                    "callParserFunction expects a function name",
                ));
            }
            let args = match opts.get::<Value>("args")? {
                Value::Table(t) => args_from_table(&t)?,
                _ => Vec::new(),
            };
            Ok(FrameRequest::CallParserFunction { name, args })
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
            let mut out = format!("{{{{#{name}");
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
    }
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

/// Render an expansion to the text a module receives.
///
/// These frame methods return strings — the manual says so of each — and that
/// string is the *rendered* expansion, i.e. HTML. Scribunto's `mw.lua` binds
/// them to `php.expandTemplate`/`php.preprocess`, whose PHP side runs the result
/// through `recursiveTagParse`, which yields HTML; the module then concatenates
/// it as text. `'''bold'''` therefore comes back as `<b>bold</b>`.
///
/// The markup is produced by running the expansion's tokens through the same
/// tree builder and serializer a page uses, so a module gets what the page would
/// have shown. A `mw:Transclusion` marker is Parsoid bookkeeping rather than
/// output and is dropped.
pub fn render_answer(items: &[Item], config: &dyn crate::traits::SiteConfig) -> Result<String> {
    let kept: Vec<Item> = items
        .iter()
        .filter(|item| match item {
            // Both the opening marker and its `mw:Transclusion/End` partner are
            // bookkeeping, not output.
            Item::Tok(tok) => !is_transclusion_marker(tok),
            Item::Str(_) => true,
        })
        .cloned()
        .collect();
    let stage = crate::pipeline::tree_builder_stage::TreeBuilderStage::new(false);
    let ast = stage.to_ast_with_fragments(kept, None, config, std::collections::HashMap::new());
    let serializer =
        crate::html::serialize::HtmlSerializer::new(crate::options::ParserOptions::for_page(""));
    let html = serializer
        .serialize(&ast)
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    Ok(inner_html(&html))
}

/// Whether a token is a `mw:Transclusion` marker or its `/End` partner.
fn is_transclusion_marker(token: &crate::wikitext::tokens_v2::ParsoidToken) -> bool {
    token
        .get_attribute_v("typeof")
        .is_some_and(|ty| ty.starts_with("mw:Transclusion"))
}

/// The body content of a serialized document.
///
/// The serializer emits a whole document (`<!DOCTYPE html>…<body>…`); a module
/// wants just the fragment, since its output is embedded in a page rather than
/// being one.
fn inner_html(html: &str) -> String {
    let Some(start) = html.find("<body>").map(|i| i + "<body>".len()) else {
        return html.to_string();
    };
    let end = html.rfind("</body>").unwrap_or(html.len());
    html[start..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
