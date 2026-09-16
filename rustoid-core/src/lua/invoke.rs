//! `#invoke` — running a Scribunto module from wikitext.
//!
//! MediaWiki's `{{#invoke:Module|func|args…}}` hands the rest of the call to
//! Lua. Parsoid implements none of this in standalone mode, which is why the
//! fixture suite cannot measure it; a live wiki runs the real thing, so online
//! parity needs it.
//!
//! # Why modules are fetched before the module runs
//!
//! Scribunto's `require` is a plain synchronous Lua call, but fetching a module
//! is asynchronous here. Rather than make the whole Lua integration async, the
//! modules a call can reach are **preloaded** before execution: the parser
//! fetches the entry module, scans its source for `require`/`mw.loadData`
//! literals, fetches those, and repeats. `require` inside Lua is then a
//! registry lookup in [`crate::lua::engine`].
//!
//! The obvious limits of scanning, stated plainly: a module that computes a
//! module name at runtime (`require('Module:' .. name)`) cannot be anticipated,
//! and its `require` fails with a clear message rather than returning nil. That
//! is a deliberate trade against rewriting the engine for async; the scan
//! covers the form virtually all real modules use.

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::error::{Result, RustoidError};
use crate::lua::engine::{
    Arg, FrameContext, LuaContext, LuaEngine, LuaEngineConfig, LuaSite, TitleFacts,
};
use crate::traits::DataSource;

/// One `{{#invoke:…}}` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoke {
    /// Module name without the `Module:` prefix, e.g. `Weather box`.
    pub module: String,
    /// Entry point, e.g. `main`.
    pub function: String,
    /// Arguments in call order; `None` name means positional.
    pub args: Vec<(Option<String>, String)>,
}

impl Invoke {
    /// Parse the text after `#invoke:` — `Module|func|arg|name=value`.
    ///
    /// An omitted or *empty* function name resolves to `main`, which is
    /// Scribunto's documented default. The empty form is not hypothetical:
    /// `Template:Citation needed` calls `{{#invoke:Unsubst||date=…}}`, and that
    /// template is on hundreds of thousands of pages, so the empty function has
    /// to work. `Module:Unsubst` defines only `p.main`, which is the evidence
    /// for the default.
    ///
    /// Returns `None` only when there is no module to invoke at all.
    pub fn parse(pf_arg: &str) -> Option<Self> {
        let mut parts = pf_arg.split('|');
        let module = parts.next()?.trim();
        if module.is_empty() {
            return None;
        }
        let function = match parts.next() {
            Some(f) if !f.trim().is_empty() => f.trim().to_string(),
            _ => "main".to_string(),
        };

        let mut args = Vec::new();
        for part in parts {
            match part.split_once('=') {
                // A leading `=` is part of a positional value, not a name
                // separator, and a name may not be empty.
                Some((name, value)) if !name.trim().is_empty() => {
                    args.push((Some(name.trim().to_string()), value.to_string()));
                }
                _ => args.push((None, part.to_string())),
            }
        }

        Some(Self {
            module: module.to_string(),
            function,
            args,
        })
    }

    /// Full title of the module page.
    pub fn module_title(&self) -> String {
        format!("Module:{}", self.module.replace('_', " "))
    }

    /// The frame arguments, in Scribunto's shape.
    pub fn frame_args(&self) -> Vec<Arg> {
        self.args
            .iter()
            .map(|(name, value)| match name {
                Some(n) => Arg::Named(n.clone(), value.clone()),
                None => Arg::Positional(value.clone()),
            })
            .collect()
    }
}

/// Module sources fetched ahead of execution, keyed by full title.
pub type Registry = HashMap<String, String>;

/// How many modules one `#invoke` may pull in.
///
/// A module graph is small; a runaway chain is a bug or a hostile page, and
/// either way it should stop rather than fetch forever. This is a guard on
/// *breadth*, matching the depth guard templates have.
const MAX_MODULES: usize = 200;

/// Fetch the entry module and everything it can reach by `require`/`loadData`.
///
/// Returns whatever could be fetched. A module that does not exist is simply
/// absent, so the eventual `require` reports it by name.
pub async fn preload<S: DataSource + ?Sized>(source: &S, entry: &str) -> Registry {
    let mut registry = Registry::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    queue.push_back(entry.to_string());
    seen.insert(entry.to_string());

    while let Some(title) = queue.pop_front() {
        if registry.len() >= MAX_MODULES {
            break;
        }
        let Some(body) = fetch_module(source, &title).await else {
            continue;
        };
        for needed in required_modules(&body) {
            if seen.insert(needed.clone()) {
                queue.push_back(needed);
            }
        }
        registry.insert(title, body);
    }

    registry
}

/// Fetch one module page, tolerating a data-source failure as "absent".
///
/// A missing module makes `#invoke` fail with a script error, which is both what
/// MediaWiki shows and what the comparison should see; propagating the transport
/// error would abort the whole page instead.
async fn fetch_module<S: DataSource + ?Sized>(source: &S, title: &str) -> Option<String> {
    let parsed = crate::title::Title::new_main(title.to_string());
    source.get_module(&parsed).await.ok().flatten()
}

/// Module titles named by `require('…')` / `mw.loadData('…')` literals.
///
/// Only string literals immediately after the call are collected; a computed
/// argument is skipped (see the module docs).
fn required_modules(source: &str) -> Vec<String> {
    let mut out = Vec::new();

    // Track which keyword matched. Searching for the other one inside the tail
    // would find a *later* call and mis-read its argument.
    let mut hits: Vec<(usize, &str)> = source
        .match_indices("require")
        .chain(source.match_indices("loadData"))
        .collect();
    hits.sort_unstable();

    for (idx, keyword) in hits {
        // The identifier must stand alone: `prerequire` is not `require`.
        if let Some(prev) = source[..idx].chars().next_back()
            && (prev.is_alphanumeric() || prev == '_')
        {
            continue;
        }
        let after_name = idx + keyword.len();
        let tail = source[after_name..].trim_start();
        let tail = tail.strip_prefix('(').unwrap_or(tail);
        let tail = tail.trim_start();
        let Some(quote) = tail.chars().next() else {
            continue;
        };
        if quote != '\'' && quote != '"' {
            continue;
        }
        let Some(end) = tail[1..].find(quote) else {
            continue;
        };
        let title = &tail[1..1 + end];
        // Only module-space titles are loadable; `require('foo')` refers to a
        // Lua core library, which the sandbox does not expose.
        if title.len() > 7 && title[..7].eq_ignore_ascii_case("Module:") {
            let name = title[7..].replace('_', " ");
            out.push(format!("Module:{name}"));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Run an `#invoke` call, returning its wikitext output.
///
/// The output is wikitext, not HTML: Scribunto's result is fed back through the
/// parser, so templates and parser functions the module returns are still
/// expanded by the caller.
pub fn run(
    invoke: &Invoke,
    registry: Registry,
    site: LuaSite,
    page_title: &str,
    frame: &FrameContext,
) -> Result<String> {
    // Scribunto's entry module must be present; a `#invoke` of a non-existent
    // module is an error, and the caller turns it into MediaWiki's message.
    let title = invoke.module_title();
    let Some(entry) = registry.get(&title).cloned() else {
        return Err(RustoidError::Lua(format!("module {title} does not exist")));
    };

    let mut frame = frame.clone();
    frame.modules = registry;
    let ctx = LuaContext::with_parent(site, page_title.to_string(), frame);
    let engine = LuaEngine::new(LuaEngineConfig::default(), ctx)?;

    let args = invoke.frame_args();
    engine.execute_in(&entry, &title, &invoke.function, &args)
}

/// How many times an `#invoke` may be re-run after discovering a module it
/// needed but could not be anticipated.
///
/// A module graph is small and each round adds at least one module, so a handful
/// of rounds is plenty; the bound exists so a module that asks for something
/// impossible cannot loop.
const MAX_PRELOAD_ROUNDS: usize = 6;

/// Parse `module X was not preloaded` into `X`.
///
/// The engine's message is the only place this knowledge lives, so the coupling
/// is deliberate and asserted by a test rather than left implicit.
fn missing_module_from(message: &str) -> Option<String> {
    let start = message.find("module ")? + "module ".len();
    let rest = &message[start..];
    let end = rest.find(" was not preloaded")?;
    let title = rest[..end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// How many pages one `#invoke` may look up.
///
/// A module that asks about hundreds of pages is either doing something this
/// scan cannot follow or is a bulk-tagging tool; either way the cost is real
/// network traffic, so it is bounded like the module preload is.
const MAX_TITLES: usize = 60;

/// Fetch the page facts modules can observe but cannot compute.
///
/// `existence`, `redirect` and `content` cannot be answered from inside
/// synchronous Lua, so the titles a module is likely to ask about are gathered
/// beforehand — the same trade the module preload makes, and for the same
/// reason. Titles are discovered by scanning the preloaded sources for
/// `mw.title.new('…')` string literals.
///
/// What this cannot see: a title built at runtime (`mw.title.new(prefix .. name)`).
/// Those report as non-existent, which is the conservative answer, and the gap is
/// recorded in ONLINE-PARITY.md.
pub async fn preload_titles<S: DataSource + ?Sized>(
    source: &S,
    registry: &Registry,
) -> HashMap<String, TitleFacts> {
    let mut out = HashMap::new();
    let mut wanted: BTreeSet<String> = BTreeSet::new();

    for body in registry.values() {
        for title in referenced_titles(body) {
            wanted.insert(title);
        }
    }

    for title in wanted.into_iter().take(MAX_TITLES) {
        let parsed = crate::title::Title::new_main(title.clone());
        let content = source.get_page_content(&parsed).await.ok().flatten();
        let exists = content.is_some();
        // A redirect is a page whose content is `#REDIRECT [[…]]`; fetching it
        // again to find out would double the traffic for a value most modules do
        // not read.
        let is_redirect = content
            .as_deref()
            .is_some_and(|c| c.trim_start().to_uppercase().starts_with("#REDIRECT"));
        out.insert(
            title,
            TitleFacts {
                exists,
                is_redirect,
                content,
            },
        );
    }

    out
}

/// Page titles named by `mw.title.new('…')` string literals.
///
/// A second argument (a namespace id or name) is appended as a prefix, since
/// `mw.title.new('Foo', 'Template')` refers to `Template:Foo`.
fn referenced_titles(source: &str) -> Vec<String> {
    const CALL: &str = "mw.title.new";
    let mut out = Vec::new();

    for (idx, _) in source.match_indices(CALL) {
        let tail = &source[idx + CALL.len()..];
        let tail = tail.trim_start();
        let Some(tail) = tail.strip_prefix('(') else {
            continue;
        };
        let tail = tail.trim_start();
        let Some(quote) = tail.chars().next() else {
            continue;
        };
        if quote != '\'' && quote != '"' {
            continue;
        }
        let Some(end) = tail[1..].find(quote) else {
            continue;
        };
        let literal = tail[1..1 + end].trim();
        if literal.is_empty() || literal.contains("..") {
            continue;
        }

        // An explicit namespace argument, e.g. `mw.title.new('Foo', 'Template')`
        // or `mw.title.new('Foo', 10)`.
        let after = tail[1 + end + 1..].trim_start();
        let after = after.strip_prefix(',').map(str::trim_start);
        let title = match after {
            Some(rest) if rest.starts_with(['\'', '"']) => {
                let q = rest.chars().next().unwrap_or('\'');
                match rest[1..].find(q) {
                    Some(e) => {
                        let ns = rest[1..1 + e].trim();
                        if ns.is_empty() || literal.contains(':') {
                            literal.to_string()
                        } else {
                            format!("{ns}:{literal}")
                        }
                    }
                    None => literal.to_string(),
                }
            }
            _ => literal.to_string(),
        };
        out.push(title.replace('_', " "));
    }

    out.sort();
    out.dedup();
    out
}

/// Parse a `#invoke` call, preload what it needs, and run it.
///
/// Runs more than once only when a module asks for something the scan could not
/// see — a name built at runtime, e.g. `mw.loadData(cfgModule)`. When that
/// happens the named module is fetched and the call re-runs, which keeps
/// `require` synchronous inside Lua while still covering the dynamic case.
/// Without this, `Module:Message box/configuration` (reached that way) stopped
/// 36 of 39 corpus pages.
pub async fn invoke<S: DataSource + ?Sized>(
    pf_arg: &str,
    source: &S,
    site: LuaSite,
    page_title: &str,
    // `frame.titles` is filled in here, since only this function knows which
    // modules were preloaded.
    frame: FrameContext,
) -> Result<String> {
    let Some(call) = Invoke::parse(pf_arg) else {
        return Err(RustoidError::Lua(format!("malformed #invoke: {pf_arg:?}")));
    };
    let mut registry = preload(source, &call.module_title()).await;
    let mut frame = FrameContext {
        titles: preload_titles(source, &registry).await,
        ..frame
    };
    let mut last_missing: Option<String> = None;

    for _ in 0..MAX_PRELOAD_ROUNDS {
        match run(&call, registry.clone(), site.clone(), page_title, &frame) {
            Ok(out) => return Ok(out),
            Err(RustoidError::Lua(msg)) => {
                match missing_module_from(&msg) {
                    Some(title) if !registry.contains_key(&title) => {
                        // Fetch it, plus anything *it* needs, then try again.
                        registry.extend(preload(source, &title).await);
                        // A newly loaded module may reference new titles.
                        frame.titles.extend(preload_titles(source, &registry).await);
                        last_missing = Some(title);
                    }
                    _ => return Err(RustoidError::Lua(msg)),
                }
            }
            Err(other) => return Err(other),
        }
    }

    // Name the module that stayed missing: "gave up" alone leaves the reader to
    // guess, and the usual reason is that it could not be fetched rather than
    // that the chain was long.
    Err(RustoidError::Lua(format!(
        "could not load {} after {MAX_PRELOAD_ROUNDS} rounds (still missing: {})",
        call.module_title(),
        last_missing.as_deref().unwrap_or("unknown")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_titles_in_new_literals() {
        let src = r#"
            local a = mw.title.new('Template:Wrapper')
            local b = mw.title.new("Foo_bar")
            local c = mw.title.new('Baz', 'Template')
            local d = mw.title.new(prefix .. name)
        "#;
        assert_eq!(
            referenced_titles(src),
            vec![
                // Sorted, which is why `Foo bar` precedes `Template:*`; the
                // explicit namespace argument became a prefix.
                "Foo bar".to_string(),
                "Template:Baz".to_string(),
                "Template:Wrapper".to_string(),
            ]
        );
    }

    #[test]
    fn parses_a_bare_call() {
        let i = Invoke::parse("Weather box|main").unwrap();
        assert_eq!(i.module, "Weather box");
        assert_eq!(i.function, "main");
        assert!(i.args.is_empty());
        assert_eq!(i.module_title(), "Module:Weather box");
    }

    #[test]
    fn parses_positional_and_named_args_in_order() {
        let i = Invoke::parse("Math|sum|1|2|round=2").unwrap();
        assert_eq!(
            i.args,
            vec![
                (None, "1".to_string()),
                (None, "2".to_string()),
                (Some("round".to_string()), "2".to_string()),
            ]
        );
    }

    /// `=` at the start of a value is not a name separator: MediaWiki treats an
    /// empty name as positional, not as a named argument called "".
    #[test]
    fn a_value_starting_with_equals_stays_positional() {
        let i = Invoke::parse("M|f|=x").unwrap();
        assert_eq!(i.args, vec![(None, "=x".to_string())]);
    }

    #[test]
    fn a_malformed_call_is_rejected() {
        // No module at all: nothing to invoke.
        assert!(Invoke::parse("").is_none());
        assert!(Invoke::parse("|main").is_none());
        assert!(Invoke::parse("  ").is_none());
    }

    /// An omitted or empty function name means `main`.
    ///
    /// `Template:Citation needed` calls `{{#invoke:Unsubst||date=…}}` and is on
    /// hundreds of thousands of pages, so this form has to expand.
    #[test]
    fn an_omitted_or_empty_function_defaults_to_main() {
        let bare = Invoke::parse("Weather box").unwrap();
        assert_eq!(bare.function, "main");

        let empty = Invoke::parse("Unsubst||date=2024").unwrap();
        assert_eq!(empty.module, "Unsubst");
        assert_eq!(empty.function, "main");
        assert_eq!(
            empty.args,
            vec![(Some("date".to_string()), "2024".to_string())]
        );
    }

    #[test]
    fn positional_args_keep_their_order_and_names_attach() {
        let i = Invoke::parse("M|f|a|b|c=d").unwrap();
        let args = i.frame_args();
        assert_eq!(args[0], Arg::Positional("a".to_string()));
        assert_eq!(args[1], Arg::Positional("b".to_string()));
        assert_eq!(args[2], Arg::Named("c".to_string(), "d".to_string()));
    }

    #[test]
    fn finds_required_and_loaded_modules() {
        let src = r#"
            local x = require('Module:Yesno')
            local y = require( "Module:String" )
            local d = mw.loadData('Module:Weather box/data')
            local core = require('table')
        "#;
        assert_eq!(
            required_modules(src),
            vec![
                "Module:String".to_string(),
                "Module:Weather box/data".to_string(),
                "Module:Yesno".to_string(),
            ]
        );
    }

    /// A computed module name cannot be preloaded, and must not be mistaken for
    /// a literal — a wrong title would fetch the wrong module.
    #[test]
    fn ignores_computed_and_non_module_requires() {
        let src = "require('Module:' .. name) require('os') local p = prequired";
        assert!(required_modules(src).is_empty());
    }

    /// Underscores in the literal name normalise to spaces, as titles do.
    #[test]
    fn normalises_underscored_module_names() {
        let src = "require('Module:Weather_box/data')";
        assert_eq!(
            required_modules(src),
            vec!["Module:Weather box/data".to_string()]
        );
    }

    /// The retry loop reads the module name back out of the engine's message, so
    /// the two must agree. This test is the contract between them.
    #[test]
    fn extracts_the_missing_module_name_from_an_error() {
        assert_eq!(
            missing_module_from("module Module:Message box/configuration was not preloaded"),
            Some("Module:Message box/configuration".to_string())
        );
        assert_eq!(missing_module_from("some other failure"), None);
        assert_eq!(missing_module_from("module  was not preloaded"), None);
    }
}
