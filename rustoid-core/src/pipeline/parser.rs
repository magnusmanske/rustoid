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
    next_id: &mut usize,
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
                    let mut id = 0usize;
                    render_inline_fragment(config, items, &mut f, &mut id)
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
    /// The protection levels of the page being parsed, as
    /// `{{PROTECTIONLEVEL:action}}` with no title argument reports them.
    page_protection: std::cell::RefCell<ProtectionEntry>,
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
            page_protection: std::cell::RefCell::new(ProtectionEntry::default()),
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
        // PHP Parsoid numbers transclusion `about` ids starting from 1.
        let id = counter.get() + 1;
        counter.set(id);
        format!("#mwt{id}")
    }

    /// Expand `wikilink` self-closing tokens into `<a>`/`<link>` tag sequences
    /// (mirrors the TT2 `WikiLinkHandler`, whose rendering path lives in
    /// `pipeline::wiki_link_render`).
    fn render_links(
        &self,
        tokens: Vec<Item>,
        fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
        next_id: &mut usize,
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
                    let mut id = 0usize;
                    self.build_inline_fragment(items, &mut f, &mut id)
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
        next_id: &mut usize,
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
        next_id: &mut usize,
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
        let mut next_id = 0usize;
        if source.is_some() {
            tokens = self
                .expand_templates(frame, tokens, source, about_counter, true, body)
                .await;
            // TT2 order: ExtensionHandler precedes the AttributeExpander.
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                &mut next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    &mut next_id,
                )
                .await;
        }
        // The quote transformer flushes pending quotes only on a newline or EOF
        // token; append a synthetic EOF so quotes (`'''bold'''`) flush.
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        let mut frag = self.build_inline_fragment(tokens, &mut fragments, &mut next_id);
        flatten_nowiki_spans(&mut frag);
        frag
    }

    /// Build an inline fragment document from an already-tokenized (and
    /// optionally template-expanded) token stream.
    fn fragment_from_tokens(&self, tokens: Vec<Item>) -> Node {
        let mut fragments = std::collections::HashMap::new();
        let mut next_id = 0usize;
        self.build_inline_fragment(tokens, &mut fragments, &mut next_id)
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
        next_id: &mut usize,
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
        next_id: &mut usize,
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
        next_id: &mut usize,
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
        next_id: &mut usize,
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
            let id = *next_id;
            *next_id += 1;
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
        next_id: &mut usize,
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
            let id = *next_id;
            *next_id += 1;
            fragments.insert(id, sub);
            emit_wrapper_extension_placeholder(stt, ext_name, wrapper, id, &mut out);
        }

        (out, fragments)
    }

    /// Synchronous [`expand_wrapper_tag`] for the `wikitext_to_ast` path.
    fn expand_wrapper_tag_sync(
        &self,
        tokens: Vec<Item>,
        next_id: &mut usize,
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
            let id = *next_id;
            *next_id += 1;
            fragments.insert(id, sub);
            emit_wrapper_extension_placeholder(stt, ext_name, wrapper, id, &mut out);
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
        let mut next_id = 0usize;
        if source.is_some() {
            tokens = self
                .expand_templates(frame, tokens, source, about_counter, in_template, body)
                .await;
            // TT2 order: ExtensionHandler precedes the AttributeExpander.
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                &mut next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    &mut next_id,
                )
                .await;
        }
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        let mut ast = if inline {
            self.build_inline_fragment(tokens, &mut fragments, &mut next_id)
        } else {
            let stage = TreeBuilderStage::new(false);
            let mut ast = stage.to_ast_with_fragments(tokens, None, self.config, fragments.clone());
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
        let mut next_id = 0usize;
        let tokens = self.render_links(tokens, &mut fragments, &mut next_id, None);
        let tokens = self.render_external_links(tokens, &mut fragments, &mut next_id);
        let tokens = self.render_behavior_switches(tokens);
        let tokens = self.render_language_variants(tokens);

        let stage = TreeBuilderStage::new(inline);
        let mut ast = stage.to_ast_with_fragments(tokens, None, self.config, fragments);
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
        next_id: &mut usize,
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
            let id = *next_id;
            *next_id += 1;
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
        let mut next_id = 0usize;
        if source.is_some() {
            tokens = self
                .expand_templates(frame, tokens, source, about_counter, false, &caption)
                .await;
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                &mut next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    &mut next_id,
                )
                .await;
        }
        let mut frag = self.build_inline_fragment(tokens, &mut fragments, &mut next_id);
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
        let mut next_id = 0usize;
        if source.is_some() {
            tokens = self
                .expand_templates(frame, tokens, source, about_counter, false, &wikitext)
                .await;
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                &mut next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    &mut next_id,
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
                    &mut next_id,
                    &mut |items| {
                        let mut f = std::collections::HashMap::new();
                        let mut id = 0usize;
                        render_inline_fragment(self.config, items, &mut f, &mut id)
                    },
                )
            })
            .collect();
        let tokens = self.render_external_links(tokens, &mut fragments, &mut next_id);
        let mut tokens = self.render_behavior_switches(tokens);
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        // Build the inline tree, splicing the caption `mw:dom-fragment-token`
        // placeholders via the fragments map populated by `renderFile`.
        let stage = TreeBuilderStage::new(true);
        let frag = stage.to_ast_with_fragments(tokens, None, self.config, fragments);
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
        next_id: &mut usize,
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
            let id = *next_id;
            *next_id += 1;
            fragments.insert(id, frag);
            out.push(emit_gallery_placeholder(stt, id));
        }

        out
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
    async fn expand_templatestyles(
        &self,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        next_id: &mut usize,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        let Some(source) = source else {
            return (tokens, std::collections::HashMap::new());
        };
        let mut out: Vec<Item> = Vec::with_capacity(tokens.len());
        let mut fragments: std::collections::HashMap<usize, Node> =
            std::collections::HashMap::new();

        for item in tokens {
            let Some(stt) = templatestyles_target(&item) else {
                out.push(item);
                continue;
            };
            // `src` and `wrapper` arrive as `data-mw` rich attribs: the tokenizer
            // stores the parsed start-tag attributes there, not as plain token
            // attributes.
            let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
            // A value that came through Lua reaches here with its quotes
            // backslash-escaped (`\"Z.css\"`), because Scribunto passes the
            // string through unchanged and `pf_tag` cannot unescape it the way it
            // does for a plain tag. Strip both forms before treating it as a
            // title: an attribute value is not part of the page name.
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
                        // No revision means the source cannot be pinned, and a
                        // dedup key that names no revision would still have to
                        // match Parsoid's.
                        _ => None,
                    }
                }
                None => None,
            };

            let Some((body, revid, src)) = resolved else {
                out.push(Item::Tok(ParsoidToken::SelfclosingTag(stt.clone())));
                continue;
            };

            let css = crate::pipeline::templatestyles::render(&body, attr("wrapper").as_deref());
            let node = crate::pipeline::templatestyles::style_node(
                &css,
                revid,
                &src,
                &self.new_about_id(about_counter),
            );
            let mut frag = crate::dom::node::Node::document();
            frag.push_child(node);
            let id = *next_id;
            *next_id += 1;
            fragments.insert(id, frag);
            out.extend(emit_style_placeholder(stt, id));
        }

        (out, fragments)
    }

    /// Synchronous [`expand_gallery`] for the `wikitext_to_ast` path (no data
    /// source, so no template expansion).
    fn expand_gallery_sync(
        &self,
        tokens: Vec<Item>,
        about_counter: &std::cell::Cell<usize>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &mut usize,
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
            let id = *next_id;
            *next_id += 1;
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
        let mut next_id = 0usize;
        let tokens = self.render_links(tokens, &mut fragments, &mut next_id, context_title);
        let tokens = self.render_external_links(tokens, &mut fragments, &mut next_id);
        let tokens = self.render_behavior_switches(tokens);
        let tokens = self.render_language_variants(tokens);
        let (tokens, pre_fragments) = self.expand_wikitext_pre_sync(tokens, &mut next_id);
        fragments.extend(pre_fragments);
        let (tokens, wrapper_fragments) = self.expand_wrapper_tag_sync(tokens, &mut next_id);
        fragments.extend(wrapper_fragments);
        let tokens = self.expand_gallery_sync(
            tokens,
            &std::cell::Cell::new(0usize),
            &mut fragments,
            &mut next_id,
        );
        let stage = TreeBuilderStage::new(false);
        let mut ast = stage.to_ast_with_fragments(tokens, Some(wikitext), self.config, fragments);
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
        *self.page_protection.borrow_mut() = page_protection;
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
            .expand_templates(&frame, tokens, source, about_counter, false, page_source)
            .await;
        // TT2 order: ExtensionHandler runs after TemplateHandler and before
        // AttributeExpander (PHP `ParserPipelineFactory::STAGES`). Attribute
        // values are sub-pipelines, so the extension handler must reach inside
        // them before attributes are expanded.
        let mut fragments: std::collections::HashMap<usize, crate::dom::node::Node> =
            std::collections::HashMap::new();
        let mut next_id = 0usize;
        let tokens = crate::pipeline::extension_handler::expand_in_attributes(
            tokens,
            self.config,
            &mut fragments,
            &mut next_id,
        );
        let tokens = self
            .expand_attributes(
                &frame,
                tokens,
                source,
                about_counter,
                Some(page_source),
                &mut fragments,
                &mut next_id,
            )
            .await;
        let tokens = self.render_links(tokens, &mut fragments, &mut next_id, Some(&title));
        let tokens = self.render_external_links(tokens, &mut fragments, &mut next_id);
        let tokens = self.render_behavior_switches(tokens);
        let tokens = self.render_language_variants(tokens);
        // Route `format="wikitext"` extension bodies through the inline
        // sub-pipeline, producing `mw:dom-fragment-token` placeholders + their
        // pre-built sub-fragments.
        let (tokens, pre_fragments) = self
            .expand_wikitext_pre(tokens, source, &frame, about_counter, &mut next_id)
            .await;
        fragments.extend(pre_fragments);
        let (tokens, wrapper_fragments) = self
            .expand_wrapper_tag(tokens, source, &frame, about_counter, &mut next_id)
            .await;
        fragments.extend(wrapper_fragments);
        let tokens = self
            .expand_gallery(
                tokens,
                source,
                &frame,
                about_counter,
                &mut fragments,
                &mut next_id,
            )
            .await;
        // `<templatestyles>` needs a page fetch, so like `gallery` it runs here
        // rather than in the synchronous extension handler.
        let (tokens, style_fragments) = self
            .expand_templatestyles(tokens, source, about_counter, &mut next_id)
            .await;
        fragments.extend(style_fragments);

        let stage = TreeBuilderStage::new(false);
        let mut ast =
            stage.to_ast_with_fragments(tokens, Some(page_source), self.config, fragments);
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
                let mut next = next_note_id.get();
                let mut fragments = note_fragments.borrow_mut();
                let frag = render_inline_fragment(
                    self.config,
                    self.tokenize(body).unwrap_or_default(),
                    &mut fragments,
                    &mut next,
                );
                next_note_id.set(next);
                frag
            };
            crate::ext::cite::run(&mut ast, &page_title_prefixed, &mut ids, &render_body);
            // Hand the advanced counter back: ids are allocated in document
            // order, so anything numbered after Cite must continue from here.
            about_counter.set(ids.about_counter());
        }
        wrap_sections_in_ast(&mut ast, options.wrap_sections);
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

    /// Expand `template`/`templatearg` tokens in-place.
    ///
    /// `in_template` mirrors PHP's `wrapTemplates = !$options['inTemplate']`:
    /// when true (nested template / extension-content context), expanded
    /// templates are returned *without* `mw:Transclusion` encapsulation.
    ///
    /// `src_text` is the wikitext the tokens were tokenized from, used to
    /// recover argument source spans (`ParamInfo`'s `valueWt`). Tokens whose
    /// ranges carry their own source ignore it.
    async fn expand_templates(
        &self,
        frame: &Frame,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
        src_text: &str,
    ) -> Vec<Item> {
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
                let about_id = self.new_about_id(about_counter);
                let expanded =
                    TemplateHandler.handle_template_arg_token(frame, tok, about_id, !in_template);
                self.leave_pp_node();
                for e in &expanded {
                    track_table(e, &mut table_depth);
                }
                out.extend(expanded);
                continue;
            }

            {
                let mut d = table_depth;
                track_table(&item, &mut d);
                table_depth = d;
            }
            out.push(item);
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
        let in_tpl = in_template || stt.data_parsoid.tmp.in_arg_value;

        let about_id = self.new_about_id(about_counter);

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

        let mut out = Vec::new();
        match resolve_template_target(self.config, Some(frame.title()), &target_str) {
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
                // Scribunto's `#invoke` is not an ordinary parser
                // function: everything after the colon is its argument
                // list, and the tokenizer has already split that on `|`,
                // so the pieces must be put back together.
                let invoke_arg = invoke_arg_text(pf_arg, &params);
                // Scribunto's `frame:getParent()` is the frame of the
                // *calling template*, and modules read its args
                // constantly (`Module:Infobox`, `Module:Check for
                // conflicting parameters` both do it on their first
                // lines). The parent's arguments are this frame's.
                let parent_args = frame.args().args.clone();
                let expanded = self
                    .expand_invoke(
                        source,
                        frame,
                        &invoke_arg,
                        about_id,
                        tok,
                        in_template,
                        src_text,
                        parent_args,
                    )
                    .await;
                for e in &expanded {
                    track_table(e, table_depth);
                }
                out.extend(expanded);
            }
            Some(ResolvedTarget::Template { name, title }) => {
                let expanded = self
                    .expand_one_template(
                        source,
                        frame,
                        &name,
                        &target_str,
                        &title,
                        &params,
                        about_id,
                        tok,
                        about_counter,
                        in_tpl,
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
                let arg_value = stt.data_parsoid.tmp.in_arg_value;
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
                let expanded = TemplateHandler.process(
                    self.config,
                    frame,
                    about_counter,
                    &self.protection_context(),
                    vec![expanded_item],
                );
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
    async fn expand_target_templates(
        &self,
        frame: &Frame,
        mut attribs: Vec<crate::wikitext::tokens_v2::KV>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
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
        next_id: &mut usize,
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
            let outer_next_id = &mut *next_id;
            let fragments = std::cell::RefCell::new(std::mem::take(outer_fragments));
            let next_id = std::cell::Cell::new(*outer_next_id);
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
                &|kv| {
                    let mut id = next_id.get();
                    let html = self.value_to_dom_html(kv, &mut fragments.borrow_mut(), &mut id);
                    next_id.set(id);
                    html
                },
                page_source,
            );
            out.extend(result);
            *outer_fragments = fragments.into_inner();
            *outer_next_id = next_id.get();
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
        target_str: &str,
        title: &crate::title::Title,
        params: &crate::pipeline::parser_functions::Params,
        about_id: String,
        token: &ParsoidToken,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
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
            if in_template {
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
            if in_template {
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
        // that is what Parsoid records — so only the body source and the frame
        // title are replaced here.
        let (template_src, title) = match follow_template_redirect(src, title).await {
            Redirected::Followed { body, title } => (body, title),
            // The page exists and *is* a redirect, but its target could not be
            // fetched. Treat it as a missing template rather than falling back to
            // the redirect's own body: the `#REDIRECT [[…]]` line and its
            // `[[Category:…]]` trailer are bookkeeping, and rendering them as
            // article text invents content Parsoid never emits.
            Redirected::Unresolved => {
                if in_template {
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
        let expanded = Box::pin(self.expand_templates(
            &child_frame,
            spliced,
            Some(src),
            about_counter,
            in_template,
            &template_src,
        ))
        .await;

        if in_template || target_has_comment {
            // Nested/extension-content context, or a comment in the template
            // target (`{{f<!---->oo}}`): no `mw:Transclusion` wrapping.
            return expanded;
        }

        let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
        let mut info = template_info_from(None, Some(name), vec![]);
        info.target_wt = Some(target_str.to_string());
        info.href = Some(crate::title::make_link(&title, self.config));
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
        pf_arg: &str,
        about_id: String,
        token: &ParsoidToken,
        in_template: bool,
        _page_source: &str,
        parent_args: Vec<crate::wikitext::tokens_v2::KV>,
    ) -> Vec<Item> {
        // Without a data source there is nothing to fetch the module from, so
        // leave the call as source text (the standalone behaviour of every
        // parser function rustoid cannot implement).
        let Some(src) = source else {
            return vec![Item::Str(format!("{{{{#invoke:{pf_arg}}}}}"))];
        };

        // The module name is needed for the `data-mw` target as well as for the
        // lookup, and `invoke()` would only re-derive it, so it is parsed here.
        // The caller has already resolved this call as `invoke`, so `parse`
        // cannot fail on it; the guard is for defence rather than for a case the
        // tokenizer can produce.
        let Some(call) = crate::lua::invoke::Invoke::parse(pf_arg) else {
            return vec![Item::Str(format!("{{{{#invoke:{pf_arg}}}}}"))];
        };
        let module = call.module;

        let site = crate::lua::engine::LuaSite::from_config(self.config);
        // The parent frame's args and title are the calling template's, which
        // modules read through `frame:getParent()`. A `#invoke` written directly
        // on a page has no parent frame at all, which is distinguishable from
        // one inside a template by the frame's namespace: a template frame is
        // always in the Template namespace.
        let parent = frame_args_to_lua(&parent_args);
        let parent_title = frame.title().full_text();
        let inside_template = self
            .config
            .canonical_namespace_id("Template")
            .is_some_and(|ns| frame.title().namespace_id == ns);
        let ctx = crate::lua::engine::FrameContext {
            parent_args: parent,
            parent_title: Some(parent_title),
            has_parent: inside_template,
            page_source: _page_source.to_string(),
            page_title: Some(self.page_title.borrow().clone()),
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
            pf_arg,
            src,
            site,
            &frame.title().full_text(),
            ctx,
            |request| self.expand_lua_request(source, frame, request, about_id.clone(), token),
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
        let expanded = Box::pin(self.expand_templates(
            &child,
            items,
            source,
            &std::cell::Cell::new(0usize),
            in_template,
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

        if in_template {
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
        let mut info = template_info_from(Some("invoke"), None, vec![]);
        info.target_wt = Some(format!("#invoke:{module}"));
        info.param_infos = std::iter::once({
            let mut p = ParamInfo::new("1".to_string());
            p.value_wt = call.function.clone();
            p
        })
        .chain(call.args.iter().enumerate().map(|(i, (name, value))| {
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
        let expanded = Box::pin(self.expand_templates(
            &child,
            items,
            Some(source),
            &std::cell::Cell::new(0usize),
            /* in_template */ true,
            &text,
        ))
        .await;
        self.lua_expansion_depth
            .set(self.lua_expansion_depth.get() - 1);

        // A parser function expands in place, with no transclusion wrapper: the
        // caller wrote `#uc`, not a page name, so there is nothing to attribute
        // a separate transclusion to.
        if matches!(request, FrameRequest::CallParserFunction { .. }) {
            return crate::pipeline::lua_deferred::render_answer(&expanded);
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

/// Render a token chunk's `key`/`value` back to source text, for cases where a
/// construct is re-read as text rather than expanded in place.
fn kv_to_source_text(kv: &crate::wikitext::tokens_v2::KV) -> Option<String> {
    use crate::wikitext::token_utils::key_value_to_string;
    let value = key_value_to_string(&kv.value);
    let key = key_value_to_string(&kv.key);
    if key.trim().is_empty() {
        Some(value)
    } else {
        Some(format!("{}={value}", key.trim()))
    }
}

/// Rebuild the `#invoke:` argument text from the resolved colon argument and the
/// token's remaining parameters.
///
/// `{{#invoke:M|f|a|b=c}}` hands the module the text `M|f|a|b=c`; the tokenizer
/// has already split that on `|` into the colon argument (`M`) plus parameters
/// (`f`, `a`, `b=c`), so they are joined back in order.
fn invoke_arg_text(pf_arg: &str, params: &crate::pipeline::parser_functions::Params) -> String {
    let mut parts = vec![pf_arg.trim().to_string()];
    parts.extend(params.args.iter().skip(1).filter_map(kv_to_source_text));
    parts.join("|")
}

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
        let value = key_value_to_string(&kv.value);
        let trimmed = key.trim();
        match (trimmed.parse::<usize>(), trimmed.is_empty()) {
            (Ok(_), false) => out.push(Arg::Positional(value)),
            (_, true) => out.push(Arg::Positional(value)),
            _ => out.push(Arg::Named(trimmed.to_string(), value)),
        }
    }
    out
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
        let mut next_id = 0usize;
        let html = parser.value_to_dom_html(&kv, &mut fragments, &mut next_id);
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
        assert!(html.matches("<p>").count() >= 2, "got: {html}");
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
        assert!(html.contains("<ol><li>"), "got: {html}");
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
}
