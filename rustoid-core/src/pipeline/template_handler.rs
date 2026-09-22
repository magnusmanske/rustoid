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

use crate::expand::transclusion;
use crate::title::{Title, TitleParser};
use crate::traits::{DataSource, SiteConfig};
use crate::wikitext::token_utils::{is_entity_span_token, key_value_to_string, match_type_of};
use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};
use crate::wikitext::tokens_v2::{Item, ParsoidToken};
use std::collections::HashMap;

use super::parser_functions::{Params, ParserFunctions};
use super::template_encapsulator::{
    TemplateEncapsulator, TemplateInfo, strip_include_directives, template_info_from,
};

/// Is this token an annotation meta tag? Mirrors PHP
/// `WTUtils::ANNOTATION_META_TYPE_REGEXP`.
fn is_annotation_meta(token: &ParsoidToken) -> bool {
    matches!(token, ParsoidToken::SelfclosingTag(t) if t.name == "meta")
        && match_type_of(token, "#^mw:Annotation/\\w+(/End)?$#").is_some()
}

/// Item-flavoured variant of `is_annotation_meta`.
fn is_annotation_meta_item(item: &Item) -> bool {
    match item {
        Item::Tok(t) => is_annotation_meta(t),
        Item::Str(_) => false,
    }
}

/// Item-flavoured variant of `is_includes_meta`.
fn is_includes_meta_item(item: &Item) -> bool {
    match item {
        Item::Tok(t) => is_includes_meta(t),
        Item::Str(_) => false,
    }
}

/// Is this token an include-directive meta (`mw:Includes/IncludeOnly` etc.)?
/// Mirrors the `#^mw:Includes/#` check in `processToString`.
fn is_includes_meta(token: &ParsoidToken) -> bool {
    matches!(token, ParsoidToken::SelfclosingTag(t) if t.name == "meta")
        && match_type_of(token, "#^mw:Includes/#").is_some()
}

/// Strip comments and process include/annotation preprocessor pieces.
/// Mirrors PHP's `TemplateHandler::processPreprocToString`.
///
/// `in_template` controls whether `<noinclude>`/`<includeonly>` contents are
/// dropped or kept.
pub fn process_preproc_to_string(tokens: &[Item], in_template: bool) -> Vec<Item> {
    let mut result = Vec::new();
    for token in tokens {
        let mut include_contents = false;
        let mut skip = false;

        let meta = match token {
            Item::Tok(t) => match t {
                ParsoidToken::SelfclosingTag(stt) if stt.name == "meta" => Some(stt),
                _ => None,
            },
            Item::Str(_) => None,
        };
        if let Some(stt) = meta
            && is_includes_meta_item(token)
            && let Some(kv) = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
        {
            let ty = kv.value.as_str().unwrap_or("");
            match ty {
                "mw:Includes/OnlyInclude" | "mw:Includes/OnlyInclude/End" => {
                    include_contents = true;
                }
                "mw:Includes/NoInclude" | "mw:Includes/NoInclude/End" => {
                    if in_template {
                        skip = true;
                    } else {
                        include_contents = true;
                    }
                }
                "mw:Includes/IncludeOnly" | "mw:Includes/IncludeOnly/End" => {
                    if in_template {
                        include_contents = true;
                    } else {
                        skip = true;
                    }
                }
                _ => {
                    // Annotation metas are ignored in template targets
                    // (T295834).
                    if is_annotation_meta_item(token) {
                        include_contents = true;
                    }
                }
            }
        }

        if include_contents {
            // Annotate/meta pieces recurse into their contents; we don't have
            // a nested contents field, so emit the token itself as a marker and
            // let processToString drop it via the annotation-meta check.
            result.push(token.clone());
        } else if !skip {
            result.push(token.clone());
        }
    }
    result
}

/// The result of `process_to_string`: either a fully-stringified target
/// (`rest` is `None`) or a partial string plus the unprocessed token tail.
#[derive(Debug, Clone, PartialEq)]
pub struct ToStringResult {
    pub target: String,
    pub rest: Option<Vec<Item>>,
}

/// Take output of `tokens_to_string` and further postprocess it.
/// Mirrors PHP's `TemplateHandler::processToString` (the string and
/// token-tail loop).
pub fn process_to_string(tokens: &[Item], in_template: bool) -> ToStringResult {
    let tokens = process_preproc_to_string(tokens, in_template);

    let mut buf = String::new();
    let mut pre_nl_content: Option<String> = None;

    // First pass: find the first token boundary (template/tag/etc.) where
    // stringification must stop; accumulate the string form of leading
    // strings and inline quotable/comment/nl tokens.
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        match token {
            Item::Str(s) => {
                buf.push_str(s);
                if pre_nl_content.is_some()
                    && !s.trim_matches(|c: char| c.is_whitespace()).is_empty()
                {
                    // Intervening non-ws after a newline means this is an
                    // invalid template target.
                    let tail = std::iter::once(Item::Str(buf.clone()))
                        .chain(tokens[i..].to_vec())
                        .collect();
                    return ToStringResult {
                        target: pre_nl_content.unwrap_or_default(),
                        rest: Some(tail),
                    };
                }
                i += 1;
            }
            Item::Tok(t) => match t {
                ParsoidToken::SelfclosingTag(stt) => {
                    if stt.name == "mw-quote" {
                        if let Some(v) = stt
                            .attribs
                            .iter()
                            .find(|kv| kv.key.as_str() == Some("value"))
                        {
                            buf.push_str(v.value.as_str().unwrap_or(""));
                        }
                        i += 1;
                    } else if !matches!(t, ParsoidToken::EmptyLine(_))
                        && stt.name != "template"
                        && stt.name != "templatearg"
                        && !is_annotation_meta(t)
                        && !is_includes_meta(t)
                    {
                        // We are okay with empty (comment-only) lines,
                        // {{..}} and {{{..}}} in template targets.
                        return ToStringResult {
                            target: pre_nl_content.unwrap_or(buf),
                            rest: Some(tokens[i..].to_vec()),
                        };
                    } else {
                        // EmptyLine, template, templatearg, annotation, or
                        // includes meta: ignored in a template target.
                        i += 1;
                    }
                }
                ParsoidToken::Tag(_tag_tk) => {
                    if is_entity_span_token(t) {
                        // Entity span: append the following token (its text).
                        if let Some(Item::Str(s)) = tokens.get(i + 1) {
                            buf.push_str(s);
                        }
                        i += 2;
                    } else {
                        return ToStringResult {
                            target: pre_nl_content.unwrap_or(buf),
                            rest: Some(tokens[i..].to_vec()),
                        };
                    }
                }
                ParsoidToken::EndTag(_) => {
                    return ToStringResult {
                        target: pre_nl_content.unwrap_or(buf),
                        rest: Some(tokens[i..].to_vec()),
                    };
                }
                ParsoidToken::Comment(_) => {
                    i += 1;
                }
                ParsoidToken::Nl(_) => {
                    if buf.trim_matches(|c: char| c.is_whitespace()).is_empty() {
                        buf.push('\n');
                        i += 1;
                    } else if pre_nl_content.is_none() {
                        pre_nl_content = Some(buf.clone());
                        buf = "\n".to_string();
                        i += 1;
                    } else {
                        let tail = std::iter::once(Item::Str(buf.clone()))
                            .chain(tokens[i..].to_vec())
                            .collect();
                        return ToStringResult {
                            target: pre_nl_content.unwrap_or_default(),
                            rest: Some(tail),
                        };
                    }
                }
                // All other token types cannot appear in a stringifiable target.
                _ => {
                    return ToStringResult {
                        target: pre_nl_content.unwrap_or(buf),
                        rest: Some(tokens[i..].to_vec()),
                    };
                }
            },
        }
    }

    // All good: no newline / only whitespace/comments post newline.
    ToStringResult {
        target: format!("{}{}", pre_nl_content.unwrap_or_default(), buf),
        rest: None,
    }
}

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

/// Classify a resolved (string) template target. Mirrors the tail of PHP's
/// `resolveTemplateTarget` once `$target` has been stringified.
fn resolve_target_string(
    config: &dyn SiteConfig,
    context_title: Option<&crate::title::Title>,
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

    // Check for a parser function. PHP's `getMagicWordForParserFunction` looks the
    // whole prefix (including any leading `#`) up in the function-synonym table,
    // whose keys carry a `#` unless the function is listed in `$noHashFunctions`.
    // Core registers several functions that way (`dir`, `ns`, `lc`, …), so
    // `{{#dir:en|bcp47}}` is a *valid* invocation, while an unknown `#…` is a
    // "broken" one that `TemplateHandler::resolveTemplateTarget` still treats as
    // a parser function so the `#` is not mistaken for part of a template name.
    //
    // The synonym table for a no-hash function holds the bare name, so
    // `{{anchorencode:x}}` (no `#`) is a parser function too — but only when a
    // colon follows (PHP nulls `$canonicalFunctionName` for a no-hash, no-colon
    // invocation, leaving `{{dir}}` a plain template reference).
    let known_hook = |name: &str| {
        config
            .function_hooks()
            .iter()
            .any(|h| h.eq_ignore_ascii_case(name))
    };
    if has_hash {
        let stripped = prefix.trim_start_matches(['#', '＃']);
        if !stripped.is_empty() {
            let is_known = known_hook(stripped);
            let name = if is_known {
                stripped.to_lowercase()
            } else {
                stripped.to_string()
            };
            let title = TitleParser::parse(&format!("Special:ParserFunction/{stripped}"), config);
            return Some(ResolvedTarget::ParserFunction {
                name,
                local_name: prefix.clone(),
                title,
                pf_arg,
                colon,
                broken: !is_known,
            });
        }
    } else if have_colon && known_hook(&prefix) {
        let title = TitleParser::parse(&format!("Special:ParserFunction/{prefix}"), config);
        return Some(ResolvedTarget::ParserFunction {
            name: prefix.to_lowercase(),
            local_name: prefix.clone(),
            title,
            pf_arg,
            colon,
            broken: false,
        });
    }

    // Resolve a possibly-relative link (/Subpage, ../foo) against the context
    // title before template processing, mirroring PHP's `$env->resolveTitle($target)`
    // (a template target in a subpage-supporting namespace picks up the
    // namespace/fragment prefix). This runs only *after* magic-variable and
    // parser-function detection, so `#tag:pre` and `{{PAGENAME}}` aren't
    // fragment-resolved. `orig_target` (pre-resolution) drives the
    // Template-namespace default decision, matching PHP's `$namespaceId`.
    let orig_target = target.clone();
    let target = crate::title::resolve_subpage(config, context_title, &target);

    // Resolve as a template title. The Template-namespace default is omitted
    // for a relative target (leading ':', '#', '/', or '../'), which resolves in
    // the current namespace instead (mirrors PHP's `$namespaceId = strspn($target,
    // ':#/') > 0 || str_starts_with($target, "../") ? null : template_ns`).
    let namespace_id = if orig_target.starts_with([':', '#', '/']) || orig_target.starts_with("../")
    {
        None
    } else {
        config.canonical_namespace_id("Template")
    };

    // PHP parses the title with `makeTitleFromURLDecodedStr($title, $namespaceId,
    // true)`, whose `$noExceptions` flag makes an invalid title (bad characters,
    // `%hh` sequences, char references, relative path components, `~~~`, an
    // over-long title, or an empty one) return `null`, bailing the whole
    // template to literal text ("Entities in transclusions aren't decoded in the
    // PHP parser"). Mirror that by rejecting here.
    let parsed = TitleParser::try_parse(&target, config)?;
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

/// Resolve a template target from a plain string. Convenience wrapper around
/// `resolve_target_string` (the common case where the target is already text).
/// `context_title` is the current page title (for relative `/Subpage`/`../foo`
/// resolution).
pub fn resolve_template_target(
    config: &dyn SiteConfig,
    context_title: Option<&crate::title::Title>,
    target: &str,
) -> Option<ResolvedTarget> {
    resolve_target_string(config, context_title, target)
}

/// Resolve a template target from a token chunk, mirroring PHP's
/// `resolveTemplateTarget($state, $targetToks, $srcOffsets)`.
///
/// `in_template` mirrors `$this->options['inTemplate']`.
pub fn resolve_template_target_tokens(
    config: &dyn SiteConfig,
    context_title: Option<&crate::title::Title>,
    target_toks: &[Item],
    in_template: bool,
) -> Option<ResolvedTarget> {
    let processed = process_to_string(target_toks, in_template);

    // Additional tokens are only justifiable in parser-function scenarios.
    // If we still have unprocessed tokens and the target has no colon, the
    // target is not a valid parser function call.
    if processed.rest.is_some() {
        // The target has no colon: reject (mirrors PHP's `!$haveColon && $additionalToks`).
        let target = processed.target.trim();
        if !target.contains(':') && !target.contains('：') {
            return None;
        }
    }

    resolve_target_string(config, context_title, &processed.target)
}

/// Is `name` the `safesubst` magic word? Mirrors the essential safesubst check.
fn is_safe_subst(name: &str) -> bool {
    name == "safesubst"
}

/// The target of a page whose wikitext is a redirect, or `None` if it is not one.
///
/// A transcluded redirect is ordinary on a wiki — `{{db}}` → `Template:Delete` —
/// and MediaWiki follows it, transcluding the *target*. `Template:Pp-semi`
/// (`#REDIRECT [[Template:Protected page]]`) is the redirect that surfaced this:
/// rendering its body literally produces a redirect listing instead of the icon.
///
/// Detection mirrors MediaWiki's `WikiPage::isRedirect`: the redirect word must
/// be the first thing in the page, after whitespace and optional comments. A body
/// that merely mentions `#REDIRECT` inline is **not** a redirect, so this must
/// anchor at the start rather than search.
///
/// `#REDIRECT` is localisable and carries optional aliases (`#REDIRECT`, with
/// magic-word synonyms), but a wiki's aliases are not guaranteed to be in
/// `SiteConfig`, so only the canonical spellings are recognised here. The
/// alternative — treating any leading word as a redirect — would misread ordinary
/// content, which is much worse than missing a localised redirect.
pub fn redirect_target_of(body: &str) -> Option<String> {
    // Strip leading comments, which MediaWiki allows before the redirect word
    // (`<!--x-->#REDIRECT [[A]]` is a redirect).
    let mut rest = body.trim_start();
    while let Some(after) = rest.strip_prefix("<!--") {
        let end = after.find("-->")?;
        rest = after[end + 3..].trim_start();
    }

    let after_word = rest
        .strip_prefix("#REDIRECT")
        .or_else(|| rest.strip_prefix("#redirect"))
        .or_else(|| rest.strip_prefix("#Redirect"))?;

    // The redirect word must be a whole word: `#REDIRECTED` is not a redirect.
    // A colon is allowed (`#REDIRECT: [[A]]`).
    let after_word = after_word.strip_prefix(':').unwrap_or(after_word);
    if !after_word.starts_with(|c: char| c.is_whitespace() || c == '[') {
        return None;
    }

    // The target is the first wikilink. A piped target (`[[A|label]]`) redirects
    // to `A`; the label is ignored, as MediaWiki ignores it too.
    let after_word = after_word.trim_start();
    let after_open = after_word.strip_prefix("[[")?;
    let end = after_open.find("]]")?;
    let target = &after_open[..end];
    let target = target.split('|').next()?.trim();
    if target.is_empty() {
        return None;
    }
    Some(target.replace('_', " "))
}

/// Parse a `{{{...}}}` template-argument source string into its argument name
/// and optional default. Mirrors the `k` / `v` attribution of PHP's
/// `templatearg` token.
pub fn parse_template_arg_src(src: &str) -> Option<(String, Option<String>)> {
    let inner = src.strip_prefix("{{{")?.strip_suffix("}}}")?;
    // Split on the first '|' for name | default.
    let (name, default) = match inner.split_once('|') {
        Some((n, d)) => (n.trim().to_string(), Some(d.to_string())),
        None => (inner.trim().to_string(), None),
    };
    if name.is_empty() {
        return None;
    }
    Some((name, default))
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

/// The full `action|title` argument of a protection magic word.
///
/// `pf_arg` holds everything after the first colon, which is the whole argument
/// when the tokenizer folded it into the target (`{{PROTECTIONLEVEL:edit|Canada}}`
/// is one parameter whose key is the entire string). The tokenizer does not
/// always do that, and when it does not, the title sits in the *value* of the
/// next parameter with an empty key — so both are read here rather than one.
fn protection_full_arg(pf_arg: &str, params: &Params) -> String {
    if pf_arg.contains('|') {
        return pf_arg.to_string();
    }
    let rest = params
        .args
        .get(1)
        .map(|kv| {
            let key = crate::wikitext::token_utils::key_value_to_string(&kv.key);
            if key.is_empty() {
                crate::wikitext::token_utils::key_value_to_string(&kv.value)
            } else {
                key
            }
        })
        .unwrap_or_default();
    if rest.is_empty() {
        pf_arg.to_string()
    } else {
        format!("{pf_arg}|{rest}")
    }
}

/// Process the special `!` magic word. Mirrors PHP's
/// `TemplateHandler::processSpecialMagicWord`.
///
/// `magic_word_type === '!'` is `{{!}}`, which expands to a literal `|` at
/// the top level, or to a table cell (`<td>`) inside a template (so the token
/// can be recognized as a cell in the enclosing table).
pub fn process_special_magic_word(magic_word_type: &str, in_template: bool) -> Vec<Item> {
    use crate::wikitext::tokens_v2::{DataParsoid, ParsoidToken, TagTk};

    if magic_word_type == "!" {
        if in_template {
            // Inside a template, `{{!}}` produces a `<td>` so the token can be
            // recognized as a cell in the enclosing table; the empty `attrSrc`
            // plus the `AT_SRC_START` flag tell `TableFixups` to reinterpret it as
            // a literal `|` content separator (mirrors PHP's `processSpecialMagicWord`).
            let mut dp = DataParsoid::default();
            dp.tmp.attr_src = Some(String::new());
            dp.tmp.at_src_start = true;
            let td = TagTk::new("td", vec![], dp);
            vec![Item::Tok(ParsoidToken::Tag(td))]
        } else {
            vec![Item::Str("|".to_string())]
        }
    } else {
        // PHP throws an unreachable here for unsupported magic word types.
        // We return an empty chunk rather than panicking.
        Vec::new()
    }
}

/// What `{{PROTECTIONLEVEL:…}}` and `{{PROTECTIONEXPIRY:…}}` can answer.
///
/// These are core magic words, but unlike `{{SITENAME}}` they need data no site
/// configuration carries: the protection actually applied to a page. Expansion
/// is synchronous, so the answers must be in hand before the tokens are walked —
/// the parser fetches the page's own protection up front and this carries it.
///
/// A title the parser did not prefetch reports as unprotected, which is
/// MediaWiki's answer for a title that does not exist and the conservative one
/// for a title that was simply not asked about.
pub struct ProtectionContext<'a> {
    /// The page being parsed, for the no-title form.
    pub page: std::cell::Ref<'a, crate::traits::ProtectionEntry>,
    /// Protection for any other title a call named.
    pub titles: std::cell::Ref<'a, HashMap<String, crate::traits::ProtectionEntry>>,
}

impl<'a> ProtectionContext<'a> {
    /// Borrow both maps for as long as the returned context lives.
    ///
    /// Taking the `Ref`s here rather than at the call sites keeps the two
    /// borrows together: a caller cannot accidentally hold one and drop the
    /// other, and the expansion paths that need protection never touch the
    /// `RefCell`s directly.
    pub fn new(
        page: &'a std::cell::RefCell<crate::traits::ProtectionEntry>,
        titles: &'a std::cell::RefCell<HashMap<String, crate::traits::ProtectionEntry>>,
    ) -> Self {
        Self {
            page: page.borrow(),
            titles: titles.borrow(),
        }
    }
}

impl ProtectionContext<'_> {
    /// Resolve the first argument of a protection magic word.
    ///
    /// With no argument the page being parsed is meant; with a title (or an
    /// empty one) the named page. `None` means the title is not one the parser
    /// looked up, which every caller answers as "unprotected" — the same answer
    /// MediaWiki gives for a page that does not exist.
    fn entry(&self, arg: &str) -> Option<&crate::traits::ProtectionEntry> {
        let arg = arg.trim();
        if arg.is_empty() {
            return Some(&self.page);
        }
        // The wiki normalises underscores to spaces and capitalises the first
        // letter, so a module writing `{{PROTECTIONLEVEL:edit|template:foo}}`
        // must find what the parser stored under the prefixed spelling.
        let normalized = arg.replace('_', " ");
        self.titles
            .get(arg)
            .or_else(|| self.titles.get(&normalized))
            .or_else(|| self.titles.get(&capitalize(&normalized)))
    }

    /// Shared shape of the two magic words: split `action` and the optional
    /// title out of the colon argument, resolve the title's protection, and take
    /// one field from it.
    ///
    /// `raw` is everything after the first colon, so `"edit"` or
    /// `"edit|Canada"`. A missing action or an unprotected page both answer with
    /// the empty string, which is what MediaWiki returns and what callers test
    /// for.
    pub fn answer(
        &self,
        raw: &str,
        pick: impl Fn(&crate::traits::ProtectionEntry, &str) -> Option<String>,
    ) -> String {
        let (action, title) = match raw.split_once('|') {
            Some((a, t)) => (a, t),
            None => (raw, ""),
        };
        self.entry(title)
            .and_then(|e| pick(e, action.trim()))
            .unwrap_or_default()
    }
}

/// Uppercase the first character, as MediaWiki title normalisation does.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The target of a `{{…}}` source string, before any nested template in it is
/// expanded. `None` when the source is not a well-formed template.
fn raw_template_target(src: &str) -> Option<&str> {
    let inner = src.strip_prefix("{{")?.strip_suffix("}}")?;
    Some(inner.split_once('|').map_or(inner, |(t, _)| t))
}

/// Choose the wikitext to record for a parser function's `data-mw` target.
///
/// The recorded form is the source as written, so a nested template in the colon
/// argument survives in `target.wt` — `{\{#switch: {{lc:YES}} |yes=…\}\}`
/// records `"#switch: {{lc:YES}} "`. The expanded `target_str` is only a
/// fallback for a token whose source is unavailable (one spliced in from an
/// argument value), where recording something is better than recording nothing.
///
/// Preprocessor directives are stripped either way, matching MediaWiki:
/// `{{#if:<includeonly>X</includeonly> |yes|no}}` records `"#if: "`.
fn info_target_wt(target_str: &str, raw_target: Option<&str>) -> String {
    let text = raw_target.unwrap_or(target_str);
    strip_include_directives(text)
}

/// Enforce template loop / depth constraints. Mirrors PHP's
/// `TemplateHandler::enforceTemplateConstraints`.
///
/// Returns `Some` error tokens (`<span class=error>` + message +
/// `<a href>` + `</span>`) when a loop / depth violation is detected,
/// else `None`.
pub fn enforce_template_constraints(
    config: &dyn SiteConfig,
    frame: &super::frame::Frame,
    target: &str,
    title: &Title,
    max_depth: usize,
    ignore_loop: bool,
) -> Option<Vec<Item>> {
    use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, KV, KeyValue, TagTk};

    let error = frame.loop_and_depth_check(title, max_depth, ignore_loop)?;

    let mut span = TagTk::new("span", vec![], DataParsoid::default());
    span.attribs.push(KV {
        key: KeyValue::Str("class".to_string()),
        value: KeyValue::Str("error".to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });

    let wikilink = template_to_wikilink(target, config);

    Some(vec![
        Item::Tok(ParsoidToken::Tag(span)),
        Item::Str(error),
        wikilink,
        Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "span",
            vec![],
            DataParsoid::default(),
        ))),
    ])
}

/// Post-process a token chunk emitted after feeding a template. Mirrors
/// PHP's `TemplateHandler::processTemplateTokens`.
///
/// - strips the EOF token;
/// - clears `tsr` (template content has synthetic source ranges);
/// - turns `mw:Placeholder` metas into empty strings (so they aren't
///   foster-parented);
/// - discards comments when template expansion is disabled.
pub fn process_template_tokens(chunk: Vec<Item>, expand_templates: bool) -> Vec<Item> {
    let mut out = Vec::new();
    for mut item in chunk {
        match &mut item {
            Item::Tok(t) => match t {
                ParsoidToken::Eof(_) => continue,
                ParsoidToken::SelfclosingTag(stt)
                    if stt.name == "meta"
                        && crate::wikitext::token_utils::has_type_of(t, "mw:Placeholder") =>
                {
                    // Replace with an empty string (not an empty Item, which
                    // would keep a phantom token in the chunk).
                    out.push(Item::Str(String::new()));
                    continue;
                }
                ParsoidToken::Comment(_) if !expand_templates => continue,
                _ => {}
            },
            Item::Str(_) => {}
        }

        // Clear template-internal source ranges.
        if let Item::Tok(t) = &mut item
            && let Some(dp) = t.data_parsoid_mut()
        {
            dp.tsr = None;
        }
        out.push(item);
    }
    out
}

/// Wrap parser-function output tokens. Mirrors PHP's
/// `TemplateHandler::parserFunctionsWrapper`: filter out empty strings,
/// then run `processTemplateTokens`.
pub fn parser_functions_wrapper(tokens: Vec<Item>) -> Vec<Item> {
    let filtered: Vec<Item> = tokens
        .into_iter()
        .filter(|t| !matches!(t, Item::Str(s) if s.is_empty()))
        .collect();
    process_template_tokens(filtered, /* expand_templates */ true)
}

/// The fallback a template that could not be fetched is replaced by.
///
/// Mirrors `Parser::braceSubstitution`. When every lookup fails — no such page,
/// no parser function, no variable — PHP does *not* return the call as text; it
/// sets `$found = true` and substitutes:
///
/// ```php
/// # If the title is valid but undisplayable, make a link to it
/// if ( !$found && ( $this->ot['html'] || $this->ot['pre'] ) ) {
///     $text = "[[:$titleText]]";
///     $found = true;
/// }
/// ```
///
/// The link text is what lets a construct terminate. A module such as
/// `Module:Autotaxobox` reads the answer back and uses it as the *next template
/// title*, so an answer that is not a title has it splice markup into one and
/// recurse. Answering with a link is what the live service does, and the answer
/// is measurable — see `render_answer`'s doc for the `String.sub` probe that
/// reads its bytes rather than its rendered surface.
///
/// Two details of that answer are load-bearing and are recorded here because
/// this function is the one that supplies them:
///
/// - The link is `[[:Title]]`, with the leading colon: a bare `[[Template:Foo]]`
///   is a *transclusion* of it, and the answer must read as a link.
/// - `src` carries that wikitext, because this token reaches a *module* rather
///   than a page in one case — `frame:expandTemplate`'s answer is the expansion's
///   *source* — and the stringifier reconstructs from `src`.
///
/// The returned anchor is otherwise a plain `wikilink` token, not a finished
/// `<a>`: the red-link marking (`class="new"`, `data-mw-i18n`,
/// `?action=edit&redlink=1`) belongs to [`crate::pipeline::add_red_links`], which
/// needs `page_info`, and reusing it is what keeps this from being a hand-built
/// anchor that merely approximates Parsoid.
pub fn template_to_wikilink(name: &str, config: &dyn SiteConfig) -> Item {
    use crate::wikitext::tokens_v2::{DataParsoid, KV, KeyValue, ParsoidToken, SelfclosingTagTk};

    let target = crate::title::TitleParser::parse(name, config);
    let prefixed = target.get_prefixed_text();
    // PHP's `$originalTitle`: a target that names a namespace is written with a
    // leading colon (`[[Template:Foo]]` would be a *transclusion* of it).
    let href = if name.starts_with(':') || target.namespace_id != 0 {
        format!(":{prefixed}")
    } else {
        prefixed.clone()
    };

    let dp = DataParsoid {
        stx: Some("simple".to_string()),
        src: Some(format!("[[{href}]]")),
        ..Default::default()
    };

    let mut tk = SelfclosingTagTk::new("wikilink", vec![], dp);
    let mut push = |key: &str, value: String| {
        tk.attribs.push(KV {
            key: KeyValue::Str(key.to_string()),
            value: KeyValue::Str(value),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
    };
    // The `href` is the resolved target, *without* the colon: `:` means "force
    // mainspace" in MediaWiki's title grammar, so `:Template:Foo` would resolve
    // to a mainspace page named `Template:Foo` rather than the template. The
    // colon belongs to the source (`src`), not to the target.
    push("href", prefixed);
    // The target as written, which differs from `prefixed` when the call used an
    // underscore. The link *text* is deliberately absent: the renderer derives it
    // from the target, exactly as for a written `[[Template:Foo]]`.
    push("wt", name.to_string());

    Item::Tok(ParsoidToken::SelfclosingTag(tk))
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
        context_title: Option<&crate::title::Title>,
        params: &Params,
        about_id: String,
        token: &crate::wikitext::tokens_v2::ParsoidToken,
        protection: &ProtectionContext,
    ) -> Vec<Item> {
        // The target as it appears in the source, before any nested template in it
        // is expanded. `data-mw` records this form (see the parser-function arm).
        let raw_target = token
            .data_parsoid()
            .and_then(|dp| dp.src.as_deref())
            .and_then(raw_template_target)
            .map(str::to_string);
        // Extract the target (first arg key).
        let target_str = params
            .args
            .first()
            .map(|kv| {
                crate::wikitext::token_utils::tokens_to_string(&[match &kv.key {
                    crate::wikitext::tokens_v2::KeyValue::Str(s) => Item::Str(s.clone()),
                    crate::wikitext::tokens_v2::KeyValue::Tokens(t) => {
                        Item::Str(crate::wikitext::token_utils::tokens_to_string(t))
                    }
                }])
            })
            .unwrap_or_default();

        match resolve_template_target(config, context_title, &target_str) {
            Some(ResolvedTarget::Variable {
                name,
                magic_word_type,
                pf_arg,
                ..
            }) => {
                // The `{{!}}` magic word expands to a literal `|` (table-pipe).
                // This is a special token-level substitution (PHP
                // `processSpecialMagicWord` with `magicWordType === '!'`); it must
                // NOT be string-permuted or re-tokenized into a table delimiter.
                if magic_word_type.as_deref() == Some("!") {
                    return vec![Item::Str("|".to_string())];
                }
                // `pf_arg` is only the text before the first `|` (the resolver
                // splits on `:` first), so the title — when the tokenizer kept it
                // as a separate parameter rather than folding it into the target
                // — is read from the remaining parameters. Both spellings reach a
                // page in practice, which is why both are handled.
                let value = match name.as_str() {
                    "protectionlevel" | "protectionexpiry" => {
                        let raw = protection_full_arg(&pf_arg, params);
                        protection.answer(&raw, |e, action| {
                            if name == "protectionlevel" {
                                e.level(action).map(str::to_string)
                            } else {
                                e.expiry(action).map(str::to_string)
                            }
                        })
                    }
                    _ => Self::variable_value(config, &name, &pf_arg),
                };
                let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
                let mut info = template_info_from(Some(&name), None, vec![]);
                info.target_wt = Some(target_str.clone());
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
                let token_src = token.data_parsoid().and_then(|dp| dp.src.clone());
                let result = Self::call_parser_function(
                    config,
                    &name,
                    &pf_params,
                    token_src.as_deref(),
                    protection,
                );
                let mut encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
                if !colon.is_empty() {
                    encap.set_colon(Some(colon));
                }
                let mut info = template_info_from(Some(&name), None, vec![]);
                // v3 parser-function output marks the transclusion with a
                // `mw:ParserFunction/<name>` typeof and a `"parserfunction"`
                // data-mw parts key; v2 uses `"old-parserfunction"` (serialized
                // back to `"template"` with a `func` in `DataMw::toJsonArray`).
                info.ty = if config.parsoid_experimental_parser_function_output() {
                    Some("parserfunction".to_string())
                } else {
                    Some("old-parserfunction".to_string())
                };
                // `data-mw` records the target *as written*, not as expanded. A
                // nested template in the colon argument therefore stays visible:
                // `{{#switch: {{lc:YES}} |yes=…}}` records
                // `"#switch: {{lc:YES}} "`, while the argument values below come
                // from `srcOffsets` for the same reason. Recording the expanded
                // `target_str` instead wrote `"#switch: yes"`, so every page
                // comparing `data-mw` differed at the first parser function whose
                // argument held a template — which is `Template:Yesno`, and
                // through it `Template:Infobox` and the documentation stack.
                info.target_wt = Some(info_target_wt(&target_str, raw_target.as_deref()));
                info.param_infos = super::template_encapsulator::prepare_pf_param_infos(
                    info.target_wt.as_deref().unwrap_or(&target_str),
                    params,
                    // The argument wikitext is read from the source range, which
                    // needs the ambient text the call came from.
                    token
                        .data_parsoid()
                        .and_then(|dp| dp.src.as_deref())
                        .unwrap_or(""),
                );
                encap.encap_tokens(result, &info)
            }
            Some(ResolvedTarget::Template { name, .. }) => {
                // Native template fetching is deferred; emit a redlink wikilink.
                let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
                let mut info = template_info_from(None, Some(&name), vec![]);
                info.target_wt = Some(target_str.clone());
                info.param_infos = super::template_encapsulator::prepare_tpl_param_infos(
                    params,
                    token
                        .data_parsoid()
                        .and_then(|dp| dp.src.as_deref())
                        .unwrap_or(""),
                );
                encap.encap_tokens(vec![template_to_wikilink(&name, config)], &info)
            }
            None => Self::convert_to_string(token, false, false),
        }
    }

    /// Bail an unexpandable template back to literal text. Mirrors PHP's
    /// `TemplateHandler::convertToString`: re-tokenize the token's source with
    /// the outer `{{`/`}}` stripped (`substr($src, 2, -2)`), then re-emit the
    /// literal `{{` … `}}` around the re-tokenized content.
    ///
    /// `in_template` is the tokenizer's `inTemplate` option (PHP keeps the parent
    /// pipeline's `inTemplate` for the re-tokenization). PHP additionally threads
    /// an `expandTemplates` flag (true for the `onTemplateArg`/`onTemplate` bail
    /// paths); it only matters when template expansion is enabled, which is
    /// already decided by the caller's pipeline options.
    ///
    /// PHP runs the re-tokenized content through the full TT2 pipeline
    /// (`wikitext-to-expanded-tokens`), so nested templates in the bailed
    /// source are expanded — the caller is responsible for that pass in
    /// rustoid (see `Parser::expand_templates`).
    pub(crate) fn convert_to_string(
        token: &crate::wikitext::tokens_v2::ParsoidToken,
        in_template: bool,
        in_table: bool,
    ) -> Vec<Item> {
        let dp = token.data_parsoid();
        let Some(src) = dp.and_then(|dp| dp.src.as_deref()) else {
            // No recorded source: fall back to a no-op re-serialization.
            return vec![Item::Tok(token.clone())];
        };
        // `substr( $src, 2, -2 )` — the source always has the `{{`/`}}` delimiters.
        let inner = src
            .strip_prefix("{{")
            .and_then(|s| s.strip_suffix("}}"))
            .unwrap_or(src);

        let mut out = vec![Item::Str("{{".to_string())];
        // Re-tokenizing bailed source: the caller knows whether a table was open,
        // so the flag is enforced here (PHP's `convertToString` re-tokenizes under
        // the ambient `tableDataBlock` context).
        out.extend(tokenize_wikitext_to_items_with_sol_and_table(
            inner,
            in_template,
            &[],
            false,
            in_table,
            true,
        ));
        out.push(Item::Str("}}".to_string()));
        out
    }

    /// Resolve a magic variable to its string value. Mirrors the common
    /// variable cases (full page-name variables require the page context,
    /// which isn't yet wired). `pf_arg` carries the colon argument for
    /// parameterized magic words like `{{ns:…}}`/`{{nse:…}}`.
    fn variable_value(config: &dyn SiteConfig, name: &str, pf_arg: &str) -> String {
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
            "ns" | "nse" => {
                // `{{ns:index}}` maps a namespace index (or localized name) to its
                // canonical name. A non-numeric arg is looked up as a namespace
                // alias; an unknown index echoes the raw argument (mirrors
                // `Language::getNsText` returning `false` → the arg is passed
                // through unchanged).
                let arg = pf_arg.trim();
                if let Ok(index) = arg.parse::<i32>() {
                    config
                        .namespace_name(index)
                        .unwrap_or_else(|| arg.to_string())
                } else {
                    arg.to_string()
                }
            }
            // Any other variable: empty for now (requires page context).
            _ => String::new(),
        }
    }

    /// Dispatch a parser function name to the `ParserFunctions` implementation.
    ///
    /// `token_src` is the original `{{#name:...}}` source. For an unknown
    /// ("broken") parser function the transclusion content is the verbatim
    /// source, mirroring the integrated-mode path where MediaWiki's preprocessor
    /// leaves an unrecognized `{{#…}}` invocation unexpanded (the standalone
    /// "Parser function implementation … missing" message is a native-only
    /// branch not exercised by the integrated fixtures).
    ///
    /// `protection` answers the two protection magic words, which reach here in
    /// their `{{#…}}` spelling. That spelling is not cosmetic: it is what a module
    /// produces, because `frame:callParserFunction` renders the call as wikitext
    /// and re-expands it, and a module cannot know the wiki spells the same word
    /// without a hash on a page.
    fn call_parser_function(
        config: &dyn SiteConfig,
        name: &str,
        params: &Params,
        token_src: Option<&str>,
        protection: &ProtectionContext,
    ) -> Vec<Item> {
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
            "tag" => ParserFunctions::pf_tag(config, params),
            "urlencode" => ParserFunctions::pf_urlencode(params),
            "anchorencode" => ParserFunctions::pf_anchorencode(params),
            // `{{#dir:code}}` — the directionality (`ltr`/`rtl`) of a language.
            // Core `$noHashFunctions` member (invoked with a leading `#`).
            "dir" => ParserFunctions::pf_dir(params),
            // `{{ns:…}}`/`{{nse:…}}` reached as *functions* (`{{safesubst:ns:0}}`,
            // `{{#ns:0}}`) rather than as bare magic words. Both spellings mean the
            // same thing, so both answer from the same place: the logic already
            // exists in `variable_value`, and duplicating it here would let the two
            // drift.
            //
            // Without this arm the name is a registered function hook with no
            // implementation, so it fell through to the unknown-parser-function
            // fallback and returned its own source — which is how
            // `Template:Main other` came to compare `{{safesubst:<noinclude/>ns:0}}`
            // as text rather than as the empty namespace name.
            "ns" | "nse" => {
                let arg = params
                    .args
                    .first()
                    .map(|kv| key_value_to_string(&kv.key))
                    .unwrap_or_default();
                vec![Item::Str(Self::variable_value(config, name, &arg))]
            }
            // The `#`-spelled protection words, which is how a module reaches them.
            // The no-hash spelling is answered from the variable arm instead, because
            // a *page* spells it that way and the site's magic-word table routes it
            // there. Both spellings must agree, so both call `answer`.
            "protectionlevel" | "protectionexpiry" => {
                let raw = protection_full_arg(
                    &params
                        .args
                        .first()
                        .map(|kv| crate::wikitext::token_utils::key_value_to_string(&kv.key))
                        .unwrap_or_default(),
                    params,
                );
                let level = name == "protectionlevel";
                vec![Item::Str(protection.answer(&raw, |e, action| {
                    if level {
                        e.level(action).map(str::to_string)
                    } else {
                        e.expiry(action).map(str::to_string)
                    }
                }))]
            }
            // Unknown parser function: preserve the original source verbatim.
            _ => vec![Item::Str(token_src.unwrap_or("").to_string())],
        }
    }

    /// Handle a bare `{{{...}}}` template-argument token at the top level.
    /// Mirrors PHP's `TemplateHandler::onTemplateArg`: expand the argument
    /// via the frame and wrap it with `mw:Param` markers when outside a
    /// template context.
    pub fn handle_template_arg(
        &self,
        frame: &super::frame::Frame,
        src: &str,
        about_id: String,
        token: &ParsoidToken,
        wrap: bool,
    ) -> Vec<Item> {
        let (name, default) = match parse_template_arg_src(src) {
            Some(pair) => pair,
            None => return vec![Item::Str(src.to_string())],
        };

        let mut toks = frame.expand_template_arg(&name);
        if toks.is_empty()
            && let Some(default) = default
        {
            toks = vec![Item::Str(default)];
        }

        self.encap_template_arg(toks, about_id, token, wrap)
    }

    /// Expand a `templatearg` token through `Frame::expandTemplateArg` and
    /// encapsulate the result. Unlike [`Self::handle_template_arg`], this works
    /// off the token's attribs, so the `{{{name|default}}}` default is honoured
    /// (mirrors `TemplateHandler::onTemplateArg`).
    pub fn handle_template_arg_token(
        &self,
        frame: &super::frame::Frame,
        token: &ParsoidToken,
        about_id: String,
        wrap: bool,
    ) -> Vec<Item> {
        let toks = frame.expand_template_arg_token(token);
        self.encap_template_arg(toks, about_id, token, wrap)
    }

    /// Wrap an expanded template-argument chunk in its `mw:Param`
    /// encapsulation when `wrap` is set.
    fn encap_template_arg(
        &self,
        toks: Vec<Item>,
        about_id: String,
        token: &ParsoidToken,
        wrap: bool,
    ) -> Vec<Item> {
        if wrap {
            let encap = TemplateEncapsulator::new("mw:Param", about_id, token);
            let info = TemplateInfo::default();
            encap.encap_tokens(toks, &info)
        } else {
            toks
        }
    }

    /// Process a chunk of tokens, dispatching `template`/`template3` and
    /// `templatearg` self-closing tokens through `handle_template` and
    /// `handle_template_arg` respectively. Mirrors PHP's
    /// `TemplateHandler::onTag` + `XMLTagBasedHandler::process` (the
    /// TokenTransform2 dispatch loop).
    pub fn process(
        &self,
        config: &dyn SiteConfig,
        frame: &super::frame::Frame,
        about_counter: &std::cell::Cell<usize>,
        protection: &ProtectionContext,
        tokens: Vec<Item>,
    ) -> Vec<Item> {
        let mut out = Vec::new();
        for item in tokens {
            let Item::Tok(tok) = &item else {
                out.push(item);
                continue;
            };
            let ParsoidToken::SelfclosingTag(stt) = tok else {
                out.push(item);
                continue;
            };
            if stt.name == "template" || stt.name == "template3" {
                let about_id = {
                    let id = about_counter.get();
                    about_counter.set(id + 1);
                    format!("#mwt{id}")
                };
                // Build a `Params` from the token's attribs.
                let params = Params::new(stt.attribs.clone());
                let context_title = frame.title();
                let expanded = self.handle_template(
                    config,
                    Some(context_title),
                    &params,
                    about_id,
                    tok,
                    protection,
                );
                out.extend(expanded);
                continue;
            }
            if stt.name == "templatearg"
                && let Some(name) = stt.attribs.first().and_then(|kv| kv.key.as_str())
            {
                let about_id = {
                    let id = about_counter.get();
                    about_counter.set(id + 1);
                    format!("#mwt{id}")
                };
                let src = format!("{{{{{name}}}}}");
                let expanded = self.handle_template_arg(frame, &src, about_id, tok, true);
                out.extend(expanded);
                continue;
            }
            out.push(item);
        }
        out
    }

    /// Fetch, expand, and tokenize a template natively. Mirrors the
    /// `fetchTemplateAndTitle` + `processTemplateSource` path of PHP's
    /// `TemplateHandler::expandTemplateNatively` for a resolved template
    /// target.
    ///
    /// Template source is fetched via `DataSource::get_template`, then
    /// re-parsed with the (approximate) tokenizer and encapsulated with
    /// `mw:Transclusion` markers. Argument substitution uses the existing
    /// string-level transclusion engine until the token-level
    /// `AttributeTransformManager` is ported.
    pub async fn expand_template_natively(
        config: &dyn SiteConfig,
        source: &dyn DataSource,
        name: &str,
        title: &Title,
        params: &Params,
        about_id: String,
        token: &ParsoidToken,
    ) -> Vec<Item> {
        // Build the template invocation (target + arguments) for the legacy
        // string-level substitution engine, from the token's attribute list
        // (args[0] is the target; the rest are positional/named args).
        use crate::expand::transclusion::TemplateInvocation;
        use crate::wikitext::token_utils::key_value_to_string;

        let mut positional_args = Vec::new();
        let mut named_args = std::collections::HashMap::new();
        for kv in params.args.iter().skip(1) {
            let k = key_value_to_string(&kv.key);
            let v = key_value_to_string(&kv.value);
            if k.trim().is_empty() {
                positional_args.push(v);
            } else {
                named_args.insert(k.trim().to_string(), v);
            }
        }
        let invocation = TemplateInvocation {
            name: name.to_string(),
            positional_args,
            named_args,
        };

        // Fetch the template source; missing templates become a redlink.
        let fetched = source.get_template(title).await.ok().flatten();
        let Some(src) = fetched else {
            let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
            let info = template_info_from(None, Some(name), vec![]);
            return encap.encap_tokens(vec![template_to_wikilink(name, config)], &info);
        };

        // Substitute template arguments (string-level for now).
        let expanded =
            transclusion::substitute_args(&src, &invocation.to_template_args(), 40).unwrap_or(src);

        // Re-tokenize the expanded source.
        let items = tokenize_wikitext_to_items(&expanded, /* in_template */ true, &[]);

        let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
        let info = template_info_from(None, Some(name), vec![]);
        encap.encap_tokens(items, &info)
    }
}

/// Tokenize a plain wikitext string into a flat `Vec<Item>`. Used to
/// re-tokenize expanded template source.
pub fn tokenize_wikitext_to_items(
    wikitext: &str,
    in_template: bool,
    ext_tags: &[String],
) -> Vec<Item> {
    tokenize_wikitext_to_items_with_sol(wikitext, in_template, ext_tags, true)
}

/// As [`tokenize_wikitext_to_items`], carrying the caller's table-data-block
/// context (PHP's `tableDataBlock` parameter). A template body tokenized while a
/// table is open must still recognize `|` as a cell separator, even though the
/// `{|` is in a different expansion — see `TokenizerOptions::table_data_block`.
pub fn tokenize_wikitext_to_items_in_table(
    wikitext: &str,
    in_template: bool,
    ext_tags: &[String],
    table_data_block: bool,
) -> Vec<Item> {
    tokenize_wikitext_to_items_with_sol_and_table(
        wikitext,
        in_template,
        ext_tags,
        true,
        table_data_block,
        false,
    )
}

/// As [`tokenize_wikitext_to_items`], with an explicit start-of-line flag
/// (mirrors the `sol` option of PHP's `PegTokenizer::tokenizeSync`).
pub fn tokenize_wikitext_to_items_with_sol(
    wikitext: &str,
    in_template: bool,
    ext_tags: &[String],
    sol: bool,
) -> Vec<Item> {
    tokenize_wikitext_to_items_with_sol_and_table(
        wikitext,
        in_template,
        ext_tags,
        sol,
        false,
        false,
    )
}

/// [`tokenize_wikitext_to_items_with_sol`] with an explicit table-data-block flag.
///
/// `enforce_table_data_block` is true for re-tokenization of bailed source, where
/// the caller knows the surrounding table context, so the flag can meaningfully
/// decide whether a leading `|` is a cell (PHP's `convertToString`).
pub fn tokenize_wikitext_to_items_with_sol_and_table(
    wikitext: &str,
    in_template: bool,
    ext_tags: &[String],
    sol: bool,
    table_data_block: bool,
    enforce_table_data_block: bool,
) -> Vec<Item> {
    let options = TokenizerOptions {
        in_template,
        ext_tags: ext_tags.to_vec(),
        sol,
        table_data_block,
        enforce_table_data_block,
        ..Default::default()
    };
    let mut tokenizer = PegTokenizer::new(wikitext, &options);
    let items = match tokenizer.tokenize() {
        Ok(chunks) => chunks
            .into_iter()
            .map(|either| match either {
                crate::wikitext::tokens_v2::Either::Left(s) => Item::Str(s),
                crate::wikitext::tokens_v2::Either::Right(t) => Item::Tok(t),
            })
            .collect(),
        Err(_) => vec![Item::Str(wikitext.to_string())],
    };
    filter_include_directives(items, in_template)
}

/// Drop/keep `<includeonly>`/`<noinclude>`/`<onlyinclude>` regions according to
/// the template context, mirroring PHP `TemplateHandler::processPreprocToString`:
/// - `<noinclude>` content is kept on the page (`in_template == false`) but dropped
///   when transcluded (`in_template == true`).
/// - `<includeonly>` content is the reverse (dropped on the page, kept in
///   transclusion).
/// - `<onlyinclude>` content is always kept.
///
/// The boundary markers emitted by the tokenizer are `meta` self-closing tokens
/// with `typeof="mw:Includes/<Type>[/End]"`. We track a stack of open regions to
/// support (the common) non-nested uses and ignore unbalanced/misordered markers
/// defensively.
pub fn filter_include_directives(items: Vec<Item>, in_template: bool) -> Vec<Item> {
    fn inc_type_of(item: &Item) -> Option<(String, bool)> {
        let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
            return None;
        };
        if stt.name != "meta" {
            return None;
        }
        let ty = stt
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("typeof"))?;
        let ty = ty.value.as_str()?;
        let k = ty.strip_prefix("mw:Includes/")?;
        let (kind, closing) = match k.strip_suffix("/End") {
            Some(k) => (k.to_string(), true),
            None => (k.to_string(), false),
        };
        Some((kind, closing))
    }

    let mut out: Vec<Item> = Vec::new();
    // Whether the current top-of-stack region is being dropped (its content
    // excluded). `true` when the region's content should be kept but the markers
    // dropped is encoded by simply not pushing the markers.
    let mut stack: Vec<bool> = Vec::new(); // each entry: is-content-dropped

    for item in items {
        if let Some((kind, closing)) = inc_type_of(&item) {
            match kind.as_str() {
                "NoInclude" => {
                    if !closing {
                        // Content dropped when transcluding; kept otherwise.
                        stack.push(in_template);
                    } else {
                        stack.pop();
                    }
                    // Boundary markers are never emitted.
                }
                "IncludeOnly" => {
                    if !closing {
                        // Content kept when transcluding; dropped otherwise.
                        stack.push(!in_template);
                    } else {
                        let _ = stack.pop();
                    }
                }
                "OnlyInclude" => {
                    if !closing {
                        stack.push(false); // always keep content
                    } else {
                        let _ = stack.pop();
                    }
                }
                _ => {}
            }
            continue;
        }

        // Emit content only if the innermost region is not a dropped one.
        if stack.last().copied().unwrap_or(false) {
            continue;
        }
        out.push(item);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    #[test]
    fn test_resolve_magic_variable() {
        let config = MockSiteConfig::new();
        let target = resolve_template_target(&config, None, "PAGENAME").unwrap();
        match target {
            ResolvedTarget::Variable { name, .. } => {
                assert_eq!(name, "pagename");
            }
            other => panic!("expected variable, got {:?}", other),
        }
    }

    #[test]
    fn redirect_target_is_read_from_a_redirect_body() {
        assert_eq!(
            redirect_target_of("#REDIRECT [[Template:Protected page]]").as_deref(),
            Some("Template:Protected page")
        );
        // MediaWiki's canonical bodies have a trailing newline and a category.
        assert_eq!(
            redirect_target_of("#REDIRECT [[Template:B]]\n[[Category:C]]").as_deref(),
            Some("Template:B")
        );
        // A piped target redirects to the page, not the label.
        assert_eq!(
            redirect_target_of("#REDIRECT [[Template:B|shown]]").as_deref(),
            Some("Template:B")
        );
        // Underscores are the wire spelling of a space.
        assert_eq!(
            redirect_target_of("#REDIRECT [[Template:Protected_page]]").as_deref(),
            Some("Template:Protected page")
        );
        // Leading comments and whitespace are allowed, and the colon is optional.
        assert_eq!(
            redirect_target_of("\n<!-- c --> #REDIRECT: [[A]]").as_deref(),
            Some("A")
        );
    }

    /// A body that merely mentions the redirect word is not a redirect.
    ///
    /// This is the case that makes anchoring at the start load-bearing: a false
    /// positive silently replaces a template's real content with another page.
    #[test]
    fn a_redirect_word_in_the_middle_is_not_a_redirect() {
        assert_eq!(redirect_target_of("see #REDIRECT [[A]] for details"), None);
        assert_eq!(redirect_target_of("{{foo}}\n#REDIRECT [[A]]"), None);
        // A longer word beginning with the redirect word is not it either.
        assert_eq!(redirect_target_of("#REDIRECTED [[A]]"), None);
        // No link, nothing to redirect to.
        assert_eq!(redirect_target_of("#REDIRECT"), None);
        assert_eq!(redirect_target_of("#REDIRECT "), None);
        assert_eq!(redirect_target_of("#REDIRECT [[]]"), None);
        // Ordinary content.
        assert_eq!(redirect_target_of("just some text"), None);
        assert_eq!(redirect_target_of(""), None);
    }

    #[test]
    fn test_resolve_parser_function() {
        let config = MockSiteConfig::new();
        let target = resolve_template_target(&config, None, "#if:a|b|c").unwrap();
        match target {
            ResolvedTarget::ParserFunction { name, broken, .. } => {
                assert_eq!(name, "if");
                // `if` is a registered function hook, so the invocation is known.
                assert!(!broken);
            }
            other => panic!("expected parser function, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_unknown_hash_prefix_is_broken() {
        let config = MockSiteConfig::new();
        let target = resolve_template_target(&config, None, "#nope:a").unwrap();
        match target {
            ResolvedTarget::ParserFunction { name, broken, .. } => {
                assert_eq!(name, "nope");
                assert!(broken);
            }
            other => panic!("expected parser function, got {:?}", other),
        }
    }

    #[test]
    fn test_no_hash_function_is_not_a_variable() {
        // `dir` is a function hook, not a magic variable: `{{dir}}` (no colon)
        // must resolve as a *template* named `Dir`, not as a variable, while
        // `{{#dir:en}}` is a known parser function. Registering it as a magic
        // word instead would make `{{dir}}` resolve as a variable.
        let config = MockSiteConfig::new();
        match resolve_template_target(&config, None, "dir").unwrap() {
            ResolvedTarget::Template { name, .. } => assert_eq!(name, "Template:Dir"),
            other => panic!("expected template, got {:?}", other),
        }
        match resolve_template_target(&config, None, "#dir:en|bcp47").unwrap() {
            ResolvedTarget::ParserFunction { name, broken, .. } => {
                assert_eq!(name, "dir");
                assert!(!broken);
            }
            other => panic!("expected parser function, got {:?}", other),
        }
    }

    #[test]
    fn test_no_hash_function_with_colon_is_a_parser_function() {
        // `anchorencode` is a registered function hook and is stored in the
        // synonym table *without* a `#`, so `{{anchorencode:[foo]}}` (no colon
        // hash) is a parser function — but only because a colon follows.
        let config = MockSiteConfig::new();
        match resolve_template_target(&config, None, "anchorencode:[foo]").unwrap() {
            ResolvedTarget::ParserFunction {
                name,
                broken,
                pf_arg,
                ..
            } => {
                assert_eq!(name, "anchorencode");
                assert_eq!(pf_arg, "[foo]");
                assert!(!broken);
            }
            other => panic!("expected parser function, got {:?}", other),
        }

        // Without a colon the same name is an ordinary template reference.
        match resolve_template_target(&config, None, "anchorencode").unwrap() {
            ResolvedTarget::Template { name, .. } => {
                assert_eq!(name, "Template:Anchorencode")
            }
            other => panic!("expected template, got {:?}", other),
        }

        // A name that is not a function hook stays a template even with a colon.
        match resolve_template_target(&config, None, "NotAFunction:x").unwrap() {
            ResolvedTarget::Template { name, .. } => assert_eq!(name, "Template:NotAFunction:x"),
            other => panic!("expected template, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_template_title() {
        let config = MockSiteConfig::new();
        // Plain template name defaults to the Template namespace.
        let target = resolve_template_target(&config, None, "Foo").unwrap();
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
    fn test_process_special_magic_word() {
        // Top level: {{!}} is a literal pipe.
        let toks = process_special_magic_word("!", false);
        assert_eq!(toks, vec![Item::Str("|".to_string())]);

        // Inside a template: {{!}} becomes a <td>.
        let toks = process_special_magic_word("!", true);
        assert!(
            matches!(&toks[0], Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::Tag(t)) if t.name == "td")
        );

        // Unsupported magic-word types yield an empty chunk (no panic).
        assert!(process_special_magic_word("something-else", false).is_empty());
    }

    #[test]
    fn test_handle_parser_function() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        let config = MockSiteConfig::new();
        let handler = TemplateHandler;
        // These tests exercise dispatch, not protection, so both maps are empty.
        let page_protection = std::cell::RefCell::new(Default::default());
        let titles_protection = std::cell::RefCell::new(HashMap::new());

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

        let out = handler.handle_template(
            &config,
            None,
            &params,
            "#mwt1".to_string(),
            &token,
            &ProtectionContext::new(&page_protection, &titles_protection),
        );

        // Should be wrapped with mw:Transclusion markers and contain "yes".
        assert!(
            matches!(&out[0], Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(t)) if t.name == "meta")
        );
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "yes"))
        );
    }

    #[test]
    fn test_convert_to_string_keeps_delimiters() {
        // A template whose target resolves to an invalid title bails to literal
        // text with the `{{`/`}}` delimiters preserved (PHP `convertToString`).
        let dp = crate::wikitext::tokens_v2::DataParsoid {
            src: Some("{{Bar%C3%A9}}".to_string()),
            ..Default::default()
        };
        let token = crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(
            crate::wikitext::tokens_v2::SelfclosingTagTk::new("template", vec![], dp),
        );

        let out = TemplateHandler::convert_to_string(&token, false, false);
        let s: String = out
            .iter()
            .map(|it| match it {
                Item::Str(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(s, "{{Bar%C3%A9}}");
    }

    #[test]
    fn test_process_to_string_plain() {
        let tokens = vec![Item::Str("Foo".to_string())];
        let result = process_to_string(&tokens, false);
        assert_eq!(result.target, "Foo");
        assert_eq!(result.rest, None);
    }

    #[test]
    fn test_process_to_string_stops_at_tag() {
        let config = MockSiteConfig::new();

        // `uc:foo [[wikilink]] bar` stringifies up to the wikilink, leaving
        // the wikilink + tail unprocessed. Because the string has a colon, it
        // is still a valid parser-function target (PHP keeps going).
        let tokens = vec![
            Item::Str("uc:foo ".to_string()),
            Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(
                crate::wikitext::tokens_v2::SelfclosingTagTk::new(
                    "wikilink",
                    vec![],
                    crate::wikitext::tokens_v2::DataParsoid::default(),
                ),
            )),
            Item::Str(" bar".to_string()),
        ];
        let result = process_to_string(&tokens, false);
        assert_eq!(result.target, "uc:foo ");
        assert!(result.rest.is_some());

        // Colon present -> still resolvable as a parser function.
        assert!(resolve_template_target_tokens(&config, None, &tokens, false).is_some());

        // No colon -> additional tokens make this an invalid template target.
        let tokens_no_colon = vec![
            Item::Str("foo ".to_string()),
            Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(
                crate::wikitext::tokens_v2::SelfclosingTagTk::new(
                    "wikilink",
                    vec![],
                    crate::wikitext::tokens_v2::DataParsoid::default(),
                ),
            )),
        ];
        assert!(resolve_template_target_tokens(&config, None, &tokens_no_colon, false).is_none());
    }

    #[test]
    fn test_process_to_string_ignores_comments() {
        let tokens = vec![
            Item::Str("Foo".to_string()),
            Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::Comment(
                crate::wikitext::tokens_v2::CommentTk::new(
                    "ignored",
                    crate::wikitext::tokens_v2::DataParsoid::default(),
                ),
            )),
        ];
        let result = process_to_string(&tokens, false);
        assert_eq!(result.target, "Foo");
        assert_eq!(result.rest, None);
    }

    #[test]
    fn test_process_to_string_quotes() {
        let mut quote = crate::wikitext::tokens_v2::SelfclosingTagTk::new(
            "mw-quote",
            vec![],
            crate::wikitext::tokens_v2::DataParsoid::default(),
        );
        quote.add_attribute_str("value", "'");

        let tokens = vec![
            Item::Str("a".to_string()),
            Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(
                quote,
            )),
            Item::Str("b".to_string()),
        ];
        let result = process_to_string(&tokens, false);
        assert_eq!(result.target, "a'b");
        assert_eq!(result.rest, None);
    }

    #[test]
    fn test_process_to_string_includes() {
        use crate::wikitext::tokens_v2::ParsoidToken;

        let include =
            ParsoidToken::SelfclosingTag(crate::wikitext::tokens_v2::SelfclosingTagTk::new(
                "meta",
                vec![crate::wikitext::tokens_v2::KV {
                    key: crate::wikitext::tokens_v2::KeyValue::Str("typeof".to_string()),
                    value: crate::wikitext::tokens_v2::KeyValue::Str(
                        "mw:Includes/OnlyInclude".to_string(),
                    ),
                    src_offsets: None,
                    ksrc: None,
                    vsrc: None,
                }],
                crate::wikitext::tokens_v2::DataParsoid::default(),
            ));

        let tokens = vec![Item::Str("a".to_string()), Item::Tok(include)];
        let result = process_to_string(&tokens, false);
        assert_eq!(result.target, "a");
        assert_eq!(result.rest, None);
    }

    #[tokio::test]
    async fn test_expand_template_natively() {
        use crate::mock::MockDataSource;
        use crate::wikitext::tokens_v2::{DataParsoid, KV, KeyValue, ParsoidToken, TagTk};

        let source = MockDataSource::new();
        source.add_template("Template:Foo", "Hello {{{1}}}!");

        let title = Title::new(10, "Foo");

        let args = vec![
            KV {
                key: KeyValue::Str("Foo".to_string()),
                value: KeyValue::Str("".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str("".to_string()),
                value: KeyValue::Str("world".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ];
        let params = Params::new(args);

        let token = ParsoidToken::Tag(TagTk::new("template", vec![], DataParsoid::default()));

        let out = TemplateHandler::expand_template_natively(
            &crate::mock::MockSiteConfig::default(),
            &source,
            "Template:Foo",
            &title,
            &params,
            "#mwt1".to_string(),
            &token,
        )
        .await;

        // Wrapped in mw:Transclusion markers, and the source was expanded so
        // that `{{{1}}}` resolves to `world`.
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "meta"));
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "Hello world"))
        );
    }

    #[test]
    fn test_parse_template_arg_src() {
        assert_eq!(
            parse_template_arg_src("{{{1}}}"),
            Some(("1".to_string(), None))
        );
        assert_eq!(
            parse_template_arg_src("{{{ name | default }}}"),
            Some(("name".to_string(), Some(" default ".to_string())))
        );
        assert_eq!(parse_template_arg_src("{{{}}}"), None);
        assert_eq!(parse_template_arg_src("not-an-arg"), None);
    }

    #[test]
    fn test_process_pfifies_template() {
        use crate::wikitext::tokens_v2::{DataParsoid, KV, KeyValue, SelfclosingTagTk};

        let config = MockSiteConfig::new();
        let title = TitleParser::parse("Template:Foo", &config);
        let frame = crate::pipeline::frame::Frame::new(title, vec![]);
        let about = std::cell::Cell::new(0usize);
        let page_protection = std::cell::RefCell::new(Default::default());
        let titles_protection = std::cell::RefCell::new(HashMap::new());

        // A `template` token with `{{#if:x|yes|no}}`.
        let mut stt = SelfclosingTagTk::new("template", vec![], DataParsoid::default());
        stt.attribs = vec![
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

        let input = vec![Item::Tok(ParsoidToken::SelfclosingTag(stt))];
        let out = TemplateHandler.process(
            &config,
            &frame,
            &about,
            &ProtectionContext::new(&page_protection, &titles_protection),
            input,
        );

        // Wrapped with mw:Transclusion and contains "yes".
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "meta"));
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "yes"))
        );
    }

    #[test]
    fn test_enforce_template_constraints() {
        let config = MockSiteConfig::new();
        let title = TitleParser::parse("Template:Foo", &config);
        let frame = crate::pipeline::frame::Frame::new(title.clone(), vec![]);

        // Same title => loop => error tokens.
        let err = enforce_template_constraints(&config, &frame, "Template:Foo", &title, 40, false);
        assert!(err.is_some());
        let tokens = err.unwrap();
        assert!(matches!(&tokens[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "span"));
        assert!(
            tokens
                .iter()
                .any(|it| matches!(it, Item::Str(s) if s.contains("Template loop")))
        );

        // ignore_loop bypasses loop detection.
        assert!(
            enforce_template_constraints(&config, &frame, "Template:Foo", &title, 40, true)
                .is_none()
        );
    }

    #[test]
    fn test_process_template_tokens() {
        use crate::wikitext::tokens_v2::{DataParsoid, TagTk};

        let mut tk = TagTk::new("span", vec![], DataParsoid::default());
        tk.data_parsoid.tsr = Some(crate::wikitext::tokens_v2::SourceRange::new(0, 4));
        let chunk = vec![
            Item::Tok(ParsoidToken::Tag(tk)),
            Item::Str("x".to_string()),
            Item::Tok(ParsoidToken::Eof(crate::wikitext::tokens_v2::EOFTk)),
        ];

        let out = process_template_tokens(chunk, true);
        assert_eq!(out.len(), 2);
        if let Item::Tok(ParsoidToken::Tag(t)) = &out[0] {
            assert!(t.data_parsoid.tsr.is_none());
        }
    }

    #[test]
    fn test_handle_template_arg() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        let config = MockSiteConfig::new();
        let title = TitleParser::parse("Template:Foo", &config);
        let args = vec![KV {
            key: KeyValue::Str("".to_string()),
            value: KeyValue::Str("world".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        }];
        let frame = crate::pipeline::frame::Frame::new(title, args);

        let token =
            ParsoidToken::SelfclosingTag(crate::wikitext::tokens_v2::SelfclosingTagTk::new(
                "templatearg",
                vec![],
                crate::wikitext::tokens_v2::DataParsoid::default(),
            ));

        let handler = TemplateHandler;
        let out = handler.handle_template_arg(&frame, "{{{1}}}", "#mwt1".to_string(), &token, true);

        // Wrapped in mw:Param markers, content resolves to "world".
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "meta"));
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "world"))
        );
    }
}
