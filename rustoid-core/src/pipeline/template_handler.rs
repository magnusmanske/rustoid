//! TemplateHandler (target resolution) — port of the target classification
//! from PHP Parsoid's `src/Wt2Html/TT/TemplateHandler.php`.
//!
//! `resolve_template_target` classifies a `{{target}}` first attribute into:
//! - a magic variable (e.g. `{{PAGENAME}}`),
//! - a parser function (e.g. `{{#if:...}}`),
//! - or a template title (e.g. `{{Foo}}` / `{{Template:Foo}}`).
//!
//! This module covers the pure classification logic; native template
//! expansion, the preprocessor, and argument handling are layered in once the
//! frame/preprocessor/data-access infrastructure is available.

use crate::title::{Title, TitleParser};
use crate::traits::SiteConfig;
use crate::wikitext::tokens_v2::Item;

/// The result of classifying a template target. Mirrors PHP's return array
/// from `resolveTemplateTarget`.
#[derive(Debug, Clone)]
pub enum ResolvedTarget {
    /// A magic variable (e.g. `{{PAGENAME}}`).
    Variable {
        name: String,
        magic_word_type: Option<String>,
        title: Title,
        pf_arg: String,
        colon: String,
    },
    /// A parser function (e.g. `{{#if:...}}`).
    ParserFunction {
        name: String,
        local_name: String,
        title: Title,
        pf_arg: String,
        colon: String,
        broken: bool,
    },
    /// A template title.
    Template { name: String, title: Title },
}

/// Resolve a template target string. Mirrors `TemplateHandler::resolveTemplateTarget`
/// for the magic-variable, parser-function, and template-title classification
/// (target string form, no additional token arrays).
pub fn resolve_template_target(
    config: &dyn SiteConfig,
    target_toks: &str,
) -> Option<ResolvedTarget> {
    let mut target = target_toks.trim().to_string();

    // Split on ASCII ':' or fullwidth '：'.
    let mut pieces: Vec<String> = target.split([':', '：']).map(|s| s.to_string()).collect();
    if pieces.is_empty() {
        return None;
    }

    let mut prefix = pieces[0].trim().to_string();
    let has_hash = target.starts_with('#') || target.starts_with('＃');
    let mut have_colon = pieces.len() > 1;

    // safesubst found in content should be treated as if no modifier were
    // present (see Help:Substitution).
    if have_colon && is_safe_subst(&prefix) {
        let cut = pieces[0].len() + 1;
        target = target[cut..].to_string();
        pieces = target.split([':', '：']).map(|s| s.to_string()).collect();
        if pieces.is_empty() {
            return None;
        }
        prefix = pieces[0].trim().to_string();
        have_colon = pieces.len() > 1;
    }

    let untrimmed_prefix_len = pieces[0].len();
    let pf_arg = if have_colon {
        target[untrimmed_prefix_len + 1..].to_string()
    } else {
        String::new()
    };
    let colon = if have_colon {
        target[untrimmed_prefix_len..untrimmed_prefix_len + 1].to_string()
    } else {
        String::new()
    };

    // Check for a magic variable (in the site's magic-word map).
    if let Some((canonical, true)) = magic_word_for_variable(config, &prefix) {
        let title = TitleParser::parse(&format!("Special:Variable/{canonical}"), config);
        return Some(ResolvedTarget::Variable {
            name: canonical.clone(),
            magic_word_type: if canonical == "!" {
                Some("!".to_string())
            } else {
                None
            },
            title,
            pf_arg,
            colon,
        });
    }

    // Check for a parser function (starts with '#').
    if has_hash {
        let canonical = prefix.trim_start_matches(['#', '＃']);
        if !canonical.is_empty() {
            let title = TitleParser::parse(&format!("Special:ParserFunction/{canonical}"), config);
            return Some(ResolvedTarget::ParserFunction {
                name: canonical.to_string(),
                local_name: prefix.clone(),
                title,
                pf_arg,
                colon,
                broken: true,
            });
        }
    }

    // Resolve as a template title.
    let namespace_id = if target.starts_with([':', '#', '/']) || target.starts_with("../") {
        None
    } else {
        config.canonical_namespace_id("Template")
    };

    // Parse the target; if a Template namespace default applies, force it.
    let parsed = TitleParser::parse(&target, config);
    let title = if let Some(ns) = namespace_id {
        Title::new(ns, parsed.text)
    } else {
        parsed
    };

    Some(ResolvedTarget::Template {
        name: title.get_full_db_key(),
        title,
    })
}

/// Is `name` the `safesubst` magic word? Mirrors the essential safesubst check.
fn is_safe_subst(name: &str) -> bool {
    name == "safesubst"
}

/// Find a magic variable whose alias matches `name`. Mirrors
/// `SiteConfig::getMagicWordForVariable`.
/// Returns (canonical name, whether it's a variable). Variables are magic
/// words without a `#` and without an `=`-style value.
fn magic_word_for_variable(config: &dyn SiteConfig, name: &str) -> Option<(String, bool)> {
    let lower = name.to_lowercase();
    for (canonical, entry) in config.magic_words() {
        // Skip media/parser-function magic words (img_*, timedmedia_*).
        if canonical.starts_with("img_") || canonical.starts_with("timedmedia_") {
            continue;
        }
        if entry.aliases.iter().any(|a| a.to_lowercase() == lower) {
            return Some((canonical.clone(), true));
        }
    }
    None
}

/// Convert a template target to a wikilink (for the redlink path). Mirrors the
/// fallback in `expandTemplateNatively` when the template isn't found.
pub fn template_to_wikilink(name: &str) -> Item {
    let mut tk = crate::wikitext::tokens_v2::SelfclosingTagTk::new(
        "wikilink",
        vec![],
        crate::wikitext::tokens_v2::DataParsoid::default(),
    );
    let href_src = format!(":{}", name.replace('_', " "));
    tk.attribs.push(crate::wikitext::tokens_v2::KV {
        key: crate::wikitext::tokens_v2::KeyValue::Str("href".to_string()),
        value: crate::wikitext::tokens_v2::KeyValue::Str(href_src),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });
    Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(tk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    #[test]
    fn test_resolve_magic_variable() {
        let config = MockSiteConfig::new();
        let target = resolve_template_target(&config, "PAGENAME").unwrap();
        match target {
            ResolvedTarget::Variable { name, .. } => {
                assert_eq!(name, "pagename");
            }
            other => panic!("expected variable, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_parser_function() {
        let config = MockSiteConfig::new();
        let target = resolve_template_target(&config, "#if:a|b|c").unwrap();
        match target {
            ResolvedTarget::ParserFunction { name, broken, .. } => {
                assert_eq!(name, "if");
                assert!(broken);
            }
            other => panic!("expected parser function, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_template_title() {
        let config = MockSiteConfig::new();
        // Plain template name defaults to the Template namespace.
        let target = resolve_template_target(&config, "Foo").unwrap();
        match target {
            ResolvedTarget::Template { name, title } => {
                assert_eq!(name, "Template:Foo");
                assert_eq!(title.namespace_id, 10);
                assert_eq!(title.text, "Foo");
            }
            other => panic!("expected template, got {:?}", other),
        }
    }
}
