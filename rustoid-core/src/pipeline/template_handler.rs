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

use super::parser_functions::{Params, ParserFunctions};
use super::template_encapsulator::{TemplateEncapsulator, template_info_from};

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

/// The TemplateHandler — ties together target resolution, parser-function
/// evaluation, and template encapsulation. Mirrors the flow of
/// `TemplateHandler::expandTemplate` for magic-variable and parser-function
/// targets (native template fetching is deferred).
pub struct TemplateHandler;

impl TemplateHandler {
    /// Handle a `{{target|...}}` template token, returning the expanded and
    /// encapsulated token chunk. `params` is the token's attribute list
    /// (the first entry is the target). Mirrors `onTemplate` for the
    /// non-template-fetch cases.
    pub fn handle_template(
        &self,
        config: &dyn SiteConfig,
        params: &Params,
        about_id: String,
        token: &crate::wikitext::tokens_v2::ParsoidToken,
    ) -> Vec<Item> {
        // Extract the target (first arg key).
        let target_str = params
            .args
            .first()
            .map(|kv| {
                crate::wikitext::token_utils::tokens_to_string(&[Item::Str(match &kv.key {
                    crate::wikitext::tokens_v2::KeyValue::Str(s) => s.clone(),
                    crate::wikitext::tokens_v2::KeyValue::Tokens(t) => {
                        crate::wikitext::token_utils::tokens_to_string(
                            &t.iter().cloned().map(Item::Tok).collect::<Vec<_>>(),
                        )
                    }
                })])
            })
            .unwrap_or_default();

        match resolve_template_target(config, &target_str) {
            Some(ResolvedTarget::Variable { name, .. }) => {
                let value = Self::variable_value(config, &name);
                let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
                let info = template_info_from(Some(&name), None, vec![]);
                encap.encap_tokens(vec![Item::Str(value)], &info)
            }
            Some(ResolvedTarget::ParserFunction {
                name,
                pf_arg,
                colon,
                ..
            }) => {
                // Rebuild params: args[0].k is the first argument (from the
                // colon syntax); remaining params.args[1..] are positional/named.
                let mut pf_params = params.clone();
                pf_params.args[0] = crate::wikitext::tokens_v2::KV {
                    key: crate::wikitext::tokens_v2::KeyValue::Str(pf_arg),
                    value: crate::wikitext::tokens_v2::KeyValue::Str(String::new()),
                    src_offsets: None,
                    ksrc: None,
                    vsrc: None,
                };
                let result = Self::call_parser_function(&name, &pf_params);
                let mut encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
                if !colon.is_empty() {
                    encap.set_colon(Some(colon));
                }
                let info = template_info_from(Some(&name), None, vec![]);
                encap.encap_tokens(result, &info)
            }
            Some(ResolvedTarget::Template { name, .. }) => {
                // Native template fetching is deferred; emit a redlink wikilink.
                let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
                let info = template_info_from(None, Some(&name), vec![]);
                encap.encap_tokens(vec![template_to_wikilink(&name)], &info)
            }
            None => vec![Item::Str(target_str)],
        }
    }

    /// Resolve a magic variable to its string value. Mirrors the common
    /// variable cases (full page-name variables require the page context,
    /// which isn't yet wired).
    fn variable_value(config: &dyn SiteConfig, name: &str) -> String {
        match name {
            "sitename" => "MediaWiki".to_string(),
            "server" => config.server_url().to_string(),
            "servername" => config
                .server_url()
                .strip_prefix("http://")
                .or_else(|| config.server_url().strip_prefix("https://"))
                .unwrap_or(config.server_url())
                .to_string(),
            "contentlanguage" | "contentlang" => config.language_code().to_string(),
            "scriptpath" => config.script_path().to_string(),
            // Any other variable: empty for now (requires page context).
            _ => String::new(),
        }
    }

    /// Dispatch a parser function name to the `ParserFunctions` implementation.
    fn call_parser_function(name: &str, params: &Params) -> Vec<Item> {
        match name {
            "if" => ParserFunctions::pf_if(params),
            "ifeq" => ParserFunctions::pf_ifeq(params),
            "switch" => ParserFunctions::pf_switch(params),
            "expr" => ParserFunctions::pf_expr(params),
            "ifexpr" => ParserFunctions::pf_ifexpr(params),
            "iferror" => ParserFunctions::pf_iferror(params),
            "lc" => ParserFunctions::pf_lc(params),
            "uc" => ParserFunctions::pf_uc(params),
            "ucfirst" => ParserFunctions::pf_ucfirst(params),
            "lcfirst" => ParserFunctions::pf_lcfirst(params),
            "padleft" => ParserFunctions::pf_padleft(params),
            "padright" => ParserFunctions::pf_padright(params),
            "tag" => ParserFunctions::pf_tag(params),
            "urlencode" => ParserFunctions::pf_urlencode(params),
            // Unknown parser function: return its name in braces.
            _ => vec![Item::Str(format!("{{{{{{#{name}|...}}}}}}"))],
        }
    }
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

    #[test]
    fn test_handle_parser_function() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        let config = MockSiteConfig::new();
        let handler = TemplateHandler;

        // {{#if:x|yes|no}}: args[0].k is the full target before the first '|'.
        let args = vec![
            KV {
                key: KeyValue::Str("#if:x".to_string()),
                value: KeyValue::Str("".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str("".to_string()),
                value: KeyValue::Str("yes".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str("".to_string()),
                value: KeyValue::Str("no".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ];
        let params = Params::new(args);

        let token =
            crate::wikitext::tokens_v2::ParsoidToken::Tag(crate::wikitext::tokens_v2::TagTk::new(
                "template",
                vec![],
                crate::wikitext::tokens_v2::DataParsoid::default(),
            ));

        let out = handler.handle_template(&config, &params, "#mwt1".to_string(), &token);

        // Should be wrapped with mw:Transclusion markers and contain "yes".
        assert!(
            matches!(&out[0], Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(t)) if t.name == "meta")
        );
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "yes"))
        );
    }
}
