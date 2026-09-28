//! Public `Parser` facade — ties the V2 token pipeline together into a single
//! entry point for wikitext → HTML.
//!
//! This is the Rust port of PHP Parsoid's top-level `Parser`/`Wikitext` entry
//! points, using the V2 token pipeline (`PegTokenizer` → TT2 handlers →
//! `TreeBuilderStage`).

use crate::dom::node::Node;
use crate::error::Result;
use crate::options::ParserOptions;
use crate::pipeline::frame::Frame;
use crate::pipeline::template_encapsulator::{ParamInfo, TemplateEncapsulator, template_info_from};
use crate::pipeline::template_handler::{
    ProtectionContext, TemplateHandler, resolve_template_target,
};
use crate::pipeline::tree_builder_stage::TreeBuilderStage;
use crate::title::TitleParser;
use crate::traits::{DataSource, ProtectionEntry, SiteConfig};
use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};
use crate::wikitext::tokens_v2::{Either, Item, ParsoidToken};

type ResolvedTarget = crate::pipeline::template_handler::ResolvedTarget;

/// Extract the raw template target (the text between `{{` and the first
/// top-level `|` or the closing `}}`) from a template token's source string.
///
/// This is the *unprocessed* target as written in the wikitext, before the
/// PHP preprocessor strips comments. Used to detect a comment in the target
/// (`{{f<!---->oo}}`), which suppresses `mw:Transclusion` encapsulation.
fn raw_template_target(src: &str) -> Option<&str> {
    let inner = src.strip_prefix("{{")?.strip_suffix("}}")?;
    Some(inner.split_once('|').map_or(inner, |(t, _)| t))
}

/// Track PHP's `tableDataBlock` context over a token: true while the walk sits
/// inside an unclosed `table` tag.
///
/// A template body expanded from a call site inherits the flag, which is what
/// lets the body's `|` split cells when the governing `{|` came from an *earlier*
/// expansion (`{{start}}\n|a\n{{end}}` with `Template:start` = `{|`). Free rather than
/// a closure because two functions track it over different streams.
fn track_table(item: &Item, depth: &mut usize) {
    match item {
        Item::Tok(ParsoidToken::Tag(tk)) if tk.name == "table" => *depth += 1,
        Item::Tok(ParsoidToken::EndTag(tk)) if tk.name == "table" => {
            *depth = depth.saturating_sub(1)
        }
        _ => {}
    }
}

/// Flag a text-returning parser function's branch as such, in place.
///
/// See [`crate::wikitext::tokens_v2::TempData::in_text_branch`]: core answers
/// `#if`/`#ifeq`/`#ifexpr`/`#iferror` with a *string* and re-tokenizes it, so the
/// wikitext targets inside the branch were resolved before any attribute pass
/// saw them. rustoid expands the branch in place instead, which leaves those
/// targets templated; the flag is what tells the marking to behave as the service
/// does.
fn mark_in_text_branch(items: &mut [Item]) {
    for item in items.iter_mut() {
        let Item::Tok(tok) = item else { continue };
        if let Some(dp) = tok.data_parsoid_mut() {
            dp.tmp.in_text_branch = Some(true);
        }
        strip_token_source_range(tok);
    }
}

/// Drop the source *range* from `items`, recursing through attribute values
/// (which is where a wikilink's target lives).
///
/// Only the range goes. `src` and `srcContent` are left alone: clearing those
/// too once made `Unix` take over 60 s and render nothing, and the loop that
/// reads them has not been found. `ONLINE-PARITY.md` records it.
fn strip_source_ranges(items: &mut [Item]) {
    for item in items.iter_mut() {
        let Item::Tok(tok) = item else { continue };
        strip_token_source_range(tok);
    }
}

fn strip_token_source_range(tok: &mut ParsoidToken) {
    if let Some(dp) = tok.data_parsoid_mut() {
        dp.tsr = None;
        dp.dsr = None;
    }
    if let Some(attribs) = tok.attribs_mut() {
        for kv in attribs.iter_mut() {
            if let crate::wikitext::tokens_v2::KeyValue::Tokens(nested) = &mut kv.value {
                strip_source_ranges(nested);
            }
        }
    }
}

/// The `<span class="error">…</span>` a tripped expansion limit leaves in place
/// of the expansion.
///
/// Both messages are hardcoded in PHP rather than i18n lookups, and the wrapper
/// is `<span class="error">` — *not* the `<strong class="error">` that the
/// preview-only messages use. Getting either wrong makes the output differ from
/// the wiki's for exactly the pages where a limit tripped.
fn error_span(message: &str) -> Vec<Item> {
    use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, KV, KeyValue, TagTk};

    let mut span = TagTk::new("span", vec![], DataParsoid::default());
    span.attribs.push(KV {
        key: KeyValue::Str("class".to_string()),
        value: KeyValue::Str("error".to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });

    vec![
        Item::Tok(ParsoidToken::Tag(span)),
        Item::Str(message.to_string()),
        Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "span",
            vec![],
            DataParsoid::default(),
        ))),
    ]
}

/// If `item` is a `<pre format="wikitext">` extension token, return the
/// self-closing token; otherwise `None`.
fn wikitext_pre_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    if stt.name != "extension" {
        return None;
    }
    let name = stt
        .attribs
        .iter()
        .find(|a| a.key.as_str() == Some("name"))
        .and_then(|a| a.value.as_str());
    let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
    let format = attrs
        .iter()
        .find(|kv| kv.key.as_str() == Some("format"))
        .and_then(|kv| kv.value.as_str());
    if name == Some("pre") && format == Some("wikitext") {
        Some(stt)
    } else {
        None
    }
}

/// If `item` is a `<templatestyles>` extension token, return the self-closing
/// token.
///
/// Recognised by the extension token's `name` attribute, the same way the
/// extension handler dispatches.
fn templatestyles_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    if stt.name != "extension" {
        return None;
    }
    let name = stt
        .attribs
        .iter()
        .find(|a| a.key.as_str() == Some("name"))
        .and_then(|a| a.value.as_str());
    (name == Some("templatestyles")).then_some(stt)
}

/// Normalise a `data-mw` attribute value for use as a title or a selector.
///
/// Two spellings reach this: a plain `"Z.css"` (from a literal tag, whose
/// quotes the tokenizer already handled) and a backslash-escaped `\"Z.css\"`
/// (from `#tag`, where the value travelled through Lua and `pf_tag` could not
/// unescape it). Both mean `Z.css`.
fn unquote_attr_value(raw: &str) -> String {
    let unescaped = raw.replace("\\\"", "\"").replace("\\'", "'");
    let trimmed = unescaped.trim();
    let inner = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        });
    match inner {
        // A `""` value means empty, not a page named `"`.
        Some("") => String::new(),
        Some(inner) => inner.trim().to_string(),
        None => trimmed.to_string(),
    }
}

/// Resolve a `<templatestyles src>` value to a title.
///
/// A bare name is in the Template namespace (`Hlist/styles.css` is
/// `Template:Hlist/styles.css`), while an explicit prefix is honoured as written
/// (`Module:Hatnote/styles.css`). Mirrors `$wgTemplateStylesDefaultNamespace`.
fn templatestyles_title(config: &dyn crate::traits::SiteConfig, src: &str) -> crate::title::Title {
    let (namespace, rest) = match src.split_once(':') {
        Some((head, rest)) if config.namespaces().values().any(|ns| ns.canonical == head) => {
            (Some(head), rest)
        }
        _ => (None, src),
    };
    let (ns_id, name) = match namespace {
        Some(ns) => (config.namespace_id(ns).unwrap_or(10), rest),
        None => (10, rest),
    };
    crate::title::Title::new(ns_id, name.replace('_', " "))
}

/// Extract the raw body source from a `<pre format="wikitext">` extension token.
fn extension_body(stt: &crate::wikitext::tokens_v2::SelfclosingTagTk) -> String {
    let ext_src = stt
        .attribs
        .iter()
        .find(|a| a.key.as_str() == Some("source"))
        .and_then(|a| a.value.as_str())
        .unwrap_or("");
    crate::pipeline::extension_handler::extract_ext_body(stt, ext_src)
}

/// If `item` is a `<gallery>` extension token, return the self-closing token.
fn gallery_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    if stt.name != "extension" {
        return None;
    }
    let name = stt
        .attribs
        .iter()
        .find(|a| a.key.as_str() == Some("name"))
        .and_then(|a| a.value.as_str());
    if name == Some("gallery") {
        Some(stt)
    } else {
        None
    }
}

/// If `item` is a `divtag`/`spantag` extension token, return the self-closing
/// token, the extension tag name, and the wrapper tag name (`div`/`span`).
/// Mirrors the `ParserHook` transparent-wrapper extensions.
fn wrapper_tag_target(
    item: &Item,
) -> Option<(
    &crate::wikitext::tokens_v2::SelfclosingTagTk,
    &'static str,
    &'static str,
)> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    if stt.name != "extension" {
        return None;
    }
    match stt
        .attribs
        .iter()
        .find(|a| a.key.as_str() == Some("name"))
        .and_then(|a| a.value.as_str())
    {
        Some("divtag") => Some((stt, "divtag", "div")),
        Some("spantag") => Some((stt, "spantag", "span")),
        _ => None,
    }
}

/// If `item` is an `indicator` extension token, return it.
///
/// The indicator is handled with the wrapper extensions rather than inside
/// `extension_handler::run` because it needs the sub-fragment machinery: its
/// `<meta>` is rendering-transparent and must keep the `about` of the enclosing
/// transclusion, so it is stashed and re-inserted by `unpack_dom_fragments`
/// exactly as a `divtag` body is.
fn indicator_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    (stt.name == "extension"
        && stt
            .attribs
            .iter()
            .find(|a| a.key.as_str() == Some("name"))
            .and_then(|a| a.value.as_str())
            == Some("indicator"))
    .then_some(stt)
}

/// Is this item a `template`/`template3` token? Used to decide whether the
/// template token's target chunk still needs template expansion (see
/// `Parser::expand_target_templates`).
fn is_template_item(item: &Item) -> bool {
    matches!(item, Item::Tok(ParsoidToken::SelfclosingTag(t))
        if t.name == "template" || t.name == "template3")
}

/// Report whether a `mw:maybeContent` value contains a nested wikilink that must
/// trigger PHP's `Link-in-link` bail. A `[[` inside a `<nowiki>` body, a
/// recognized HTML tag's quoted attribute value, a template, or a language
/// variant is not a nested link, so those are skipped.
fn key_value_has_nested_wikilink(value: &crate::wikitext::tokens_v2::KeyValue) -> bool {
    use crate::wikitext::token_utils::key_value_to_string;
    use crate::wikitext::tokenizer_v2::contains_toplevel_wikilink_open;

    match value {
        // A token array already distinguishes a nested `wikilink` token.
        crate::wikitext::tokens_v2::KeyValue::Tokens(items) => items.iter().any(
            |it| matches!(it, Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "wikilink"),
        ),
        crate::wikitext::tokens_v2::KeyValue::Str(_) => {
            contains_toplevel_wikilink_open(&key_value_to_string(value))
        }
    }
}

/// Reconstruct the full wikilink source (`[[href|content0|content1…]]`) from a
/// `wikilink` token's `href` and `mw:maybeContent` KVs. Used by the media-in-link
/// bail to re-tokenize the source without the leading `[` (mirrors PHP's
/// `bailTokens`, which strips the first `[` from the TSR source). Nested wikilinks
/// are preserved verbatim (our tokenizer keeps them as `[[…]]` text).
fn reconstruct_link_src(stt: &crate::wikitext::tokens_v2::SelfclosingTagTk, href: &str) -> String {
    use crate::wikitext::token_utils::key_value_to_string;
    let mut src = String::from("[[");
    src.push_str(href);
    for kv in stt
        .attribs
        .iter()
        .filter(|kv| kv.key.as_str() == Some("mw:maybeContent"))
    {
        src.push('|');
        src.push_str(&key_value_to_string(&kv.value));
    }
    src.push_str("]]");
    src
}

/// The `mw:DOMFragment` placeholder starting at `items[i]`, if there is one:
/// the exclusive end index, the placeholder's tokens, and the name its strip
/// marker carries.
///
/// A placeholder is either a bare `mw:dom-fragment-token` (a gallery) or a
/// `typeof="mw:DOMFragment"` element pair — `<style>…</style>`,
/// `<span>…</span>` — whose body is opaque.
fn placeholder_span(items: &[Item], i: usize) -> Option<(usize, Vec<Item>, String)> {
    fn attrs_typeof(t: &crate::wikitext::tokens_v2::TagTk) -> Option<&str> {
        t.attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("typeof"))
            .and_then(|kv| kv.value.as_str())
    }
    let is_fragment = |ty: Option<&str>| {
        ty.is_some_and(|ty| {
            ty.split_whitespace()
                .any(|t| t == "mw:DOMFragment" || t.starts_with("mw:DOMFragment/"))
        })
    };
    match &items[i] {
        Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "mw:dom-fragment-token" => {
            Some((i + 1, vec![items[i].clone()], "gallery".to_string()))
        }
        Item::Tok(ParsoidToken::Tag(t)) if is_fragment(attrs_typeof(t)) => {
            let name = t.name.clone();
            let mut depth = 0usize;
            for (k, item) in items.iter().enumerate().skip(i) {
                match item {
                    Item::Tok(ParsoidToken::Tag(u)) if u.name == name => depth += 1,
                    Item::Tok(ParsoidToken::EndTag(u)) if u.name == name => {
                        depth -= 1;
                        if depth == 0 {
                            // A `<style>` placeholder is a `<templatestyles>`;
                            // the name is what `Module:Infobox` matches on.
                            let tag = if name == "style" {
                                "templatestyles".to_string()
                            } else {
                                "extension".to_string()
                            };
                            return Some((k + 1, items[i..=k].to_vec(), tag));
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

/// Emit the `mw:dom-fragment-token` placeholder for a `<gallery>` extension,
/// referencing the pre-built gallery fragment by `id` (mirrors the generic
/// extension encapsulation in `extension_handler::gallery_items`).
fn emit_gallery_placeholder(stt: &crate::wikitext::tokens_v2::SelfclosingTagTk, id: usize) -> Item {
    use crate::wikitext::tokens_v2::{KeyValue, SelfclosingTagTk};

    let mut dp = stt.data_parsoid.clone();
    dp.src = None;
    dp.src_content = None;
    // Keep `ext_tag_offsets` (the `<gallery …>` open/close widths) so the
    // `mw:DOMFragment` placeholder carries them into ComputeDSR, which stamps
    // the DSR open/close widths. `UnpackDOMFragments::transfer_metadata` then
    // forwards that DSR (`tsr`/`extTagOffsets`) onto the unpacked `<ul>`, so the
    // selser serializer can recover the original body via `getOrigSrc`.
    let mut frag_tok = SelfclosingTagTk::new("mw:dom-fragment-token", vec![], dp);
    frag_tok.attribs.push(crate::wikitext::tokens_v2::KV {
        key: KeyValue::Str("data-fragment-id".to_string()),
        value: KeyValue::Str(id.to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });
    Item::Tok(ParsoidToken::SelfclosingTag(frag_tok))
}

/// The `<style typeof="mw:DOMFragment" data-fragment-id=…>` + `</style>`
/// placeholder pair for a resolved stylesheet, referencing the sub-fragment by
/// `id`.
///
/// Mirrors the tail of `extension_handler::style_items`: a `<style>` fragment is
/// reached through a `<style typeof="mw:DOMFragment">` wrapper, so the tree
/// builder stashes the real element and `unpack_dom_fragments` restores it once
/// the surrounding structure is known.
fn emit_style_placeholder(
    stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    id: usize,
) -> Vec<Item> {
    use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, TagTk};

    let mut dp = stt.data_parsoid.clone();
    dp.src = None;
    dp.src_content = None;
    dp.ext_tag_offsets = None;

    let mut open = TagTk::new("style", vec![], dp);
    open.add_attribute_str("typeof", "mw:DOMFragment");
    open.add_attribute_str("data-fragment-id", id.to_string().as_str());
    let close = EndTagTk::new("style", vec![], DataParsoid::default());

    vec![
        Item::Tok(ParsoidToken::Tag(open)),
        Item::Tok(ParsoidToken::EndTag(close)),
    ]
}

/// Emit the `<pre typeof="mw:Extension/pre">` + `mw:dom-fragment-token` +
/// `</pre>` placeholder sequence for a `<pre format="wikitext">` extension,
/// referencing the sub-fragment by `id`.
fn emit_pre_placeholder(
    stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    id: usize,
    out: &mut Vec<Item>,
) {
    emit_wrapper_placeholder(stt, "pre", "mw:Extension/pre", id, out);
}

/// Emit the `<div typeof="mw:Extension/divtag">`/`<span …/spantag>` +
/// `mw:dom-fragment-token` + closing placeholder sequence for a `divtag`/`spantag`
/// extension, referencing the sub-fragment by `id` (mirrors `getWrapperTokens` +
/// `tunnelDOMThroughTokens`, which emit only the shallow wrapper tags around a
/// tunnelled DOM fragment). `ext_name` is the extension tag name (`divtag`/
/// `spantag`), distinct from the HTML wrapper tag (`div`/`span`).
fn emit_wrapper_extension_placeholder(
    stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    ext_name: &str,
    wrapper: &str,
    id: usize,
    out: &mut Vec<Item>,
) {
    let typeof_ = format!("mw:Extension/{ext_name}");
    emit_wrapper_placeholder(stt, wrapper, &typeof_, id, out);
}

/// The common placeholder emitter: `<tag typeof=…>` + `mw:dom-fragment-token` +
/// `</tag>`, with the start-tag attributes sanitized as for the wrapper tag.
fn emit_wrapper_placeholder(
    stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    tag: &str,
    typeof_: &str,
    id: usize,
    out: &mut Vec<Item>,
) {
    use crate::wikitext::tokens_v2::{EndTagTk, KeyValue, SelfclosingTagTk, TagTk};

    let attrs: Vec<crate::wikitext::tokens_v2::KV> =
        crate::pipeline::extension_handler::extension_kv_attrs(stt);
    let sanitized = crate::sanitizer::sanitize_tag_attrs(tag, attrs, |_proto| true);
    let mut dp = stt.data_parsoid.clone();
    dp.src = None;
    dp.src_content = None;
    dp.ext_tag_offsets = None;
    dp.stx = Some("html".to_string());
    let mut open = TagTk::new(tag, sanitized, dp);
    open.data_mw = None;
    open.add_attribute_str("typeof", typeof_);

    let mut frag = SelfclosingTagTk::new("mw:dom-fragment-token", vec![], stt.data_parsoid.clone());
    frag.attribs.push(crate::wikitext::tokens_v2::KV {
        key: KeyValue::Str("data-fragment-id".to_string()),
        value: KeyValue::Str(id.to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });

    out.push(Item::Tok(ParsoidToken::Tag(open)));
    out.push(Item::Tok(ParsoidToken::SelfclosingTag(frag)));
    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        tag,
        vec![],
        crate::wikitext::tokens_v2::DataParsoid::default(),
    ))));
}

/// Emit the `mw:dom-fragment-token` placeholder for a resolved `<indicator>`,
/// referencing the pre-built `<meta>` fragment by `id`.
///
/// Unlike the wrapper extensions, whose placeholder is a real element pair around
/// a tunnelled body, an indicator *is* the fragment: the `<meta>` is the whole
/// output, and `typeof`/`data-mw` describe it. So both travel on the bare
/// `mw:dom-fragment-token` here, and the tree builder carries them onto the
/// element it inserts. Those keys describe the *element the placeholder stands
/// for* — the indicator's own `name` and body — not the placeholder itself.
fn emit_indicator_placeholder(
    stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    id: usize,
) -> Vec<Item> {
    use crate::wikitext::tokens_v2::{KV, SelfclosingTagTk};

    let mut dp = stt.data_parsoid.clone();
    dp.src = None;
    dp.src_content = None;
    dp.ext_tag_offsets = None;

    let mut frag = SelfclosingTagTk::new("mw:dom-fragment-token", vec![], dp);
    let kv = |k: &str, v: String| KV {
        key: crate::wikitext::tokens_v2::KeyValue::Str(k.to_string()),
        value: crate::wikitext::tokens_v2::KeyValue::Str(v),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    };
    frag.attribs
        .push(kv("typeof", "mw:Extension/indicator".to_string()));
    frag.attribs
        .push(kv("data-mw", indicator_placeholder_data_mw(stt)));
    frag.attribs.push(kv("data-fragment-id", id.to_string()));

    vec![Item::Tok(ParsoidToken::SelfclosingTag(frag))]
}

/// The `data-mw` an indicator placeholder carries, taken from the fragment the
/// caller already built so the two cannot disagree.
fn indicator_placeholder_data_mw(stt: &crate::wikitext::tokens_v2::SelfclosingTagTk) -> String {
    let body = extension_body(stt);
    let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
    crate::pipeline::extension_handler::indicator_data_mw(&body, &attrs)
}

/// Extract the body-content children from a tree-builder document (`<html>`
/// wrapped), returning them as a fragment document. Mirrors the `body`
/// extraction in `HtmlSerializer::split_structure`.
fn extract_fragment_children(ast: &Node) -> Node {
    for child in &ast.children {
        if let crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::Other(tag)) =
            &child.kind
            && tag == "html"
        {
            let mut frag = crate::dom::node::Node::document();
            frag.children = child.children.clone();
            return frag;
        }
    }
    // No `<html>` wrapper (e.g. a plain text body returned as a text node).
    ast.clone()
}

/// Render an already-tokenized inline caption fragment into a fragment document
/// (mirrors PHP's `processContentInPipeline` with `inlineContext => true`).
/// Resolves wikilinks, external links/autolinks, and behavior switches, flushes
/// pending quotes with a synthetic EOF, then runs the inline tree builder and
/// the post-pwrap transforms (so transclusion markers become `mw:Transclusion`
/// spans).
///
/// Shared by the `Parser::build_inline_fragment` caption path and the gallery
/// caption renderer (`gallery::caption_to_nodes`), so both produce identical
/// markup. `fragments`/`next_id` are threaded through for nested media captions.
pub fn render_inline_fragment(
    config: &dyn SiteConfig,
    tokens: Vec<Item>,
    fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
    next_id: &std::cell::Cell<usize>,
) -> Node {
    use crate::pipeline::external_link_handler::{on_ext_link, on_url_link};
    use crate::pipeline::wiki_link_render::{
        WikiLinkContext, get_wiki_link_target_info, render_wiki_link_dispatched,
    };
    use crate::wikitext::token_utils::key_value_to_string;

    // Step 1: render wikilinks (`[[…]]` → `<a>`/`<link>` tags).
    let mut link_ctx = WikiLinkContext::new(config);
    let tokens: Vec<Item> = tokens
        .into_iter()
        .flat_map(|item| {
            let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = &item else {
                return vec![item];
            };
            if stt.name != "wikilink" {
                return vec![item];
            }
            let href = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .map(|kv| key_value_to_string(&kv.value))
                .unwrap_or_default();
            let href_src = href.clone();
            let target = match get_wiki_link_target_info(&link_ctx, &href, &href_src) {
                Ok(t) => t,
                Err(_) => {
                    // Invalid title (bad chars, stray `%hh`, multiple colons): bail
                    // the link back to wikitext. Mirrors PHP's
                    // `WikiLinkHandler::bailTokens`, which re-runs the link's
                    // *source* through the token pipeline with the leading `[`
                    // dropped — so `[[http://x|y]]` becomes `[http://x|y]]`, whose
                    // `[...]` then re-tokenizes as an external link instead of a
                    // literal `[[`.
                    //
                    // `tsr`/`src` are relative to the source the link was
                    // tokenized from (a template body, when the link came in as
                    // an argument), which is exactly what PHP's
                    // `$tsr->substr($frameSrc)` uses.
                    let link_src = stt
                        .data_parsoid
                        .tsr
                        .as_ref()
                        .map(|tsr| tsr.substr(""))
                        .filter(|s| s.len() > 1)
                        .or_else(|| stt.data_parsoid.src.clone().filter(|s| s.len() > 1));
                    let Some(rest) = link_src.as_deref().map(|s| &s[1..]) else {
                        return vec![Item::Str(format!("[[{href}]]"))];
                    };
                    return crate::pipeline::template_handler::tokenize_wikitext_to_items_with_sol(
                        rest,
                        false,
                        config.extension_tags(),
                        false,
                    );
                }
            };
            render_wiki_link_dispatched(
                &mut link_ctx,
                &ParsoidToken::SelfclosingTag(stt.clone()),
                &target,
                false,
                fragments,
                next_id,
                &mut |items| {
                    let mut f = std::collections::HashMap::new();
                    let id = std::cell::Cell::new(0usize);
                    render_inline_fragment(config, items, &mut f, &id)
                },
            )
        })
        .collect();

    // Step 2: render external links / autolinks.
    let clean = |href: &str| {
        crate::sanitizer::clean_url(href, "external", |proto| config.has_valid_protocol(proto))
    };
    let clean_link = |href: &str| {
        crate::sanitizer::clean_url(href, "wikilink", |proto| config.has_valid_protocol(proto))
    };
    let mut out: Vec<Item> = Vec::with_capacity(tokens.len());
    for item in tokens {
        let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = &item else {
            out.push(item);
            continue;
        };
        match stt.name.as_str() {
            "extlink" => {
                let token = ParsoidToken::SelfclosingTag(stt.clone());
                let render_content = |items: Vec<Item>| {
                    // Re-render link content (nested `[[…]]`) inline and wrap the
                    // result in a DOM-fragment token (mirrors PHP's
                    // `getDOMFragmentToken`).
                    let frag = render_inline_fragment(config, items, fragments, next_id);
                    crate::pipeline::wiki_link_render::dom_fragment_token(
                        frag, &token, fragments, next_id,
                    )
                };
                match on_ext_link(&token, clean, config.relative_link_prefix(), render_content) {
                    Some(rendered) => out.extend(rendered),
                    None => out.push(item),
                }
            }
            "urllink" => {
                let content_href = stt
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))
                    .and_then(|kv| kv.value.as_str())
                    .unwrap_or("")
                    .to_string();
                match on_url_link(
                    &ParsoidToken::SelfclosingTag(stt.clone()),
                    &content_href,
                    clean,
                    clean_link,
                ) {
                    Some(rendered) => out.extend(rendered),
                    None => out.push(item),
                }
            }
            _ => out.push(item),
        }
    }
    let mut tokens = out;

    // Step 3: behavior switches → `mw:PageProp` metas.
    tokens = crate::pipeline::behavior_switch_handler::BehaviorSwitchHandler.run(tokens);

    // Step 3b: `language-variant` → `mw:LanguageVariant` elements (mirrors the
    // TT2 `LanguageVariantHandler`, which runs in the token pipeline before tree
    // building). Captions can carry language-converter markup (`-{R|…}-`), so the
    // inline fragment path must render them too.
    tokens = crate::pipeline::language_variant_handler::LanguageVariantHandler.run(config, tokens);

    // Step 4: flush pending quotes with a synthetic EOF, build the inline tree,
    // and apply the pwrap transforms (transclusion encapsulation).
    tokens.push(Item::Tok(ParsoidToken::Eof(
        crate::wikitext::tokens_v2::EOFTk,
    )));
    let stage = TreeBuilderStage::new(true);
    let mut frag = extract_fragment_children(&stage.to_ast_with_fragments(
        tokens,
        None,
        config,
        fragments.clone(),
        None,
    ));
    let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&frag);
    crate::pipeline::tree_builder_html::post_pwrap_transforms(&mut frag, &depths, None);
    // AddLinkAttributes also runs in the nested fragment pipeline (mirrors PHP's
    // `NESTED_PIPELINE_DOM_TRANSFORMS`, where `linkclasses` precedes
    // `linkneighbours+dom-unpack`): it assigns `class`/`rel` to links inside
    // media/gallery captions, which would otherwise only get the bare `rel`
    // set by the token handler.
    crate::pipeline::add_link_attributes::run(&mut frag, config);
    frag
}

/// Mark the `{{…}}` template tokens inside an argument value as "expand in
/// template context".
///
/// PHP expands argument values under a hard-coded `inTemplate => true`
/// (`AttributeTransformManager::process` → `Frame::expand`), which affects both
/// `processSpecialMagicWord` (`{{!}}` → `<td>` cell token instead of a literal
/// `|`) and `wrapTemplates` (a template nested inside an argument value is not
/// separately encapsulated). The template *body* uses the caller's flag instead
/// (`processTemplateSource` forwards `$this->options`).
fn mark_arg_value_tokens(items: &mut [Item]) {
    for item in items.iter_mut() {
        let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item else {
            continue;
        };
        if t.name != "template" && t.name != "template3" {
            continue;
        }
        t.data_parsoid.tmp.in_arg_value = true;
        for kv in t.attribs.iter_mut() {
            for value in [&mut kv.key, &mut kv.value] {
                if let crate::wikitext::tokens_v2::KeyValue::Tokens(inner) = value {
                    mark_arg_value_tokens(inner);
                }
            }
        }
    }
}

/// Re-tokenize an all-plain-string template expansion at start of line.
///
/// A template body consisting solely of argument references (e.g. `1x`'s
/// `{{{1}}}`) yields its substituted text at the position where the body began,
/// which is start of line — so leading list/table syntax must be tokenized as
/// such (`{{1x|!!foo}}` → a `<th>`). A body with any other content before the
/// trailing text (`{{{attr|}}}{{{cmt|}}}| foo`) is left alone.
///
/// Returns the input unchanged when it is not all plain strings.
fn re_tokenize_sol_prefix(spliced: Vec<Item>, ext_tags: &[String]) -> Vec<Item> {
    if !spliced.iter().all(|it| matches!(it, Item::Str(_))) {
        return spliced;
    }
    let text: String = spliced
        .iter()
        .map(|it| match it {
            Item::Str(s) => s.as_str(),
            _ => "",
        })
        .collect();
    crate::pipeline::template_handler::tokenize_wikitext_to_items(
        &text, /* in_template */ true, ext_tags,
    )
}

/// Flatten `mw:Nowiki` spans into their text content (mirrors PHP
/// `Pre::sourceToDom` + `removeNowikiEscapesFromContent`): inside a
/// `<pre format="wikitext">` body the `<nowiki>` content has already been
/// protected from markup expansion during the fragment parse, so the nowiki
/// wrapper is redundant and is unwrapped to its literal text. The `mw:Entity`
/// children contribute their *decoded* text (so `&amp;` re-escapes correctly
/// when the `<pre>` is rendered).
fn flatten_nowiki_spans(root: &mut Node) {
    let children = std::mem::take(&mut root.children);
    let mut out: Vec<Node> = Vec::with_capacity(children.len());
    for mut child in children {
        if is_nowiki_span(&child) {
            let text = text_content(&child);
            if !text.is_empty() {
                out.push(Node::text(text));
            }
        } else {
            if matches!(child.kind, crate::dom::node::NodeKind::Element(_)) {
                flatten_nowiki_spans(&mut child);
            }
            out.push(child);
        }
    }
    root.children = out;
}

/// Is this an element node carrying a `mw:Nowiki` `typeof`?
fn is_nowiki_span(node: &Node) -> bool {
    node.get_attr("typeof")
        .is_some_and(|ty| ty.split_whitespace().any(|t| t == "mw:Nowiki"))
}

/// Concatenate all descendant text nodes (a faithful `Node::textContent`).
fn text_content(node: &Node) -> String {
    let mut out = String::new();
    fn collect(node: &Node, out: &mut String) {
        match &node.kind {
            crate::dom::node::NodeKind::Text(t) => out.push_str(t),
            _ => {
                for child in &node.children {
                    collect(child, out);
                }
            }
        }
    }
    collect(node, &mut out);
    out
}

/// Locate the `<body>` element in the tree-builder output and wrap its children
/// in `<section>` wrappers (see `pipeline::section_wrapper`).
///
/// No-op when `wrap_sections` is false (the fragment-rendering case).
fn wrap_sections_in_ast(ast: &mut Node, wrap_sections: bool) {
    use crate::dom::node::{ElementKind, NodeKind};

    if !wrap_sections {
        return;
    }

    // The tree builder runs in fragment mode: it produces a synthetic `<html>`
    // whose children are the body content (no `<head>`/`<body>` wrappers). Wrap
    // those children in sections.
    for html in &mut ast.children {
        if let NodeKind::Element(ElementKind::Other(tag)) = &html.kind
            && tag == "html"
        {
            crate::pipeline::section_wrapper::wrap_sections(html);
            return;
        }
    }
}

/// The wikitext parser, bound to a site configuration.
pub struct Parser<'a, C: SiteConfig> {
    config: &'a C,
    /// How deep a Lua frame method's expansion currently is.
    ///
    /// Modules reach the parser only through these methods, and the manual is
    /// explicit that their *output* is not re-parsed for templates — so a
    /// well-behaved module nests a handful of levels at most. The counter stops
    /// a pathological one from recursing until the stack runs out.
    lua_expansion_depth: std::cell::Cell<usize>,
    /// Title of the page being parsed, recorded by `build_ast`.
    ///
    /// A `#invoke` inside a template needs the *root* page title — a module
    /// asking `getEntityIdForCurrentPage` means the article, while the frame it
    /// was called from is a template. Threading the title through every
    /// expansion call would touch a dozen signatures to serve one caller, so it
    /// is recorded once per parse like the expansion depth above.
    page_title: std::cell::RefCell<String>,
    /// Whether `data-parsoid` is omitted from output, recorded by `build_ast`
    /// from the options.
    ///
    /// The serializer honours the option directly, but one output does not go
    /// through it: `data-mw`'s `attribs[].html` fields are HTML *strings* built
    /// during expansion (`value_to_dom_html`), long before the final serialise,
    /// and a wiki's served HTML has no `data-parsoid` inside those either. Same
    /// reason as `page_title` for living here rather than being threaded: one
    /// caller, reached from several expansion paths.
    strip_data_parsoid: std::cell::Cell<bool>,
    /// How many times the preprocessor has entered a node during this parse.
    /// Mirrors `Parser::mPPNodeCount`.
    pp_node_count: std::cell::Cell<u64>,
    /// How deeply expansion is currently nested. Mirrors the `static
    /// $expansionDepth` local in `PPFrame_Hash::expand`.
    ///
    /// PHP's is a `static`, i.e. process-global and never reset — it only
    /// unwinds when every `expand()` returns, and leaks outright on an abnormal
    /// exit. That is an implementation accident rather than a semantic, and a
    /// pooled parser would carry the leftover depth into the next request, so
    /// this is a per-parse counter instead (reset in [`Parser::reset_expansion`]).
    /// Within one parse the observable behaviour is identical.
    expansion_depth: std::cell::Cell<u64>,
    /// Protection level per action for the titles this page's `{{PROTECTIONLEVEL:…}}`
    /// and `{{PROTECTIONEXPIRY:…}}` calls asked about, plus the page's own.
    ///
    /// Populated once, before the AST is built, from
    /// [`crate::traits::DataSource::get_title_protection`]. It is gated with the
    /// `page_title` above and for the same reason: one place needs it and it is
    /// reached from several expansion paths, so threading it through every
    /// signature would cost more than it explains.
    protection: std::cell::RefCell<std::collections::HashMap<String, ProtectionEntry>>,
    /// Whether an `#ifexist` title exists, keyed by the title as the call named
    /// it.
    ///
    /// `#ifexist` is a *synchronous* parser function whose answer is a fetch, so
    /// the answer has to be in hand before the token walk reaches the call. Unlike
    /// `protection` above it cannot be pre-scanned from the unexpanded stream: the
    /// title is written inside a template's body (`Category:{{{1}}} {{{2}}}
    /// {{{3}}}`), so it is only known *after* the surrounding arguments are
    /// substituted. [`Parser::prime_ifexist`] therefore resolves it at the point
    /// the call is reached, which is where the substitution has already happened.
    ifexist: std::cell::RefCell<std::collections::HashMap<String, bool>>,
    /// Title facts already fetched this render, keyed by the title as written.
    ///
    /// `preload_titles` runs once per `#invoke`, and the titles it wants — a
    /// module's `mw.title.new` literals plus the frame's `/doc`, `/sandbox`,
    /// `/testcases` — barely change from one call to the next. Without a
    /// render-wide cache each call re-fetches them: `{{Infobox person}}` asked
    /// for `Sandbox/doc` 29 times over 357 fetches and never finished. The facts
    /// cannot change mid-parse, so remembering them is a correctness-preserving
    /// dedupe as well as the difference between a render and a hang.
    title_facts:
        std::cell::RefCell<std::collections::HashMap<String, crate::lua::engine::TitleFacts>>,
    /// The protection levels of the page being parsed, as
    /// `{{PROTECTIONLEVEL:action}}` with no title argument reports them.
    page_protection: std::cell::RefCell<ProtectionEntry>,
    /// Sub-fragments built during expansion, waiting for the tree builder.
    ///
    /// A `<templatestyles>` has to be resolved **while expansion is running**,
    /// not in a pass afterwards: its `about` id comes from the document sequence
    /// and the service numbers it where the expansion reaches it, so a
    /// post-expansion pass numbers it behind everything the expansion already
    /// took. On `Template:Infobox` the two stylesheets are ids 2 and 3, directly
    /// after the `#invoke` wrapper's 1, which is only reachable from inside.
    ///
    /// Held on the parser rather than threaded through [`Parser::expand_templates`]
    /// and its twelve call sites for the reason the other `Cell` fields here are:
    /// one consumer, reached from several paths.
    ext_fragments: std::cell::RefCell<std::collections::HashMap<usize, Node>>,
    ext_next_id: std::cell::Cell<usize>,
    /// Sundered extension output, addressed by a `UNIQ…QINU` strip marker.
    ///
    /// A `frame:extensionTag('templatestyles', …)` answer is the extension's
    /// *output*, which reaches the token stream as an `mw:DOMFragment`
    /// placeholder with no wikitext form. Scribunto hands the module a strip
    /// marker that survives concatenation and string handling and is spliced
    /// back in when the module's output is parsed, and modules depend on that
    /// shape — `Module:Infobox` reorders its stylesheets by matching
    /// `\127…UNIQ--templatestyles-…QINU…\127` against `</tr>`. This is that
    /// marker: text the module may hold and move, mapped back to the placeholder
    /// tokens to splice when the output is re-expanded.
    strip_markers: std::cell::RefCell<std::collections::HashMap<String, Vec<Item>>>,
}

impl<'a, C: SiteConfig> Parser<'a, C> {
    pub fn new(config: &'a C) -> Self {
        Self {
            config,
            lua_expansion_depth: std::cell::Cell::new(0),
            page_title: std::cell::RefCell::new(String::new()),
            pp_node_count: std::cell::Cell::new(0),
            expansion_depth: std::cell::Cell::new(0),
            strip_data_parsoid: std::cell::Cell::new(false),
            protection: std::cell::RefCell::new(std::collections::HashMap::new()),
            ifexist: std::cell::RefCell::new(std::collections::HashMap::new()),
            title_facts: std::cell::RefCell::new(std::collections::HashMap::new()),
            page_protection: std::cell::RefCell::new(ProtectionEntry::default()),
            ext_fragments: std::cell::RefCell::new(std::collections::HashMap::new()),
            ext_next_id: std::cell::Cell::new(0),
            strip_markers: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// The protection answers `{{PROTECTIONLEVEL:…}}`/`{{PROTECTIONEXPIRY:…}}`
    /// may give during this parse.
    ///
    /// Borrows for as long as the returned context lives, so the caller must
    /// not hold an expansion that writes the map. Both maps are written exactly
    /// once, in [`Parser::build_ast`] before expansion starts, so a borrow taken
    /// during expansion can never conflict with a write.
    fn protection_context(&self) -> ProtectionContext<'_> {
        ProtectionContext::new(&self.page_protection, &self.protection)
    }

    /// Resolve the existence an `#ifexist` call is about to read, if it is not
    /// already known.
    ///
    /// `#ifexist` is a synchronous parser function but existence is a fact the
    /// parser can only fetch asynchronously, so the answer has to be resolved
    /// before the (synchronous) parser-function call runs — the same shape
    /// `PROTECTIONLEVEL` uses, except that the title is only known *after* the
    /// surrounding arguments are substituted, so it cannot be pre-scanned from
    /// the unexpanded token stream. [`Parser::expand_template_token`] calls this
    /// once the target is resolved and its `{{{…}}}` are substituted.
    ///
    /// A fetch failure records `false`, which is MediaWiki's answer for a title
    /// that does not exist: the alternative is to leak the raw `{{#ifexist:…}}`
    /// back into the output, which is what falling through to the
    /// unknown-parser-function arm did.
    async fn prime_ifexist(&self, title: &str, source: Option<&dyn DataSource>) {
        let title = title.trim();
        if title.is_empty() || self.ifexist.borrow().contains_key(title) {
            return;
        }
        // MediaWiki bounds expensive parser functions (`#ifexist` is one of
        // them, at 500 per page) so page content cannot drive unbounded
        // database work. rustoid fetches once per *distinct* title, not per
        // call, so the bound is on the map's size; beyond it the answer is
        // `false`, which is also what an unanswered call reads as.
        if self.ifexist.borrow().len() >= MAX_IFEXIST_TITLES {
            return;
        }
        let Some(source) = source else {
            return;
        };
        let titles = [title.to_string()];
        let exists = source
            .get_page_info(&titles)
            .await
            .ok()
            .and_then(|m| m.get(title).map(|i| !i.missing))
            .unwrap_or(false);
        self.ifexist.borrow_mut().insert(title.to_string(), exists);
    }

    /// Tokenize raw wikitext into the V2 `Item` stream.
    fn tokenize(&self, wikitext: &str) -> Result<Vec<Item>> {
        self.tokenize_with(wikitext, false, true)
    }

    /// Tokenize wikitext in *inline* context (no start-of-line, so a leading
    /// `#`/`*`/`=` does not begin a list/heading). Mirrors PHP's
    /// `processContentInPipeline` with `inlineContext => true` (and the `sol`
    /// flag `false` passed by `extArgToDOM`), used for media/gallery captions.
    fn tokenize_inline(&self, wikitext: &str) -> Result<Vec<Item>> {
        self.tokenize_with(wikitext, true, false)
    }

    fn tokenize_with(&self, wikitext: &str, inline_context: bool, sol: bool) -> Result<Vec<Item>> {
        let mut options = TokenizerOptions {
            inline_context,
            sol,
            magic_links: crate::wikitext::tokenizer_v2::MagicLinkConfig {
                rfc: self.config.magic_link_enabled("RFC"),
                pmid: self.config.magic_link_enabled("PMID"),
                isbn: self.config.magic_link_enabled("ISBN"),
            },
            lang_conv_enabled: self.config.lang_converter_enabled(),
            ..TokenizerOptions::default()
        };
        // Localized synonyms for the `redirect` magic word (each including the
        // leading `#`), mirroring PHP's `getMagicWordMatcher( 'redirect' )`.
        if let Some(entry) = self.config.magic_words().get("redirect") {
            options.redirect_words = entry.aliases.clone();
        }
        options.ext_tags = self.config.extension_tags().to_vec();
        options.protocols = self
            .config
            .protocols()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut tokenizer = PegTokenizer::new(wikitext, &options);
        let chunks = tokenizer.tokenize()?;
        Ok(chunks
            .into_iter()
            .map(|e| match e {
                Either::Left(s) => Item::Str(s),
                Either::Right(t) => Item::Tok(t),
            })
            .collect())
    }

    fn new_about_id(&self, counter: &std::cell::Cell<usize>) -> String {
        crate::pipeline::attribute_expander::new_about_id(counter)
    }

    /// Expand `wikilink` self-closing tokens into `<a>`/`<link>` tag sequences
    /// (mirrors the TT2 `WikiLinkHandler`, whose rendering path lives in
    /// `pipeline::wiki_link_render`).
    fn render_links(
        &self,
        tokens: Vec<Item>,
        fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
        next_id: &std::cell::Cell<usize>,
        context_title: Option<&crate::title::Title>,
    ) -> Vec<Item> {
        use crate::pipeline::wiki_link_render::{
            WikiLinkContext, get_wiki_link_target_info, render_redirect,
            render_wiki_link_dispatched,
        };
        use crate::wikitext::token_utils::key_value_to_string;

        let mut ctx = WikiLinkContext::new(self.config);
        if let Some(title) = context_title {
            ctx.set_context_title(title);
        }
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = &item else {
                out.push(item);
                continue;
            };

            // `mw:redirect` is handled separately: it becomes a single
            // `<link rel="mw:PageProp/redirect" .../>` token.
            if stt.name == "mw:redirect" {
                let href = stt
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))
                    .map(|kv| key_value_to_string(&kv.value))
                    .unwrap_or_default();
                // A redirect to a `<nowiki>` target cannot be rendered as a clean
                // link; mirror PHP's `onRedirect`/`bailTokens` bail-out. A
                // templated target is expanded upstream (AttributeExpander), so
                // by the time we get here any remaining `{{` indicates a failure
                // to expand and must bail.
                if href.contains("<nowiki") {
                    out.extend(self.bail_dirty_redirect(stt, &href, fragments, next_id));
                } else {
                    out.extend(render_redirect(
                        &mut ctx,
                        &ParsoidToken::SelfclosingTag(stt.clone()),
                    ));
                }
                continue;
            }

            if stt.name != "wikilink" {
                out.push(item);
                continue;
            }

            let href = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .map(|kv| key_value_to_string(&kv.value))
                .unwrap_or_default();
            let href_src = href.clone();

            // Don't allow internal links to pages containing a PROTO (scheme).
            // `[[http://…]]` is really a (bracketed) external/autonumber link;
            // bail to plain text and let the external-link pass re-render it
            // (mirrors `onWikiLink` → `bailTokens` on `hasValidProtocol`).
            if !href.is_empty() && self.config.has_valid_protocol(&href) {
                let src = reconstruct_link_src(stt, &href);
                out.push(Item::Str("[".to_string()));
                let sub = self.tokenize(&src[1..]).unwrap_or_default();
                let sub = self.render_links(sub, fragments, next_id, context_title);
                out.extend(sub);
                continue;
            }

            let target = match get_wiki_link_target_info(&ctx, &href, &href_src) {
                Ok(t) => t,
                Err(_) => {
                    // Invalid title: bail to literal `[[…]]` text, re-tokenizing the
                    // source so entities render as `mw:Entity` spans (mirrors PHP's
                    // `bailTokens` re-processing the link source).
                    let src = reconstruct_link_src(stt, &href);
                    out.push(Item::Str("[".to_string()));
                    let sub = self.tokenize(&src[1..]).unwrap_or_default();
                    let sub = self.render_links(sub, fragments, next_id, context_title);
                    out.extend(sub);
                    continue;
                }
            };

            // Media-in-link / Link-in-link: a nested wikilink inside the link text
            // cannot nest inside the outer `<a>`, so the link bails to literal
            // syntax with the nested media/link rendered in place (mirrors PHP's
            // `addLinkAttributesAndGetContent` throwing `Media-in-link`/`Link-in-link`,
            // caught by `renderWikiLink` → `bailTokens`). Only `renderFile` skips
            // `addLinkAttributesAndGetContent`, so every other dispatch path bails.
            // Our tokenizer keeps nested `[[…]]` as literal text, so detect a
            // *top-level* `[[` here — one inside a `<nowiki>` body, an HTML tag's
            // quoted attribute, or a template is not a nested link.
            let is_file_path = target.title.as_ref().is_some_and(|t| {
                !target.from_colon_escaped_text
                    && !target.href.starts_with('#')
                    && Some(t.namespace_id) == ctx.config.canonical_namespace_id("File")
            });
            let content_has_nested = stt.attribs.iter().any(|kv| {
                kv.key.as_str() == Some("mw:maybeContent")
                    && key_value_has_nested_wikilink(&kv.value)
            });
            if !is_file_path && content_has_nested {
                let src = reconstruct_link_src(stt, &href);
                out.push(Item::Str("[".to_string()));
                let sub = self.tokenize(&src[1..]).unwrap_or_default();
                let sub = self.render_links(sub, fragments, next_id, context_title);
                out.extend(sub);
                continue;
            }

            let rendered = render_wiki_link_dispatched(
                &mut ctx,
                &ParsoidToken::SelfclosingTag(stt.clone()),
                &target,
                false,
                fragments,
                next_id,
                &mut |items| {
                    // Build the caption fragment with a fresh sub-pipeline context
                    // (nested captions resolve their own nested fragments locally).
                    let mut f = std::collections::HashMap::new();
                    let id = std::cell::Cell::new(0usize);
                    self.build_inline_fragment(items, &mut f, &id)
                },
            );
            out.extend(rendered);
        }

        out
    }

    /// Reconstruct an invalid redirect (target contains `<nowiki>` or a
    /// template) as a `#` list item, mirroring PHP's `onRedirect` bail + `bailTokens`.
    /// The redirect word (minus the leading `#`) is followed by the re-tokenized
    /// wikilink source with its leading `[` restored.
    fn bail_dirty_redirect(
        &self,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        href: &str,
        fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        // The redirect word source (e.g. `#REDIRECT `).
        let src = stt.data_parsoid.src.clone().unwrap_or_default();
        let word = src.strip_prefix('#').unwrap_or(&src).to_string();

        // Re-tokenize the wikilink inner (`[{href}]]`, mirroring PHP's
        // `bailTokens` which strips the first `[`) and expand `<nowiki>`.
        //
        // The fragments registered here must live in the caller's map: an
        // emitted `mw:DOMFragment` wrapper is resolved by `unpack_dom_fragments`
        // against that map, so a local map would leave a dangling fragment id.
        let re_src = format!("[{href}]]");
        let tokens = self.tokenize(&re_src).unwrap_or_default();
        let expanded =
            crate::pipeline::extension_handler::run(tokens, self.config, fragments, next_id);

        let mut li = crate::wikitext::tokens_v2::TagTk::new(
            "listItem",
            vec![],
            crate::wikitext::tokens_v2::DataParsoid::default(),
        );
        li.add_attribute_str("bullets", "#");

        let mut out = vec![
            Item::Tok(ParsoidToken::Tag(li)),
            Item::Str(word),
            Item::Str("[".to_string()),
        ];
        out.extend(expanded);
        out
    }

    /// Expand `extlink`/`urllink` self-closing tokens into `<a>`/`<img>` tag
    /// sequences (mirrors the TT2 `ExternalLinkHandler`).
    fn render_external_links(
        &self,
        tokens: Vec<Item>,
        fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        use crate::pipeline::external_link_handler::{on_ext_link, on_url_link};

        let mut out: Vec<Item> = Vec::new();
        for item in tokens {
            let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = &item else {
                out.push(item);
                continue;
            };

            let clean = |href: &str| {
                crate::sanitizer::clean_url(href, "external", |proto| {
                    self.config.has_valid_protocol(proto)
                })
            };
            let clean_link = |href: &str| {
                crate::sanitizer::clean_url(href, "wikilink", |proto| {
                    self.config.has_valid_protocol(proto)
                })
            };

            match stt.name.as_str() {
                "extlink" => {
                    let token = ParsoidToken::SelfclosingTag(stt.clone());
                    let render_content = |items: Vec<Item>| {
                        // Re-tokenize and render the link content so nested
                        // `[[…]]` (media/links) are expanded, then wrap in a
                        // DOM-fragment token (mirrors PHP's `getDOMFragmentToken`).
                        let mut src_items = items;
                        // Render any nested wikilinks, then external links.
                        let rendered = if src_items
                            .iter()
                            .any(|it| matches!(it, Item::Str(s) if s.contains("[[")))
                        {
                            // Re-tokenize each string token that contains `[[`.
                            let mut expanded: Vec<Item> = Vec::new();
                            for it in src_items.drain(..) {
                                match it {
                                    Item::Str(s) if s.contains("[[") => {
                                        let sub = self.tokenize_inline(&s).unwrap_or_default();
                                        let sub = self.render_links(sub, fragments, next_id, None);
                                        expanded.extend(sub);
                                    }
                                    other => expanded.push(other),
                                }
                            }
                            expanded
                        } else {
                            src_items
                        };
                        let frag = self.build_inline_fragment(rendered, fragments, next_id);
                        crate::pipeline::wiki_link_render::dom_fragment_token(
                            frag, &token, fragments, next_id,
                        )
                    };
                    let Some(rendered) = on_ext_link(
                        &token,
                        clean,
                        self.config.relative_link_prefix(),
                        render_content,
                    ) else {
                        out.push(item);
                        continue;
                    };
                    out.extend(rendered);
                }
                "urllink" => {
                    let content_href = stt
                        .attribs
                        .iter()
                        .find(|kv| kv.key.as_str() == Some("href"))
                        .and_then(|kv| kv.value.as_str())
                        .unwrap_or("")
                        .to_string();
                    let Some(rendered) = on_url_link(
                        &ParsoidToken::SelfclosingTag(stt.clone()),
                        &content_href,
                        clean,
                        clean_link,
                    ) else {
                        out.push(item);
                        continue;
                    };
                    out.extend(rendered);
                }
                _ => out.push(item),
            }
        }
        out
    }

    /// Expand `behavior-switch` tokens into `mw:PageProp` metas (mirrors the
    /// TT2 `BehaviorSwitchHandler`).
    fn render_behavior_switches(&self, tokens: Vec<Item>) -> Vec<Item> {
        crate::pipeline::behavior_switch_handler::BehaviorSwitchHandler.run(tokens)
    }

    /// Expand `language-variant` tokens into `mw:LanguageVariant` elements
    /// (mirrors the TT2 `LanguageVariantHandler`).
    fn render_language_variants(&self, tokens: Vec<Item>) -> Vec<Item> {
        crate::pipeline::language_variant_handler::LanguageVariantHandler.run(self.config, tokens)
    }

    /// Run an inline sub-pipeline over an extension-tag body and return the
    /// body-content children as a fragment document (mirrors
    /// `PipelineUtils::processContentInPipeline` with `pipelineType
    /// = 'wikitext-to-fragment'` + `inlineContext`).
    ///
    /// When a data source and frame are supplied, nested templates/parser
    /// functions in the body are expanded first (mirroring the
    /// `expandTemplates` parse option of the PHP fragment pipeline).
    async fn process_fragment_body(
        &self,
        body: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Node {
        let mut tokens = match self.tokenize(body) {
            Ok(t) => t,
            Err(_) => return crate::dom::node::Node::document(),
        };
        // Expand nested templates/parser functions when a data source is
        // available (the synchronous `wikitext_to_ast` path has none).
        let mut fragments: std::collections::HashMap<usize, Node> =
            std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        if source.is_some() {
            tokens = self
                .expand_templates(frame, tokens, source, about_counter, true, false, body)
                .await;
            // TT2 order: ExtensionHandler precedes the AttributeExpander.
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                &next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    &next_id,
                )
                .await;
        }
        // The quote transformer flushes pending quotes only on a newline or EOF
        // token; append a synthetic EOF so quotes (`'''bold'''`) flush.
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        let mut frag = self.build_inline_fragment(tokens, &mut fragments, &next_id);
        flatten_nowiki_spans(&mut frag);
        frag
    }

    /// Build an inline fragment document from an already-tokenized (and
    /// optionally template-expanded) token stream.
    fn fragment_from_tokens(&self, tokens: Vec<Item>) -> Node {
        let mut fragments = std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        self.build_inline_fragment(tokens, &mut fragments, &next_id)
    }

    /// Build an inline fragment document from caption tokens, resolving links
    /// and suppressing p-wrapping via the inline tree-builder context (mirrors
    /// PHP's `processContentInPipeline` with `inlineContext => true`).
    /// `fragments`/`next_id` are threaded through so any nested media captions
    /// register their own sub-fragments in the same map the outer tree builder
    /// will splice.
    fn build_inline_fragment(
        &self,
        tokens: Vec<Item>,
        fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> crate::dom::node::Node {
        render_inline_fragment(self.config, tokens, fragments, next_id)
    }

    /// Serialize an attribute key/value source (a token array or plain string)
    /// into a DOM-fragment HTML string, for the `html` field of a
    /// `data-mw.attribs` entry. Mirrors PHP's `PipelineUtils::
    /// expandAttrValueToDOM` (which pipes the value through the
    /// `expanded-tokens-to-fragment` pipeline in inline context and serializes).
    ///
    /// The result carries `data-parsoid`/`data-mw`/`about`/`typeof` intact
    /// (matching Parsoid's round-trippable attribute fragments); the caller
    /// HTML-escapes it when embedding in the `data-mw` JSON envelope.
    ///
    /// `fragments`/`next_id` are threaded through because an attribute value can
    /// carry a `mw:DOMFragment` placeholder (e.g. the `<nowiki>` in T280115's
    /// `title="foo<nowiki>|</nowiki>"`): the placeholder must resolve against
    /// the same map the extension handler registered it in.
    fn value_to_dom_html(
        &self,
        kv: &crate::wikitext::tokens_v2::KeyValue,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> String {
        use crate::pipeline::attribute_transform_manager::key_value_to_items;

        let items = key_value_to_items(kv);
        let mut frag = self.build_inline_fragment(items, fragments, next_id);
        crate::pipeline::unpack_dom_fragments::run(&mut frag);
        let serializer =
            crate::html::serialize::HtmlSerializer::new(crate::options::ParserOptions {
                body_only: true,
                strip_data_parsoid: self.strip_data_parsoid.get(),
                ..crate::options::ParserOptions::for_page("")
            });
        serializer.serialize(&frag).unwrap_or_default()
    }

    /// Build an inline fragment document from raw body wikitext, without
    /// template expansion (used by the synchronous `wikitext_to_ast` path).
    fn fragment_from_body(&self, body: &str) -> Node {
        let mut tokens = match self.tokenize(body) {
            Ok(t) => t,
            Err(_) => return crate::dom::node::Node::document(),
        };
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        let mut frag = self.fragment_from_tokens(tokens);
        flatten_nowiki_spans(&mut frag);
        frag
    }

    /// Expand `<pre format="wikitext">` extension tokens in place: emit the
    /// `<pre typeof="mw:Extension/pre">` wrapper, and tunnel the body through
    /// the inline sub-pipeline as a `mw:dom-fragment-token` placeholder. Returns
    /// the token stream and a map of fragment id → pre-built sub-`Node`.
    async fn expand_wikitext_pre(
        &self,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
        next_id: &std::cell::Cell<usize>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        self.expand_wikitext_pre_with(source, frame, about_counter, tokens, next_id)
            .await
    }

    /// Synchronous variant of [`expand_wikitext_pre`] for the `wikitext_to_ast`
    /// path, which has no data source and therefore performs no nested-template
    /// expansion.
    fn expand_wikitext_pre_sync(
        &self,
        tokens: Vec<Item>,
        next_id: &std::cell::Cell<usize>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        let mut fragments = std::collections::HashMap::new();
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Some(pre_stt) = wikitext_pre_target(&item) else {
                out.push(item);
                continue;
            };
            let body = extension_body(pre_stt);
            let sub = self.fragment_from_body(&body);
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, sub);
            emit_pre_placeholder(pre_stt, id, &mut out);
        }

        (out, fragments)
    }

    /// Expand `<divtag>`/`<spantag>` extension tokens in place: parse the body as
    /// wikitext in *block* context (so `<p>` wrapping applies) and tunnel it
    /// through a `mw:dom-fragment-token` placeholder wrapped in the `<div>`/`<span>`
    /// `mw:Extension/*` container. Mirrors `ParserHook::sourceToDom`'s
    /// `divtag`/`spantag` transparent-wrapper case (via `extTagToDOM` in block
    /// context).
    async fn expand_wrapper_tag(
        &self,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
        next_id: &std::cell::Cell<usize>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        let mut fragments = std::collections::HashMap::new();
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Some((stt, ext_name, wrapper)) = wrapper_tag_target(&item) else {
                out.push(item);
                continue;
            };
            let body = extension_body(stt);
            // PHP's `SpanTag`/`DivTag` handler passes
            // `parseOpts.context = $isDiv ? 'block' : 'inline'`: the body of a
            // `<spantag>` is parsed in *inline* context, so it gets neither
            // p-wrapping nor indent-pre (T278565).
            let inline = wrapper == "span";
            let sub = if source.is_some() {
                // `ParsoidExtensionAPI::wikitextToDOM` parses the body with
                // `inTemplate => $this->inTemplate()` — false unless the extension
                // was reached from inside a template (`expand_wrapper_tag` runs on
                // the top-level stream, so false here).
                self.process_block_fragment_body(&body, source, frame, about_counter, inline, false)
                    .await
            } else if inline {
                self.fragment_from_body(&body)
            } else {
                self.block_fragment_from_body(&body)
            };
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, sub);
            emit_wrapper_extension_placeholder(stt, ext_name, wrapper, id, &mut out);
        }

        (out, fragments)
    }

    /// Expand `<indicator>` extension tokens in place.
    ///
    /// An indicator is rendered by MediaWiki into a fixed slot in the page
    /// header, not into the article's own text; Parsoid's job is to record the
    /// declaration for that header. So unlike every other extension here the body
    /// is *never* parsed or resolved through the frame — it is passed through as
    /// wikitext for the header to render — and the element is a `<meta>` carrying
    /// only metadata. That is also why this needs neither a `Frame` nor a
    /// `DataSource`, and so is the one extension expansion that is not `async`.
    fn expand_indicator(
        &self,
        tokens: Vec<Item>,
        next_id: &std::cell::Cell<usize>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        self.expand_indicator_with(tokens, next_id, |body, attrs| {
            Node::document_with_child(crate::pipeline::extension_handler::indicator_node(
                &body, &attrs,
            ))
        })
    }

    /// Synchronous [`expand_wrapper_tag`] for the `wikitext_to_ast` path.
    fn expand_wrapper_tag_sync(
        &self,
        tokens: Vec<Item>,
        next_id: &std::cell::Cell<usize>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        let mut fragments = std::collections::HashMap::new();
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Some((stt, ext_name, wrapper)) = wrapper_tag_target(&item) else {
                out.push(item);
                continue;
            };
            let body = extension_body(stt);
            let sub = if wrapper == "span" {
                self.fragment_from_body(&body)
            } else {
                self.block_fragment_from_body(&body)
            };
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, sub);
            emit_wrapper_extension_placeholder(stt, ext_name, wrapper, id, &mut out);
        }

        (out, fragments)
    }

    /// Synchronous `<indicator>` expansion for the `wikitext_to_ast` path.
    ///
    /// The body is *not* parsed: it is recorded verbatim as the fragment, because
    /// the indicator's content reaches the reader's page header as wikitext rather
    /// than as page markup, and the extension output only carries it for the
    /// header to render. So this is the one extension whose body must survive
    /// unexpanded, and `extract_ext_body` is the whole of its handling.
    fn expand_indicator_sync(
        &self,
        tokens: Vec<Item>,
        next_id: &std::cell::Cell<usize>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        self.expand_indicator_with(tokens, next_id, |body, attrs| {
            Node::document_with_child(crate::pipeline::extension_handler::indicator_node(
                &body, &attrs,
            ))
        })
    }

    /// Shared driver for the indicator path, parameterized by the fragment
    /// builder so the async and sync entry points cannot drift apart in the
    /// placeholder handling (which is the fiddly part).
    fn expand_indicator_with(
        &self,
        tokens: Vec<Item>,
        next_id: &std::cell::Cell<usize>,
        build: impl Fn(String, Vec<crate::wikitext::tokens_v2::KV>) -> Node,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        let mut fragments = std::collections::HashMap::new();
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Some(stt) = indicator_target(&item) else {
                out.push(item);
                continue;
            };
            let body = extension_body(stt);
            let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, build(body, attrs));
            out.extend(emit_indicator_placeholder(stt, id));
        }

        (out, fragments)
    }

    /// Build a *block*-context fragment document from raw body wikitext, without
    /// template expansion (used by the synchronous `wikitext_to_ast` path). Unlike
    /// [`fragment_from_body`] (inline), p-wrapping is enabled so bare text becomes
    /// `<p>…</p>`.
    fn block_fragment_from_body(&self, body: &str) -> Node {
        let mut tokens = match self.tokenize(body) {
            Ok(t) => t,
            Err(_) => return crate::dom::node::Node::document(),
        };
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        self.block_fragment_from_tokens(tokens)
    }

    /// Process a `divtag`/`spantag` body (wikitext) into a block-context fragment,
    /// expanding nested templates/parser functions when a data source is present.
    async fn process_block_fragment_body(
        &self,
        body: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
        inline: bool,
        in_template: bool,
    ) -> Node {
        let mut tokens = match self.tokenize(body) {
            Ok(t) => t,
            Err(_) => return crate::dom::node::Node::document(),
        };
        let mut fragments: std::collections::HashMap<usize, Node> =
            std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        if source.is_some() {
            tokens = self
                .expand_templates(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    in_template,
                    false,
                    body,
                )
                .await;
            // TT2 order: ExtensionHandler precedes the AttributeExpander.
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                &next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    &next_id,
                )
                .await;
        }
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        let mut ast = if inline {
            self.build_inline_fragment(tokens, &mut fragments, &next_id)
        } else {
            let stage = TreeBuilderStage::new(false);
            let mut ast =
                stage.to_ast_with_fragments(tokens, None, self.config, fragments.clone(), None);
            let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
            crate::pipeline::p_wrap::run(&mut ast);
            crate::pipeline::tree_builder_html::post_pwrap_transforms(&mut ast, &depths, None);
            crate::pipeline::cleanup::run(&mut ast);
            crate::pipeline::unpack_dom_fragments::run(&mut ast);
            extract_fragment_children(&ast)
        };
        let _ = &mut ast;
        ast
    }

    /// Build a block-context fragment document from an already-tokenized token
    /// stream (with p-wrapping, unlike the inline [`fragment_from_tokens`]).
    fn block_fragment_from_tokens(&self, tokens: Vec<Item>) -> Node {
        self.fragment_from_tokens_with_context(tokens, false)
    }

    /// Build a fragment document from an already-tokenized token stream, in
    /// either block context (p-wrapping + indent-pre enabled) or inline context.
    /// Mirrors PHP's `parseOpts.context` (`'block'` vs `'inline'`).
    fn fragment_from_tokens_with_context(&self, tokens: Vec<Item>, inline: bool) -> Node {
        // Render links, external links, behavior switches, and language variants
        // (the token-level stages that run before tree building on the main page).
        let mut fragments = std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        let tokens = self.render_links(tokens, &mut fragments, &next_id, None);
        let tokens = self.render_external_links(tokens, &mut fragments, &next_id);
        let tokens = self.render_behavior_switches(tokens);
        let tokens = self.render_language_variants(tokens);

        let stage = TreeBuilderStage::new(inline);
        let mut ast = stage.to_ast_with_fragments(tokens, None, self.config, fragments, None);
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
        if !inline {
            crate::pipeline::p_wrap::run(&mut ast);
        }
        crate::pipeline::tree_builder_html::post_pwrap_transforms(&mut ast, &depths, None);
        crate::pipeline::cleanup::run(&mut ast);
        extract_fragment_children(&ast)
    }

    /// Shared driver for [`expand_wikitext_pre`] / [`expand_wikitext_pre_sync`]:
    /// route `format="wikitext"` `<pre>` extension bodies through the inline
    /// sub-pipeline, emitting `<pre>` + `mw:dom-fragment-token` placeholders.
    async fn expand_wikitext_pre_with(
        &self,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
        tokens: Vec<Item>,
        next_id: &std::cell::Cell<usize>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        let mut fragments = std::collections::HashMap::new();
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Some(pre_stt) = wikitext_pre_target(&item) else {
                out.push(item);
                continue;
            };
            let body = extension_body(pre_stt);
            let sub = self
                .process_fragment_body(&body, source, frame, about_counter)
                .await;
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, sub);
            emit_pre_placeholder(pre_stt, id, &mut out);
        }

        (out, fragments)
    }

    /// Render a gallery caption (wikitext) into fragment children, expanding
    /// templates/parser functions when a data source is present. Mirrors PHP's
    /// `renderMedia` caption handling (`processContentInPipeline` with
    /// `inlineContext => true` + `expandTemplates`).
    async fn render_caption_expanded(
        &self,
        caption: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Vec<Node> {
        // Whitespace-normalize the caption in inline context (mirrors PHP's
        // `extArgToDOM`, which does `preg_replace('/[\t\r\n ]/', ' ', $vsrc)` so
        // a multi-line `caption=` (e.g. `# …` with blank lines) is folded onto one
        // line and a leading `#`/`*` does not start a list).
        let caption = caption
            .chars()
            .map(|c| {
                if matches!(c, '\t' | '\r' | '\n' | ' ') {
                    ' '
                } else {
                    c
                }
            })
            .collect::<String>();
        let mut tokens = match self.tokenize_inline(&caption) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let mut fragments: std::collections::HashMap<usize, Node> =
            std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        if source.is_some() {
            tokens = self
                .expand_templates(frame, tokens, source, about_counter, false, false, &caption)
                .await;
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                &next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    &next_id,
                )
                .await;
        }
        let mut frag = self.build_inline_fragment(tokens, &mut fragments, &next_id);
        std::mem::take(&mut frag.children)
    }

    /// Render a single gallery line's media as a block `<figure>` (or, when the
    /// line's options force inline, a `<span>`), mirroring PHP's
    /// `ParsoidExtensionAPI::renderMedia(forceBlock=true, suppressMediaFormats=true)`.
    ///
    /// Builds `[[titleStr|optsStr|none]]`, runs it through tokenize → template/
    /// attribute expansion → `renderFile`, then `AddMediaInfo` (so the file is
    /// resolved — `<img>` + `title`/`alt` — while the `<figcaption>` is still
    /// attached). The caller detaches the `<figcaption>` into the gallery text.
    #[allow(clippy::too_many_arguments)]
    async fn render_gallery_media(
        &self,
        title_str: &str,
        opts_str: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Option<Node> {
        use crate::pipeline::wiki_link_render::{
            WikiLinkContext, get_wiki_link_target_info, render_wiki_link_dispatched,
        };
        use crate::wikitext::token_utils::key_value_to_string;

        // Assemble the wikitext `[[<title>|<opts>|none]]` (the trailing `|none`
        // forces the block figure, per `renderMedia`'s `$pieces[] = '|none'`).
        let wikitext = format!("[[{title_str}|{opts_str}|none]]");
        let mut tokens = self.tokenize(&wikitext).ok()?;
        let mut fragments = std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        if source.is_some() {
            tokens = self
                .expand_templates(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    false,
                    false,
                    &wikitext,
                )
                .await;
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                &next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    &next_id,
                )
                .await;
        }

        // Render the wikilink (→ `renderFile`) with media formats suppressed.
        let mut link_ctx = WikiLinkContext::new(self.config);
        link_ctx.set_suppress_media_formats();
        let tokens: Vec<Item> = tokens
            .into_iter()
            .flat_map(|item| {
                let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = &item else {
                    return vec![item];
                };
                if stt.name != "wikilink" {
                    return vec![item];
                }
                let href = stt
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))
                    .map(|kv| key_value_to_string(&kv.value))
                    .unwrap_or_default();
                let href_src = href.clone();
                let target = match get_wiki_link_target_info(&link_ctx, &href, &href_src) {
                    Ok(t) => t,
                    Err(_) => return vec![Item::Str(format!("[[{href}]]"))],
                };
                render_wiki_link_dispatched(
                    &mut link_ctx,
                    &ParsoidToken::SelfclosingTag(stt.clone()),
                    &target,
                    false,
                    &mut fragments,
                    &next_id,
                    &mut |items| {
                        let mut f = std::collections::HashMap::new();
                        let id = std::cell::Cell::new(0usize);
                        render_inline_fragment(self.config, items, &mut f, &id)
                    },
                )
            })
            .collect();
        let tokens = self.render_external_links(tokens, &mut fragments, &next_id);
        let mut tokens = self.render_behavior_switches(tokens);
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        // Build the inline tree, splicing the caption `mw:dom-fragment-token`
        // placeholders via the fragments map populated by `renderFile`.
        let stage = TreeBuilderStage::new(true);
        let frag = stage.to_ast_with_fragments(tokens, None, self.config, fragments, None);
        let mut frag = extract_fragment_children(&frag);
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&frag);
        crate::pipeline::tree_builder_html::post_pwrap_transforms(&mut frag, &depths, None);

        // Resolve the file (replace the broken span with `<img>` and stamp
        // `title`/`alt` from the caption) before the figcaption is detached —
        // mirrors `renderMedia`'s sub-pipeline running `AddMediaInfo`.
        if let Some(src) = source {
            crate::pipeline::add_media_info::run(&mut frag, src, self.config).await;
        }

        // The media is the sole child of the inline fragment.
        frag.children.into_iter().next()
    }

    /// Expand `<gallery>` extension tokens in place: build each gallery fragment
    /// (expanding caption templates when a data source is present) and tunnel it
    /// through an `mw:dom-fragment-token` placeholder. Uses the shared `next_id`
    /// counter so gallery fragments never collide with `render_links`'s caption
    /// fragments (mirrors PHP's `Gallery::sourceToDom` + extension encapsulation).
    async fn expand_gallery(
        &self,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Some(stt) = gallery_target(&item) else {
                out.push(item);
                continue;
            };
            let mut ul = crate::pipeline::gallery::build_with(
                stt,
                self.config,
                |caption: &str| {
                    let caption = caption.to_string();
                    async move {
                        self.render_caption_expanded(&caption, source, frame, about_counter)
                            .await
                    }
                },
                |title: &str, opts: &str| {
                    let title = title.to_string();
                    let opts = opts.to_string();
                    async move {
                        self.render_gallery_media(&title, &opts, source, frame, about_counter)
                            .await
                    }
                },
            )
            .await;
            // The gallery `<ul>` is an extension encapsulation wrapper; it must
            // carry an `about` id (mirrors PHP's `ExtensionHandler` adding
            // `about` to extension top-level nodes) so the html2wt serializer
            // recognizes it as `mw:Extension/gallery` rather than a plain list.
            if let crate::dom::node::NodeKind::Element(_) = ul.kind {
                ul.set_attr("about", self.new_about_id(about_counter));
            }
            let mut frag = crate::dom::node::Node::document();
            frag.push_child(ul);
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, frag);
            out.push(emit_gallery_placeholder(stt, id));
        }

        out
    }

    /// Resolve one `<templatestyles>` extension token into a placeholder plus a
    /// stashed `<style>` fragment, taking the `about` id from `about_counter`.
    ///
    /// Returns the token unchanged when the tag cannot be resolved (no source, no
    /// page, no revision) — leaving it visible in the diff rather than silently
    /// emitting an empty stylesheet.
    async fn expand_one_templatestyles(
        &self,
        item: &Item,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        let Some(stt) = templatestyles_target(item) else {
            return vec![item.clone()];
        };
        let Some(source) = source else {
            return vec![item.clone()];
        };
        // `src` and `wrapper` arrive as `data-mw` rich attribs: the tokenizer
        // stores the parsed start-tag attributes there, not as plain token
        // attributes.
        let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
        // A value that came through Lua reaches here with its quotes
        // backslash-escaped (`\"Z.css\"`), because Scribunto passes the string
        // through unchanged and `pf_tag` cannot unescape it the way it does for a
        // plain tag. Strip both forms before treating it as a title.
        let attr = |key: &str| {
            attrs
                .iter()
                .find(|kv| kv.key.as_str() == Some(key))
                .and_then(|kv| kv.value.as_str())
                .map(unquote_attr_value)
                .filter(|v| !v.is_empty())
        };

        let resolved = match attr("src") {
            Some(src) => {
                let title = templatestyles_title(self.config, &src);
                match source.get_page_with_revision(&title).await {
                    Ok(Some((body, Some(revid)))) => Some((body, revid, src.to_string())),
                    _ => None,
                }
            }
            None => None,
        };
        let Some((body, revid, src)) = resolved else {
            return vec![item.clone()];
        };

        let css = crate::pipeline::templatestyles::render(&body, attr("wrapper").as_deref());
        // The id is taken *now*, before the fragment is stashed, so the sequence
        // follows the expansion rather than the tree build.
        let node = crate::pipeline::templatestyles::style_node(
            &css,
            revid,
            &src,
            &self.new_about_id(about_counter),
        );
        let mut frag = Node::document();
        frag.push_child(node);
        let id = self.ext_next_id.get();
        self.ext_next_id.set(id + 1);
        self.ext_fragments.borrow_mut().insert(id, frag);
        emit_style_placeholder(stt, id)
    }

    /// Resolve one `<indicator>` extension token into a placeholder plus a
    /// stashed `<meta>` fragment, spending an `about` id.
    ///
    /// Inline, for the same reason as [`Parser::expand_one_templatestyles`]: the
    /// id belongs to the expansion's position in the document. It matters here
    /// because a module emits the indicator from *inside* a template; a pass
    /// running after expansion would number it after everything the template
    /// expansion produced, and the transclusion that follows the template would
    /// take the id the indicator should have spent. PHP's `ExtensionHandler::
    /// onDocumentFragment` allocates an id for every extension except `nowiki`,
    /// and the `<meta>` ends up sharing the enclosing transclusion's `about` in
    /// the output — the id is spent and discarded, but the spend is what shifts
    /// every following transclusion by one.
    ///
    /// Returns `None` when `item` is not an indicator token, so the caller can
    /// fall through to the other extension paths.
    fn expand_one_indicator(
        &self,
        item: &Item,
        about_counter: &std::cell::Cell<usize>,
    ) -> Option<Vec<Item>> {
        let stt = indicator_target(item)?;
        let body = extension_body(stt);
        let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
        let node = Node::document_with_child(crate::pipeline::extension_handler::indicator_node(
            &body, &attrs,
        ));
        // The extension spends the id whether or not it survives into the
        // output (see the method comment). Allocating it *before* stashing the
        // fragment also keeps the sequence in expansion order.
        let _ = self.new_about_id(about_counter);
        let id = self.ext_next_id.get();
        self.ext_next_id.set(id + 1);
        self.ext_fragments.borrow_mut().insert(id, node);
        Some(emit_indicator_placeholder(stt, id))
    }

    /// Inline every `<templatestyles>` stylesheet.
    ///
    /// Async because the CSS lives on a wiki page, like a template's source; the
    /// extension handler is synchronous and token-only, so it cannot reach a
    /// `DataSource` and leaves this tag alone.
    ///
    /// Each resolved stylesheet becomes a `<style>` element stashed as a
    /// sub-fragment, the way `style_items` does it, and reached through a
    /// `mw:DOMFragment` placeholder. That is what keeps the CSS text from being
    /// re-parsed as wikitext.
    ///
    /// Returns the tokens *and* the fragments they reference: the caller must
    /// hand those to the tree builder, or the stylesheet is lost. Ids are
    /// allocated from `next_id` so they cannot collide with the caller's.
    ///
    /// A tag is left untouched when its `src` is missing, its page does not
    /// exist, or the page has no usable revision — all of which keep the
    /// unexpanded tag visible in the diff instead of silently emitting an empty
    /// stylesheet. A stylesheet that *does* resolve but sanitises to nothing
    /// produces an empty `<style>`, which is what Parsoid does.
    ///
    /// This runs as a safety net for tokens that reach it unresolved — the
    /// common case is handled inline by [`Parser::expand_one_templatestyles`]
    /// during expansion, so that the id lands in document order.
    async fn expand_templatestyles(
        &self,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        let mut out: Vec<Item> = Vec::with_capacity(tokens.len());
        for item in tokens {
            if templatestyles_target(&item).is_none() {
                out.push(item);
                continue;
            }
            out.extend(
                self.expand_one_templatestyles(&item, source, about_counter)
                    .await,
            );
        }
        out
    }

    /// Synchronous [`expand_gallery`] for the `wikitext_to_ast` path (no data
    /// source, so no template expansion).
    fn expand_gallery_sync(
        &self,
        tokens: Vec<Item>,
        about_counter: &std::cell::Cell<usize>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        let mut out: Vec<Item> = Vec::new();
        for item in tokens {
            let Some(stt) = gallery_target(&item) else {
                out.push(item);
                continue;
            };
            let mut ul = crate::pipeline::gallery::build_with_sync(stt, self.config);
            if let crate::dom::node::NodeKind::Element(_) = ul.kind {
                ul.set_attr("about", self.new_about_id(about_counter));
            }
            let mut frag = crate::dom::node::Node::document();
            frag.push_child(ul);
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, frag);
            out.push(emit_gallery_placeholder(stt, id));
        }
        out
    }

    /// Convert wikitext to the format-agnostic AST (no template expansion).
    ///
    /// When `wrap_sections` is true, heading content is wrapped in `<section>`
    /// elements (matching PHP's `WrapSections` DOM post-processor in document
    /// mode). Fragment rendering leaves it false.
    pub fn wikitext_to_ast(&self, wikitext: &str, wrap_sections: bool) -> Result<Node> {
        self.wikitext_to_ast_with_title(wikitext, wrap_sections, None)
    }

    /// [`Parser::wikitext_to_ast`] with an explicit context title, so relative
    /// links (`[[../sibling]]`, `[[/subpage]]`) and lone fragments resolve
    /// against the page being parsed, as PHP's `Env::getContextTitle()` provides.
    pub fn wikitext_to_ast_with_title(
        &self,
        wikitext: &str,
        wrap_sections: bool,
        context_title: Option<&crate::title::Title>,
    ) -> Result<Node> {
        let tokens = self.tokenize(wikitext)?;
        let mut fragments: std::collections::HashMap<usize, crate::dom::node::Node> =
            std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        let tokens = self.render_links(tokens, &mut fragments, &next_id, context_title);
        let tokens = self.render_external_links(tokens, &mut fragments, &next_id);
        let tokens = self.render_behavior_switches(tokens);
        let tokens = self.render_language_variants(tokens);
        let (tokens, pre_fragments) = self.expand_wikitext_pre_sync(tokens, &next_id);
        fragments.extend(pre_fragments);
        let (tokens, wrapper_fragments) = self.expand_wrapper_tag_sync(tokens, &next_id);
        fragments.extend(wrapper_fragments);
        let (tokens, indicator_fragments) = self.expand_indicator_sync(tokens, &next_id);
        fragments.extend(indicator_fragments);
        let tokens = self.expand_gallery_sync(
            tokens,
            &std::cell::Cell::new(0usize),
            &mut fragments,
            &next_id,
        );
        let stage = TreeBuilderStage::new(false);
        let mut ast =
            stage.to_ast_with_fragments(tokens, Some(wikitext), self.config, fragments, None);
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
        crate::pipeline::p_wrap::run(&mut ast);
        // AddLinkAttributes runs *before* `dom-unpack` (mirrors PHP's
        // `NESTED_PIPELINE_DOM_TRANSFORMS`, where `linkclasses` precedes
        // `linkneighbours+dom-unpack`). The `<a>` still holds its
        // `mw:DOMFragment` placeholder child at this point, so an extlink whose
        // content is a nested link/media gets `external text` (reflecting
        // wikitext intent) rather than `external autonumber` (empty DOM).
        crate::pipeline::add_link_attributes::run(&mut ast, self.config);
        crate::pipeline::tree_builder_html::post_pwrap_transforms(
            &mut ast,
            &depths,
            Some(wikitext),
        );
        crate::pipeline::cleanup::run(&mut ast);
        crate::pipeline::headings::gen_anchors(&mut ast);
        // Repair `<a>`-inside-`<a>` bad nesting created by DOM-fragment unpack.
        // (The sync path has no media resolution, so this runs after the other
        // DOM transforms here; the async `build_ast` runs it after `AddMediaInfo`.)
        crate::pipeline::unpack_dom_fragments::fix_bad_nesting(&mut ast);
        wrap_sections_in_ast(&mut ast, wrap_sections);
        Ok(ast)
    }

    /// Convert wikitext to an HTML string (no native template expansion).
    pub fn wikitext_to_html(&self, wikitext: &str, options: &ParserOptions) -> Result<String> {
        let ast = self.wikitext_to_ast(wikitext, options.wrap_sections)?;
        let serializer = crate::html::serialize::HtmlSerializer::new(options.clone());
        serializer.serialize(&ast)
    }

    /// Convert wikitext to an HTML string with native template expansion.
    pub async fn wikitext_to_html_expanded(
        &self,
        wikitext: &str,
        source: &dyn DataSource,
        options: &ParserOptions,
    ) -> Result<String> {
        let ast = self
            .wikitext_to_ast_expanded(wikitext, source, options)
            .await?;
        let serializer = crate::html::serialize::HtmlSerializer::new(options.clone());
        serializer.serialize(&ast)
    }

    /// Convert wikitext to an AST (with `data-parsoid` DSR metadata) using the
    /// native (async) template expansion path. This is the AST counterpart of
    /// [`wikitext_to_html_expanded`], returning the expanded DOM so downstream
    /// stages (e.g. selective serialization / selser) can operate on it.
    pub async fn wikitext_to_ast_expanded(
        &self,
        wikitext: &str,
        source: &dyn DataSource,
        options: &ParserOptions,
    ) -> Result<Node> {
        let tokens = self.tokenize(wikitext)?;
        let about_counter = std::cell::Cell::new(0usize);
        Ok(self
            .build_ast(
                tokens,
                Some(source),
                &options.page_title,
                &about_counter,
                wikitext,
                options,
            )
            .await)
    }

    /// Run the TT2 stage (template/parser-function/magic-variable expansion)
    /// over a token stream, then the TT3 tree-building stage, producing an AST.
    ///
    /// `options` is passed whole rather than field by field: the passes near the end
    /// of the pipeline each read a different flag, and threading them individually
    /// invites a new flag reaching one call site and not another.
    async fn build_ast(
        &self,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        page_title: &str,
        about_counter: &std::cell::Cell<usize>,
        page_source: &str,
        options: &ParserOptions,
    ) -> Node {
        let title = TitleParser::parse(page_title, self.config);
        let page_title_prefixed = title.get_prefixed_text();
        // Recorded for `expand_invoke`, which needs the root page's title for
        // `mw.wikibase`; see the field's own comment.
        *self.page_title.borrow_mut() = title.get_prefixed_text();
        self.strip_data_parsoid.set(options.strip_data_parsoid);
        // The protection of the page being parsed, for `{{PROTECTIONLEVEL:action}}`
        // with no title argument. Fetched once here rather than on demand because
        // expansion is synchronous: a magic word cannot await a fetch, so the
        // answer has to be in hand before the tokens are walked.
        //
        // Keyed by the *prefixed* title and also by the title as given, because
        // `{{PROTECTIONLEVEL:edit|Foo}}` names it the way a module would, without
        // knowing the namespace prefix was implied.
        let mut page_protection = ProtectionEntry::default();
        if let Some(source) = source {
            let wanted = vec![title.get_prefixed_text(), page_title.to_string()];
            if let Some(entry) = source
                .get_title_protection(&wanted)
                .await
                .unwrap_or_default()
                .remove(&title.get_prefixed_text())
            {
                page_protection = entry;
            }
        }
        *self.page_protection.borrow_mut() = page_protection.clone();
        // The page's own protection is a Lua fact as well as a magic-word one:
        // `Module:Protection banner` reads
        // `mw.title.getCurrentTitle().protectionLevels`, which goes through the
        // title-facts map rather than through `page_protection`. Seed both from
        // the one fetch so the module and the magic word cannot disagree.
        if !page_protection.levels.is_empty() {
            self.title_facts.borrow_mut().insert(
                page_title_prefixed.clone(),
                crate::lua::engine::TitleFacts {
                    exists: true,
                    protection: page_protection,
                    ..Default::default()
                },
            );
        }
        // Titles named by a `{{PROTECTIONLEVEL:action|title}}` on this page, which
        // the magic word cannot fetch for itself (expansion is synchronous).
        //
        // Scanned off the *unexpanded* token stream, which is what makes it
        // work: a protection check inside a transcluded template is not visible
        // here, but `tokens` at this point already includes every template this
        // page transcludes only after expansion. A call that arrives from a
        // template therefore reads as unprotected, which is recorded in
        // ONLINE-PARITY.md rather than papered over.
        if let Some(source) = source {
            let mut named = Vec::new();
            collect_protection_titles(&tokens, &mut named);
            if !named.is_empty() {
                *self.protection.borrow_mut() = source
                    .get_title_protection(&named)
                    .await
                    .unwrap_or_default();
            }
        }
        // A parse starts with the preprocessor counters clear, mirroring the part
        // of `Parser::clearState` that zeroes `mPPNodeCount`.
        self.reset_expansion();
        let frame = Frame::new(title.clone(), vec![]);

        let tokens = self
            .expand_templates(
                &frame,
                tokens,
                source,
                about_counter,
                false,
                false,
                page_source,
            )
            .await;
        // TT2 order: ExtensionHandler runs after TemplateHandler and before
        // AttributeExpander (PHP `ParserPipelineFactory::STAGES`). Attribute
        // values are sub-pipelines, so the extension handler must reach inside
        // them before attributes are expanded.
        let mut fragments: std::collections::HashMap<usize, crate::dom::node::Node> =
            std::collections::HashMap::new();
        // One fragment-id space for the whole document, exactly as PHP's
        // `Env::newFragmentId` provides: a link's tunnelled caption, a
        // stylesheet, and a gallery all draw from the same counter, so no two
        // placeholders can share an id and resolve to each other's content.
        // `expand_one_templatestyles` allocates from this same counter while
        // expansion is still running.
        let next_id = &self.ext_next_id;
        let tokens = crate::pipeline::extension_handler::expand_in_attributes(
            tokens,
            self.config,
            &mut fragments,
            next_id,
        );
        let tokens = self
            .expand_attributes(
                &frame,
                tokens,
                source,
                about_counter,
                Some(page_source),
                &mut fragments,
                next_id,
            )
            .await;
        let tokens = self.render_links(tokens, &mut fragments, next_id, Some(&title));
        let tokens = self.render_external_links(tokens, &mut fragments, next_id);
        let tokens = self.render_behavior_switches(tokens);
        let tokens = self.render_language_variants(tokens);
        // Route `format="wikitext"` extension bodies through the inline
        // sub-pipeline, producing `mw:dom-fragment-token` placeholders + their
        // pre-built sub-fragments.
        let (tokens, pre_fragments) = self
            .expand_wikitext_pre(tokens, source, &frame, about_counter, next_id)
            .await;
        fragments.extend(pre_fragments);
        let (tokens, wrapper_fragments) = self
            .expand_wrapper_tag(tokens, source, &frame, about_counter, next_id)
            .await;
        fragments.extend(wrapper_fragments);
        let (tokens, indicator_fragments) = self.expand_indicator(tokens, next_id);
        fragments.extend(indicator_fragments);
        let tokens = self
            .expand_gallery(
                tokens,
                source,
                &frame,
                about_counter,
                &mut fragments,
                next_id,
            )
            .await;
        // `<templatestyles>` is normally resolved inline during expansion, so
        // its `about` id lands in document order; anything left over here is a
        // tag the inline pass could not resolve.
        let tokens = self
            .expand_templatestyles(tokens, source, about_counter)
            .await;
        // Sub-fragments built during expansion (stylesheets). Collected after
        // every pass has run, so none is lost.
        fragments.extend(std::mem::take(&mut *self.ext_fragments.borrow_mut()));

        let stage = TreeBuilderStage::new(false);
        // The `about` counter is still shared with the tree builder: a fragment
        // that reaches it unresolved takes its id there rather than not at all.
        let tree_about_counter = std::rc::Rc::new(std::cell::Cell::new(about_counter.get()));
        let mut ast = stage.to_ast_with_fragments(
            tokens,
            Some(page_source),
            self.config,
            fragments,
            Some(std::rc::Rc::clone(&tree_about_counter)),
        );
        // Anything the builders handed out is now part of the document sequence,
        // so a later transclusion must not reuse those ids.
        about_counter.set(tree_about_counter.get());
        drop(tree_about_counter);
        // Capture transclusion marker depth map over the freshly-built DOM
        // (before p-wrapping restructures it), mirroring PHP's
        // `transclusionMetaTagDepthMap` recorded at tree-build time.
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
        // DOM-level p-wrapping runs before transclusion encapsulation (mirrors
        // PHP's `pwrap` … `tplwrap` order).
        crate::pipeline::p_wrap::run(&mut ast);
        // AddLinkAttributes precedes `dom-unpack` (see `wikitext_to_ast`).
        crate::pipeline::add_link_attributes::run(&mut ast, self.config);
        crate::pipeline::tree_builder_html::post_pwrap_transforms(
            &mut ast,
            &depths,
            Some(page_source),
        );
        // HandleLinkNeighbours (`linkneighbours`) runs after `tplwrap` so it can
        // merge link trail/prefix text that straddles a transclusion
        // encapsulation boundary (setting `dp->tail`/`dp->prefix` + migrating
        // `data-mw.parts`).
        crate::pipeline::handle_link_neighbours::run(&mut ast, self.config);
        crate::pipeline::table_fixups::run(&mut ast, self.config, Some(page_source));
        // DedupeStyles: the third handler of the `fixups` traverser
        // (`MigrateTrailingCategories,TableFixups,DedupeStyles`). It replaces
        // every repeat of an already-emitted `<templatestyles>` with a `<link>`.
        crate::pipeline::dedupe_styles::run(&mut ast);
        // DisplaySpace (`displayspace`): armor French spaces. PHP runs it as a
        // global DOM pass after extension post-processing and before `cleanup`.
        crate::pipeline::display_space::run(&mut ast);
        crate::pipeline::cleanup::run(&mut ast);
        crate::pipeline::headings::gen_anchors(&mut ast);
        // AddRedLinks: resolve which wikilink targets exist, marking missing
        // ones as red links. Gather the relevant page titles, batch-check their
        // existence via the data source, then apply the pass.
        let mut titles = Vec::new();
        crate::pipeline::add_red_links::collect_wikilink_titles(&ast, &mut titles);
        if !titles.is_empty() {
            let page_info = match source {
                Some(source) => source.get_page_info(&titles).await.unwrap_or_default(),
                None => std::collections::HashMap::new(),
            };
            crate::pipeline::add_red_links::run(&mut ast, &page_info, &page_title_prefixed);
        }
        // AddMediaInfo: resolve file metadata for `mw:File` containers and
        // replace broken-media placeholders with real `<img>` elements (or mark
        // missing files as `mw:Error`). Mirrors PHP's `AddMediaInfo` pass.
        if let Some(source) = source {
            crate::pipeline::add_media_info::run(&mut ast, source, self.config).await;
        }
        // After media resolution, repair bad-nesting (`<a>` inside `<a>`) that the
        // DOM-fragment unpack created. This must follow `AddMediaInfo` (mirroring
        // PHP's `media` … `linkneighbours+dom-unpack` order) so the foster-out
        // operates on resolved media rather than a broken-media anchor.
        crate::pipeline::unpack_dom_fragments::fix_bad_nesting(&mut ast);
        // Cite runs after the tree is final, because a marker's number depends on
        // refs that appear later in the document and `<references>` renders the
        // notes from the bottom of the page. A note's body is wikitext, so it is
        // rendered through the inline pipeline here rather than inside the Cite
        // module, which knows nothing about parsing.
        {
            let mut ids = crate::ext::cite::DocIds::new();
            // Cite's `about` ids come out of the document's transclusion
            // sequence, so it continues the counter the expansion already used
            // rather than starting a second one.
            ids.set_about_counter(about_counter.get());
            // `RefCell`/`Cell` because the body renderer must be `Fn` rather than
            // `FnMut`: Cite may call it for several notes, and a `FnMut` would force
            // the Cite module to hold a mutable borrow of state it does not own.
            let note_fragments = std::cell::RefCell::new(std::collections::HashMap::new());
            let next_note_id = std::cell::Cell::new(0usize);
            let render_body = |body: &str| -> Node {
                let mut fragments = note_fragments.borrow_mut();
                render_inline_fragment(
                    self.config,
                    self.tokenize(body).unwrap_or_default(),
                    &mut fragments,
                    &next_note_id,
                )
            };
            crate::ext::cite::run(&mut ast, &page_title_prefixed, &mut ids, &render_body);
            // Hand the advanced counter back: ids are allocated in document
            // order, so anything numbered after Cite must continue from here.
            about_counter.set(ids.about_counter());
        }
        wrap_sections_in_ast(&mut ast, options.wrap_sections);
        // PHP marks the `data-parsoid` a transclusion's interior nodes may not
        // keep as its cleanup traverser stores them, i.e. just before the ids are
        // handed out. Doing it here leaves every earlier pass its `dp`.
        crate::pipeline::cleanup::mark_discardable_data_parsoid(&mut ast);
        // Page-bundle node ids are allocated last, after every pass that can create
        // or destroy an element. The ids are positional — one element inserted
        // earlier shifts every id after it — so the assignment cannot run before
        // the tree is final. `wrap_sections` must come first too, because the
        // `<section>` wrappers are themselves numbered.
        //
        // Gated on `node_ids` because this is what a *wiki* serves, not what
        // Parsoid's standalone mode produces. The fixture suite compares against
        // standalone output and so must not see ids; see the option's own comment.
        if options.node_ids {
            crate::pagebundle::assign_node_ids(&mut ast);
        }
        ast
    }

    /// Enter one preprocessor node on behalf of `expand_templates`.
    ///
    /// Faithful to `PPFrame_Hash::expand`'s prologue, with one deliberate
    /// difference of *placement*: PHP counts one `expand()` call per node it
    /// descends into, while this loop walks a flat token list and so counts one
    /// node per `template`/`parser function` token it is about to expand. The
    /// two agree on what matters — an expansion costs a node, plain text costs
    /// nothing, and a repeat costs again — and the flat loop is exactly why PHP
    /// needs the counting at all: a template whose body has 500 children pays 1
    /// per `expand()`, not 500.
    ///
    /// Returns `Some(error tokens)` when a limit has been hit near the current
    /// position, following the two rules PHP is explicit about:
    ///
    /// - The node count is `++`ed and compared with strict `>`, so it trips at
    ///   `max + 1`.
    /// - The depth check runs *before* the increment, also with strict `>`.
    ///
    /// On a hit the caller emits an error span in this position and carries on:
    /// neither limit aborts the page, and neither is sticky.
    fn enter_pp_node(&self) -> Option<Vec<Item>> {
        let limits = self.config.expansion_limits();
        let count = self.pp_node_count.get() + 1;
        self.pp_node_count.set(count);
        if count > limits.max_pp_node_count {
            return Some(error_span("Node-count limit exceeded"));
        }
        let depth = self.expansion_depth.get();
        if depth > limits.max_pp_expand_depth {
            return Some(error_span("Expansion depth limit exceeded"));
        }
        self.expansion_depth.set(depth + 1);
        None
    }

    /// Leave the node [`enter_pp_node`](Self::enter_pp_node) entered. Mirrors the
    /// `--$expansionDepth` that `PPFrame_Hash::expand` runs on every return path.
    fn leave_pp_node(&self) {
        self.expansion_depth
            .set(self.expansion_depth.get().saturating_sub(1));
    }

    /// Reset the preprocessor counters. Mirrors the part of
    /// `Parser::clearState` that zeroes `mPPNodeCount`.
    fn reset_expansion(&self) {
        self.pp_node_count.set(0);
        self.expansion_depth.set(0);
    }

    /// Render a Lua frame call's expansion to the string the module receives,
    /// replacing each `mw:DOMFragment` placeholder with a `UNIQ…QINU` strip
    /// marker and remembering the placeholder tokens under that marker.
    ///
    /// [`crate::pipeline::lua_deferred::render_answer`] converts tokens to their
    /// wikitext source, which a fragment placeholder does not have — it is a
    /// built sub-tree, not markup. Dropping it is what lost every stylesheet a
    /// module emitted through `frame:extensionTag`; the marker is Scribunto's
    /// own answer shape and keeps the output alive through the module's string
    /// handling.
    fn render_answer_markers(&self, items: &[Item]) -> String {
        let mut replaced: Vec<Item> = Vec::with_capacity(items.len());
        let mut i = 0;
        while i < items.len() {
            match placeholder_span(items, i) {
                Some((end, tokens, tag)) => {
                    let marker = format!(
                        "\u{7f}UNIQ--{tag}-{:08X}-QINU\u{7f}",
                        self.strip_markers.borrow().len()
                    );
                    self.strip_markers
                        .borrow_mut()
                        .insert(marker.clone(), tokens);
                    replaced.push(Item::Str(marker));
                    i = end;
                }
                None => {
                    replaced.push(items[i].clone());
                    i += 1;
                }
            }
        }
        crate::pipeline::lua_deferred::render_answer(&replaced)
    }

    /// Splice remembered strip markers back into their placeholder tokens.
    ///
    /// The counterpart to [`Parser::render_answer_markers`]: a module's output
    /// is parsed as wikitext, so a marker that reaches a token stream becomes
    /// the fragment placeholder it stood for, exactly where the module left it.
    fn substitute_strip_markers(&self, tokens: Vec<Item>) -> Vec<Item> {
        if !tokens
            .iter()
            .any(|it| matches!(it, Item::Str(s) if s.contains('\u{7f}')))
        {
            return tokens;
        }
        let mut out = Vec::with_capacity(tokens.len());
        let mut pending: Vec<String> = Vec::new();
        let flush = |pending: &mut Vec<String>, out: &mut Vec<Item>| {
            if pending.is_empty() {
                return;
            }
            let joined = pending.join("");
            pending.clear();
            // A marker is `\x7f…\x7f` with no sentinel in between; everything
            // outside one is ordinary text.
            let mut rest = joined.as_str();
            while let Some(start) = rest.find('\u{7f}') {
                if start > 0 {
                    out.push(Item::Str(rest[..start].to_string()));
                }
                let after = &rest[start + 1..];
                let Some(end) = after.find('\u{7f}') else {
                    out.push(Item::Str(rest[start..].to_string()));
                    return;
                };
                let marker = &rest[start..start + end + 2];
                match self.strip_markers.borrow().get(marker) {
                    Some(items) => out.extend(items.iter().cloned()),
                    None => out.push(Item::Str(marker.to_string())),
                }
                rest = &after[end + 1..];
            }
            if !rest.is_empty() {
                out.push(Item::Str(rest.to_string()));
            }
        };
        for item in tokens {
            // The tokenizer splits a marker at every `-`, so the sentinel may be
            // several adjacent `Str` items. Join exactly the runs that hold one;
            // a run without a marker keeps its boundaries.
            if let Item::Str(s) = &item {
                pending.push(s.clone());
                if !s.contains('\u{7f}') && !pending.iter().any(|p| p.contains('\u{7f}')) {
                    out.append(&mut pending.drain(..).map(Item::Str).collect());
                }
                continue;
            }
            flush(&mut pending, &mut out);
            out.push(item);
        }
        flush(&mut pending, &mut out);
        out
    }

    /// Expand `template`/`templatearg` tokens in-place.
    ///
    /// `in_template` mirrors PHP's `wrapTemplates = !$options['inTemplate']`:
    /// when true (nested template / extension-content context), expanded
    /// templates are returned *without* `mw:Transclusion` encapsulation.
    ///
    /// `src_text` is the wikitext the tokens were tokenized from, used to
    /// recover argument source spans (`ParamInfo`'s `valueWt`). Tokens whose
    /// ranges carry their own source ignore it.
    #[allow(clippy::too_many_arguments)]
    async fn expand_templates(
        &self,
        frame: &Frame,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
        // Is this chunk a *nested pipeline's* content — a template body, an
        // extension body, a module's output? PHP sets `inTemplate` on the
        // pipeline that runs such a chunk, and `wrapTemplates = !inTemplate`
        // follows from it, so nothing in here is encapsulated. It is a separate
        // flag from `in_template`, which stays the *caller's* value: `{{!}}`
        // keys off `atTopLevel` and the fixtures pin it to `|` one level down.
        body: bool,
        src_text: &str,
    ) -> Vec<Item> {
        // A marker a module held and moved becomes the fragment placeholder it
        // stood for, in the position the module left it.
        let tokens = self.substitute_strip_markers(tokens);
        let mut out = Vec::new();
        // PHP's `tableDataBlock` context for the token being expanded: true while
        // the walk sits inside an unclosed `table` tag. A template body expanded
        // here inherits it, which is what lets the body's `|` split cells when the
        // governing `{|` came from an *earlier* expansion (`{{start}}\n|a\n{{end}}`
        // with `Template:start` = `{|`).
        //
        // Tracked over *both* the input stream and the tokens emitted so far: a
        // `{|` produced by expanding a previous template (`{{tbl-start}}`) opens
        // the table for the tokens after it, exactly as a literal `{|` would.
        let mut table_depth = 0usize;
        for item in tokens {
            track_table(&item, &mut table_depth);
            let Item::Tok(tok) = &item else {
                out.push(item);
                continue;
            };
            let ParsoidToken::SelfclosingTag(stt) = tok else {
                out.push(item);
                continue;
            };

            // Resolve a `<templatestyles>` here, in document order, so its
            // `about` id comes from the same sequence as the transclusions
            // around it. The service numbers the two stylesheets of
            // `Template:Infobox` 2 and 3 — immediately after the `#invoke`
            // wrapper's 1 — which a pass running after expansion cannot
            // reproduce, because by then the whole documentation subtree has
            // taken 4 onwards.
            if templatestyles_target(&item).is_some() {
                let emitted = self
                    .expand_one_templatestyles(&item, source, about_counter)
                    .await;
                out.extend(emitted);
                continue;
            }

            if stt.name == "template" || stt.name == "template3" {
                // Charge one preprocessor node for this expansion. On a limit
                // trip PHP substitutes an error span here and returns, leaving
                // the rest of the chunk to expand normally.
                if let Some(error) = self.enter_pp_node() {
                    out.extend(error);
                    continue;
                }
                let expanded = self
                    .expand_template_token(
                        frame,
                        &item,
                        stt,
                        source,
                        about_counter,
                        in_template,
                        body,
                        src_text,
                        &mut table_depth,
                    )
                    .await;
                self.leave_pp_node();
                for e in &expanded {
                    track_table(e, &mut table_depth);
                }
                out.extend(expanded);
                continue;
            }

            if stt.name == "templatearg" {
                if let Some(error) = self.enter_pp_node() {
                    out.extend(error);
                    continue;
                }
                // PHP builds the `TemplateEncapsulator` for a template argument
                // only when it wraps it (`onTemplateArg`'s `wrapTemplates &&
                // expandTemplates`), so a `{{{…}}}` inside a template body takes no
                // about id at all. Taking one anyway advanced the page's counter
                // for every argument in an expansion — `Template:Short description`
                // has a dozen — and put the next transclusion's `about` far ahead
                // of the service's (11 where the service has 2).
                let wrap = !in_template;
                let about_id = if wrap {
                    self.new_about_id(about_counter)
                } else {
                    String::new()
                };
                let produced =
                    TemplateHandler.handle_template_arg_token(frame, tok, about_id, wrap);
                self.leave_pp_node();
                // The default is wikitext, and PHP's `Frame::expand` runs the
                // chunk it comes from through the whole pipeline — so a template
                // in a default expands: `{{{p|{{T}}}}}` answers `T`'s expansion,
                // not its source. Re-walking is what expands it here.
                let expanded = Box::pin(self.expand_templates(
                    frame,
                    produced,
                    source,
                    about_counter,
                    in_template,
                    body,
                    src_text,
                ))
                .await;
                for e in &expanded {
                    track_table(e, &mut table_depth);
                }
                out.extend(expanded);
                continue;
            }

            // An `<indicator>` is resolved *here*, in document order, for the
            // same reason as `<templatestyles>` above: the extension spends an
            // `about` id (PHP's `ExtensionHandler::onDocumentFragment` calls
            // `newAboutId` for every tag except `nowiki`), and a module emits
            // the indicator from *inside* a template, so a pass running after
            // expansion would number it too late — the transclusion following
            // the template would take the id the indicator should have spent.
            if let Some(emitted) = self.expand_one_indicator(&item, about_counter) {
                out.extend(emitted);
                continue;
            }

            {
                let mut d = table_depth;
                track_table(&item, &mut d);
                table_depth = d;
            }
            out.push(item);
        }

        // TT2's `ExtensionHandler` numbers every extension token with
        // `$env->newAboutId()`, and `TokenHandlerPipeline::processChunk` runs each
        // transformer over the *whole* chunk — `TemplateHandler` then
        // `ExtensionHandler` — so at each level the chunk's templates are
        // expanded and numbered first, the extensions after. The id has to be
        // spent here, inside the expansion, because a pass that runs once the
        // tree is built cannot reproduce that order: `Bicycle`'s infobox `<ref>`
        // is `#mwt11` in the service and `#mwt199` here, the 188 in between
        // being infobox rows the late Cite pass ran past before numbering the
        // ref.
        //
        // Only the extensions whose output consumes the id are numbered here.
        // `<ref>`/`<references>` keep it on the `<extension>` element the Cite
        // pass later reads; `<pre>` keeps it on the `<pre>` element (see
        // `extension_handler::pre_items`). `<nowiki>` is lean markup with no
        // `about` at all. The remaining extensions are rebuilt from their rich
        // `data-mw` attribs (`extension_kv_attrs`), which do not carry a token
        // attribute, so numbering them here would spend an id the output never
        // shows — a second, different drift. They are left to a follow-up.
        for item in out.iter_mut() {
            let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item else {
                continue;
            };
            if t.name != "extension" {
                continue;
            }
            let name = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("name"))
                .and_then(|kv| kv.value.as_str());
            if !matches!(name, Some("ref") | Some("references") | Some("pre")) {
                continue;
            }
            if t.attribs.iter().any(|kv| kv.key.as_str() == Some("about")) {
                continue;
            }
            let about = self.new_about_id(about_counter);
            t.add_attribute_str("about", &about);
        }
        out
    }

    /// Expand one `template` token, whose preprocessor node has already been
    /// charged by the caller. The `table_depth` is carried in and out because an
    /// expansion's own tokens change whether the tokens *after* it sit in a table.
    #[allow(clippy::too_many_arguments)]
    async fn expand_template_token(
        &self,
        frame: &Frame,
        _item: &Item,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
        body: bool,
        src_text: &str,
        table_depth: &mut usize,
    ) -> Vec<Item> {
        // The paths below were written against the enclosing `ParsoidToken`; keep
        // that shape by borrowing the one token this self-closing child came from.
        let token = ParsoidToken::SelfclosingTag(stt.clone());
        let tok = &token;

        // A token spliced in from an *argument value* is expanded in
        // template context: PHP expands argument values with a hard-coded
        // `inTemplate => true` (see `mark_arg_value_tokens`).
        let arg_value = stt.data_parsoid.tmp.in_arg_value;
        let in_tpl = in_template || arg_value;

        // PHP's `wrapTemplates`, which decides whether this expansion gets its own
        // `mw:Transclusion` markers. Nothing inside a template *body* is wrapped:
        // the body runs in a nested pipeline, and its tokens are spliced into the
        // caller's expansion, which carries the one wrapper. That includes the
        // argument values spliced into it — PHP does not expand them up front
        // (`AttributeTransformManager` there runs with `expandTemplates => false`)
        // but hands them to `Frame::expand` inside the body, so
        // `{{1x|{{T}}}}` has *one* wrapper, around the `1x` call, and the fixture
        // suite pins that.
        let wrap = !body && !in_tpl;

        // The id is taken exactly when the expansion is wrapped. PHP builds its
        // `TemplateEncapsulator` — the object that owns the id — only where a
        // wrapper is produced, and the service's `about` sequence shows it: a
        // template's own expansion takes no id, so a `{{…}}` written in its source
        // must not advance the counter either. `body` carries that context
        // (the same pair of flags as `wrap` above). Taking an id for these instead
        // put every transclusion after a busy template far ahead of the service:
        // on `List of sovereign states` the Redirect2 hatnote was `#mwt11` where
        // the service has `#mwt2`, because `Template:Short description`'s own
        // expansion had consumed 2…10.
        //
        // A variable or parser function takes its own id inside
        // `TemplateHandler::process`, which is a separate path.
        let take_id = || {
            if in_tpl || body {
                String::new()
            } else {
                self.new_about_id(about_counter)
            }
        };

        // The *target* (attribs[0]) may hold a nested template
        // (`{{ {{T}} }}`): PHP's `expandTemplate` calls
        // `AttributeExpander::expandFirstAttribute` before resolving the
        // target, and `Frame::expand` runs the chunk through the TT2
        // pipeline, expanding that inner template. rustoid's
        // `Frame::expand` is synchronous, so do that pass here first and
        // only then hand the (template-free) target to the
        // AttributeTransformManager for the `{{{...}}}` substitution it
        // owns. Argument values are deliberately left alone: PHP expands
        // the template token's *body* arguments later, with
        // `expandTemplates => false`.
        let attribs = self
            .expand_target_templates(
                frame,
                stt.attribs.clone(),
                source,
                about_counter,
                in_tpl,
                body,
                src_text,
            )
            .await;
        // Expand `{{{…}}}` template-argument references in the token's
        // argument keys/values against the current frame (mirrors PHP's
        // `expandTemplateNatively` → `AttributeTransformManager::process`, so
        // `{{#tag:pre|{{{1}}}|…}}` sees the argument's *value*).
        let attribs =
            crate::pipeline::attribute_transform_manager::process(frame, false, in_tpl, &attribs)
                .unwrap_or(attribs);
        let params = crate::pipeline::parser_functions::Params::new(attribs.clone());

        // The target may hold a nested template (`{{ {{T}} }}`), in which
        // case the KV's key carries *tokens*, not a string: the
        // AttributeTransformManager above expands it against the frame,
        // and a multi-item result stays `Tokens`. Stringify it the way
        // PHP's `processToString` does — a plain `as_str()` silently
        // yielded `""`, producing an empty target.
        let target_str = attribs
            .first()
            .map(|kv| match &kv.key {
                crate::wikitext::tokens_v2::KeyValue::Str(s) => s.clone(),
                crate::wikitext::tokens_v2::KeyValue::Tokens(t) => {
                    crate::wikitext::token_utils::tokens_to_string(t)
                }
            })
            .unwrap_or_default();

        // The target may hold a nested template
        // A comment in the template *target* (`{{f<!---->oo}}`) is
        // stripped by the PHP preprocessor before target resolution,
        // so the template still expands, but Parsoid does not wrap the
        // expansion in a `mw:Transclusion` wrapper (the target is no
        // longer cleanly stringifiable). Detect this from the raw
        // template source so the expansion matches PHP's
        // `convertToString(..., /* expandTemplates */ true)` path,
        // which emits the expansion unencapsulated.
        let target_has_comment = stt
            .data_parsoid
            .src
            .as_deref()
            .and_then(raw_template_target)
            .map(|t| t.contains("<!--"))
            .unwrap_or(false);

        // The transclusion's `data-mw.target.wt` is the *source substring* of
        // the target key, not the cleaned name: PHP's
        // `TemplateEncapsulator::getTemplateInfo` takes it from
        // `$params[0]->srcOffsets->key->substr($src)`, which skips leading
        // whitespace but keeps trailing whitespace and comments. So
        // `{{Infobox machine\n|…}}` records `"Infobox machine\n"` and
        // `{{Use dmy dates|…}}` records `"Use dmy dates"`, while `target_str`
        // (trimmed, comments stripped) is what resolves the template.
        let target_wt = stt
            .data_parsoid
            .src
            .as_deref()
            .and_then(raw_template_target)
            .map(|t| t.trim_start().to_string())
            .unwrap_or_else(|| target_str.clone());

        let mut out = Vec::new();
        let resolved = resolve_template_target(self.config, Some(frame.title()), &target_str);
        // Whether this call's result is a *string* the token stream re-tokenizes,
        // rather than the branch's own tokens. See
        // [`TemplateHandler::expands_branch_to_text`]; the `_` arm below is where
        // it takes effect.
        let branch_is_text = matches!(
            &resolved,
            Some(ResolvedTarget::ParserFunction { name, .. })
                if TemplateHandler::expands_branch_to_text(name)
        );
        // `#ifexist` needs its answer before the synchronous parser-function call
        // below. The title is known here — the target's `{{{…}}}` have already
        // been substituted into `target_str` — which is exactly why this cannot be
        // a pre-pass over the unexpanded stream.
        if let Some(ResolvedTarget::ParserFunction { name, pf_arg, .. }) = &resolved
            && name.eq_ignore_ascii_case("ifexist")
        {
            self.prime_ifexist(pf_arg, source).await;
        }
        match resolved {
            // `#invoke` is Scribunto, not a parser function: MediaWiki
            // hands the call to Lua and feeds the result back through the
            // parser. Parsoid implements none of it in standalone mode,
            // which is why the fixture suite cannot exercise it.
            //
            // It is intercepted here, ahead of the synchronous
            // parser-function path, because fetching a module is async.
            Some(ResolvedTarget::ParserFunction {
                name, ref pf_arg, ..
            }) if name.eq_ignore_ascii_case("invoke") => {
                let about_id = take_id();
                // Scribunto's `#invoke` is not an ordinary parser
                // function: everything after the colon is its argument
                // list, and the tokenizer has already split that on `|`.
                //
                // Each argument value is *expanded* first, because that is what
                // Scribunto receives: `{{#invoke:String|len|x{{#invoke:String|
                // len|abc}}y}}` measures `x3y` and answers 3, not the 17 that
                // the unexpanded text gives. The text handed to the module and
                // the wikitext recorded in `data-mw` are therefore different
                // things, and `data-mw` still reads the raw source through its
                // own path (`prepare_pf_param_infos`).
                // `args[0]` is the target, which has already been resolved, so
                // only the arguments after it are handed over.
                let expanded_args = self
                    .expand_invoke_args(
                        &params.args[1..],
                        frame,
                        source,
                        about_counter,
                        body,
                        src_text,
                    )
                    .await;
                // The call is built *structurally* from `pf_arg` (the module) and
                // the expanded arguments, not by re-joining them into text: the
                // first argument is the function name, the rest are the module's
                // arguments, and re-joining would re-derive a value's name from a
                // string that cannot tell a `|` or `=` inside a value from a
                // separator. See [`crate::lua::invoke::Invoke::from_parts`].
                let mut arg_pairs: Vec<(Option<String>, String)> =
                    expanded_args.iter().map(expanded_arg_pair).collect();
                let function = if arg_pairs.is_empty() {
                    String::new()
                } else {
                    arg_pairs.remove(0).1
                };
                let Some(call) =
                    crate::lua::invoke::Invoke::from_parts(pf_arg, &function, arg_pairs)
                else {
                    return vec![Item::Str(format!("{{{{#invoke:{pf_arg}}}}}"))];
                };
                // Scribunto's `frame:getParent()` is the frame of the
                // *calling template*, and modules read its args
                // constantly (`Module:Infobox`, `Module:Check for
                // conflicting parameters` both do it on their first
                // lines). The parent's arguments are this frame's.
                //
                // They must be **expanded**, exactly like the `#invoke` call's own
                // arguments above: Scribunto hands a module the expanded text, so
                // `{{If empty|…}}` written in a template's argument arrives as its
                // value. Passing the raw source instead left the string
                // `{{If empty|…}}` inside `parent.args`, and `Module:Infobox`
                // expands what it reads — re-entering the parser, re-invoking the
                // module, and expanding the same helpers again. The expansion
                // breadth never grew, so the depth limit never fired:
                // `{{Infobox person}}` alone fetched 337 templates in ten seconds
                // and never finished.
                // The parent's argument list has no `#invoke:` target, so every
                // entry is an argument — including the first. Skipping one here (as
                // the call above does for its target) silently left the calling
                // template's *first* argument unexpanded, so a module read its
                // `{{{…}}}` verbatim: `{{see Wiktionary|…}}` hands its text to
                // `Module:Hatnote` through exactly that position.
                let parent_args = self
                    .expand_invoke_args(
                        &frame.args().args.clone(),
                        frame,
                        source,
                        about_counter,
                        body,
                        src_text,
                    )
                    .await;
                let expanded = self
                    .expand_invoke(
                        source,
                        frame,
                        &call,
                        &params,
                        about_id,
                        tok,
                        // `wrapTemplates` for this call, then the caller's
                        // `inTemplate` for the nested pipeline that runs the
                        // module's output. Passing the caller's flags *without*
                        // `wrap` here inverted both: a page-level `#invoke` was
                        // left unwrapped (no `about`/`typeof`/`data-mw`, where the
                        // service emits `<span about="#mwt1"
                        // typeof="mw:Transclusion">3</span>`), while one inside a
                        // template body was wrapped — and a wrapped wrapper is not
                        // stashable, which is what stopped a template's trailing
                        // category run from being grouped.
                        wrap,
                        body,
                        src_text,
                        parent_args,
                        about_counter,
                    )
                    .await;
                for e in &expanded {
                    track_table(e, table_depth);
                }
                out.extend(expanded);
            }
            Some(ResolvedTarget::Template { name, title }) => {
                let about_id = take_id();
                let expanded = self
                    .expand_one_template(
                        source,
                        frame,
                        &name,
                        &target_wt,
                        &title,
                        &params,
                        about_id,
                        tok,
                        about_counter,
                        in_tpl,
                        wrap,
                        target_has_comment,
                        src_text,
                        *table_depth > 0,
                    )
                    .await;
                for e in &expanded {
                    track_table(e, table_depth);
                }
                out.extend(expanded);
            }
            Some(ResolvedTarget::Variable {
                magic_word_type: Some(magic),
                ..
            }) => {
                // The `{{!}}` magic word expands to a literal `|` at the
                // top level, or a `<td>` inside a template (so TableFixups
                // can reinterpret it as a cell separator). PHP's
                // `expandTemplate` → `processSpecialMagicWord` does this at
                // the token level; it must not be string-substituted and
                // re-tokenized into a table delimiter.
                //
                // A token spliced in from an *argument value* always counts
                // as in-template: PHP expands argument values under a
                // hard-coded `inTemplate => true` (see
                // `mark_arg_value_tokens`).
                out.extend(
                    crate::pipeline::template_handler::process_special_magic_word(
                        &magic,
                        in_template || arg_value,
                    ),
                );
            }
            None => {
                // The target is not a template / variable / parser
                // function (e.g. an invalid title such as
                // `{{ {{T}} }}` → `Main Page|Something else`, whose `|`
                // makes it an illegal title). Bail back to literal
                // `{{` … `}}` around the re-tokenized source.
                let bailed = TemplateHandler::convert_to_string(tok, in_tpl, *table_depth > 0);
                // PHP's `convertToString` runs the bailed chunk through
                // `wikitext-to-expanded-tokens`, so a nested template in
                // the source (the `{{T290526}}` above) still expands.
                let expanded = Box::pin(self.expand_templates(
                    frame,
                    bailed,
                    source,
                    about_counter,
                    in_tpl,
                    body,
                    src_text,
                ))
                .await;
                for e in &expanded {
                    track_table(e, table_depth);
                }
                out.extend(expanded);
            }
            _ => {
                // Rebuild the token with expanded argument references so
                // parser-function / `mw:Param` / variable paths see the arg
                // *values* (e.g. `{{#tag:pre|{{{1}}}|…}}`).
                let mut expanded_tok = stt.clone();
                expanded_tok.attribs = attribs;
                let expanded_item = Item::Tok(ParsoidToken::SelfclosingTag(expanded_tok));
                let produced = TemplateHandler.process(
                    self.config,
                    frame,
                    about_counter,
                    &self.protection_context(),
                    &self.ifexist.borrow(),
                    vec![expanded_item],
                    wrap,
                );
                // A parser function hands back a *branch*, and a branch is
                // wikitext: PHP's handlers return its tokens unexpanded and the
                // token stream processes them again, so `{{#ifeq:1|1|{{Large|…}}|z}}`
                // expands `Large` and `{{#if:1|{{PAGENAME}}}}` answers the page
                // name. Returning them verbatim instead leaked the raw
                // `template` token through to the DOM builder, which named an
                // element after it (`<template Large="" 1="x">`) — the whole
                // family of `Help:Introduction`'s first difference, which is a
                // `{{SHORTDESC:…}}` inside `Template:Short description`'s
                // `#ifeq` branch. Expansion happens *after* the wrapper is built,
                // so the wrapper's own id still precedes its children's, as on
                // the service.
                let expanded = Box::pin(self.expand_templates(
                    frame,
                    produced,
                    source,
                    about_counter,
                    in_tpl,
                    body,
                    src_text,
                ))
                .await;
                let mut expanded = expanded;
                if branch_is_text {
                    // The branch is text in the service, so nothing in it is a
                    // templated *attribute* by the time it is spliced in — see
                    // [`TemplateHandler::expands_branch_to_text`]. rustoid expands
                    // the branch in place, which leaves the templates of a
                    // wikitext target unexpanded until `expand_attributes` looks
                    // at them; flagging them here is what keeps that difference
                    // from turning into a marking the service does not have.
                    mark_in_text_branch(&mut expanded);
                }
                for e in &expanded {
                    track_table(e, table_depth);
                }
                out.extend(expanded);
            }
        }
        out
    }

    /// Expand nested templates in a template token's **target** (attribs[0]).
    ///
    /// PHP's `expandTemplate` calls `AttributeExpander::expandFirstAttribute`
    /// before resolving the target, and `Frame::expand` runs the chunk through
    /// the whole TT2 pipeline (`peg-tokens-to-expanded-tokens`), so a `template`
    /// token in the target — as in `{{ {{T290526}} }}` — is expanded there.
    /// rustoid's [`Frame::expand`] is synchronous and only handles `{{{...}}}`
    /// references, so the template half is done here, in the async parser loop,
    /// before [`attribute_transform_manager::process`] runs.
    ///
    /// Only attribs[0] is touched, matching `expandFirstAttribute`: an argument
    /// *value* holding a template stays unexpanded for the template body
    /// expansion to handle.
    #[allow(clippy::too_many_arguments)]
    async fn expand_target_templates(
        &self,
        frame: &Frame,
        mut attribs: Vec<crate::wikitext::tokens_v2::KV>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
        body: bool,
        src_text: &str,
    ) -> Vec<crate::wikitext::tokens_v2::KV> {
        let Some(target) = attribs.first_mut() else {
            return attribs;
        };
        let crate::wikitext::tokens_v2::KeyValue::Tokens(items) = &target.key else {
            return attribs;
        };
        if !items.iter().any(is_template_item) {
            return attribs;
        }
        let items = items.clone();
        let expanded = Box::pin(self.expand_templates(
            frame,
            items,
            source,
            about_counter,
            in_template,
            body,
            src_text,
        ))
        .await;
        target.key = crate::wikitext::tokens_v2::KeyValue::Tokens(expanded);
        attribs
    }

    /// Expand templated attribute keys/values on `Tag`/`SelfclosingTag` tokens,
    /// then run `buildExpandedAttrs` to finalize the attributes (reparse-KV,
    /// `mw:ExpandedAttrs` marking). Mirrors the TT2 `AttributeExpander` handler
    /// (`onAny` → `processComplexAttributes`).
    #[allow(clippy::too_many_arguments)]
    async fn expand_attributes(
        &self,
        frame: &Frame,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        page_source: Option<&str>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        let mut out = Vec::new();
        for item in tokens {
            let Item::Tok(tok) = &item else {
                out.push(item);
                continue;
            };
            if !matches!(tok, ParsoidToken::Tag(_) | ParsoidToken::SelfclosingTag(_)) {
                out.push(item);
                continue;
            }
            if tok.get_name() == "mw:dom-fragment-token" {
                out.push(item);
                continue;
            }

            let attribs = tok.get_attribs().to_vec();
            let has_tokens = attribs.iter().any(|kv| {
                matches!(kv.key, KeyValue::Tokens(_)) || matches!(kv.value, KeyValue::Tokens(_))
            });
            if !has_tokens {
                out.push(item);
                continue;
            }

            // Expand each templated key/value (template args and templates).
            let outer_fragments = &mut *fragments;
            let fragments = std::cell::RefCell::new(std::mem::take(outer_fragments));
            let mut expanded_attrs: Vec<KV> = Vec::with_capacity(attribs.len());
            for kv in &attribs {
                let new_key = if let KeyValue::Tokens(toks) = &kv.key {
                    let expanded = self
                        .expand_templates(
                            frame,
                            toks.clone(),
                            source,
                            about_counter,
                            false,
                            false,
                            page_source.unwrap_or(""),
                        )
                        .await;
                    crate::pipeline::attribute_transform_manager::items_to_key_value(expanded)
                } else {
                    kv.key.clone()
                };
                let new_value = if let KeyValue::Tokens(toks) = &kv.value {
                    let expanded = self
                        .expand_templates(
                            frame,
                            toks.clone(),
                            source,
                            about_counter,
                            false,
                            false,
                            page_source.unwrap_or(""),
                        )
                        .await;
                    crate::pipeline::attribute_transform_manager::items_to_key_value(expanded)
                } else {
                    kv.value.clone()
                };
                expanded_attrs.push(KV {
                    key: new_key,
                    value: new_value,
                    src_offsets: kv.src_offsets.clone(),
                    ksrc: kv.ksrc.clone(),
                    vsrc: kv.vsrc.clone(),
                });
            }

            let result = crate::pipeline::attribute_expander::build_expanded_attrs(
                tok.clone(),
                &attribs,
                expanded_attrs,
                about_counter,
                false,
                &|kv| self.value_to_dom_html(kv, &mut fragments.borrow_mut(), next_id),
                page_source,
            );
            out.extend(result);
            *outer_fragments = fragments.into_inner();
        }
        out
    }

    /// Fetch, expand, and recursively re-process a single template. Mirrors the
    /// native template expansion path (`fetchTemplateAndTitle` +
    /// `processTemplateSource`), then recursively runs the fetched+substituted
    /// source through template expansion with a child frame carrying the
    /// template's arguments.
    #[allow(clippy::too_many_arguments)]
    async fn expand_one_template(
        &self,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        name: &str,
        // The raw source of the target, for `data-mw.target.wt`.
        target_wt: &str,
        title: &crate::title::Title,
        params: &crate::pipeline::parser_functions::Params,
        about_id: String,
        token: &ParsoidToken,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
        // PHP's `wrapTemplates`: whether this expansion gets its own
        // `mw:Transclusion` markers. False inside a template body, where the
        // tokens are spliced into the caller's expansion and the caller carries
        // the wrapper.
        wrap: bool,
        target_has_comment: bool,
        page_source: &str,
        // Whether the *call site* was inside an open table (PHP's
        // `tableDataBlock`). Carried into the body tokenization so a body's `|`
        // still splits cells when the governing `{|` came from elsewhere.
        in_table: bool,
    ) -> Vec<Item> {
        // `$wgMaxTemplateDepth`, MediaWiki's own default. This is *transclusion*
        // nesting (`Frame::depth`), a different thing from the preprocessor's
        // expansion depth that [`enter_pp_node`](Self::enter_pp_node) tracks.
        const MAX_TEMPLATE_DEPTH: usize = 100;

        // Enforce loop / recursion constraints. Mirrors `Parser::braceSubstitution`,
        // whose node-count and expansion-depth checks have already run in
        // `expand_templates` by the time the template is resolved.
        if let Some(err) = crate::pipeline::template_handler::enforce_template_constraints(
            self.config,
            frame,
            name,
            title,
            MAX_TEMPLATE_DEPTH,
            false,
        ) {
            return err;
        }

        // Without a data source, a template becomes a redlink.
        let Some(src) = source else {
            if !wrap {
                return vec![crate::pipeline::template_handler::template_to_wikilink(
                    name,
                    self.config,
                )];
            }
            let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
            let info = template_info_from(None, Some(name), vec![]);
            return encap.encap_tokens(
                vec![crate::pipeline::template_handler::template_to_wikilink(
                    name,
                    self.config,
                )],
                &info,
            );
        };

        let fetched = src.get_template(title).await.ok().flatten();
        let Some(template_src) = fetched else {
            if !wrap {
                return vec![crate::pipeline::template_handler::template_to_wikilink(
                    name,
                    self.config,
                )];
            }
            let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
            let info = template_info_from(None, Some(name), vec![]);
            return encap.encap_tokens(
                vec![crate::pipeline::template_handler::template_to_wikilink(
                    name,
                    self.config,
                )],
                &info,
            );
        };

        // A transcluded **redirect** is followed to its target, which is what
        // MediaWiki does: `Template:Pp-semi` is `#REDIRECT [[Template:Protected
        // page]]`, and rendering that line literally yields a redirect listing
        // instead of the protected-page icon. Standard on any wiki (`{{db}}`),
        // and a whole class of the corpus's templates.
        //
        // Arguments are carried over untouched: the *caller's* parameters belong
        // to the target (`{{pp-semi|small=yes}}` reads `{{{small|}}}` there), and
        // the child frame below is built from `title` only after this, so the
        // redirect must be resolved before that frame exists.
        //
        // `data-mw` still names the *redirect* the page called, not the target —
        // that is what Parsoid records (`{{pp-semi-indef}}` is written with
        // `"wt":"pp-semi-indef","href":"./Template:Pp-semi-indef"` even though
        // the body comes from `Template:Semi-protected indefinitely`) — so only
        // the body source and the frame title are replaced here. `called_title`
        // is what the `href` is built from, and the `data-mw` targets below it.
        let called_title = title.clone();
        let (template_src, title) = match follow_template_redirect(src, title).await {
            Redirected::Followed { body, title } => (body, title),
            // The page exists and *is* a redirect, but its target could not be
            // fetched. Treat it as a missing template rather than falling back to
            // the redirect's own body: the `#REDIRECT [[…]]` line and its
            // `[[Category:…]]` trailer are bookkeeping, and rendering them as
            // article text invents content Parsoid never emits.
            Redirected::Unresolved => {
                if !wrap {
                    return vec![crate::pipeline::template_handler::template_to_wikilink(
                        name,
                        self.config,
                    )];
                }
                let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
                let info = template_info_from(None, Some(name), vec![]);
                return encap.encap_tokens(
                    vec![crate::pipeline::template_handler::template_to_wikilink(
                        name,
                        self.config,
                    )],
                    &info,
                );
            }
            Redirected::NotARedirect => (template_src, title.clone()),
        };

        // Build a child frame carrying the template's arguments (params[1..]).
        //
        // PHP expands the template token's *attributes* — i.e. the argument keys
        // and values — with `Frame::expand(...)` under a hard-coded
        // `[ 'expandTemplates' => false, 'inTemplate' => true ]`
        // (TemplateHandler.php:988). The `inTemplate => true` matters: a `{{!}}`
        // *inside an argument value* becomes a `<td>` cell token, which is what
        // lets `{{1x|1={{!}}bar}}` split its enclosing cell. Templates in the
        // *template body* are expanded with the caller's flag instead
        // (`processTemplateSource` forwards `$this->options`).
        //
        // The values stay unexpanded here (rustoid expands them together with the
        // body), so mark their template tokens instead: the body expansion below
        // reads the mark to pick the right `inTemplate`.
        let mut child_args: Vec<crate::wikitext::tokens_v2::KV> =
            params.args.iter().skip(1).cloned().collect();
        for arg in child_args.iter_mut() {
            let crate::wikitext::tokens_v2::KeyValue::Tokens(items) = &mut arg.value else {
                continue;
            };
            mark_arg_value_tokens(items);
        }
        let child_frame = frame.new_child(title.clone(), child_args);

        // Resolve the include directives (`<noinclude>` / `<includeonly>` /
        // `<onlyinclude>`) in the template source before substituting arguments,
        // mirroring `expandTemplate`'s pre-processing (a transcluded template's
        // `<noinclude>` self-documentation is *not* part of the expansion).
        let template_src = crate::expand::transclusion::strip_noinclude_sections(&template_src);
        let template_src = crate::expand::transclusion::extract_includeonly_sections(&template_src);
        let template_src = crate::expand::transclusion::extract_onlyinclude_sections(&template_src);

        // Tokenize the template source *without* string substitution (mirrors PHP's
        // `processTemplateSource`, which tokenizes with `inTemplate=true` and defers
        // argument substitution to `Frame::expand`). Extension tags must be
        // registered so their bodies are captured as `extension` tokens.
        let items = crate::pipeline::template_handler::tokenize_wikitext_to_items_in_table(
            &template_src,
            /* in_template */ true,
            self.config.extension_tags(),
            in_table,
        );

        // Argument substitution replaces each `{{{…}}}` with its value's
        // tokens. A plain-string value (the default `{{{attr|}}}`, or a string
        // argument) stays a string, exactly as PHP's `Frame::expandArg` returns
        // `[ $arg ]` for a string.
        //
        // The result is normally *not* re-tokenized: PHP tokenizes the template
        // body source once, so a `|` that follows an argument reference is already
        // known to be mid-line content (`{{{attr|}}}{{{cmt|}}}| foo`) and must not
        // be promoted to a table-cell separator.
        //
        // The one case that does need re-tokenizing is a body that was *only* an
        // argument reference (or several back to back), because the substituted
        // text then lands where the body began — i.e. at start of line. PHP gets
        // this for free: its `{{1x|!!foo}}` puts `!!foo` at the very start of the
        // body. Detect it by checking that every item before the text run is
        // itself a `templatearg` reference at the body start.
        let spliced = child_frame.expand(&items);
        // The substituted text is at start of line only when the body began with a
        // run of argument references and nothing else preceded the trailing text.
        let body_was_arg_refs = items.iter().all(|it| {
            matches!(it, Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "templatearg")
        });
        let spliced = if body_was_arg_refs {
            re_tokenize_sol_prefix(spliced, self.config.extension_tags())
        } else {
            spliced
        };

        // Expand the spliced body's remaining templates. PHP's
        // `processTemplateSource` forwards `$this->options` — the *caller's*
        // `inTemplate` — into the nested `wikitext-to-expanded-tokens` pipeline
        // (TemplateHandler.php:621, :648-666), so the flag must be forwarded too:
        // `{{1x|1= {{!}}bar}}` at the top level expands `{{!}}` to a literal `|`
        // (the `<td>` form is reserved for argument values, expanded above).
        //
        // It is also the flag that decides whether a nested transclusion gets
        // its own `mw:Transclusion` span, and there the caller's value is the
        // correct one: `{{ {{T}} }}` must keep the inner span, so forcing `true`
        // for a body breaks it.
        // Expand the spliced body's remaining templates, in a nested pipeline:
        // PHP's `processTemplateSource` hard-codes `'inTemplate' => true` for the
        // body's own `wikitext-to-expanded-tokens` run, and `wrapTemplates =
        // !inTemplate` follows from it, so nothing inside the body is
        // encapsulated. The body's tokens are spliced into the caller's
        // expansion, which carries the one wrapper. A `{{SHORTDESC:…}}` inside
        // `Template:Short description` is invisible on the service for exactly
        // this reason — it expands to nothing, and with no wrapper there is
        // nothing left where the empty span would have been.
        //
        // `in_template` stays the *caller's* flag: `{{!}}` keys off PHP's
        // `atTopLevel` and the fixtures pin it to a literal `|` one level down,
        // and an argument value spliced into this body was expanded in the
        // caller's pipeline, so it keeps its wrapper (see `wrap` above).
        let expanded = Box::pin(self.expand_templates(
            &child_frame,
            spliced,
            Some(src),
            about_counter,
            in_template,
            /* body */ true,
            &template_src,
        ))
        .await;

        // A comment in a template body is dropped. PHP's `processTemplateTokens`
        // strips top-level comments whenever the pipeline ran with
        // `expandTemplates => false`, which is exactly the body pipeline — so
        // `Template:Short description`'s `<!-- Start tracking -->` markers never
        // reach the reader, and the same `if (!expandTemplates)` guard is what
        // makes them survive on a *page*.
        let expanded: Vec<Item> = expanded
            .into_iter()
            .filter(|it| !matches!(it, Item::Tok(ParsoidToken::Comment(_))))
            .collect();

        if !wrap || target_has_comment {
            // Nested/extension-content context, or a comment in the template
            // target (`{{f<!---->oo}}`): no `mw:Transclusion` wrapping.
            return expanded;
        }

        let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
        let mut info = template_info_from(None, Some(name), vec![]);
        info.target_wt = Some(target_wt.to_string());
        // The *called* title, not the redirect's target: `data-mw.target` names
        // what the page wrote, and a followed redirect must not rename it.
        info.href = Some(crate::title::make_link(&called_title, self.config));
        info.param_infos =
            crate::pipeline::template_encapsulator::prepare_tpl_param_infos(params, page_source);
        encap.encap_tokens(expanded, &info)
    }

    /// Expand a `{{#invoke:Module|func|…}}` call.
    ///
    /// Scribunto's result is *wikitext*, not HTML: MediaWiki feeds it back
    /// through the parser, so templates the module returns expand in turn. The
    /// result is therefore tokenized and expanded like a template body, and
    /// wrapped in the same `mw:Transclusion` markers a template gets.
    ///
    /// A failure (missing module, Lua error) becomes MediaWiki's script-error
    /// message rather than a panic or a silent empty string: the comparison is
    /// only meaningful if a broken call looks broken.
    #[allow(clippy::too_many_arguments)]
    async fn expand_invoke(
        &self,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        // The resolved call, with its arguments already expanded.
        call: &crate::lua::invoke::Invoke,
        // The token's *own* arguments, unexpanded. `data-mw` records the call as
        // written, so a nested `#invoke` in an argument stays literal there even
        // though the module receives its expansion — `call` carries the expanded
        // form and would put the answer into the metadata.
        raw_params: &crate::pipeline::parser_functions::Params,
        about_id: String,
        token: &ParsoidToken,
        // PHP's `wrapTemplates` for *this* call, and whether the call is being
        // expanded as part of a template body — the latter is what the nested
        // pipeline that runs the module's output inherits.
        wrap: bool,
        body: bool,
        _page_source: &str,
        parent_args: Vec<crate::wikitext::tokens_v2::KV>,
        about_counter: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        // Without a data source there is nothing to fetch the module from, so
        // leave the call as source text (the standalone behaviour of every
        // parser function rustoid cannot implement).
        let Some(src) = source else {
            return vec![Item::Str(call.source_text())];
        };

        // `call` was parsed by the caller, which already resolved the target as
        // `invoke`; the module name is needed for the `data-mw` target as well as
        // for the lookup.
        let module = call.module.clone();

        let site = crate::lua::engine::LuaSite::from_config(self.config);
        // The parent frame's args and title are the calling template's, which
        // modules read through `frame:getParent()`. A `#invoke` written directly
        // on a page has no parent frame at all, which is distinguishable from
        // one inside a template by the frame's namespace: a template frame is
        // always in the Template namespace.
        let parent = frame_args_to_lua(&parent_args);
        let parent_title = frame.title().full_text();
        // `frame:getParent()` is never nil here, whatever the namespace. The
        // manual is explicit: it "returns the frame for the page that called
        // `{{#invoke:}}` ... regardless of whether this function is called
        // directly from the main module invoked by `{{#invoke:}}` or from library
        // module code accessed via `require()`", and only the debug console and
        // `mw.loadData` see nil.
        //
        // Keying this on the Template namespace was wrong in a way that looked
        // right: a call *inside* a template did get a parent, so the common case
        // passed, while a direct `{{#invoke:}}` from an article returned nil and
        // `Module:Check for unknown parameters` — which opens with
        // `frame:getParent().args` — failed on it.
        let ctx = crate::lua::engine::FrameContext {
            parent_args: parent,
            parent_title: Some(parent_title),
            has_parent: true,
            page_source: _page_source.to_string(),
            page_title: Some(self.page_title.borrow().clone()),
            // Seed the frame with every title already resolved this render, so
            // `preload_titles` does not re-fetch them; see `title_facts`.
            titles: self.title_facts.borrow().clone(),
            ..Default::default()
        };

        // A module may call back into the parser (`frame:expandTemplate`,
        // `frame:preprocess`). Those calls cannot happen inside Lua — the
        // pipeline is async — so each one is deferred and the module re-run
        // with the answer available; see [`crate::pipeline::lua_deferred`].
        //
        // The closure is `Fn`, not `FnOnce`: the answer is expanded once per
        // distinct request, and `invoke` may call it several times.
        // The module's output is wikitext, not HTML: templates it returns are
        // expanded by the caller. A failure (missing module, Lua error) becomes
        // MediaWiki's script-error markup, so a broken call reads as broken
        // rather than as silently absent text.
        let output = match crate::lua::invoke::invoke(
            call,
            src,
            site,
            &frame.title().full_text(),
            ctx,
            |request| {
                self.expand_lua_request(
                    source,
                    frame,
                    request,
                    about_id.clone(),
                    token,
                    about_counter,
                )
            },
            &self.title_facts,
        )
        .await
        {
            Ok(out) => out,
            Err(e) => script_error(&e.to_string()),
        };

        let items = crate::pipeline::template_handler::tokenize_wikitext_to_items(
            &output,
            /* in_template */ true,
            self.config.extension_tags(),
        );

        let child = frame.new_child(frame.title().clone(), vec![]);
        // The document's counter, not a fresh one. A module's output can carry a
        // `<templatestyles>` — `Module:Infobox` emits one through
        // `frame:extensionTag` — and that stylesheet's `about` id belongs to the
        // page's sequence. A fresh counter started it at 1, so the `<style>`
        // reused the id the enclosing `#invoke` wrapper had just been given.
        let expanded = Box::pin(self.expand_templates(
            &child,
            items,
            source,
            about_counter,
            body,
            false,
            /* src_text */ "",
        ))
        .await;

        // A module that reaches `<templatestyles>` through `frame:extensionTag`
        // lowers it to `#tag`, whose extension token is built during the
        // expansion above rather than appearing in the top-level stream the pass
        // in `build_ast` walks. Resolving it here is not possible without
        // threading the fragment map through `expand_templates` (13 recursive
        // call sites), because a created `<style>` fragment *must* reach the tree
        // builder or it is silently dropped — and silently dropping output is
        // worse than leaving the call unexpanded. The gap is recorded in
        // `ONLINE-PARITY.md`.

        if !wrap {
            return expanded;
        }

        // The `mw:Transclusion` metadata records the call the way the live
        // service does for a Scribunto module: the part key `"template"`, a
        // `function` of `invoke`, and the arguments in `params`. Verified against
        // `{{#invoke:String|len|x}}` →
        // `{"template":{"target":{"wt":"#invoke:String","function":"invoke"},
        //   "params":{"1":{"wt":"len"},"2":{"wt":"x"}},"i":0}}`.
        //
        // Two things an earlier shape got wrong: `"parserfunction"` as the part
        // key (the live one is `"template"`, since `invoke` is not a modern
        // PFragment handler), and the target built by prepending `#invoke` to a
        // string that already contained it — `pf_arg` is the text *after*
        // `#invoke:`, but `target_str` is the whole `#invoke:Module`, so the two
        // must not be concatenated.
        //
        // The parameters follow Scribunto's own view of the call: the function
        // name is argument 1, because the tokenizer's `|`-split makes it the first
        // piece after the module.
        // `old-parserfunction` rather than `parserfunction`: the two differ only
        // in this field name (PHP's `TemplateInfo::toJsonArray` writes `function`
        // for the former, `key` for the latter), and the live service writes
        // `function` for `#invoke`. Both spell it under the `template` parts key.
        // The parameter wikitext comes from the token's own arguments, *not* from
        // the parsed `pf_arg` — that one has been expanded, and `data-mw` records
        // the call as written. Reading the parsed arguments here put the answer
        // into the metadata: `{{#invoke:String|sub|abcde|0|
        // {{#invoke:String|len|xy}}}}` recorded `"4":{"wt":"2"}` where the
        // service records `"4":{"wt":"{{#invoke:String|len|xy}}"}`.
        let raw_args: Vec<(Option<String>, String)> = raw_params
            .args
            .iter()
            .skip(2)
            .map(|kv| {
                let key = crate::wikitext::token_utils::key_value_to_string(&kv.key);
                let value = kv_value_source(kv);
                let name = key.trim();
                if name.is_empty() {
                    (None, value)
                } else {
                    (Some(name.to_string()), value)
                }
            })
            .collect();
        let mut info = template_info_from(Some("invoke"), None, vec![]);
        info.target_wt = Some(format!("#invoke:{module}"));
        info.param_infos = std::iter::once({
            let mut p = ParamInfo::new("1".to_string());
            p.value_wt = call.function.clone();
            p
        })
        .chain(raw_args.iter().enumerate().map(|(i, (name, value))| {
            let mut p = ParamInfo::new((i + 2).to_string());
            p.value_wt = match name {
                Some(n) => format!("{n}={value}"),
                None => value.clone(),
            };
            p.named = name.is_some();
            p
        }))
        .collect();
        let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
        encap.encap_tokens(expanded, &info)
    }

    /// Expand a list of `#invoke` arguments, in place.
    ///
    /// Scribunto receives expanded text, so a nested template or `#invoke` in an
    /// argument is substituted before the module runs. The tokenizer keeps such a
    /// value as tokens holding an unexpanded `template` token, which stringifies
    /// to nothing useful — that is why the unexpanded form answered 17 where the
    /// service answers 3.
    ///
    /// The caller passes only the arguments to expand, so the two shapes that
    /// need this cannot be confused: the `#invoke` call's own list starts at
    /// `args[1]` because `args[0]` is the already-resolved target, while a
    /// **parent** frame's list has no target and is passed whole. Skipping one
    /// entry there silently left the calling template's *first* argument
    /// unexpanded, so a module read its `{{{…}}}` verbatim — the shape
    /// `{{see Wiktionary|…}}` hands to `Module:Hatnote` as `args[1]`.
    async fn expand_invoke_args(
        &self,
        args: &[crate::wikitext::tokens_v2::KV],
        frame: &Frame,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        body: bool,
        src_text: &str,
    ) -> Vec<crate::wikitext::tokens_v2::KV> {
        use crate::wikitext::tokens_v2::KeyValue;

        let mut out = args.to_vec();
        for kv in out.iter_mut() {
            let KeyValue::Tokens(items) = &kv.value else {
                continue;
            };
            // A template *argument reference* (`{{{1}}}`) is not a template, but it
            // has to expand here too. Scribunto receives a `#invoke` argument with
            // its `{{{…}}}` already substituted, and rustoid's `Frame::expand` —
            // the substitute used by the paths that do expand it — is synchronous
            // and only runs where the frame still has the argument. Left as written
            // the text reaches the module verbatim: a `Module:Protected page`
            // call whose `alt` was `Page {{{protectionlevel}}}` made the module
            // emit `type(msg) ~= 'string'`, so the whole padlock indicator was
            // replaced by a Lua error where the service renders
            // `Page semi-protected` (the `Help:Introduction` difference at byte
            // 403).
            //
            // The scan looks *inside* tokens as well as at the top level, because a
            // `wikilink` keeps its target in a token list: `[[{{PAGENAME}}]]` has no
            // top-level template at all, and a top-level-only scan skipped it — so
            // the module received the target as written.
            let found = expandable_content(items);
            let (has_template, has_arg_ref) = (found.template, found.arg);
            if !has_template && !has_arg_ref {
                continue;
            }
            // The argument reference is substituted from the **parent** frame, as
            // MediaWiki does: the value was written in the calling template, and
            // the child frame built below has no arguments of its own.
            let substituted = if has_arg_ref {
                frame.expand(items)
            } else {
                items.clone()
            };
            let child = frame.new_child(frame.title().clone(), vec![]);
            let expanded = Box::pin(self.expand_templates(
                &child,
                substituted,
                source,
                about_counter,
                /* in_template */ true,
                body,
                src_text,
            ))
            .await;
            // A target the expansion left nested — `[[{{PAGENAME}}]]` — is
            // resolved here, because nothing else will look at these tokens:
            // the value is flattened to the string a module receives, and the
            // `expand_attributes` pass that does this on a page runs on the
            // *document*, not on an argument.
            let expanded = Box::pin(self.expand_attrib_templates(
                &child,
                expanded,
                source,
                about_counter,
                true,
                body,
                src_text,
            ))
            .await;
            kv.value = KeyValue::Tokens(expanded);
        }
        out
    }

    /// Expand the templates a chunk holds in its tokens' *attributes*.
    ///
    /// [`Parser::expand_templates`] walks the top level of a chunk and leaves a
    /// `wikilink` alone there — its target is a token list inside the token, not a
    /// top-level item — so `[[{{PAGENAME}}]]` reaches the argument renderer with
    /// its target still unexpanded. On a page the `expand_attributes` pass
    /// resolves it afterwards, and `render_links` then sees a plain target; an
    /// argument has no such pass, and Scribunto receives the *expanded* text, so
    /// the target has to be resolved before the value is rendered.
    ///
    /// This is the expansion half of `expand_attributes`, without its
    /// `mw:ExpandedAttrs` marking: that marking describes the DOM a link
    /// eventually becomes, and here the tokens are about to be flattened to the
    /// string a module is handed.
    #[allow(clippy::too_many_arguments)]
    async fn expand_attrib_templates(
        &self,
        frame: &Frame,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
        body: bool,
        src_text: &str,
    ) -> Vec<Item> {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        let mut out = Vec::with_capacity(tokens.len());
        for item in tokens {
            let Item::Tok(tok) = &item else {
                out.push(item);
                continue;
            };
            if !matches!(tok, ParsoidToken::Tag(_) | ParsoidToken::SelfclosingTag(_)) {
                out.push(item);
                continue;
            }
            let attribs = tok.get_attribs();
            if !attribs.iter().any(|kv| {
                matches!(kv.key, KeyValue::Tokens(_)) || matches!(kv.value, KeyValue::Tokens(_))
            }) {
                out.push(item);
                continue;
            }
            let mut expanded_attrs: Vec<KV> = Vec::with_capacity(attribs.len());
            for kv in attribs {
                let mut new_kv = kv.clone();
                for field in [&mut new_kv.key, &mut new_kv.value] {
                    let KeyValue::Tokens(toks) = field else {
                        continue;
                    };
                    let expanded = self
                        .expand_templates(
                            frame,
                            toks.clone(),
                            source,
                            about_counter,
                            in_template,
                            body,
                            src_text,
                        )
                        .await;
                    *field =
                        crate::pipeline::attribute_transform_manager::items_to_key_value(expanded);
                }
                expanded_attrs.push(new_kv);
            }
            let mut new_tok = tok.clone();
            new_tok.set_attribs(expanded_attrs);
            out.push(Item::Tok(new_tok));
        }
        out
    }

    /// Expand one deferred frame-call request into the text the module gets.
    ///
    /// All three methods **return a string** — Scribunto's manual is explicit
    /// for each — so the answer is the expanded *wikitext*, which the module may
    /// concatenate, compare or slice. That is also why all three can share the
    /// `preprocess` path: once the call has been rendered to wikitext, expanding
    /// it is the same job.
    ///
    /// The difference between them is scope and shape, not mechanism:
    /// `expandTemplate` is transclusion and carries `mw:Transclusion` markers;
    /// a parser function expands in place and carries none; `preprocess` gets
    /// whatever the module wrote. The markers matter for attribution, so the two
    /// synthesised calls are distinguished here.
    ///
    /// The manual also records that this is the only way a module can get
    /// templates expanded in its output at all: a module returning
    /// `"Hello {{welcome}}"` has that read literally, because module output is
    /// not re-parsed for templates.
    async fn expand_lua_request(
        &self,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        request: crate::pipeline::lua_deferred::FrameRequest,
        about_id: String,
        token: &ParsoidToken,
        about_counter: &std::cell::Cell<usize>,
    ) -> String {
        use crate::pipeline::lua_deferred::FrameRequest;
        // The manual is explicit that module output is not re-parsed for
        // templates, so a module cannot reach the parser recursively through
        // its own return value; `expandTemplate`/`preprocess` are the only
        // doors. A module that calls them from inside an expansion this deep is
        // already thousands of frames down, which no real module does.
        if self.lua_expansion_depth.get() >= MAX_LUA_EXPANSION_DEPTH {
            return String::new();
        }
        let Some(source) = source else {
            // No data source at all: nothing can be fetched, so the call cannot
            // be answered. The empty string is the safe answer — handing back
            // the call itself would have the module return `{{#invoke:…}}`,
            // which the pipeline re-expands and recurses forever.
            return String::new();
        };

        let text = crate::pipeline::lua_deferred::render_call(&request);
        let items = crate::pipeline::template_handler::tokenize_wikitext_to_items(
            &text,
            /* in_template */ true,
            self.config.extension_tags(),
        );
        // A module reaching a protection magic word is the common case, not an
        // edge one: `Module:Effective protection expiry` and
        // `Module:Effective protection level` are both built entirely from
        // `frame:callParserFunction('PROTECTIONEXPIRY', …)`. The page-level scan
        // cannot see these — the call is created at expansion time — so the
        // titles are fetched here, where the module's own call is in hand and an
        // await is available. Merged rather than replaced, because the page's
        // own scan may already have fetched other titles.
        {
            let mut named = Vec::new();
            collect_protection_titles(&items, &mut named);
            if !named.is_empty() {
                let fetched = source
                    .get_title_protection(&named)
                    .await
                    .unwrap_or_default();
                if !fetched.is_empty() {
                    self.protection.borrow_mut().extend(fetched);
                }
            }
        }
        // A call made from inside a module is a call from the module's own
        // scope, so `{{{1}}}` resolves against the module's frame, not the
        // caller's — hence a child of *this* frame.
        //
        // `in_template: true` is load-bearing: this expansion is nested inside
        // the `#invoke` that asked for it, so anything it produces must not be
        // encapsulated again, and the flag carries that into the recursion.
        let child = frame.new_child(frame.title().clone(), vec![]);
        self.lua_expansion_depth
            .set(self.lua_expansion_depth.get() + 1);
        // The about counter is the *document's*, not a fresh one per deferred
        // call: a module's `frame:extensionTag('templatestyles', …)` emits a
        // `<style>` that takes its id where the expansion reaches it, which is
        // inside the enclosing `#invoke` and before anything that follows the
        // module on the page. A fresh counter restarted at 1 for every deferred
        // call, so ids repeated and every later element was numbered too high.
        // A parser function's own expansion carries no wrapper, but its
        // arguments can still contain other transclusions, so it must share the
        // sequence as well.
        let expanded = Box::pin(self.expand_templates(
            &child,
            items,
            Some(source),
            about_counter,
            /* in_template */ true,
            /* body */ false,
            &text,
        ))
        .await;
        self.lua_expansion_depth
            .set(self.lua_expansion_depth.get() - 1);

        // A parser function expands in place, with no transclusion wrapper: the
        // caller wrote `#uc`, not a page name, so there is nothing to attribute
        // a separate transclusion to.
        if matches!(request, FrameRequest::CallParserFunction { .. }) {
            return self.render_answer_markers(&expanded);
        }
        // `expandTemplate` is transclusion, so its expansion is wrapped the way
        // a template's is — but with the *module's* call as the source, not the
        // `#invoke` that contains it. Reusing the enclosing token made a missing
        // template render back as the `#invoke` call, which the module returned
        // and the pipeline expanded again, forever.
        //
        // The `data-mw` names the module handler, matching the shape the live
        // service serves for a module-emitted transclusion: part key `template`,
        // target `#invoke`, and the canonical `invoke` as the function.
        let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
        let mut info = template_info_from(Some("invoke"), None, vec![]);
        info.target_wt = Some("#invoke".to_string());
        let encapped = encap.encap_tokens(expanded, &info);
        crate::pipeline::lua_deferred::render_answer(&encapped)
    }
}

/// Convert a frame's raw parameters into Scribunto `Arg`s.
///
/// A numeric key is positional, anything else named — the same split the
/// template path makes for arguments.
///
/// The wikitext of a `KV`'s value, preferring its source range.
///
/// `tokensToString` has no arm for a plain tag, so stringifying an argument that
/// holds one drops it: a module handed `{{#invoke:String|len|<div>X</div>}}`
/// answered `1` where the live service answers `12`, and the loss showed up in
/// `data-mw` as `{"wt":"X"}` instead of the whole tag. The range is the source
/// as written, which is what `data-mw` records, and it is the same technique
/// `prepare_tpl_param_infos` uses for template parameters.
fn kv_value_source(kv: &crate::wikitext::tokens_v2::KV) -> String {
    kv.src_offsets
        .as_ref()
        .map(|so| so.value_substr(""))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::wikitext::token_utils::key_value_to_string(&kv.value))
}
/// One `#invoke` argument as the module receives it: its name (when it has one) and
/// its value.
///
/// The value is the **expanded** tokens rendered by [`argument_value_or_source`],
/// because that is what Scribunto receives: a nested template or `#invoke` is
/// substituted before the module sees it, so `{{#invoke:String|len|x{{#invoke:Str
/// ing|len|abc}}y}}` measures `x3y` and answers **3**, not the 17 the unexpanded
/// text gives. A value the renderer declines keeps to its source range, which is
/// what `data-mw` records and what `{{#invoke:String|len|<div>X</div>}}` needs
/// (the service answers 12 there, because the tag reaches the module as text).
///
/// The name is the tokenizer's, not re-derived from the value: only the tokenizer
/// can tell a `=` that names an argument from one inside a value.
fn expanded_arg_pair(kv: &crate::wikitext::tokens_v2::KV) -> (Option<String>, String) {
    let key = crate::wikitext::token_utils::key_value_to_string(&kv.key);
    let value = argument_value_or_source(kv);
    let name = key.trim();
    if name.is_empty() {
        (None, value)
    } else {
        // A named argument's value is trimmed before the module sees it, the
        // same rule [`frame_args_to_lua`] already applies to the parent frame's
        // arguments and MediaWiki's preprocessor applies to both. The `#invoke`
        // call's own arguments went through untrimmed, which is visible in
        // `Template:Infobox OS`: its `{{#invoke:Unsubst||$B=\n{{Main other|…}}}}`
        // carries a leading newline in `$B`, and left in place it survives into
        // the module's return value and renders as a stray
        // `<span about=…> </span>` the service does not have.
        //
        // [`frame_args_to_lua`]: frame_args_to_lua
        (Some(name.to_string()), value.trim().to_string())
    }
}

/// What expansion still has to resolve in a token chunk.
#[derive(Default, Clone, Copy)]
struct ExpandableContent {
    /// A `{{…}}` template, parser function or magic variable.
    template: bool,
    /// A `{{{…}}}` argument reference.
    arg: bool,
}

/// Scan a chunk for what expansion has to resolve, looking inside tokens.
///
/// A `wikilink` keeps its target — and its display parts — in token lists held
/// *inside* the token, so a top-level scan of `[[{{PAGENAME}}]]` finds nothing to
/// expand. That is not a detail: it is why such a target reached a module
/// unexpanded, and why an `#invoke` argument has to be inspected this way before
/// deciding it needs no work.
fn expandable_content(items: &[Item]) -> ExpandableContent {
    use crate::wikitext::tokens_v2::{KeyValue, ParsoidToken};
    let mut found = ExpandableContent::default();
    for item in items {
        let Item::Tok(tok) = item else { continue };
        if let ParsoidToken::SelfclosingTag(t) = tok {
            match t.name.as_str() {
                "template" | "template3" => found.template = true,
                "templatearg" => found.arg = true,
                _ => {}
            }
        }
        for kv in tok.get_attribs() {
            for field in [&kv.key, &kv.value] {
                if let KeyValue::Tokens(toks) = field {
                    let nested = expandable_content(toks);
                    found.template |= nested.template;
                    found.arg |= nested.arg;
                }
            }
        }
    }
    found
}

/// How many `#ifexist` titles one render may resolve.
///
/// MediaWiki caps expensive parser functions at 500 per page and `#ifexist` is
/// one of them. The bound is on *distinct* titles here because rustoid answers
/// each title once and reuses the answer, which is a stricter budget than
/// MediaWiki's per-call count and therefore never exceeds it.
const MAX_IFEXIST_TITLES: usize = 500;

/// How many redirect hops may be followed before giving up.
///
/// MediaWiki bounds redirect resolution (`$wgMaxRedirects`); a redirect cycle
/// (`A → B → A`) is otherwise unbounded, and the depth guard for template
/// transclusion does not catch it, because each hop is a *fetch* rather than a
/// nested expansion.
const MAX_REDIRECT_HOPS: usize = 5;

/// What [`follow_template_redirect`] found.
///
/// The three cases are genuinely different for the caller, which is why this is
/// not an `Option`: "not a redirect" means keep the body already fetched, while
/// "a redirect whose target is missing" means the template has no content and
/// must not render its own redirect line.
enum Redirected {
    /// A redirect, followed to the page that supplies the content.
    Followed {
        body: String,
        title: crate::title::Title,
    },
    /// The body is a redirect, but no target could be fetched.
    Unresolved,
    /// The body is ordinary content, so the caller keeps it as-is.
    NotARedirect,
}

/// Follow a template's redirect chain, returning the final body and its title.
///
/// A redirected template's *own* body is discarded entirely — MediaWiki
/// transcludes the target, it does not render the redirect line — and the
/// target's title replaces the redirect's so the child frame, `{{PAGENAME}}` and
/// any further transclusion resolve against the page that actually supplies the
/// content.
///
async fn follow_template_redirect(src: &dyn DataSource, title: &crate::title::Title) -> Redirected {
    let mut current = title.clone();
    let Some(mut body) = src.get_template(&current).await.ok().flatten() else {
        return Redirected::NotARedirect;
    };
    let mut hops = 0;

    while let Some(target) = crate::pipeline::template_handler::redirect_target_of(&body) {
        if hops >= MAX_REDIRECT_HOPS {
            // The chain is too long to be a real one. Report it unresolved rather
            // than rendering a redirect line as content.
            return Redirected::Unresolved;
        }
        hops += 1;

        // The target is resolved as a **full title**: a redirect body names the
        // page it points at, namespace included. Forcing the redirect's own
        // namespace onto it would turn `Template:Pp-semi`'s
        // `#REDIRECT [[Template:Protected page]]` into `Template:Template:…`,
        // which is why this must not guess.
        //
        // `resolve_redirect` is consulted first when the data source implements
        // it, because a source may know the canonical target (title
        // normalisation, an interwiki) that parsing the wikitext cannot express.
        let parsed = crate::title::Title::new_main(target);
        let next = src
            .resolve_redirect(&current)
            .await
            .ok()
            .flatten()
            .unwrap_or(parsed);
        if next == current {
            // A self-redirect: there is no content to be had.
            return Redirected::Unresolved;
        }
        match src.get_template(&next).await.ok().flatten() {
            Some(next_body) => {
                body = next_body;
                current = next;
            }
            // The target does not exist, or is not available offline.
            None => return Redirected::Unresolved,
        }
    }

    if hops == 0 {
        Redirected::NotARedirect
    } else {
        Redirected::Followed {
            body,
            title: current,
        }
    }
}

/// How many Lua frame methods may nest before the expansion is abandoned.
///
/// Each level is one `frame:expandTemplate`/`preprocess` call whose argument
/// expansion asks for another. Real modules nest a few levels; the bound exists
/// so a module that asks for itself cannot run until the stack overflows.
const MAX_LUA_EXPANSION_DEPTH: usize = 16;

/// Convert a frame's raw parameters into Scribunto `Arg`s.
///
/// A numeric key is positional, anything else named — the same split the
/// template path makes for arguments.
fn frame_args_to_lua(args: &[crate::wikitext::tokens_v2::KV]) -> Vec<crate::lua::engine::Arg> {
    use crate::lua::engine::Arg;
    use crate::wikitext::token_utils::key_value_to_string;

    let mut out = Vec::new();
    for kv in args {
        let key = key_value_to_string(&kv.key);
        let value = expanded_argument_text(kv);
        let trimmed = key.trim();
        match (trimmed.parse::<usize>(), trimmed.is_empty()) {
            (Ok(_), false) => out.push(Arg::Positional(value)),
            (_, true) => out.push(Arg::Positional(value)),
            // A named argument's value is trimmed, exactly as `{{{name}}}`
            // substitution trims it ([`Frame::expand_template_arg`]) and as
            // MediaWiki's preprocessor does. Without this `{{Automatic taxobox
            // | taxon = Equus (Hippotigris)}}` handed the module ` Equus
            // (Hippotigris)`, so `Module:Autotaxobox` looked up
            // `Template:Taxonomy/ Equus (Hippotigris)` (with the space), missed,
            // and walked a broken taxonomy chain that expanded `Template:Taxonomy/`
            // recursively until the node-count limit fired.
            //
            // [`Frame::expand_template_arg`]: crate::pipeline::frame::Frame::expand_template_arg
            _ => out.push(Arg::Named(trimmed.to_string(), value.trim().to_string())),
        }
    }
    out
}

/// A parent-frame argument's text, as Scribunto hands it to the module.
///
/// The **tokens** are the answer when every one of them has an exact textual form
/// ([`argument_value_text`]), because they are the *expanded* value while the
/// recorded source range still holds the wikitext as written — `{{{1|}}}`
/// included — and so is stale. Reading the range gave `Module:SDcat` the literal
/// `{{{1|}}}` where the service gave the short description, and made it report
/// "is different from Wikidata" for a page whose description matches.
///
/// Anything else keeps to the source range, which is the lesser evil only for the
/// tokens the renderer declines: it is *unexpanded*, so it is wrong in a different
/// way, but it at least keeps the construct visible instead of dropping it.
fn expanded_argument_text(kv: &crate::wikitext::tokens_v2::KV) -> String {
    argument_value_or_source(kv)
}

/// An argument value as the module receives it: the tokens rendered when they all
/// have an exact textual form, else the recorded source.
///
/// The single place both argument paths agree on, so the `#invoke` call's own
/// arguments and a parent frame's cannot diverge on what a value means.
fn argument_value_or_source(kv: &crate::wikitext::tokens_v2::KV) -> String {
    use crate::wikitext::tokens_v2::KeyValue;
    match &kv.value {
        // A string value *is* the argument text. The tokenizer leaves a plain
        // value as a `Str`, and `attribute_transform_manager` folds a
        // substituted `{{{…}}}` back to one — but the recorded source range is
        // the *unsubstituted* text, so preferring it returned `{{{fn|lang}}}`
        // where the module must see `lang`. That is the `function not found:
        // lang` failure on every `{{lang}}`/`{{langx}}` call: `Template:Lang`
        // invokes `#invoke:Lang|{{{fn|lang}}}`, and the function name reached
        // `module_function` still spelled as the reference.
        KeyValue::Str(s) => s.clone(),
        // Only a token list can need the source: its text is not always exactly
        // representable (see [`argument_value_text`]).
        KeyValue::Tokens(items) => {
            argument_value_text(items).unwrap_or_else(|| kv_value_source(kv))
        }
    }
}

/// Render an argument value's tokens to the text Scribunto receives, when every
/// token has an exact textual form.
///
/// `None` means *some* token does not, and the caller must keep to the recorded
/// source instead (see [`argument_value_or_source`]). The distinction is the whole
/// point: a renderer that returns a string for a value it cannot represent
/// faithfully drops the part it cannot render, which is how an earlier attempt
/// lost a wikilink out of a `Megadeth` hatnote.
///
/// This is deliberately **separate** from
/// [`crate::wikitext::token_utils::tokens_to_string`], which also renders DOM
/// attribute values: adding the display text of a link there changed a
/// `Module:Navbox` title attribute's length and made `Sundial` emit an
/// unrendered `[[Philosophy of space and time`. A module's argument is a
/// different question from an attribute value, so it gets its own answer.
///
/// The shapes handled, each with an exact textual form:
///
/// - `Item::Str` — the text itself.
/// - comments and newlines — dropped, as `tokensToString` drops them (MediaWiki's
///   preprocessor strips comments before a module sees an argument).
/// - `mw-quote` — its delimiter (`''`/`'''`) is in its `value` attribute, and in
///   Scribunto's string view `''x''` is quite literally `''x''`.
/// - the `{{!}}` marker — a `<td>` carrying an empty `attrSrc` and the
///   `AT_SRC_START` flag, which [`crate::pipeline::template_handler::process_special_magic_word`]
///   emits *inside a template* so `TableFixups` can reclaim a cell separator.
///   Its text is `|`, and a module reads it as exactly that: without this,
///   `{{About||the butterfly genus|Bicyclus{{!}}''Bicyclus''|other uses}}` handed
///   `Module:Hatnote list` the value `Bicyclus{{!}}''Bicyclus''`, whose
///   `parseLink` could not split on the pipe — so the hatnote checked the
///   existence of a page *named* `Bicyclus{{!}}''Bicyclus''`, and `Bicycle`
///   gained a nonexistent-page category on a blue link.
/// - `wikilink` — `[[target|display…]]`, rebuilt from the `href` and
///   `mw:maybeContent` fields. Their values are themselves token lists (the
///   target is tokenized so a templated target can be expanded), so this recurses
///   rather than reading a string. A module is handed the *wikitext* of a link, and
///   `{{#invoke:Unsubst||$B={{DMCA|A|B|C}}}}` is the case that pins it: the `$B`
///   value expands to a `wikilink`, and rendering it without this arm produced
///   `[[]]`.
/// - `extension` — the source of the tag (`<nowiki/>`), which is what a module
///   receives for one.
///
/// Anything else — a bare `<div>`, a `template` token that somehow survived
/// expansion — returns `None`, so the value keeps to its source range rather than
/// losing the construct.
fn argument_value_text(items: &[Item]) -> Option<String> {
    use crate::wikitext::tokens_v2::ParsoidToken;
    let mut out = String::new();
    for item in items {
        match item {
            Item::Str(s) => out.push_str(s),
            Item::Tok(ParsoidToken::Comment(_) | ParsoidToken::Nl(_)) => {}
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "mw-quote" => {
                let value = t.attribs.iter().find(|kv| kv.key.as_str() == Some("value"));
                out.push_str(
                    &value
                        .and_then(|kv| key_value_text(&kv.value))
                        .unwrap_or_else(|| "''".to_string()),
                );
            }
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "extension" => {
                out.push_str(t.data_parsoid.src.as_deref()?);
            }
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "wikilink" => {
                let href = t
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))?;
                out.push_str("[[");
                out.push_str(&key_value_text(&href.value)?);
                for part in t
                    .attribs
                    .iter()
                    .filter(|kv| kv.key.as_str() == Some("mw:maybeContent"))
                {
                    out.push('|');
                    out.push_str(&key_value_text(&part.value)?);
                }
                out.push_str("]]");
            }
            Item::Tok(ParsoidToken::Tag(t)) if is_bang_marker(t) => out.push('|'),
            _ => return None,
        }
    }
    Some(out)
}

/// A single key/value field rendered to text, for [`argument_value_text`].
///
/// A `Tokens` field recurses through the same renderer, so a link target that
/// holds markup (or an argument reference that was substituted into one) renders
/// exactly as the value it sits in would.
fn key_value_text(value: &crate::wikitext::tokens_v2::KeyValue) -> Option<String> {
    use crate::wikitext::tokens_v2::KeyValue;
    match value {
        KeyValue::Str(s) => Some(s.clone()),
        KeyValue::Tokens(items) => argument_value_text(items),
    }
}

/// Whether a `td` tag is the `{{!}}` marker rather than a real table cell.
///
/// [`crate::pipeline::template_handler::process_special_magic_word`] builds it
/// with an empty `attrSrc` and the `AT_SRC_START` flag, which is what
/// `TableFixups` keys on; nothing else produces that pair.
fn is_bang_marker(t: &crate::wikitext::tokens_v2::TagTk) -> bool {
    t.name == "td"
        && t.data_parsoid.tmp.at_src_start
        && t.data_parsoid.tmp.attr_src.as_deref() == Some("")
}

/// Render a Scribunto failure the way MediaWiki does, so a broken `#invoke`
/// shows up as an error in the output rather than as silently missing text.
///
/// The Lua stack traceback is reduced to its first module frame rather than
/// dropped entirely. Dropping it left messages like `attempt to index a nil
/// value` with no location at all, which cannot be attributed to anything; the
/// first frame names the module and line. The full trace is still discarded,
/// because it is long enough to distort the byte totals the scoreboard reports.
/// The titles a `{{PROTECTIONLEVEL:…}}`/`{{PROTECTIONEXPIRY:…}}` call on this
/// page names, so they can be fetched before expansion runs.
///
/// Walks the token stream rather than the AST because the fetch has to happen
/// *before* the magic word is evaluated, and because attribute values are
/// sub-pipelines whose tokens are reached through the tag, not through the tree.
///
/// A title built at runtime cannot be seen here and reads as unprotected, the
/// same gap `preload_titles` has and for the same reason.
fn collect_protection_titles(tokens: &[Item], out: &mut Vec<String>) {
    for item in tokens {
        let Item::Tok(tok) = item else { continue };
        let attribs = match tok {
            ParsoidToken::SelfclosingTag(t) => Some(&t.attribs),
            ParsoidToken::Tag(t) => Some(&t.attribs),
            _ => None,
        };
        if let Some(attribs) = attribs {
            // The target is `args[0].key`: a magic word keeps the whole
            // `NAME:action|title` text there, and the arguments after it are
            // separate. Reading both is what covers `{{P:edit|X}}` and the
            // named form alike.
            let whole = attribs
                .first()
                .map(|kv| crate::wikitext::token_utils::key_value_to_string(&kv.key))
                .unwrap_or_default();
            // A module writing `frame:callParserFunction('PROTECTIONEXPIRY', …)`
            // produces the `{{#…}}` spelling, so the leading hash has to come off
            // before the name is recognised — a page spells the same word without
            // one, and both must be collected.
            let whole = whole.strip_prefix(['#', '＃']).unwrap_or(&whole);
            for name in ["PROTECTIONLEVEL:", "PROTECTIONEXPIRY:"] {
                let Some(rest) = strip_prefix_ci(whole, name) else {
                    continue;
                };
                // `rest` is the action, and for a longer call also the title
                // after a `|`. When the tokenizer split them into separate
                // parameters the title is in the *value* of the next one —
                // verified by printing the token, not inferred: a positional
                // parameter is `key=""`, `value="Canada"`.
                let title = rest
                    .split_once('|')
                    .map(|(_, t)| t)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        attribs.get(1).map(|kv| {
                            let key = crate::wikitext::token_utils::key_value_to_string(&kv.key);
                            if key.is_empty() {
                                crate::wikitext::token_utils::key_value_to_string(&kv.value)
                            } else {
                                key
                            }
                        })
                    })
                    .unwrap_or_default();
                let title = title.trim().to_string();
                if !title.is_empty() {
                    // MediaWiki normalises underscores to spaces before it looks a
                    // title up, so the fetch has to use the same spelling the
                    // lookup will later try — otherwise `Can_ada` is fetched and
                    // then searched for as `Can ada`.
                    out.push(title.replace('_', " "));
                }
            }
        }
        // An attribute value is a sub-pipeline of its own, so a call inside one
        // is only reachable by descending into the key/value token lists.
        for kv in attribs.into_iter().flatten() {
            for side in [&kv.key, &kv.value] {
                if let crate::wikitext::tokens_v2::KeyValue::Tokens(inner) = side {
                    collect_protection_titles(inner, out);
                }
            }
        }
    }
}

/// `strip_prefix`, case-insensitively, for a magic-word spelling.
fn strip_prefix_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    let head = haystack.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &haystack[prefix.len()..])
}

fn script_error(message: &str) -> String {
    let short = message
        .split("\nstack traceback:")
        .next()
        .unwrap_or(message)
        .trim();
    let location = message
        .split("\nstack traceback:")
        .nth(1)
        .and_then(|trace| {
            trace
                .lines()
                .map(str::trim)
                .find(|line| line.contains("[string \"") && line.contains(':'))
        })
        .map(|line| {
            // `[string "Module:Foo"]:12: in function ...` — keep the source and
            // line, drop the rest of the frame description.
            let end = line.rfind(':').unwrap_or(line.len());
            let cut = line[..end].rfind(':').unwrap_or(end);
            line[..cut].trim().to_string()
        });
    match location {
        Some(loc) if !short.contains(&loc) => {
            format!("<strong class=\"error\">Script error: {short} ({loc})</strong>")
        }
        _ => format!("<strong class=\"error\">Script error: {short}</strong>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;
    use crate::options::ParserOptions;

    #[test]
    fn test_wikitext_to_html_heading() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("== Heading ==\n", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<h2"), "got: {html}");
        assert!(html.contains("Heading"), "got: {html}");
    }

    #[test]
    fn an_argument_value_with_bang_and_quotes_renders_from_its_tokens() {
        use crate::pipeline::template_handler::process_special_magic_word;
        use crate::wikitext::tokens_v2::{DataParsoid, SelfclosingTagTk};

        // `Bicyclus{{!}}''Bicyclus''` as the tokens a module's parent argument
        // holds: text, the `{{!}}` cell marker, and two `mw-quote` delimiters.
        let quote = |value: &str| {
            let mut tk = SelfclosingTagTk::new("mw-quote", vec![], DataParsoid::default());
            tk.add_attribute_str("value", value);
            Item::Tok(ParsoidToken::SelfclosingTag(tk))
        };
        let mut items = vec![Item::Str("Bicyclus".to_string())];
        items.extend(process_special_magic_word("!", true));
        items.push(quote("''"));
        items.push(Item::Str("Bicyclus".to_string()));
        items.push(quote("''"));

        assert_eq!(
            argument_value_text(&items).as_deref(),
            Some("Bicyclus|''Bicyclus''"),
            "the `{{!}}` marker is a `|` and the quotes are literal text"
        );
    }

    #[test]
    fn an_argument_value_the_renderer_cannot_represent_is_declined() {
        use crate::wikitext::tokens_v2::{DataParsoid, TagTk};
        // A real tag has no faithful text, so the renderer must return `None` and
        // let the caller keep to the source range rather than drop the tag.
        let div = TagTk::new("div", vec![], DataParsoid::default());
        let items = vec![Item::Tok(ParsoidToken::Tag(div))];
        assert_eq!(argument_value_text(&items), None);
    }

    #[test]
    fn test_wikitext_to_html_nowiki() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        // A `<nowiki>` always emits the `mw:Nowiki` span (matching PHP
        // `Nowiki::sourceToDom`).
        let html = parser
            .wikitext_to_html("<nowiki>hi</nowiki>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("mw:Nowiki"), "got: {html}");
        assert!(html.contains(">hi</"), "got: {html}");

        // The nested `</pre>` is escaped, not treated as a tag.
        let html = parser
            .wikitext_to_html("<nowiki></pre></nowiki>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("&lt;/pre>"), "got: {html}");
        assert!(!html.contains("<pre>"), "got: {html}");
    }

    /// A chain of *distinct* templates walks the depth limit without ever
    /// looking like a loop. `loop_and_depth_check` compares titles up the frame
    /// chain, so only an all-different chain isolates the preprocessor's own
    /// depth bound from the loop detector.
    ///
    /// The observable contract is the one PHP states: the offending expansion is
    /// replaced by an error span in place, the rest of the page still expands,
    /// and nothing aborts.
    #[tokio::test]
    async fn expansion_depth_limit_yields_an_error_span() {
        use crate::resource_limits::ExpansionLimits;

        let source = crate::mock::MockDataSource::new();
        const CHAIN: usize = 30;
        for i in 0..CHAIN {
            let body = if i + 1 == CHAIN {
                "leaf".to_string()
            } else {
                format!("{{{{D{}}}}}", i + 1)
            };
            source.add_template(&format!("Template:D{i}"), &body);
        }

        let mut config = MockSiteConfig::new();
        config.set_expansion_limits(ExpansionLimits {
            max_pp_expand_depth: 10,
            ..ExpansionLimits::default()
        });
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html_expanded(
                "start {{D0}} end",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();

        assert!(
            html.contains("Expansion depth limit exceeded"),
            "expected the depth error span, got: {html}"
        );
        // The error is emitted in place, not as a page-level failure: the text
        // around it survives, which is what "parsing continues" means.
        assert!(html.contains("start "), "got: {html}");
        assert!(html.contains(" end"), "got: {html}");
        // The wrapper is `<span class="error">`, not the `<strong>` the
        // preview-only messages use. Extra attributes follow the class when the
        // span is encapsulation-registered, so match the class alone.
        assert!(html.contains("<span class=\"error\""), "got: {html}");
    }

    /// The node-count limit counts *expansion invocations*, so a template whose
    /// body is large costs one node, not one per child. The other half of that
    /// fact is that a repeat costs again: N transclusions of one template pay N.
    #[tokio::test]
    async fn node_count_limit_trips_on_repeated_transclusions() {
        use crate::resource_limits::ExpansionLimits;

        let source = crate::mock::MockDataSource::new();
        // A body with many children: one expansion, one node.
        source.add_template("Template:Big", "{{P}}".repeat(50).as_str());
        source.add_template("Template:P", "x");

        let mut config = MockSiteConfig::new();
        config.set_expansion_limits(ExpansionLimits {
            max_pp_node_count: 20,
            ..ExpansionLimits::default()
        });
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html_expanded(
                &"{{Big}}\n".repeat(30),
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();

        assert!(
            html.contains("Node-count limit exceeded"),
            "expected the node-count error span, got: {html}"
        );
        // `Template:Big`'s 50 children cost one node for the template, so the
        // pages above the ceiling still produced output rather than nothing.
        assert!(html.contains('x'), "got: {html}");
    }

    /// The node counter must not charge for plain text: only an expansion does.
    /// A page of prose with no templates at all can never trip it, whatever the
    /// ceiling.
    #[tokio::test]
    async fn plain_text_never_charges_a_node() {
        use crate::resource_limits::ExpansionLimits;

        let source = crate::mock::MockDataSource::new();
        let mut config = MockSiteConfig::new();
        config.set_expansion_limits(ExpansionLimits {
            max_pp_node_count: 0,
            max_pp_expand_depth: 0,
            ..ExpansionLimits::default()
        });
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html_expanded(
                &"just words, no braces at all\n".repeat(50),
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        assert!(!html.contains("limit exceeded"), "got: {html}");
        assert!(html.contains("just words"), "got: {html}");
    }

    /// A wiki strips `data-parsoid` before serving, so a comparison against
    /// served HTML must not see it — while the default, which the fixture suite
    /// relies on, keeps it.
    #[test]
    fn strip_data_parsoid_is_opt_in() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let wikitext = "a [[link]] here";

        let kept = parser
            .wikitext_to_html(wikitext, &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(kept.contains("data-parsoid"), "got: {kept}");

        let stripped = parser
            .wikitext_to_html(
                wikitext,
                &ParserOptions {
                    strip_data_parsoid: true,
                    ..ParserOptions::for_page("Test")
                },
            )
            .unwrap();
        assert!(!stripped.contains("data-parsoid"), "got: {stripped}");
        // Only that attribute goes: the link itself is untouched.
        assert!(stripped.contains("mw:WikiLink"), "got: {stripped}");
    }

    /// The strip reaches `data-mw`'s `attribs[].html` fields too. Those are HTML
    /// *strings* built during expansion rather than nodes serialized at the end,
    /// so they take a separate path — and a wiki's served HTML has no
    /// `data-parsoid` inside them either (`Zebra` carries `mw:ExpandedAttrs` with
    /// `id` attributes in the fragment and no `data-parsoid` at all). The fixture
    /// `attributeExpanderTests.txt` shows standalone output keeping it, escaped
    /// as `&apos;` because the whole field is JSON inside an attribute.
    #[tokio::test]
    async fn strip_data_parsoid_reaches_data_mw_attrib_html() {
        let source = crate::mock::MockDataSource::new();
        source.add_template("Template:1x", "{{{1}}}");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let wikitext = "<div {{1x|1=style=\"color:red\"}}>hmm</div>";

        let kept = parser
            .wikitext_to_html_expanded(wikitext, &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        assert!(kept.contains("mw:ExpandedAttrs"), "got: {kept}");
        assert!(
            kept.contains("data-parsoid=&apos;")
                || kept.contains("data-parsoid=\\\"")
                || kept.contains("data-parsoid='"),
            "the field carries it in standalone output: {kept}"
        );

        let stripped = parser
            .wikitext_to_html_expanded(
                wikitext,
                &source,
                &ParserOptions {
                    strip_data_parsoid: true,
                    ..ParserOptions::for_page("Test")
                },
            )
            .await
            .unwrap();
        assert!(stripped.contains("mw:ExpandedAttrs"), "got: {stripped}");
        assert!(
            !stripped.contains("data-parsoid"),
            "nothing carries it when stripping: {stripped}"
        );
    }

    #[test]
    fn test_process_fragment_body_direct() {
        use crate::dom::node::{ElementKind, NodeKind};
        fn has_bold(n: &Node) -> bool {
            matches!(n.kind, NodeKind::Element(ElementKind::Bold))
                || n.children.iter().any(has_bold)
        }
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let sub = parser.fragment_from_body("'''bold'''");
        assert!(has_bold(&sub), "sub-fragment missing bold: {sub:?}");
        // Inline context must not introduce a <p> wrapper around inline content.
        assert!(!matches!(
            sub.children[0].kind,
            NodeKind::Element(ElementKind::Paragraph)
        ));
    }

    #[test]
    fn test_pre_format_wikitext_body() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "<pre format=\"wikitext\">'''bold'''</pre>",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<pre"), "got: {html}");
        assert!(html.contains("<b>bold</b>"), "got: {html}");
    }

    #[test]
    fn test_flatten_nowiki_spans() {
        use crate::dom::node::ElementKind;
        // A `mw:Nowiki` span wrapping a decoded `mw:Entity` is flattened to its
        // text content (the decoded `&`), so it re-escapes correctly inside pre.
        let mut nowiki = Node::element(ElementKind::Span);
        nowiki.set_attr("typeof", "mw:Nowiki");
        let mut entity = Node::element(ElementKind::Span);
        entity.set_attr("typeof", "mw:Entity");
        entity.push_child(Node::text("&"));
        nowiki.push_child(entity);
        nowiki.push_child(Node::text("plitude"));

        let mut root = Node::document();
        root.push_child(nowiki);

        flatten_nowiki_spans(&mut root);

        assert_eq!(root.children.len(), 1, "{root:?}");
        match &root.children[0].kind {
            crate::dom::node::NodeKind::Text(t) => assert_eq!(t, "&plitude"),
            other => panic!("expected text node, got {other:?}"),
        }
    }

    #[test]
    fn test_attr_sanitization() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        // `onmouseover` is not in the `<pre>` whitelist and must be dropped;
        // `width` is allowed.
        let html = parser
            .wikitext_to_html(
                "<pre width=\"8\" onmouseover=\"alert()\">x</pre>",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("width=\"8\""), "got: {html}");
        assert!(!html.contains("onmouseover"), "got: {html}");
    }

    #[test]
    fn test_pre_entity_and_style() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        // Entities inside `<pre>` decode to plain text (no `mw:Entity` span).
        let html = parser
            .wikitext_to_html("<pre>&lt;</pre>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<pre"), "got: {html}");
        assert!(html.contains("&lt;"), "got: {html}");
        assert!(!html.contains("mw:Entity"), "got: {html}");

        // Insecure `style` is replaced by a marker comment, not dropped.
        let html = parser
            .wikitext_to_html(
                "<pre style=\"border-width: expression(alert())\">x</pre>",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("/* insecure input */"), "got: {html}");
    }

    #[test]
    fn test_value_to_dom_html_plain_string() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        // A plain string value serializes as itself (no <p> wrapper in inline
        // context).
        let kv = crate::wikitext::tokens_v2::KeyValue::Str("color:red".to_string());
        let mut fragments = std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        let html = parser.value_to_dom_html(&kv, &mut fragments, &next_id);
        assert_eq!(html, "color:red", "got: {html:?}");
    }

    #[test]
    fn test_wikitext_to_html_bold() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("'''bold'''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<b>"), "got: {html}");
        assert!(html.contains("bold"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_wikilink() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("[[Main Page]]", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<a"), "got: {html}");
        assert!(html.contains("rel=\"mw:WikiLink\""), "got: {html}");
        assert!(html.contains("Main Page"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_extlink() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "[https://example.com Example]",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<a"), "got: {html}");
        assert!(html.contains("rel=\"mw:ExtLink nofollow\""), "got: {html}");
        assert!(html.contains("class=\"external text\""), "got: {html}");
        assert!(html.contains("https://example.com"), "got: {html}");
        assert!(html.contains("Example"), "got: {html}");
        // The structural `<html>` wrapper must appear exactly once (the
        // tree-builder fragment must not be nested inside another wrapper).
        assert_eq!(html.matches("<html").count(), 1, "got: {html}");
        assert_eq!(html.matches("<body").count(), 1, "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_bare_url() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "See https://example.com now",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("rel=\"mw:ExtLink nofollow\""), "got: {html}");
        assert!(html.contains("https://example.com"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_behavior_switch() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("__TOC__", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("mw:PageProp/toc"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_italic() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("''italic''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<i>"), "got: {html}");
        assert!(html.contains("italic"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_bold_italic() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("'''''both'''''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<b>"), "got: {html}");
        assert!(html.contains("<i>"), "got: {html}");
        assert!(html.contains("both"), "got: {html}");
    }

    #[test]
    fn test_auto_inserted_empty_bold_stripped() {
        // `''foo''''bar''` (the "annoying" misnested case) produces an
        // auto-inserted `<b></b>` at end of line in the legacy parser, but
        // Parsoid strips it via `ProcessTreeBuilderFixups::removeAutoInsertedEmptyTags`.
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("''foo''''bar''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<i>foo'<b>bar</b></i>"), "got: {html}");
        // No stray empty `<b></b>` should remain.
        assert!(!html.contains("<b></b>"), "got: {html}");
        assert!(!html.contains("<i></i>"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_unordered_list() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("* one\n* two", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<ul"), "got: {html}");
        assert!(html.contains("<li"), "got: {html}");
        assert!(html.contains("one"), "got: {html}");
        assert!(html.contains("two"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_paragraph_break() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("First\n\nSecond", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("First"), "got: {html}");
        assert!(html.contains("Second"), "got: {html}");
        // The tag carries `data-parsoid` (native mode), so match the tag boundary
        // rather than a bare `<p>`.
        let paragraphs = html.matches("<p ").count() + html.matches("<p>").count();
        assert!(paragraphs >= 2, "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_definition_list() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(";term:definition", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<dl"), "got: {html}");
        assert!(html.contains("<dt"), "got: {html}");
        assert!(html.contains("<dd"), "got: {html}");
        assert!(html.contains("term"), "got: {html}");
        assert!(html.contains("definition"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_nested_list() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("* a\n** b", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<ul"), "got: {html}");
        assert!(html.contains("a"), "got: {html}");
        assert!(html.contains("b"), "got: {html}");
        // Nested bullet means two nested <ul> elements.
        assert!(html.matches("<ul").count() >= 2, "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_ordered_list() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("# one\n# two", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<ol"), "got: {html}");
        assert!(html.contains("<li"), "got: {html}");
        assert!(html.contains("one"), "got: {html}");
        assert!(html.contains("two"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_multi_line_dl() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(";term\n:definition", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<dl"), "got: {html}");
        assert!(html.contains("<dt"), "got: {html}");
        assert!(html.contains("<dd"), "got: {html}");
        assert!(html.contains("term"), "got: {html}");
        assert!(html.contains("definition"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_heading_with_link() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("== See [[Main Page]] ==", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<h2"), "got: {html}");
        assert!(html.contains("<a"), "got: {html}");
        assert!(html.contains("rel=\"mw:WikiLink\""), "got: {html}");
        assert!(html.contains("Main Page"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_wikitext_table() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("{|\n|-\n| cell\n|}", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<table"), "got: {html}");
        assert!(html.contains("<tr"), "got: {html}");
        assert!(html.contains("<td"), "got: {html}");
        assert!(html.contains("cell"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_html_table() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "<table><tr><td>cell</td></tr></table>",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<table"), "got: {html}");
        assert!(html.contains("<tr"), "got: {html}");
        assert!(html.contains("<td"), "got: {html}");
        assert!(html.contains("cell"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_table_header() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("{|\n! header\n|}", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<table"), "got: {html}");
        assert!(html.contains("<th"), "got: {html}");
        assert!(html.contains("header"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_table_multi_cell() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("{|\n| a || b\n|}", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<table"), "got: {html}");
        assert!(html.contains("a"), "got: {html}");
        assert!(html.contains("b"), "got: {html}");
        assert!(html.matches("<td").count() >= 2, "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_redirect() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("#redirect [[Target]]", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(
            html.contains(r#"rel="mw:PageProp/redirect""#),
            "got: {html}"
        );
        assert!(html.contains(r#"href="./Target""#), "got: {html}");
        assert!(!html.contains("<mw:redirect"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_redirect_nowiki_bail() {
        // A redirect target containing `<nowiki>` cannot be rendered as a link;
        // it bails to `<ol><li>REDIRECT [[…]]</li></ol>`. The nowiki content is
        // wrapped in `mw:Nowiki` (matching PHP `Nowiki::sourceToDom`).
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "#REDIRECT [[<nowiki>[[Bar]]</nowiki>]]",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<ol") && html.contains("<li"), "got: {html}");
        assert!(html.contains("REDIRECT [["), "got: {html}");
        assert!(html.contains("mw:Nowiki"), "got: {html}");
        assert!(html.contains("[[Bar]]"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_redirect_piped_target() {
        // The redirect target is the part before the `|`; the link label is
        // ignored (matches PHP, which renders the target only).
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "#REDIRECT [[Target|label]]",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains(r#"href="./Target""#), "got: {html}");
        assert!(!html.contains("label"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_table_caption() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "{|\n|+ A caption\n|-\n| cell\n|}",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<caption"), "got: {html}");
        assert!(html.contains("A caption"), "got: {html}");
        assert!(html.contains("cell"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_table_cell_attrs() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "{|\n|-\n| style=\"color:red\" | cell\n|}",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<td"), "got: {html}");
        assert!(html.contains("color:red"), "got: {html}");
        assert!(html.contains("cell"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_hr() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("----", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<hr"), "got: {html}");
    }

    #[test]
    fn test_wikitext_literal_html_tag_stx() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("<div>foo</div>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<div"), "got: {html}");
        // Literal HTML tags carry stx:"html" in data-parsoid.
        assert!(
            html.contains("\"stx\":\"html\""),
            "expected stx:html in: {html}"
        );
    }

    #[tokio::test]
    async fn test_wikitext_to_html_template() {
        use crate::mock::MockDataSource;

        let source = MockDataSource::new();
        source.add_template("Template:Foo", "Hello {{{1}}}!");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        let html = parser
            .wikitext_to_html_expanded("{{Foo|world}}", &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        assert!(html.contains("Hello world"), "got: {html}");
        // The transclusion should carry a `data-mw` marker.
        assert!(html.contains("data-mw"), "expected data-mw in: {html}");
        // The transclusion is encapsulated in a `<span about=... typeof="mw:Transclusion">`.
        assert!(
            html.contains("typeof=\"mw:Transclusion\""),
            "expected mw:Transclusion span in: {html}"
        );
        assert!(
            html.contains("about=\"#mwt1\""),
            "expected about=#mwt1 in: {html}"
        );
    }

    #[tokio::test]
    async fn test_template_expands_to_list_encapsulation() {
        use crate::mock::MockDataSource;
        let source = MockDataSource::new();
        source.add_template("Template:1x", "{{{1}}}");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        // A template expanding to list syntax gets fostered out of the list;
        // the transclusion `about`/`typeof` must be transferred onto the `<ul>`.
        let html = parser
            .wikitext_to_html_expanded("{{1x|*bar}}", &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        assert!(
            html.contains("<ul about=\"#mwt1\" typeof=\"mw:Transclusion\""),
            "got: {html}"
        );
        assert!(html.contains("<li"), "got: {html}");
        assert!(html.contains(">bar</li>"), "got: {html}");
        assert!(!html.contains("mw:Transclusion/End"), "got: {html}");
    }

    #[tokio::test]
    async fn test_wikitext_nested_template() {
        use crate::mock::MockDataSource;

        let source = MockDataSource::new();
        source.add_template("Template:Outer", "{{Inner|world}}");
        source.add_template("Template:Inner", "Hello {{{1}}}!");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        let html = parser
            .wikitext_to_html_expanded("{{Outer}}", &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        // The nested `{{Inner|world}}` should expand to "Hello world!".
        assert!(html.contains("Hello world"), "got: {html}");
    }

    #[tokio::test]
    async fn test_wikitext_templated_template_target() {
        use crate::mock::MockDataSource;

        // `{{ {{T}} }}` — the target is itself a transclusion. The inner template
        // expands first (in document order) to `Main Page|Something else`, which
        // is not a valid title, so the outer braces stay literal around the
        // expanded inner transclusion.
        let source = MockDataSource::new();
        source.add_template("Template:T290526", "Main Page{{!}}Something else");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        let html = parser
            .wikitext_to_html_expanded(
                "{{ {{T290526}} }}",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        assert!(html.contains("{{ "), "got: {html}");
        assert!(html.contains("Main Page|Something else"), "got: {html}");
        assert!(html.contains(" }}</p>"), "got: {html}");
        // The inner transclusion must survive as a real span, not as a stray
        // unexpanded `<template …>` token.
        assert!(html.contains("mw:Transclusion"), "got: {html}");
        assert!(!html.contains("<template "), "got: {html}");
    }

    #[tokio::test]
    async fn test_wikitext_self_referential_template() {
        use crate::mock::MockDataSource;

        let source = MockDataSource::new();
        // A self-referential template would infinitely recurse without a
        // loop/depth guard.
        source.add_template("Template:Loop", "{{Loop}}");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        let html = parser
            .wikitext_to_html_expanded("{{Loop}}", &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        // The loop is detected and an error is emitted rather than hanging.
        assert!(
            html.contains("Template loop detected") || html.contains("limit exceeded"),
            "got: {html}"
        );
    }

    #[test]
    fn test_wikitext_to_html_entity_named() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("A &amp; B", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
        // The decoded `&` is HTML-escaped as `&amp;` inside the span.
        assert!(
            html.contains("> &amp;</span>") || html.contains(">&amp;</span>"),
            "got: {html}"
        );
        // The entity span carries both the raw and decoded source in
        // data-parsoid (src and srcContent), mirroring PHP.
        assert!(html.contains("\"src\":\"&amp;amp;\""), "got: {html}");
        assert!(html.contains("\"srcContent\":\"&amp;\""), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_numeric() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&#169;", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
        assert!(html.contains("©"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_hex() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&#x1F600;", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
        assert!(html.contains("😀"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_unknown_left_literal() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&foo;", &ParserOptions::for_page("Test"))
            .unwrap();
        // Unknown named entities are not wrapped in an mw:Entity span.
        assert!(!html.contains("mw:Entity"), "got: {html}");
        assert!(html.contains("&amp;foo;"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_accented() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&Aacute;", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
        assert!(html.contains("Á"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_two_codepoints() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&acE;", &ParserOptions::for_page("Test"))
            .unwrap();
        // acE decodes to two codepoints (U+223E U+0333), still wrapped.
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_magic_link_rfc() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("See RFC 1234 here", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("rel=\"mw:ExtLink nofollow\""), "got: {html}");
        assert!(
            html.contains("https://datatracker.ietf.org/doc/html/rfc1234"),
            "got: {html}"
        );
        assert!(html.contains("RFC 1234"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_magic_link_pmid() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("PMID 1234", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("rel=\"mw:ExtLink nofollow\""), "got: {html}");
        assert!(
            html.contains("//www.ncbi.nlm.nih.gov/pubmed/1234?dopt=Abstract"),
            "got: {html}"
        );
    }

    #[test]
    fn test_wikitext_to_html_magic_link_isbn() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("ISBN 978-0-123456-47-2", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("rel=\"mw:WikiLink\""), "got: {html}");
        assert!(
            html.contains("Special:BookSources/9780123456472"),
            "got: {html}"
        );
    }

    #[test]
    fn test_wikitext_to_html_section_wrapping() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let mut opts = ParserOptions::for_page("Test");
        opts.wrap_sections = true;
        let html = parser
            .wikitext_to_html("lead\n== Heading ==\nbody\n", &opts)
            .unwrap();
        // There is always a lead section, and each wikitext heading is wrapped.
        assert!(html.contains("data-mw-section-id=\"0\""), "got: {html}");
        assert!(html.contains("data-mw-section-id=\"1\""), "got: {html}");
        assert!(html.contains("<section"), "got: {html}");
        assert!(html.contains("<h2"), "got: {html}");
        assert!(html.contains("Heading"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_nested_section() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let mut opts = ParserOptions::for_page("Test");
        opts.wrap_sections = true;
        let html = parser
            .wikitext_to_html("== A ==\n=== B ===\n", &opts)
            .unwrap();
        assert!(html.contains("data-mw-section-id=\"1\""), "got: {html}");
        assert!(html.contains("data-mw-section-id=\"2\""), "got: {html}");
    }

    #[test]
    fn test_wrapper_tag_target() {
        use crate::wikitext::tokens_v2::SelfclosingTagTk;

        let mut stt = SelfclosingTagTk::new(
            "extension",
            vec![],
            crate::wikitext::tokens_v2::DataParsoid::default(),
        );
        stt.add_attribute_str("name", "divtag");
        let item = Item::Tok(ParsoidToken::SelfclosingTag(stt));
        let (_, ext_name, wrapper) = wrapper_tag_target(&item).expect("divtag recognized");
        assert_eq!(ext_name, "divtag");
        assert_eq!(wrapper, "div");

        let mut stt = SelfclosingTagTk::new(
            "extension",
            vec![],
            crate::wikitext::tokens_v2::DataParsoid::default(),
        );
        stt.add_attribute_str("name", "spantag");
        let item = Item::Tok(ParsoidToken::SelfclosingTag(stt));
        let (_, ext_name, wrapper) = wrapper_tag_target(&item).expect("spantag recognized");
        assert_eq!(ext_name, "spantag");
        assert_eq!(wrapper, "span");

        let stt = SelfclosingTagTk::new(
            "extension",
            vec![],
            crate::wikitext::tokens_v2::DataParsoid::default(),
        );
        let item = Item::Tok(ParsoidToken::SelfclosingTag(stt));
        assert!(wrapper_tag_target(&item).is_none());
    }

    /// A module receives an argument's *expanded* text, and that includes a link
    /// whose target is built from a template or a magic variable: the target is a
    /// token list held inside the `wikilink` token, so expansion has to look there
    /// rather than only at the top level. `[[{{PAGENAME}}]]` on `Test` is
    /// `[[Test]]`.
    #[tokio::test]
    async fn a_link_target_in_an_invoke_argument_is_expanded() {
        use crate::mock::MockDataSource;

        let config = MockSiteConfig::new();
        let source = MockDataSource::new();
        // Echo its second `#invoke` argument, so the value the module received is
        // what the render shows.
        source.add_module(
            "Module:Echo",
            "return { main = function(frame) return frame.args[1] end }",
        );
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html_expanded(
                "{{#invoke:Echo|main|[[{{PAGENAME}}]]}}",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        assert!(
            html.contains("./Test"),
            "the link target must be expanded to the page title: {html}"
        );
        assert!(
            html.contains("mw-selflink"),
            "the expanded target links to this page: {html}"
        );
    }

    /// `#ifexist` answers its `then`/`else` from the title's existence, which the
    /// parser resolves before the (synchronous) call runs. A title with content is
    /// not missing; a title with none is. Before this was implemented the call
    /// fell through to the unknown-parser-function arm and leaked its own source.
    #[tokio::test]
    async fn ifexist_takes_the_branch_the_title_selects() {
        use crate::mock::MockDataSource;

        let config = MockSiteConfig::new();
        let source = MockDataSource::new();
        source.add_page("Exists", "content");
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html_expanded(
                "{{#ifexist:Exists|EXISTY|EXISTN}} {{#ifexist:Absent|ABSENTY|ABSENTN}}",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        assert!(
            html.contains(">EXISTY</span>"),
            "an existing title takes `then`: {html}"
        );
        assert!(
            !html.contains(">EXISTN</span>"),
            "...and not `else`: {html}"
        );
        assert!(
            html.contains(">ABSENTN</span>"),
            "a missing title takes `else`: {html}"
        );
        assert!(
            !html.contains(">ABSENTY</span>"),
            "...and not `then`: {html}"
        );
        assert!(
            !html.contains(">{{#ifexist"),
            "the call must not leak its source as text: {html}"
        );
    }

    /// A chunk's templates are numbered before its extensions.
    ///
    /// `TokenHandlerPipeline::processChunk` runs each transformer over the whole
    /// chunk, and `TemplateHandler` precedes `ExtensionHandler`, so
    /// `ExtensionHandler::onExtension`'s `$env->newAboutId()` calls all happen
    /// after the chunk's templates have taken theirs. A `<ref>` after a template
    /// therefore takes the *next* id — and one written *before* the template
    /// still takes the later one. Both spellings pin the rule, because only the
    /// second distinguishes it from plain document order. This is why
    /// `Bicycle`'s infobox `<ref>` is `#mwt11` in the service.
    #[tokio::test]
    async fn extensions_are_numbered_after_templates_in_a_chunk() {
        let source = crate::mock::MockDataSource::new();
        source.add_template("Template:1x", "{{{1|}}}");
        let config = crate::mock::MockSiteConfig::new();
        let parser = Parser::new(&config);
        let render = |wt: &str| {
            let parser = &parser;
            let source = &source;
            let wt = wt.to_string();
            async move {
                parser
                    .wikitext_to_html_expanded(&wt, source, &ParserOptions::for_page("Test"))
                    .await
                    .unwrap()
            }
        };

        let after = render("{{1x|a}}<ref name=\"r\">b</ref>").await;
        assert!(after.contains(r##"<span about="#mwt1""##), "got: {after}");
        assert!(after.contains(r##"<sup about="#mwt2""##), "got: {after}");

        let before = render("<ref name=\"r\">b</ref>{{1x|a}}").await;
        assert!(
            before.contains(r##"<span about="#mwt1""##),
            "the template is numbered first even when written second: {before}"
        );
        assert!(before.contains(r##"<sup about="#mwt2""##), "got: {before}");

        // The reference inside a template is numbered during that template's
        // expansion, *before* the templates that follow it, so the counter has
        // already moved past the reference by the time `{{1x|a}}` is numbered.
        // The `<sup>`'s own `about` cannot show this — encapsulation overwrites
        // it with the enclosing template's — so the evidence is the *later* ids:
        // with the TT2 numbering `{{1x|a}}` is `#mwt3`; had the late Cite pass
        // numbered the reference, it would be `#mwt2`.
        //
        // The service numbers these `#mwt4`/`#mwt5`: it spends one id more than
        // rustoid on this shape, a separate gap recorded in `ONLINE-PARITY.md`
        // and deliberately not asserted here.
        let nested = render("{{1x|X<ref name=\"r\">b</ref>}}{{1x|a}}{{1x|b}}").await;
        assert!(
            nested.contains(r##"<span about="#mwt4""##),
            "the reference is numbered before the two following templates: {nested}"
        );
    }

    /// `<pre>` is a non-`nowiki` extension, so TT2 numbers it and the id reaches
    /// the output. The fixture suite cannot see this — it strips `about` — so the
    /// assertion is on the rendered `<pre>`.
    #[tokio::test]
    async fn a_pre_extension_is_numbered_and_shows_the_id() {
        let source = crate::mock::MockDataSource::new();
        source.add_template("Template:1x", "{{{1|}}}");
        let config = crate::mock::MockSiteConfig::new();
        let parser = Parser::new(&config);
        // The `<pre>` is written *first*, but the chunk's template is numbered
        // before it (templates before extensions), so the pre is `#mwt2`.
        let html = parser
            .wikitext_to_html_expanded(
                "<pre>x</pre>{{1x|a}}",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        assert!(html.contains(r##"about="#mwt1""##), "got: {html}");
        assert!(
            html.contains(r##"<pre typeof="mw:Extension/pre" about="#mwt2""##),
            "the pre must carry the id TT2 spent: {html}"
        );
    }
}
