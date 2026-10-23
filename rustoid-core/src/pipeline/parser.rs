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
use crate::pipeline::lua_deferred::{ArgSlot, ArgSource};
use crate::pipeline::template_encapsulator::{ParamInfo, TemplateEncapsulator, template_info_from};
use crate::pipeline::template_handler::{PageContext, TemplateHandler, resolve_template_target};
use crate::pipeline::tree_builder_stage::TreeBuilderStage;
use crate::title::TitleParser;
use crate::traits::{DataSource, ProtectionEntry, SiteConfig};
use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};
use crate::wikitext::tokens_v2::{Either, Item, ParsoidToken};

type ResolvedTarget = crate::pipeline::template_handler::ResolvedTarget;

/// Extract the raw template target (the text between `{{` and the first
/// *top-level* `|` or the closing `}}`) from a template token's source string.
///
/// This is the *unprocessed* target as written in the wikitext, before the
/// PHP preprocessor strips comments. Splitting at a bare `|` cut a target that
/// is itself a call — `{{{{#ifeq:…|…}}|x}}` recorded `data-mw.target.wt` as
/// `{{#ifeq:…` — so this delegates to the nesting-aware splitter.
fn raw_template_target(src: &str) -> Option<String> {
    crate::wikitext::tokenizer_v2::raw_template_target(src)
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

/// Split a top-level token stream into TT2 chunks at block boundaries.
///
/// PHP's `PegTokenizer::processChunkily` yields one top-level block per chunk —
/// one line — and `TokenHandlerPipeline::processChunk` runs every transformer
/// over the whole chunk, so the id order resets at each block. The flat token
/// stream carries the boundary on every top-level `Nl`. The leading `Nl` of a
/// chunk stays with it, exactly as `try_parse_sol` consumes it into the chunk it
/// opens.
fn split_into_chunks(tokens: Vec<Item>) -> Vec<Vec<Item>> {
    let mut chunks: Vec<Vec<Item>> = Vec::new();
    let mut chunk: Vec<Item> = Vec::new();
    for item in tokens {
        if !chunk.is_empty() && matches!(&item, Item::Tok(ParsoidToken::Nl(_))) {
            chunks.push(std::mem::take(&mut chunk));
        }
        chunk.push(item);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// How deep the attribute sub-pipeline recurses. Each level expands the
/// templated attributes of what the level above produced; the bound only guards
/// against a pathological value that reproduces itself.
const MAX_ATTR_EXPANSION_DEPTH: u8 = 8;

/// Whether any token in `items` carries a templated (token-array) attribute, so
/// that [`Parser::expand_attributes_at`] would do work over it. Lets
/// [`Parser::expand_attr_tokens`] skip the recursive sub-pipeline in the common
/// case where a value expands to plain strings.
fn has_token_attributes(items: &[Item]) -> bool {
    use crate::wikitext::tokens_v2::KeyValue;
    items.iter().any(|it| {
        let Item::Tok(tok) = it else {
            return false;
        };
        if !matches!(tok, ParsoidToken::Tag(_) | ParsoidToken::SelfclosingTag(_))
            || tok.get_name() == "mw:dom-fragment-token"
        {
            return false;
        }
        tok.get_attribs().iter().any(|kv| {
            matches!(kv.key, KeyValue::Tokens(_)) || matches!(kv.value, KeyValue::Tokens(_))
        })
    })
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
    for_each_token_mut(items, &mut |tok| {
        if let Some(dp) = tok.data_parsoid_mut() {
            dp.tmp.in_text_branch = Some(true);
        }
        strip_token_source_range(tok);
    });
}

/// Visit every token in `items`, including tokens nested in an attribute's key
/// or value.
///
/// A wikilink keeps its text in an attribute, so a tag written inside a link's
/// text (`[[T|<span title="{{…}}">…</span>]]`) is *not* a top-level item of the
/// chunk that produced it. Marking only the top level left such a tag unmarked.
fn for_each_token_mut(items: &mut [Item], f: &mut impl FnMut(&mut ParsoidToken)) {
    for item in items.iter_mut() {
        let Item::Tok(tok) = item else { continue };
        f(tok);
        let Some(attribs) = tok.attribs_mut() else {
            continue;
        };
        for kv in attribs.iter_mut() {
            for field in [&mut kv.key, &mut kv.value] {
                if let crate::wikitext::tokens_v2::KeyValue::Tokens(nested) = field {
                    for_each_token_mut(nested, f);
                }
            }
        }
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
    // A `<templatestyles src=…>` names a page; an explicit namespace prefix is
    // resolved through `namespace_id`, which matches case-insensitively (the
    // prefix in `src="template:sticky header/styles.css"` is lower-cased, and
    // the canonical name is `Template`). Comparing `canonical` directly left
    // `Template:template:sticky header/styles.css` as the title, which does not
    // exist — so the stylesheet was never fetched and the tag leaked into the
    // output unresolved. Without a recognisable prefix the source is a page in
    // the Template namespace, which is the extension's default.
    let (ns_id, name) = match src.split_once(':') {
        Some((head, rest)) => match config.namespace_id(head) {
            Some(id) => (id, rest),
            None => (10, src),
        },
        None => (10, src),
    };
    crate::title::Title::new(ns_id, name.replace('_', " "))
}

/// The `name` attribute of an `extension` token, lower-cased by the tokenizer.
fn extension_name(stt: &crate::wikitext::tokens_v2::SelfclosingTagTk) -> Option<String> {
    stt.attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("name"))
        .and_then(|kv| kv.value.as_str())
        .map(str::to_string)
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

/// Whether this node is an `mw:Extension/*` wrapper that carries its own
/// `about` id. `<nowiki>` is lean markup with no wrapper and no id.
fn is_extension_wrapper(node: &Node) -> bool {
    node.get_attr("typeof").is_some_and(|ty| {
        ty.split_whitespace()
            .any(|t| t.starts_with("mw:Extension/") && t != "mw:Extension/nowiki")
    })
}

/// Whether this node is an extension element in a rendered note body. The bare
/// `mw:Extension` marker is an extension token that has not yet been refined to
/// `mw:Extension/<name>` — a `<ref>` nested in another note is still in that
/// state in the enclosing body's render — and it carries an `about` just the
/// same, so it counts like any other wrapper.
fn is_note_extension(node: &Node) -> bool {
    is_extension_wrapper(node)
        || node
            .get_attr("typeof")
            .is_some_and(|ty| ty.split_whitespace().any(|t| t == "mw:Extension"))
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

/// If `item` is an `<imagemap>` extension token, return the self-closing token.
fn imagemap_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
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
    if name == Some("imagemap") {
        Some(stt)
    } else {
        None
    }
}

/// If `item` is a Kartographer extension token (`<mapframe>` or `<maplink>`),
/// return the self-closing token.
fn kartographer_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
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
    if matches!(name, Some("mapframe") | Some("maplink")) {
        Some(stt)
    } else {
        None
    }
}

/// The extension's tag name from a token that [`kartographer_target`] accepted.
fn kartographer_tag(stt: &crate::wikitext::tokens_v2::SelfclosingTagTk) -> &str {
    stt.attribs
        .iter()
        .find(|a| a.key.as_str() == Some("name"))
        .and_then(|a| a.value.as_str())
        .unwrap_or_default()
}

/// A path into a GeoJSON value, for the two-pass `title`/`description` parse.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GeojsonPath {
    Key(String),
    Index(usize),
}

/// Collect every non-empty `properties.title`/`properties.description` string,
/// mirroring the traversal of `recursivelySanitizeAndParseWikitext` (keys
/// starting with `_` are dropped and never visited).
fn collect_geojson_properties(
    value: &serde_json::Value,
    path: &mut Vec<GeojsonPath>,
    out: &mut Vec<(Vec<GeojsonPath>, String)>,
) {
    match value {
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                path.push(GeojsonPath::Index(index));
                collect_geojson_properties(item, path, out);
                path.pop();
            }
        }
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                if key.starts_with('_') {
                    continue;
                }
                path.push(GeojsonPath::Key(key.clone()));
                collect_geojson_properties(child, path, out);
                path.pop();
            }
            let Some(serde_json::Value::Object(properties)) = object.get("properties") else {
                return;
            };
            for prop in ["title", "description"] {
                match properties.get(prop) {
                    Some(serde_json::Value::String(text)) if !text.is_empty() => {
                        path.push(GeojsonPath::Key("properties".to_string()));
                        path.push(GeojsonPath::Key(prop.to_string()));
                        out.push((path.clone(), text.clone()));
                        path.pop();
                        path.pop();
                    }
                    Some(serde_json::Value::Object(langs)) => {
                        for (lang, text) in langs {
                            let Some(text) = text.as_str() else { continue };
                            if text.is_empty() {
                                continue;
                            }
                            path.push(GeojsonPath::Key("properties".to_string()));
                            path.push(GeojsonPath::Key(prop.to_string()));
                            path.push(GeojsonPath::Key(lang.clone()));
                            out.push((path.clone(), text.to_string()));
                            path.pop();
                            path.pop();
                            path.pop();
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Apply the parsed `title`/`description` HTML, dropping empty and non-string
/// values (and `_`-prefixed keys) exactly as `parseWikitextProperties` does.
fn apply_geojson_properties(
    value: &mut serde_json::Value,
    path: &mut Vec<GeojsonPath>,
    rendered: &std::collections::HashMap<Vec<GeojsonPath>, String>,
) {
    match value {
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                path.push(GeojsonPath::Index(index));
                apply_geojson_properties(item, path, rendered);
                path.pop();
            }
        }
        serde_json::Value::Object(object) => {
            object.retain(|key, _| !key.starts_with('_'));
            let keys: Vec<String> = object.keys().cloned().collect();
            for key in keys {
                path.push(GeojsonPath::Key(key.clone()));
                if let Some(child) = object.get_mut(&key) {
                    apply_geojson_properties(child, path, rendered);
                }
                path.pop();
            }
            let Some(serde_json::Value::Object(properties)) = object.get_mut("properties") else {
                return;
            };
            for prop in ["title", "description"] {
                path.push(GeojsonPath::Key("properties".to_string()));
                path.push(GeojsonPath::Key(prop.to_string()));
                match properties.get(prop).cloned() {
                    Some(serde_json::Value::String(text)) => {
                        if text.is_empty() {
                            properties.remove(prop);
                        } else if let Some(html) = rendered.get(path) {
                            properties
                                .insert(prop.to_string(), serde_json::Value::String(html.clone()));
                        }
                    }
                    Some(serde_json::Value::Object(langs)) => {
                        let mut kept = serde_json::Map::new();
                        for (lang, text) in langs {
                            path.push(GeojsonPath::Key(lang.clone()));
                            if let Some(text) = text.as_str()
                                && !text.is_empty()
                            {
                                let html = rendered
                                    .get(path)
                                    .cloned()
                                    .unwrap_or_else(|| text.to_string());
                                kept.insert(lang, serde_json::Value::String(html));
                            }
                            path.pop();
                        }
                        if kept.is_empty() {
                            properties.remove(prop);
                        } else {
                            properties.insert(prop.to_string(), serde_json::Value::Object(kept));
                        }
                    }
                    Some(_) => {
                        properties.remove(prop);
                    }
                    None => {}
                }
                path.pop();
                path.pop();
            }
        }
        _ => {}
    }
}

/// Parsoid does not store `data-parsoid.src` for a literal HTML tag: its
/// source is rebuilt from the tag name and the attribute shadow data. Rustoid
/// keeps the whole start-tag source (module stringification needs it), so the
/// nested serialization used for the Kartographer group hash must drop it, and
/// record only a non-lowercase spelling as `srcTagName` (as `buildXMLTag`
/// does). Entity spans keep their `&…;` source.
fn strip_literal_tag_src(node: &mut Node) {
    if let Some(dp) = node.dp.as_mut()
        && dp.stx.as_deref() == Some("html")
        && let Some(src) = dp.src.take()
    {
        if let Some(rest) = src.strip_prefix('<') {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .unwrap_or(rest.len());
            let written = &rest[..end];
            let lower = written.to_ascii_lowercase();
            if !written.is_empty() && written != lower {
                dp.src_tag_name = Some(written.to_string());
            }
        } else {
            dp.src = Some(src);
        }
        node.data_parsoid = dp.to_data_parsoid_json();
    }
    // `Env::prepareDoc` sets `serializeNewEmptyDp`, so Parsoid writes an empty
    // `data-parsoid` for every element created during parsing (e.g. the `<a>`
    // of a media description link). The nested serialization has to match.
    if matches!(node.kind, crate::dom::node::NodeKind::Element(_)) {
        if node.data_parsoid.is_none()
            && let Some(dp) = node.dp.as_ref()
        {
            node.data_parsoid = dp.to_data_parsoid_json();
        }
        if node.data_parsoid.is_none() {
            node.data_parsoid = Some("{}".to_string());
        }
    }
    for child in &mut node.children {
        strip_literal_tag_src(child);
    }
}

/// `ctype_alnum` for the ASCII strings the corpus uses.
fn is_ascii_alnum(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Whether `wfEscapeWikiText` would change `input`, i.e. whether
/// `WikitextParser::needsParsing` runs the parser. Any replacement match means
/// the escaped text differs from the input, so this is an exact test rather
/// than an approximation.
fn wf_escape_wiki_text_changes(input: &str, config: &dyn crate::traits::SiteConfig) -> bool {
    if input.len() >= 65536 {
        return true;
    }
    // `wfEscapeWikiText` prepends a newline before applying `strtr`, so a
    // line-start token at offset 0 matches as well.
    let probe = format!("\n{input}");
    const REPLACEMENTS: [&str; 35] = [
        "\"", "&", "'", "<", "=", ">", "[", "]", "{", "|", "}", ";", "!!", "\n!", "\r!", "\n#",
        "\r#", "\n*", "\r*", "\n:", "\r:", "\n ", "\r ", "\n\n", "\r\n", "\n\r", "\r\r", "\n\t",
        "\r\t", "\n----", "\r----", "__", "://", "＿", "~~~",
    ];
    if REPLACEMENTS.iter().any(|needle| probe.contains(needle)) {
        return true;
    }
    for (which, word) in [("RFC", "RFC"), ("PMID", "PMID"), ("ISBN", "ISBN")] {
        if config.magic_link_enabled(which)
            && [" ", "\t", "\r", "\n", "\u{c}"]
                .iter()
                .any(|ws| probe.contains(&format!("{word}{ws}")))
        {
            return true;
        }
    }
    let bytes = input.as_bytes();
    if let Some(first) = bytes.first()
        && matches!(first, b'+' | b'-' | b'_' | b'~')
    {
        return true;
    }
    if let Some(last) = bytes.last()
        && matches!(last, b'_' | b'~' | b'\n' | b'\r' | b'\t')
    {
        return true;
    }
    // Protocols without `://` (`mailto:`, `tel:`, `urn:`, …) are escaped by a
    // `\b(proto):` regexp.
    const COLON_PROTOCOLS: [&str; 12] = [
        "bitcoin", "geo", "magnet", "mailto", "matrix", "news", "sip", "sips", "sms", "tel", "urn",
        "xmpp",
    ];
    let lower = input.to_ascii_lowercase();
    for proto in COLON_PROTOCOLS {
        let needle = format!("{proto}:");
        let mut from = 0usize;
        while let Some(found) = lower[from..].find(&needle) {
            let at = from + found;
            let boundary = at == 0
                || !lower.as_bytes()[at - 1].is_ascii_alphanumeric()
                    && lower.as_bytes()[at - 1] != b'_';
            if boundary {
                return true;
            }
            from = at + 1;
        }
    }
    false
}

/// If `item` is a `<phonos>` extension token, return the self-closing token.
fn phonos_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
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
    if name == Some("phonos") {
        Some(stt)
    } else {
        None
    }
}

/// Name of the extension a self-closing `extension` token carries, if it is one.
fn extension_token_name(stt: &crate::wikitext::tokens_v2::SelfclosingTagTk) -> Option<&str> {
    if stt.name != "extension" {
        return None;
    }
    stt.attribs
        .iter()
        .find(|a| a.key.as_str() == Some("name"))
        .and_then(|a| a.value.as_str())
}

/// If `item` is a `<syntaxhighlight>` extension token, return the self-closing token.
fn syntaxhighlight_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    (extension_token_name(stt) == Some("syntaxhighlight")).then_some(stt)
}

/// If `item` is a `<templatedata>` extension token, return the self-closing token.
fn templatedata_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    (extension_token_name(stt) == Some("templatedata")).then_some(stt)
}

/// If `item` is an `<inputbox>` extension token, return the self-closing token.
fn inputbox_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    (extension_token_name(stt) == Some("inputbox")).then_some(stt)
}

/// The temporary attribute a template body's `<templatedata>` token carries the
/// frame's title in, so [`Parser::build_templatedata`] can name the frame the tag
/// sits in (`$frame->getTitle()` — the `/doc` subpage of the documented template).
const TEMPLATEDATA_FRAME_TITLE_ATTR: &str = "data-mw-frame-title";

/// Stamp [`TEMPLATEDATA_FRAME_TITLE_ATTR`] onto any `<templatedata>` extension
/// token in a template body. Called only for a body whose source contains the
/// tag, so the walk costs nothing for ordinary templates.
fn stamp_templatedata_frame_title(items: Vec<Item>, title: &str) -> Vec<Item> {
    items
        .into_iter()
        .map(|item| {
            let Item::Tok(ParsoidToken::SelfclosingTag(mut t)) = item else {
                return item;
            };
            if extension_token_name(&t) == Some("templatedata") {
                t.add_attribute_str(TEMPLATEDATA_FRAME_TITLE_ATTR, title);
            }
            Item::Tok(ParsoidToken::SelfclosingTag(t))
        })
        .collect()
}

/// The first element named `name` in `node`'s subtree.
fn find_element<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    if matches!(&node.kind, crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::Other(t)) if t == name)
    {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_element(c, name))
}

/// Whether `node` is an anchor — a literal `<a>`, or the DOM node a wikilink or
/// external link is stored as (both serialize to `<a>`; see
/// `HtmlSerializer::element_tag`).
fn is_anchor(node: &Node) -> bool {
    use crate::dom::node::{ElementKind, NodeKind};
    match &node.kind {
        NodeKind::Element(ElementKind::Wikilink | ElementKind::ExtLink) => true,
        NodeKind::Element(ElementKind::Other(t)) => t == "a",
        _ => false,
    }
}

/// The first anchor in `node`'s subtree.
fn find_anchor(node: &Node) -> Option<&Node> {
    if is_anchor(node) {
        return Some(node);
    }
    node.children.iter().find_map(find_anchor)
}

/// The first element named `name` in `node`'s subtree, mutably.
fn find_element_mut<'a>(node: &'a mut Node, name: &str) -> Option<&'a mut Node> {
    if matches!(&node.kind, crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::Other(t)) if t == name)
    {
        return Some(node);
    }
    node.children
        .iter_mut()
        .find_map(|c| find_element_mut(c, name))
}

/// Whether `node` is an element named `name`.
fn is_named(node: &Node, name: &str) -> bool {
    matches!(&node.kind, crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::Other(t)) if t == name)
}

/// Remove and return the first element named `name` in `node`'s subtree.
fn take_element(node: &mut Node, name: &str) -> Option<Node> {
    if let Some(i) = node.children.iter().position(|c| is_named(c, name)) {
        return Some(node.children.remove(i));
    }
    node.children.iter_mut().find_map(|c| take_element(c, name))
}

/// Remove the first element named `name` in `node`'s subtree, if any.
fn remove_element(node: &mut Node, name: &str) {
    let _ = take_element(node, name);
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

/// Whether a token-list holds a template, either directly or nested inside a
/// token-array attribute of one of its tokens.
///
/// A template call's target is tokenized as a token list, and a directive inside
/// it binds to whatever token surrounds it — `{{urlencode:[[User:{{T}}]]}}` puts
/// the `{{T}}` in the `wikilink`'s `href` token array. PHP's `Frame::expand`
/// walks the whole tree and expands it; rustoid's target pass only reaches the
/// list's own tokens, so this is the guard that says the attribute pass is
/// needed ([`Parser::expand_target_templates`]).
fn target_holds_template(items: &[Item]) -> bool {
    items
        .iter()
        .any(|it| is_template_item(it) || item_holds_templated_attribute(it))
}

/// Whether a token carries a token-array attribute that holds a template (see
/// [`target_holds_template`]).
fn item_holds_templated_attribute(item: &Item) -> bool {
    use crate::wikitext::tokens_v2::KeyValue;
    let Item::Tok(tok) = item else {
        return false;
    };
    if !matches!(tok, ParsoidToken::Tag(_) | ParsoidToken::SelfclosingTag(_)) {
        return false;
    }
    tok.get_attribs().iter().any(|kv| {
        [&kv.key, &kv.value].into_iter().any(|field| match field {
            KeyValue::Tokens(toks) => target_holds_template(toks),
            _ => false,
        })
    })
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

/// Split a `|` that arrived in a wikilink target *after* expansion.
///
/// Core's preprocessor substitutes template arguments before the parser
/// tokenizes, so `[[{{{1}}}]]` with `{{{1}}} = A{{!}}B` is `[[A|B]]`: the pipe
/// ends the target and starts the caption. rustoid tokenizes the template
/// argument, so the pipe only appears once the value is spliced into the
/// `href`. Move everything after the first top-level pipe into the link text.
fn split_expanded_link_href(stt: &mut crate::wikitext::tokens_v2::SelfclosingTagTk) {
    use crate::wikitext::tokens_v2::KeyValue;

    let Some(href_kv) = stt
        .attribs
        .iter_mut()
        .find(|kv| kv.key.as_str() == Some("href"))
    else {
        return;
    };
    let KeyValue::Tokens(items) = &mut href_kv.value else {
        return;
    };
    let Some((i, pipe)) = items.iter().enumerate().find_map(|(i, it)| match it {
        Item::Str(s) => s.find('|').map(|p| (i, p)),
        _ => None,
    }) else {
        return;
    };

    let mut rest: Vec<Item> = Vec::new();
    if let Item::Str(s) = &items[i] {
        if pipe + 1 < s.len() {
            rest.push(Item::Str(s[pipe + 1..].to_string()));
        }
        items[i] = Item::Str(s[..pipe].to_string());
    }
    rest.extend(items.drain(i + 1..));
    if rest.is_empty() {
        return;
    }

    // The caption comes before any content the source already had (the pipe we
    // split is earlier in the source).
    match stt
        .attribs
        .iter_mut()
        .find(|kv| kv.key.as_str() == Some("mw:maybeContent"))
    {
        Some(kv) => {
            let existing = std::mem::replace(&mut kv.value, KeyValue::Tokens(Vec::new()));
            rest.extend(match existing {
                KeyValue::Str(s) => vec![Item::Str(s)],
                KeyValue::Tokens(t) => t,
            });
            kv.value = KeyValue::Tokens(rest);
        }
        None => stt.attribs.push(crate::wikitext::tokens_v2::KV {
            key: KeyValue::Str("mw:maybeContent".to_string()),
            value: KeyValue::Tokens(rest),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        }),
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

/// Whether a token chunk holds an unexpanded `extension` self-closing token.
///
/// Used to tell an `extension` a value already carried (a literal `<nowiki>` in
/// the source) from one an expansion produced (`{{#tag:nowiki|…}}`) — see
/// [`Parser::expand_attr_tokens`].
fn chunk_has_extension_token(items: &[Item]) -> bool {
    items
        .iter()
        .any(|i| matches!(i, Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "extension"))
}

/// Whether a `wikilink` token's `href` names the File (or Media) namespace, the
/// targets whose rendering can allocate an `about` id.
fn is_file_link_target(href: &str) -> bool {
    let href = href.trim_start_matches("./");
    let ns = href.split(':').next().unwrap_or("");
    ns.eq_ignore_ascii_case("file")
        || ns.eq_ignore_ascii_case("image")
        || ns.eq_ignore_ascii_case("media")
}

/// The name a bare `mw:dom-fragment-token`'s strip marker carries.
///
/// The preprocessor hands a module a marker named after the *extension tag* it
/// stands for, and modules match on it: `Module:Coordinates.coordinsert` looks
/// for `UNIQ--indicator-…-QINU` to replace the indicator a coord call emitted.
/// An indicator's placeholder records that name in its `typeof`
/// (`mw:Extension/indicator`), because the fragment *is* the extension's output.
/// A tunnelled fragment with no type of its own is a gallery, which is the name
/// its marker has always carried.
fn fragment_marker_name(t: &crate::wikitext::tokens_v2::SelfclosingTagTk) -> String {
    t.attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("typeof"))
        .and_then(|kv| kv.value.as_str())
        .and_then(|ty| {
            ty.split_whitespace()
                .find_map(|part| part.strip_prefix("mw:Extension/"))
        })
        .map(str::to_string)
        .unwrap_or_else(|| "gallery".to_string())
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
            Some((i + 1, vec![items[i].clone()], fragment_marker_name(t)))
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

/// The tag name of a stashed `extension` token — a `<ref>`, `<nowiki>`, a
/// `#tag:…` — which the answer renderers carry on its own.
///
/// The preprocessor replaces every extension tag with a strip marker, so a module
/// is handed `<UNIQ--ref-…-QINU>` and never the tag's source. Rendering the
/// source instead leaked the tag's body into the module's view — a comma in a
/// ref's note made `Module:Detect singular` call a singular label plural. The
/// marker keeps the token, which `substitute_strip_markers` splices back.
fn stashed_extension_tag(item: &Item) -> Option<String> {
    let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item else {
        return None;
    };
    if t.name != "extension" {
        return None;
    }
    Some(extension_name(t).unwrap_or_else(|| "extension".to_string()))
}

/// The span a marker-carrying renderer must carry as one strip marker starting at
/// `items[i]`: the exclusive end index, the items, and the marker's name.
///
/// A `mw:DOMFragment` placeholder (see [`placeholder_span`]) and a stashed
/// extension token are carried; everything else the answer renderer writes itself
/// or drops, under its own policy.
fn answer_marker_span(items: &[Item], i: usize) -> Option<(usize, Vec<Item>, String)> {
    if let Some(span) = placeholder_span(items, i) {
        return Some(span);
    }
    stashed_extension_tag(&items[i]).map(|tag| (i + 1, vec![items[i].clone()], tag))
}

/// Emit the `mw:dom-fragment-token` placeholder for an extension whose output is
/// a pre-built DOM fragment (`<gallery>`, `<imagemap>`), referencing the stashed
/// fragment by `id` (mirrors the generic extension encapsulation in
/// `extension_handler::gallery_items`).
fn emit_dom_fragment_placeholder(
    stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    id: usize,
) -> Item {
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
    // The extension's `about` id is added by the chunk post-pass (see
    // `Parser::number_indicator_placeholder`), which the tree builder carries
    // onto the element it inserts, between `typeof` and `data-mw`.
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
    context_title: Option<&crate::title::Title>,
) -> Node {
    use crate::pipeline::external_link_handler::{on_ext_link, on_url_link};
    use crate::pipeline::wiki_link_render::{
        WikiLinkContext, get_wiki_link_target_info_for_token, render_wiki_link_dispatched,
    };
    use crate::wikitext::token_utils::key_value_to_string;

    // Step 1: render wikilinks (`[[…]]` → `<a>`/`<link>` tags).
    let mut link_ctx = WikiLinkContext::new(config);
    if let Some(title) = context_title {
        link_ctx.set_context_title(title);
    }
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
            let target = match get_wiki_link_target_info_for_token(
                &link_ctx,
                &ParsoidToken::SelfclosingTag(stt.clone()),
                &href,
                &href_src,
            ) {
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
                &mut |items, f, id| render_inline_fragment(config, items, f, id, context_title),
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
                    let frag =
                        render_inline_fragment(config, items, fragments, next_id, context_title);
                    crate::pipeline::wiki_link_render::dom_fragment_token(
                        frag, &token, fragments, next_id,
                    )
                };
                match on_ext_link(
                    &token,
                    clean,
                    |proto| config.has_valid_protocol(proto),
                    config.relative_link_prefix(),
                    render_content,
                ) {
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
    // Move expandable media-option fragments onto their containers before
    // `post_pwrap_transforms` unpacks placeholders into view. A gallery renders
    // each line through this fragment builder, so its media must be collected
    // here rather than only in `build_ast`.
    crate::pipeline::attr_mw_fragments::collect(&mut frag);
    let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&frag);
    crate::pipeline::tree_builder_html::post_pwrap_transforms(&mut frag, &depths, None, None);
    // AddLinkAttributes also runs in the nested fragment pipeline (mirrors PHP's
    // `NESTED_PIPELINE_DOM_TRANSFORMS`, where `linkclasses` precedes
    // `linkneighbours+dom-unpack`): it assigns `class`/`rel` to links inside
    // media/gallery captions, which would otherwise only get the bare `rel`
    // set by the token handler.
    crate::pipeline::add_link_attributes::run(&mut frag, config);
    frag
}

/// Re-tokenize an expanded text-returning parser function's branch.
///
/// `#if`/`#ifeq`/`#ifexpr`/`#iferror` answer a *string* in core and the parser
/// re-tokenizes it, so `{{!}}` or `|-` in a branch becomes real table syntax
/// (`Template:Taxobox/core`'s image row is `{{!}} colspan=2 … {{!}} … {{!}}-`).
/// rustoid expands the branch in place, leaving those as text; when every item has
/// a plain textual form and a line begins table syntax, re-tokenize so the
/// construct is recognized. A branch holding any other token (a link, a tag) is
/// left alone.
/// Which line shapes trigger a branch re-tokenization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchSyntax {
    /// `{|`, `|`, `!` — a branch returned as *text* (`#if`, `#ifeq`, …), where a
    /// leading `|`/`!` line can be a cell of an enclosing table.
    Cells,
    /// `{|` only — a branch that keeps its tokens (`#switch`): only a real table
    /// start needs re-tokenizing, so the branch's own nodes stay as they are.
    TableOnly,
}

impl BranchSyntax {
    fn triggers(self, line: &str) -> bool {
        let t = line.trim_start_matches(' ');
        t.starts_with("{|") || (self == Self::Cells && (t.starts_with('|') || t.starts_with('!')))
    }
}

/// Re-tokenize a parser function's branch so block syntax it carries is parsed.
///
/// MediaWiki's preprocessor is *textual*: a branch is spliced into the source and
/// re-tokenized, so a line-start `{|` starts a table and the `|-`/`!`/`|` lines
/// after it become cells. rustoid's branch is already tokenized as an argument
/// value (inline), so this pass puts the text back through the tokenizer.
///
/// The branch content sits between the `mw:Transclusion` marker metas that wrap
/// the parser-function result; those are kept so the tree builder still wraps it.
fn re_tokenize_text_branch(
    items: Vec<Item>,
    ext_tags: &[String],
    syntax: BranchSyntax,
    table_data_block: bool,
    // Whether the branch's text starts at a line start in the enclosing source
    // (see [`branch_starts_line`]).
    starts_line: bool,
    // Re-tokenize even when no block-syntax trigger matches. `#property` returns
    // escaped plain text, which can be a bare URL; the tokenizer is what turns
    // that into an external link.
    force: bool,
) -> Vec<Item> {
    re_tokenize_text_branch_inner(
        items,
        ext_tags,
        syntax,
        table_data_block,
        starts_line,
        force,
    )
}

/// Whether a parser-function call sits at the start of a source line.
///
/// Core's preprocessor splices the returned branch into the source at the
/// call's position, so this is the `sol` the branch's first line has. Everything
/// since the last newline must be whitespace (the tokenizer consumes leading
/// spaces/comments while staying at SOL).
fn branch_starts_line(tok: &ParsoidToken, src_text: &str) -> bool {
    let Some(tsr) = tok.data_parsoid().and_then(|dp| dp.tsr.as_ref()) else {
        return false;
    };
    let Some(start) = tsr.start else {
        return false;
    };
    let source = tsr.source.as_deref().unwrap_or(src_text);
    let Some(prefix) = source.get(..start) else {
        return false;
    };
    let line = prefix.rsplit_once('\n').map_or(prefix, |(_, rest)| rest);
    // PCRE `\s` also matches the vertical tab, which Rust's ASCII helper omits.
    line.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\u{000B}')
        .is_empty()
}

fn re_tokenize_text_branch_inner(
    items: Vec<Item>,
    ext_tags: &[String],
    syntax: BranchSyntax,
    table_data_block: bool,
    starts_line: bool,
    force: bool,
) -> Vec<Item> {
    fn is_wrapper(it: &Item) -> bool {
        matches!(
            it,
            Item::Tok(ParsoidToken::SelfclosingTag(t))
                if t.name == "meta"
                    && t.attribs.iter().any(|kv| {
                        kv.key.as_str() == Some("typeof")
                            && kv.value.as_str().is_some_and(|v| v.starts_with("mw:Transclusion"))
                    })
        )
    }
    fn tokenize(text: &str, ext_tags: &[String], sol: bool, table_data_block: bool) -> Vec<Item> {
        crate::pipeline::template_handler::tokenize_branch_fragment(
            text,
            ext_tags,
            sol,
            table_data_block,
        )
    }
    /// Re-tokenize a text run. `#if`/`#ifeq`/`#iferror` answer a *string* in
    /// core, so every text run of their branch was parsed as wikitext; a run is
    /// therefore always re-tokenized (`Cells`). A `#switch` branch keeps its
    /// tokens, so only a block start needs it (`TableOnly`). `at_line_start`
    /// tells the tokenizer whether the run's first line begins a source line.
    fn flush(
        out: &mut Vec<Item>,
        run: &mut Vec<Item>,
        at_line_start: bool,
        syntax: BranchSyntax,
        ext_tags: &[String],
        force: bool,
        table_data_block: bool,
    ) {
        if run.is_empty() {
            return;
        }
        let mut text = String::new();
        for it in run.iter() {
            match it {
                Item::Str(s) => text.push_str(s),
                Item::Tok(ParsoidToken::Nl(_)) => text.push('\n'),
                _ => {
                    out.append(run);
                    return;
                }
            }
        }
        let mut lines = text.lines();
        if !(at_line_start || text.starts_with('\n')) {
            lines.next();
        }
        let line_trigger = lines.any(|line| syntax.triggers(line));
        let retokenize = force
            || match syntax {
                // Core answers the branch as a string and re-parses all of it, so
                // markup that was assembled across a template boundary (a `<span`
                // whose `>` lives in another expansion) and entities have to be
                // tokenized again as well as block syntax.
                BranchSyntax::Cells => line_trigger || text.contains('<') || text.contains('&'),
                BranchSyntax::TableOnly => line_trigger,
            };
        if retokenize {
            out.extend(crate::pipeline::template_handler::tokenize_branch_fragment(
                &text,
                ext_tags,
                at_line_start,
                table_data_block,
            ));
            run.clear();
        } else {
            out.append(run);
        }
    }

    let start = items
        .iter()
        .position(|it| !is_wrapper(it))
        .unwrap_or(items.len());
    let end = items
        .iter()
        .rposition(|it| !is_wrapper(it))
        .map(|i| i + 1)
        .unwrap_or(start);
    let content = &items[start..end];

    // A branch that is only text is re-tokenized as a whole when a line begins
    // block syntax. Re-tokenizing *every* text branch was wrong: core splices
    // the branch string into the surrounding wikitext and parses it there, so a
    // branch like `#b0c4de` inside an attribute value is text, and the T2529
    // newline is inserted in context. Tokenizing it here, at SOL, made a
    // `listItem` out of it and suppressed that newline (`Template:Infobox
    // football biography`'s header style lost it).
    let all_text = content
        .iter()
        .all(|it| matches!(it, Item::Str(_) | Item::Tok(ParsoidToken::Nl(_))));
    if all_text {
        let mut text = String::new();
        for it in content {
            match it {
                Item::Str(s) => text.push_str(s),
                Item::Tok(ParsoidToken::Nl(_)) => text.push('\n'),
                _ => unreachable!("checked above"),
            }
        }
        if !force && !text.lines().any(|line| syntax.triggers(line)) {
            return items;
        }
        let mut out: Vec<Item> = items[..start].to_vec();
        out.extend(tokenize(&text, ext_tags, starts_line, table_data_block));
        out.extend_from_slice(&items[end..]);
        return out;
    }

    // Mixed content: a branch can return markup tokens *and* raw table lines —
    // `Template:Largest cities`'s branch is a `<div>` followed by the `{|`/`|-`/
    // `!` lines that `{{(!}}`/`{{!}}` expand to. The tokens stay where they are;
    // a table-start token produced from `{|` is put back into the text stream (a
    // transclusion marker is not source text, so it is dropped), and each text
    // run that carries a table start is re-tokenized on its own.
    let mut out: Vec<Item> = items[..start].to_vec();
    let mut run: Vec<Item> = Vec::new();
    // The branch is spliced where the parser-function call sits, so its first
    // text run has that position's start-of-line state.
    let mut run_starts_line = starts_line;
    // A `{{!}}` marker was folded into the run as `|`; the run must be
    // re-tokenized even if no other trigger matches.
    let mut run_has_bang = false;
    let mut i = start;
    while i < end {
        match &items[i] {
            Item::Str(_) | Item::Tok(ParsoidToken::Nl(_)) => {
                run.push(items[i].clone());
                i += 1;
            }
            it if is_wrapper(it) => {
                i += 1;
            }
            Item::Tok(ParsoidToken::Tag(t))
                if t.data_parsoid.start_tag_src.as_deref() == Some("{|")
                    && t.attribs.is_empty() =>
            {
                run.push(Item::Str("{|".to_string()));
                i += 1;
            }
            // In core's textual pipeline a cell opener inside a text-returning
            // branch is literal text in the branch *string* (`{{!}}` is `|`, and
            // a template body of `||` is `||`), so it belongs in the text stream.
            // Keeping it as a `<td>` token made the attribute box before it
            // unreachable: `|| style="…" | [[Ontario]]` tokenized the cell
            // boundary but left `style="…"` as cell content. `#switch` keeps its
            // tokens, so this applies to `Cells` only.
            Item::Tok(ParsoidToken::Tag(t))
                if syntax == BranchSyntax::Cells && cell_opener_source(t).is_some() =>
            {
                run.push(Item::Str(
                    cell_opener_source(t).unwrap_or_else(|| "|".to_string()),
                ));
                run_has_bang = true;
                i += 1;
            }
            _ => {
                flush(
                    &mut out,
                    &mut run,
                    run_starts_line,
                    syntax,
                    ext_tags,
                    run_has_bang || force,
                    table_data_block,
                );
                run_has_bang = false;
                run_starts_line = false;
                out.push(items[i].clone());
                i += 1;
            }
        }
    }
    flush(
        &mut out,
        &mut run,
        run_starts_line,
        syntax,
        ext_tags,
        run_has_bang || force,
        table_data_block,
    );
    out.extend_from_slice(&items[end..]);
    out
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
        // A nowiki span that anchors a transclusion range (`about="#mwt…"`) is
        // the range's wrapper, not a DOM-fragment placeholder: PHP's
        // `finalCleanup` keeps it even when it is empty (an `<nowiki/>` at the
        // end of a template, as in `Template:As of`). Only an unencapsulated
        // wrapper is flattened away.
        let encapsulated_root = child
            .get_attr("about")
            .is_some_and(|a| a.starts_with("#mwt"));
        if is_nowiki_span(&child) && !encapsulated_root {
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

/// A resolved `<templatestyles>` whose `about` id is not assigned yet.
///
/// Held between the walk that sees the tag and the post-pass that numbers it:
/// Parsoid's TT2 numbers a chunk's extensions only after its templates, so the
/// id cannot be taken where the tag is encountered. See
/// [`Parser::number_style_placeholders`].
struct PendingStyle {
    css: String,
    revid: u64,
    src: String,
    /// The `wrapper` attribute, if the tag carried one: it scopes the CSS under
    /// the wrapper and joins the stylesheet's dedup key. See
    /// [`crate::pipeline::templatestyles::render`] and `style_node`.
    wrapper: Option<String>,
    /// Whether the extension tag had content, which the `<style>`'s `data-mw`
    /// records as a `body` field. See [`Parser::expand_one_templatestyles`].
    has_body: bool,
}

/// Counts one nesting level of `#invoke` argument expansion.
///
/// A guard so every return path — including an error unwinding out of an
/// `await` — decrements it. See [`Parser::arg_expansion`].
struct ArgExpansion<'a> {
    arg: &'a std::cell::Cell<u32>,
    style: &'a std::cell::Cell<u32>,
}

impl Drop for ArgExpansion<'_> {
    fn drop(&mut self) {
        self.arg.set(self.arg.get().saturating_sub(1));
        self.style.set(self.style.get().saturating_sub(1));
    }
}

/// RAII guard for [`Parser::attr_expansion`]: constructing it increments the
/// counter, dropping it decrements it.
struct AttrExpansion<'a>(&'a std::cell::Cell<u32>);

impl Drop for AttrExpansion<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// RAII guard for [`Parser::style_defer`]: constructing it increments the
/// counter, dropping it decrements it.
struct StyleDefer<'a>(&'a std::cell::Cell<u32>);

impl Drop for StyleDefer<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// RAII guard for [`Parser::expansion_nesting`]: constructing it increments the
/// counter, dropping it decrements it. See [`Parser::expand_templates`].
struct ExpansionNesting<'a>(&'a std::cell::Cell<u32>);

impl Drop for ExpansionNesting<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// RAII guard for [`Parser::wrap_templates`]: it saves the flag, sets it false,
/// and restores it on drop. See [`Parser::expand_unwrapped`].
struct UnwrappedTemplates<'a> {
    flag: &'a std::cell::Cell<bool>,
    saved: bool,
}

impl Drop for UnwrappedTemplates<'_> {
    fn drop(&mut self) {
        self.flag.set(self.saved);
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
    /// OOUI's `ooui-php-N` id counter, one per parse. OOUI keeps a process
    /// global that resets each request; rustoid parses one page per `Parser`,
    /// so the counter lives here. Shared by every widget an extension emits on
    /// the page, in document order.
    ooui: crate::pipeline::ooui::OouiCounter,
    /// Title of the page being parsed, recorded by `build_ast`.
    ///
    /// A `#invoke` inside a template needs the *root* page title — a module
    /// asking `getEntityIdForCurrentPage` means the article, while the frame it
    /// was called from is a template. Threading the title through every
    /// expansion call would touch a dozen signatures to serve one caller, so it
    /// is recorded once per parse like the expansion depth above.
    page_title: std::cell::RefCell<String>,
    /// Wikitext of the page being parsed, recorded by `build_ast`.
    ///
    /// Distinct from the `src_text` threaded through expansion: that is the
    /// source of the *chunk* currently being expanded — a template body, a
    /// module output — while `mw.title.getCurrentTitle():getContent()` must
    /// always answer with the *page* (`Module:Citation/CS1/Configuration` reads
    /// it to detect `{{Use dmy dates}}`). Living here for the same reason as
    /// `page_title`: one reader, reached from several expansion paths.
    page_source: std::cell::RefCell<String>,
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
    /// Answers for `#property`, keyed by `(property argument, from)`.
    ///
    /// `#property` reads Wikidata, which is a fetch, while the parser-function
    /// call is synchronous: [`Parser::prime_property`] resolves the answer just
    /// before the call and [`TemplateHandler`] reads it back here.
    property_values: std::cell::RefCell<std::collections::HashMap<(String, String), String>>,
    /// The page's Wikidata item id. Resolved at most once per render, because
    /// every `#property` call without `from` needs the same answer.
    page_entity: std::cell::RefCell<Option<Option<String>>>,
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
    /// The revision id of the page being parsed, when the data source knows one.
    /// `{{REVISIONID}}` answers from it; see [`PageContext`].
    page_revision: std::cell::Cell<Option<u64>>,
    /// Sub-fragments built during expansion, waiting for the tree builder.
    ///
    /// A `<templatestyles>` is resolved while expansion is running — the CSS
    /// lives on a wiki page, so it cannot be fetched by a later synchronous pass
    /// — and the `<style>` element is stashed here, reached from the token
    /// stream through an `mw:DOMFragment` placeholder.
    ///
    /// Its `about` id is *not* taken where the tag was resolved: Parsoid's TT2
    /// runs `TemplateHandler` over the whole chunk and only then
    /// `ExtensionHandler`, so the chunk's transclusions take their ids first and
    /// its extensions follow, in document order. The id is therefore assigned by
    /// [`Parser::number_style_placeholders`], at the end of the chunk the
    /// placeholder ends up in — which for a value that reaches Lua and comes
    /// back is the module's output chunk, not the argument.
    ///
    /// Held on the parser rather than threaded through [`Parser::expand_templates`]
    /// and its twelve call sites for the reason the other `Cell` fields here are:
    /// one consumer, reached from several paths.
    ext_fragments: std::cell::RefCell<std::collections::HashMap<usize, Node>>,
    ext_next_id: std::cell::Cell<usize>,
    /// Resolved stylesheets by fragment id, awaiting an `about` id from the
    /// chunk's extension pass. Drained by
    /// [`Parser::number_style_placeholders`]; an entry whose placeholder never
    /// reached a token stream (an argument rendered back to source) is simply
    /// never drained, and spends no id.
    pending_styles: std::cell::RefCell<std::collections::HashMap<usize, PendingStyle>>,
    /// Non-zero while an `#invoke` argument is being expanded.
    ///
    /// Scribunto's `frame.args` hands a module the argument's *text*, and the
    /// templates in it are expanded by the preprocessor — which numbers no
    /// extensions. A `<templatestyles>` or `<ref>` inside an argument is
    /// numbered only where the module's **output** puts it, so numbering it
    /// here spends ids the service never spends (three on `Nobel Prize`).
    /// Resolving is left alone: the rendered text is the argument's source
    /// fallback either way.
    ///
    /// This also implies [`Self::style_defer`]: an `#invoke` argument defers
    /// *and* re-creates its extensions, so both the id and the indicator spend
    /// move to the output.
    arg_expansion: std::cell::Cell<u32>,
    /// Non-zero while a *template's* argument values are being expanded.
    ///
    /// The value is spliced into the callee's body rather than rendered back to
    /// text, so its extensions must still be numbered — but at the position the
    /// value lands in the output, not at expansion time. Numbering at expansion
    /// time put a value's stylesheets before the extensions the *callee* emits,
    /// which are earlier in the output: `Template:Infobox military conflict`'s
    /// args carry `{{multiple image}}`, so Multiple image's stylesheet took
    /// `#mwt12` and the infobox's own `#mwt25`, where the service has the
    /// infobox's first. Unlike [`Self::arg_expansion`] this does **not** defer
    /// the indicator spend — a template argument's `<indicator>` is numbered
    /// with its transclusion, as the service does.
    ///
    /// A Lua `frame:expandTemplate`/`preprocess` answer sets it too: that answer
    /// is wikitext the caller re-parses, so the same "numbered where it lands"
    /// rule applies (`frame:callParserFunction` does not — `#tag` runs its
    /// extension and is numbered at the call; see
    /// [`Parser::expand_lua_request`]).
    style_defer: std::cell::Cell<u32>,
    /// Non-zero while an attribute value is being expanded.
    ///
    /// T2529 inserts a newline into the *text* of an expansion, so inside an
    /// attribute value that newline is ordinary text (`style="…\n;…"`). A
    /// `Nl` token would be dropped when the value is stringified, since
    /// Parsoid's `tokensToString` discards newline tokens. See
    /// [`t2529_force_line_start`].
    attr_expansion: std::cell::Cell<u32>,
    /// How many [`Parser::expand_templates`] calls are on the stack. The
    /// page-level call is depth 1; anything deeper is being spliced into another
    /// expansion, i.e. the item is inside a transclusion. See
    /// [`Parser::number_extension_token`]: the live Cite extension spends an
    /// additional (discarded) `about` id for every `<ref>` that is rendered from
    /// inside a transclusion, which a standalone Parsoid does not reproduce.
    expansion_nesting: std::cell::Cell<u32>,
    /// Nonzero while a module's *output* (or its arguments) is being expanded.
    ///
    /// Production Parsoid expands templates through MediaWiki's legacy
    /// preprocessor, whose `TemplateHandler` passes `ignoreLoop => true`
    /// (`TemplateHandler.php`), so Parsoid's own title-chain loop check is
    /// bypassed there and recursion is bounded only by depth. rustoid's check is
    /// the only one it has, so it must stand down inside a module — otherwise a
    /// recursive template like `Template:Clade` (whose `Module:Clade` echoes
    /// nested `{{clade}}` calls into its output) reads a `Template:Clade`
    /// ancestor and is stopped. See [`Parser::expand_one_template`].
    module_scope: std::cell::Cell<u32>,
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
    /// `<ref>` bodies expanded at the ref site, keyed by the `about` id the
    /// chunk's extension pass assigned to the use that supplied the body.
    ///
    /// A `<ref>`'s content is wikitext that PHP expands while it handles the
    /// extension token (`RefTagHandler::sourceToDom` → `extTagToDOM`), i.e. in
    /// TT2 after the chunk's templates and before the marker's own `about` id is
    /// allocated (`ExtensionHandler::onDocumentFragment`). Cite's DOM pass then
    /// *moves* the already-rendered nodes; it never re-parses the body. rustoid
    /// keeps Cite as a late DOM pass, so it has to hand the expanded body over:
    /// [`Parser::expand_templates`] renders it in that same position and stashes
    /// it here for the list renderer to pick up.
    ref_bodies: std::cell::RefCell<std::collections::HashMap<String, Node>>,
    /// The same pre-expanded `ref` bodies, keyed by their wikitext. A `#tag:ref`
    /// whose ref token is wrapped in the `{{#tag:ref}}` transclusion has its
    /// `about` overwritten by the encapsulation (the transclusion's id, not the
    /// extension's), so the `about` key the body was stashed under is not the one
    /// Cite later reads from the DOM node. The body text is what Cite itself
    /// identifies a note by, so it is the stable key.
    ref_bodies_by_body: std::cell::RefCell<std::collections::HashMap<String, Node>>,
    /// The pre-expanded *bodies* of `<references>` elements, keyed by their
    /// wikitext. Those bodies hold the **list-defined** references
    /// (`{{reflist|refs=<ref name="A">…</ref>}}`), which Parsoid parses into the
    /// element's children (`ReferenceListTagHandler::sourceToDom`) and Cite then
    /// collects at the element's position — an ordinary `<ref>` there, whose body
    /// it renders like any other. rustoid keeps the element childless, so the
    /// definitions are read from here by `cite::collect`; without them a note
    /// defined this way renders empty (and its body's extension ids are never
    /// spent).
    references_bodies: std::cell::RefCell<std::collections::HashMap<String, Node>>,
    /// The `<ref>` bodies that may hold a nested `<ref>`: those whose pipeline ran
    /// with `inTemplate` (`RefTagHandler::sourceToDom`'s `allowNestedRef =
    /// $extApi->inTemplate()`). A literal `<ref>` at the page's own level is parsed
    /// without it, so Parsoid drops a nested one; rustoid's body render keeps it as
    /// an unrefined `mw:Extension` marker either way, so the state cannot be read
    /// back off the DOM and is recorded here (by body text) for Cite's nested-ref
    /// walk. See [`Parser::number_extension_token`] and `cite::collect`.
    nested_ref_bodies: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Whether the current chunk's template expansions are encapsulated in
    /// `mw:Transclusion` markers. PHP derives that from its pipeline option
    /// `wrapTemplates`, and it is false for a *fresh* extension-body pipeline
    /// such as a `<ref>`'s: the note body is parsed by Parsoid itself (with
    /// `expandTemplates` on), so every top-level `{{…}}` in it still reaches
    /// `TemplateHandler::onTemplate` and spends an `about` id through the
    /// `TemplateEncapsulator` constructor — but `encapTokens` never runs, so no
    /// marker is emitted. `false` here models that: ids are still spent, the
    /// `mw:Transclusion` wrapper is not.
    wrap_templates: std::cell::Cell<bool>,
}

/// Core's `$piece['lineStart']` for a template/parser-function call, as the
/// preprocessor computes it: `$text[$i - 1] === "\n"` — the character
/// immediately before the `{{`, with no allowance for indentation. A
/// preprocessed template body starts with `textStartsInSOLState = false`.
///
/// `None` when the call's position cannot be established: the token was
/// synthesised (no `tsr`) or re-tokenized from a *fragment* whose offsets do not
/// index `src_text`. The caller then decides from the expansion's own shape.
fn t2529_at_line_start(tok: &ParsoidToken, src_text: &str, body: bool) -> Option<bool> {
    (|| {
        let tsr = tok.data_parsoid().and_then(|dp| dp.tsr.as_ref())?;
        let start = tsr.start?;
        let source = tsr.source.as_deref().unwrap_or(src_text);
        if source.is_empty() {
            // A module's output is re-tokenized against no page source, so no
            // offset of its own names a line position.
            return None;
        }
        // A token re-tokenized from a *fragment* (a parser-function branch, an
        // argument value) carries offsets into that fragment, not into
        // `src_text`. Reading them anyway put the `{{(!}}` of
        // `Template:Largest cities` at some unrelated offset and invented a
        // newline before the table it expands to.
        if let Some(expected) = tok.data_parsoid().and_then(|dp| dp.src.as_deref())
            && !source.get(start..).is_some_and(|s| s.starts_with(expected))
        {
            // Offsets into another chunk say nothing about this one.
            return None;
        }
        if start == 0 {
            return Some(!body);
        }
        source.as_bytes().get(start - 1).map(|b| *b == b'\n')
    })()
}

/// Core `Parser::braceSubstitution`'s T2529 rule: a *nested* template's
/// expansion that begins with a table or list marker begins a new line, so a
/// `\n` is put in front of it when the call did not itself start a line.
///
/// Production Parsoid expands the page's own calls with `sol => true`
/// (`PipelineUtils::processTemplateSource`), which already tokenizes a leading
/// marker as a list; core's hack is consequently restricted to nested
/// expansions (T382464), i.e. the calls the PHP preprocessor expands inside a
/// template body. The re-tokenization below still runs for every call whose
/// expansion begins with a marker: Parsoid tokenizes an expansion's output at
/// start of line either way, and a parser function's branch (`#ifeq` returning
/// `;`) is not covered by the branch re-tokenizer.
///
/// Inside an attribute value, PHP splices the inserted newline into the
/// attribute *text* (`style="…\n;…"`), where block syntax is never parsed. A
/// `Nl` token would be dropped when the value is stringified, so the newline is
/// a plain string there and no re-tokenization happens.
fn t2529_force_line_start(
    expanded: Vec<Item>,
    at_line_start: Option<bool>,
    nested: bool,
    in_attribute: bool,
) -> Vec<Item> {
    // The expansion is wrapped in its transclusion marker(s); core checks the
    // expansion *string*, which is the first content token's source here.
    let start = expanded
        .iter()
        .position(|it| !is_wrapper_marker(it))
        .unwrap_or(expanded.len());
    let src = match expanded.get(start) {
        Some(Item::Str(s)) => s.clone(),
        Some(Item::Tok(t)) => t
            .data_parsoid()
            .map(|dp| {
                dp.src.clone().unwrap_or_else(|| {
                    // A `{|` table start is a token with no `src`; its
                    // `start_tag_src` holds the two-character marker.
                    dp.start_tag_src
                        .as_deref()
                        .filter(|s| s.starts_with('{'))
                        .unwrap_or_default()
                        .to_string()
                })
            })
            .unwrap_or_default(),
        _ => return expanded,
    };
    if !(src.starts_with("{|") || src.starts_with([':', ';', '#', '*'])) {
        return expanded;
    }
    let mut out = Vec::with_capacity(expanded.len() + 1);
    if nested
        && match at_line_start {
            Some(at_line_start) => !at_line_start,
            // Unknown position: a generated *string* marker needs the newline
            // (Parsoid's `TokenStreamPatcher` hack), and so does any marker
            // token the tokenizer built from wikitext. A table start seen only
            // through `start_tag_src` is the one exception: that fallback exists
            // for an expansion whose own source is not available (a module's
            // output), where core's preprocessor never ran and so never inserted
            // a newline (`Zebra`'s infobox table).
            None => !matches!(
                expanded.get(start),
                Some(Item::Tok(t)) if starts_with_table_marker(t)
            ),
        }
    {
        if in_attribute {
            out.push(Item::Str("\n".to_string()));
        } else {
            out.push(Item::Tok(ParsoidToken::Nl(
                crate::wikitext::tokens_v2::NlTk::new(
                    crate::wikitext::tokens_v2::SourceRange::new(0, 0),
                ),
            )));
        }
    }
    if in_attribute {
        out.extend(expanded);
        return out;
    }
    for (i, it) in expanded.into_iter().enumerate() {
        // A leading *string* was tokenized where the argument sat (mid-line),
        // so it is re-tokenized at SOL; a marker that is already a token (a
        // `listItem` from an argument that began its own line) only needed the
        // newline in front of it.
        if i == start
            && let Item::Str(s) = &it
        {
            out.extend(
                crate::pipeline::template_handler::tokenize_wikitext_to_items(
                    s,
                    /* in_template */ true,
                    &[],
                ),
            );
            continue;
        }
        out.push(it);
    }
    out
}

/// Whether `token` is a `{|` table start with no source of its own, i.e. one
/// detectable only through `start_tag_src`.
fn starts_with_table_marker(token: &ParsoidToken) -> bool {
    let Some(dp) = token.data_parsoid() else {
        return false;
    };
    dp.src.is_none()
        && dp
            .start_tag_src
            .as_deref()
            .is_some_and(|s| s.starts_with('{'))
}

/// Whether `item` is a transclusion wrapper marker (`mw:Transclusion` start or
/// end meta). `T2529` looks past these to the expansion's content.
fn is_wrapper_marker(item: &Item) -> bool {
    let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item else {
        return false;
    };
    t.name == "meta"
        && t.attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("typeof"))
            .and_then(|kv| kv.value.as_str())
            .is_some_and(|ty| {
                ty.split_whitespace()
                    .any(|c| c.starts_with("mw:Transclusion"))
            })
}

impl<'a, C: SiteConfig> Parser<'a, C> {
    pub fn new(config: &'a C) -> Self {
        Self {
            config,
            lua_expansion_depth: std::cell::Cell::new(0),
            ooui: crate::pipeline::ooui::OouiCounter::new(),
            page_title: std::cell::RefCell::new(String::new()),
            page_source: std::cell::RefCell::new(String::new()),
            pp_node_count: std::cell::Cell::new(0),
            expansion_depth: std::cell::Cell::new(0),
            strip_data_parsoid: std::cell::Cell::new(false),
            protection: std::cell::RefCell::new(std::collections::HashMap::new()),
            ifexist: std::cell::RefCell::new(std::collections::HashMap::new()),
            property_values: std::cell::RefCell::new(std::collections::HashMap::new()),
            page_entity: std::cell::RefCell::new(None),
            title_facts: std::cell::RefCell::new(std::collections::HashMap::new()),
            page_protection: std::cell::RefCell::new(ProtectionEntry::default()),
            page_revision: std::cell::Cell::new(None),
            ext_fragments: std::cell::RefCell::new(std::collections::HashMap::new()),
            ext_next_id: std::cell::Cell::new(0),
            pending_styles: std::cell::RefCell::new(std::collections::HashMap::new()),
            arg_expansion: std::cell::Cell::new(0),
            style_defer: std::cell::Cell::new(0),
            attr_expansion: std::cell::Cell::new(0),
            expansion_nesting: std::cell::Cell::new(0),
            module_scope: std::cell::Cell::new(0),
            strip_markers: std::cell::RefCell::new(std::collections::HashMap::new()),
            ref_bodies: std::cell::RefCell::new(std::collections::HashMap::new()),
            ref_bodies_by_body: std::cell::RefCell::new(std::collections::HashMap::new()),
            references_bodies: std::cell::RefCell::new(std::collections::HashMap::new()),
            nested_ref_bodies: std::cell::RefCell::new(std::collections::HashSet::new()),
            wrap_templates: std::cell::Cell::new(true),
        }
    }

    /// The protection answers `{{PROTECTIONLEVEL:…}}`/`{{PROTECTIONEXPIRY:…}}`
    /// may give during this parse.
    ///
    /// Borrows for as long as the returned context lives, so the caller must
    /// not hold an expansion that writes the map. Both maps are written exactly
    /// once, in [`Parser::build_ast`] before expansion starts, so a borrow taken
    /// during expansion can never conflict with a write.
    fn page_context(&self) -> PageContext<'_> {
        PageContext::new(&self.page_protection, &self.protection)
            .with_revision(self.page_revision.get())
            .with_property_values(&self.property_values)
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
        // The wiki normalises the title (`Title::newFromText`): `Category:1986`
        // with a doubled space is the category `Category:1986 films`, and asking
        // about the raw spelling reads it as absent. Normalise so the question —
        // and the map key the answer is read back with — is the wiki's own.
        let Some(title) = crate::title::normalize_title_for_lookup(title, self.config) else {
            // Not a valid title: it cannot exist.
            return;
        };
        if title.is_empty() || self.ifexist.borrow().contains_key(&title) {
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
        let titles = [title.clone()];
        let exists = source
            .get_page_info(&titles)
            .await
            .ok()
            .and_then(|m| m.get(&title).map(|i| !i.missing))
            .unwrap_or(false);
        self.ifexist.borrow_mut().insert(title, exists);
    }

    /// Resolve the answer a `#property` call is about to read.
    ///
    /// Like `#ifexist`, the value is a fetch and the parser-function call itself
    /// is synchronous, so the answer has to be in hand first. The property is
    /// expanded text by this point, so both the id and the optional `from`
    /// entity are known.
    async fn prime_property(
        &self,
        property_arg: &str,
        params: &crate::pipeline::parser_functions::Params,
        source: Option<&dyn DataSource>,
    ) {
        // `property_arg` is the resolver's text after the colon; `params` still
        // holds the raw target, so only its named `from` is read here.
        let property = property_arg.trim().to_string();
        let (_, from) =
            crate::pipeline::template_handler::TemplateHandler::property_call_key(params);
        if property.is_empty() {
            return;
        }
        let key = (property.clone(), from.clone());
        if self.property_values.borrow().contains_key(&key) {
            return;
        }
        let Some(source) = source else {
            return;
        };
        let value = self.resolve_property(&property, &from, source).await;
        self.property_values.borrow_mut().insert(key, value);
    }

    /// Read one property of one entity and render it as the plain wikitext
    /// `#property` returns. An unknown value, entity or property is empty, which
    /// is what the wiki serves; the parser function's error `<p>` for a property
    /// that does not exist is not reproduced (see ONLINE-PARITY.md).
    async fn resolve_property(
        &self,
        property_arg: &str,
        from: &str,
        source: &dyn DataSource,
    ) -> String {
        // A label such as `occupation` needs Wikidata's label→id index, which
        // rustoid does not hold; only an id can be resolved. Answering empty is
        // what an item without the value does, so a caller's `#if` still sees a
        // missing value rather than the unevaluated `{{#property:…}}` source.
        if !rustoid_extensions::wikibase::is_property_id(property_arg) {
            return String::new();
        }
        let property = property_arg.to_ascii_uppercase();
        let entity_id = if from.is_empty() {
            self.current_entity_id(source).await
        } else if crate::lua::wikibase::is_entity_id(from) {
            Some(from.to_ascii_uppercase())
        } else {
            None
        };
        let Some(entity_id) = entity_id else {
            return String::new();
        };
        let Some(entity_json) = source.get_entity(&entity_id).await.ok().flatten() else {
            return String::new();
        };
        let mut labels = std::collections::HashMap::new();
        for id in rustoid_extensions::wikibase::referenced_entity_ids(&entity_json, &property) {
            if labels.len() >= MAX_PROPERTY_LABELS {
                break;
            }
            if labels.contains_key(&id) {
                continue;
            }
            let Some(json) = source.get_entity(&id).await.ok().flatten() else {
                continue;
            };
            if let Some(label) =
                rustoid_extensions::wikibase::label(&json, self.config.language_code())
            {
                labels.insert(id, label);
            }
        }
        rustoid_extensions::wikibase::render_property(&entity_json, &property, &labels)
    }

    /// The page's Wikidata item id, fetched once per render.
    async fn current_entity_id(&self, source: &dyn DataSource) -> Option<String> {
        if let Some(cached) = self.page_entity.borrow().as_ref() {
            return cached.clone();
        }
        let title = self.page_title.borrow().clone();
        let id = source.get_entity_id_for_page(&title).await.ok().flatten();
        *self.page_entity.borrow_mut() = Some(id.clone());
        id
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

    fn new_about_id(&self, counter: &std::cell::Cell<usize>, tag: &str) -> String {
        crate::pipeline::attribute_expander::new_about_id(counter, tag)
    }

    /// Enter one level of `#invoke` argument expansion; the returned guard leaves
    /// it on drop. See [`Parser::arg_expansion`].
    fn begin_arg_expansion(&self) -> ArgExpansion<'_> {
        self.arg_expansion.set(self.arg_expansion.get() + 1);
        self.style_defer.set(self.style_defer.get() + 1);
        ArgExpansion {
            arg: &self.arg_expansion,
            style: &self.style_defer,
        }
    }

    /// Enter one level of attribute-value expansion; the returned guard leaves
    /// it on drop. See [`Parser::attr_expansion`].
    fn begin_attr_expansion(&self) -> AttrExpansion<'_> {
        self.attr_expansion.set(self.attr_expansion.get() + 1);
        AttrExpansion(&self.attr_expansion)
    }

    /// Whether an attribute value is currently being expanded. See
    /// [`Parser::attr_expansion`].
    fn in_attr_expansion(&self) -> bool {
        self.attr_expansion.get() > 0
    }

    /// Enter one level of *template*-argument expansion; the returned guard leaves
    /// it on drop. See [`Parser::style_defer`].
    fn begin_style_defer(&self) -> StyleDefer<'_> {
        self.style_defer.set(self.style_defer.get() + 1);
        StyleDefer(&self.style_defer)
    }

    /// Enter one level of module output/argument expansion; the returned guard
    /// leaves it on drop. While nonzero, [`Parser::expand_one_template`] passes
    /// `ignore_loop = true`. See [`Parser::module_scope`].
    fn enter_module_scope(&self) -> ExpansionNesting<'_> {
        self.module_scope.set(self.module_scope.get() + 1);
        ExpansionNesting(&self.module_scope)
    }

    /// Enter one level of [`Parser::expand_templates`]; the returned guard leaves
    /// it on drop. The page-level call is level 1. See
    /// [`Parser::expansion_nesting`].
    fn enter_expansion(&self) -> ExpansionNesting<'_> {
        self.expansion_nesting.set(self.expansion_nesting.get() + 1);
        ExpansionNesting(&self.expansion_nesting)
    }

    /// Suppress `mw:Transclusion` encapsulation for the chunk expanded while the
    /// returned guard lives, without suppressing the `about` ids the expansions
    /// still spend. This is a fresh extension-body pipeline (a `<ref>`'s), where
    /// PHP's `wrapTemplates` is false. See [`Parser::wrap_templates`].
    fn expand_unwrapped(&self) -> UnwrappedTemplates<'_> {
        let saved = self.wrap_templates.replace(false);
        UnwrappedTemplates {
            flag: &self.wrap_templates,
            saved,
        }
    }

    /// Whether an `#invoke` argument is currently being expanded, in which case
    /// extension ids must not be spent. See [`Parser::arg_expansion`].
    fn in_arg_expansion(&self) -> bool {
        self.arg_expansion.get() > 0
    }

    /// Whether an argument whose extensions are numbered where it lands in the
    /// output is being expanded. See [`Parser::style_defer`].
    fn in_style_defer(&self) -> bool {
        self.style_defer.get() > 0
    }

    /// Rewrite what a bail's re-tokenization produced, so nothing reaches the
    /// tree builder as an element it is not.
    ///
    /// A bail tokenizes a source reconstructed from the **expanded**
    /// href/content ([`reconstruct_link_src`]), so every `{{…}}` in it came *out
    /// of* an expansion. The expansion pass has already run, so re-creating an
    /// invocation resurrects one nothing will expand again: it reached the tree
    /// builder, which named an element after it (`<template …>`) and spent an
    /// `about` id. Two kinds of text look like one, and each has its own answer:
    ///
    /// - a `{{subst:…}}` is the invocation that *survives* expansion — it renders
    ///   as its own source inside a transclusion, which
    ///   [`Self::encapsulate_subst_literal`] rebuilds. That needs no fetch, so it
    ///   can be done here, and it is the one place the bail can match the service
    ///   exactly.
    /// - anything else is text by construction, and collapses back to its source.
    ///
    /// The wrapper takes an `about`, so `about_counter` must be present: it is on
    /// the async render paths and absent from the synchronous, data-source-less
    /// one, which keeps the text. `source` is the text the re-tokenized
    /// parameters' source ranges index, for `data-mw`'s `wt`, and `wrap` is the
    /// caller's `wrapTemplates` (a page-level link wraps, one produced in a
    /// template body does not).
    fn reparse_bail_source(
        &self,
        items: Vec<Item>,
        source: &str,
        about_counter: Option<&std::cell::Cell<usize>>,
        context_title: Option<&crate::title::Title>,
        wrap: bool,
    ) -> Vec<Item> {
        use crate::wikitext::token_utils::key_value_to_string;

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &item else {
                out.push(item);
                continue;
            };
            if t.name != "template" && t.name != "template3" {
                out.push(item);
                continue;
            }
            let target = t
                .attribs
                .first()
                .map(|kv| key_value_to_string(&kv.key))
                .unwrap_or_default();
            let body_src = t
                .data_parsoid
                .src
                .clone()
                .unwrap_or_else(|| format!("{{{{{target}}}}}"));
            if wrap
                && crate::wikitext::tokenizer_v2::subst_is_literal(&target)
                && let Some(counter) = about_counter
                && let Some(ResolvedTarget::Template { name, title }) =
                    resolve_template_target(self.config, context_title, &target)
            {
                let params = crate::pipeline::parser_functions::Params::new(t.attribs.clone());
                let about_id = self.new_about_id(counter, "template");
                let produced = self.encapsulate_subst_literal(
                    &ParsoidToken::SelfclosingTag(t.clone()),
                    &body_src,
                    &name,
                    &title,
                    &params,
                    about_id,
                    wrap,
                    source,
                );
                out.extend(produced);
                continue;
            }
            out.push(Item::Str(body_src));
        }
        out
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
        about_counter: Option<&std::cell::Cell<usize>>,
    ) -> Vec<Item> {
        use crate::pipeline::wiki_link_render::{
            WikiLinkContext, get_wiki_link_target_info_for_token, render_redirect,
            render_wiki_link_dispatched,
        };
        use crate::wikitext::token_utils::key_value_to_string;

        let mut tokens = tokens;
        for item in &mut tokens {
            if let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item
                && stt.name == "wikilink"
            {
                split_expanded_link_href(stt);
            }
        }

        let mut ctx = WikiLinkContext::new(self.config);
        if let Some(title) = context_title {
            ctx.set_context_title(title);
        }
        if let Some(counter) = about_counter {
            ctx.set_document_about(counter);
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
            if std::env::var_os("RUSTOID_TRACE_LINK").is_some()
                && (href.contains("Toronto") || href.contains("Greater"))
            {
                eprintln!(
                    "WIKILINK href={href:?} src={href_src:?} attribs={:?}",
                    stt.attribs
                );
            }

            let wrap = !stt.data_parsoid.tmp.synthesized;

            // Don't allow internal links to pages containing a PROTO (scheme).
            // `[[http://…]]` is really a (bracketed) external/autonumber link;
            // bail to plain text and let the external-link pass re-render it
            // (mirrors `onWikiLink` → `bailTokens` on `hasValidProtocol`).
            if !href.is_empty() && self.config.has_valid_protocol(&href) {
                let src = reconstruct_link_src(stt, &href);
                out.push(Item::Str("[".to_string()));
                let sub = self.reparse_bail_source(
                    self.tokenize(&src[1..]).unwrap_or_default(),
                    &src[1..],
                    about_counter,
                    context_title,
                    wrap,
                );
                let sub = self.render_links(sub, fragments, next_id, context_title, about_counter);
                out.extend(sub);
                continue;
            }

            let target = match get_wiki_link_target_info_for_token(
                &ctx,
                &ParsoidToken::SelfclosingTag(stt.clone()),
                &href,
                &href_src,
            ) {
                Ok(t) => t,
                Err(_) => {
                    // Invalid title: bail to literal `[[…]]` text, re-tokenizing the
                    // source so entities render as `mw:Entity` spans (mirrors PHP's
                    // `bailTokens` re-processing the link source).
                    let src = reconstruct_link_src(stt, &href);
                    out.push(Item::Str("[".to_string()));
                    let sub = self.reparse_bail_source(
                        self.tokenize(&src[1..]).unwrap_or_default(),
                        &src[1..],
                        about_counter,
                        context_title,
                        wrap,
                    );
                    let sub =
                        self.render_links(sub, fragments, next_id, context_title, about_counter);
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
                let sub = self.reparse_bail_source(
                    self.tokenize(&src[1..]).unwrap_or_default(),
                    &src[1..],
                    about_counter,
                    context_title,
                    wrap,
                );
                let sub = self.render_links(sub, fragments, next_id, context_title, about_counter);
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
                &mut |items, frags, id| {
                    // The caption's own nested fragments (a `<templatestyles>`
                    // resolved while the caption was expanded) live in the same
                    // document-wide fragment space, so they are resolved against
                    // the caller's map and counter, not a fresh local one.
                    self.build_inline_fragment(items, frags, id)
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
        about_counter: Option<&std::cell::Cell<usize>>,
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
                    let content_wrap = !stt.data_parsoid.tmp.synthesized;
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
                                        let sub = self.reparse_bail_source(
                                            self.tokenize_inline(&s).unwrap_or_default(),
                                            &s,
                                            about_counter,
                                            None,
                                            content_wrap,
                                        );
                                        let sub = self.render_links(
                                            sub,
                                            fragments,
                                            next_id,
                                            None,
                                            about_counter,
                                        );
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
                        |proto| self.config.has_valid_protocol(proto),
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
    /// `in_template` is that pipeline's `inTemplate` option: PHP's
    /// `wikitextToDOM` forwards the *enclosing* pipeline's flag, so a body
    /// reached from a template keeps it and one reached from the page does not
    /// (its nested transclusions are wrapped).
    async fn process_fragment_body(
        &self,
        body: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
    ) -> Node {
        let mut tokens = match self.tokenize(body) {
            Ok(t) => t,
            Err(_) => return crate::dom::node::Node::document(),
        };

        // Expand nested templates/parser functions when a data source is
        // available (the synchronous `wikitext_to_ast` path has none).
        let mut fragments: std::collections::HashMap<usize, Node> =
            std::collections::HashMap::new();
        // Allocate from the document-wide fragment id counter, like Parsoid's
        // `Env::newFragmentId`: a `<templatestyles>` inside the body is resolved
        // during the expansion below and its placeholder must name a fragment the
        // body build can resolve.
        let next_id = &self.ext_next_id;
        if source.is_some() {
            tokens = self
                .expand_templates(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    in_template,
                    // A body reached through an argument value (`#tag:ref`) was
                    // already expanded by the preprocessor, so a `{{…}}` left
                    // in it is text, not a token Parsoid numbers. `body` is
                    // what suppresses that id without changing how magic words
                    // expand (`in_template` stays).
                    in_template,
                    body,
                    0,
                )
                .await;
            // TT2 order: ExtensionHandler precedes the AttributeExpander.
            tokens = crate::pipeline::extension_handler::expand_in_attributes(
                tokens,
                self.config,
                &mut fragments,
                next_id,
            );
            tokens = self
                .expand_attributes(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    None,
                    &mut fragments,
                    next_id,
                )
                .await;
            // The body is a *fresh* pipeline (PHP's `sourceToDom` over the tag
            // body), so the extension passes that render a tag with output of its
            // own — a gallery, an imagemap, a Phonos button — must run here too: a
            // `{{#tag:phonos|…}}` or `<gallery>` inside a note reached only the
            // template expansion, and its token was built into the fragment
            // unresolved, leaking a bare `<extension>`.
            tokens = self
                .expand_gallery(
                    tokens,
                    source,
                    frame,
                    about_counter,
                    &mut fragments,
                    next_id,
                )
                .await;
            tokens = self
                .expand_imagemap(
                    tokens,
                    source,
                    frame,
                    about_counter,
                    &mut fragments,
                    next_id,
                )
                .await;
            tokens = self
                .expand_phonos(
                    tokens,
                    source,
                    frame,
                    about_counter,
                    &mut fragments,
                    next_id,
                )
                .await;
            tokens = self.expand_syntaxhighlight(tokens, about_counter, &mut fragments, next_id);
            tokens = self
                .expand_mapframe(
                    tokens,
                    source,
                    frame,
                    about_counter,
                    &mut fragments,
                    next_id,
                )
                .await;
        }
        // A `<templatestyles>` the body expanded stashes its `<style>` in
        // `ext_fragments` (drained into the page's map only at the end of
        // `build_ast`). The body is built *here*, so fold a copy in — a clone,
        // not a take, so the page's own placeholders still resolve later. Without
        // it the note's stylesheet vanished (`<ref>{{cite journal|…}}</ref>`).
        fragments.extend(
            self.ext_fragments
                .borrow()
                .iter()
                .map(|(k, v)| (*k, v.clone())),
        );
        // The quote transformer flushes pending quotes only on a newline or EOF
        // token; append a synthetic EOF so quotes (`'''bold'''`) flush.
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));

        let mut frag = self.build_inline_fragment(tokens, &mut fragments, next_id);
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
        // The page title is the context a fragment-only link (`[[#Foo]]`) resolves
        // against: a ref body or caption carrying one renders as
        // `./Page#Foo` with `mw-selflink-fragment`, not a redlink.
        let context_title =
            crate::title::TitleParser::parse(&self.page_title.borrow(), self.config);
        render_inline_fragment(
            self.config,
            tokens,
            fragments,
            next_id,
            Some(&context_title),
        )
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
    ///
    /// The second return value tunnels the same fragment (under attribute key
    /// `ck`) as a `mw:dom-fragment-token` child of the token, so the id pass
    /// numbers its nodes and `attr_mw_fragments` re-serializes the `html` with
    /// those ids. The eager string above is the fallback for a path that never
    /// builds the fragment into the document. See
    /// [`crate::pipeline::attribute_expander::build_expanded_attrs`].
    fn value_to_dom_html(
        &self,
        kv: &crate::wikitext::tokens_v2::KeyValue,
        ck: &str,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> (String, Option<Item>) {
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
        let html = serializer.serialize(&frag).unwrap_or_default();
        let placeholder =
            crate::pipeline::wiki_link_render::attr_mw_fragment_token(ck, frag, fragments, next_id);
        (html, Some(placeholder))
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
                    // See [`Self::process_fragment_body`]: an argument-value
                    // body has already been preprocessed.
                    in_template,
                    body,
                    0,
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
            crate::pipeline::tree_builder_html::post_pwrap_transforms(
                &mut ast, &depths, None, None,
            );
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
        let tokens = self.render_links(tokens, &mut fragments, &next_id, None, None);
        let tokens = self.render_external_links(tokens, &mut fragments, &next_id, None);
        let tokens = self.render_behavior_switches(tokens);
        let tokens = self.render_language_variants(tokens);

        let stage = TreeBuilderStage::new(inline);
        let mut ast = stage.to_ast_with_fragments(tokens, None, self.config, fragments, None);
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
        if !inline {
            crate::pipeline::p_wrap::run(&mut ast);
        }
        crate::pipeline::tree_builder_html::post_pwrap_transforms(&mut ast, &depths, None, None);
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
                .process_fragment_body(&body, source, frame, about_counter, true)
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
                .expand_templates(
                    frame,
                    tokens,
                    source,
                    about_counter,
                    false,
                    false,
                    &caption,
                    0,
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
        // The trailing `|none` forces the block figure, per `renderMedia`'s
        // `$pieces[] = '|none'`.
        let wikitext = format!("[[{title_str}|{opts_str}|none]]");
        self.render_inline_wikitext_node(&wikitext, source, frame, about_counter)
            .await
    }

    /// Render `wikitext` through the inline sub-pipeline and return its single
    /// root node.
    ///
    /// Media formats are suppressed, so a media line comes back as the plain
    /// block figure `<gallery>` and `<imagemap>` need rather than a thumbnail,
    /// and a nested `AddMediaInfo` resolves the file, so the returned `<img>`
    /// carries its `width`/`height` and `data-file-*` — `<imagemap>` reads those
    /// to scale its coordinates. Mirrors PHP's `processContentInPipeline` with
    /// `inlineContext => true`.
    async fn render_inline_wikitext_node(
        &self,
        wikitext: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Option<Node> {
        use crate::pipeline::wiki_link_render::{
            WikiLinkContext, get_wiki_link_target_info_for_token, render_wiki_link_dispatched,
        };
        use crate::wikitext::token_utils::key_value_to_string;

        let mut tokens = self.tokenize(wikitext).ok()?;
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
                    wikitext,
                    0,
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
                let target = match get_wiki_link_target_info_for_token(
                    &link_ctx,
                    &ParsoidToken::SelfclosingTag(stt.clone()),
                    &href,
                    &href_src,
                ) {
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
                    &mut |items, f, id| {
                        render_inline_fragment(
                            self.config,
                            items,
                            f,
                            id,
                            Some(&crate::title::TitleParser::parse(
                                &self.page_title.borrow(),
                                self.config,
                            )),
                        )
                    },
                )
            })
            .collect();
        let tokens = self.render_external_links(tokens, &mut fragments, &next_id, None);
        let mut tokens = self.render_behavior_switches(tokens);
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        // Build the inline tree, splicing the caption `mw:dom-fragment-token`
        // placeholders via the fragments map populated by `renderFile`.
        let stage = TreeBuilderStage::new(true);
        let frag = stage.to_ast_with_fragments(tokens, None, self.config, fragments, None);
        let mut frag = extract_fragment_children(&frag);
        // Move the expandable media-option fragments onto their containers before
        // `post_pwrap_transforms` unpacks the placeholders into view.
        crate::pipeline::attr_mw_fragments::collect(&mut frag);
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&frag);
        crate::pipeline::tree_builder_html::post_pwrap_transforms(&mut frag, &depths, None, None);

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
            // The per-chunk expansion loop stamped the id the gallery would have
            // been given at its position in the document (see `MW_ABOUT_BASE_ATTR`);
            // a gallery rendered outside that loop still allocates fresh.
            if let crate::dom::node::NodeKind::Element(_) = ul.kind {
                let about = stt
                    .attribs
                    .iter()
                    .find(|kv| {
                        kv.key.as_str()
                            == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
                    })
                    .and_then(|kv| kv.value.as_str())
                    .filter(|b| !b.is_empty())
                    .map(|b| format!("#mwt{b}"))
                    .unwrap_or_else(|| self.new_about_id(about_counter, "gallery"));
                ul.set_attr("about", about);
            }
            let mut frag = crate::dom::node::Node::document();
            frag.push_child(ul);
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, frag);
            out.push(emit_dom_fragment_placeholder(stt, id));
        }

        out
    }

    /// Build the `<figure>` for one `<imagemap>` tag. Port of
    /// `ParsoidImageMap::sourceToDom`: render the image, scale each area's
    /// coordinates by the rendered size, build the `<map>`, and replace the
    /// media's anchor with the default (whole-image) link or a `<span>`.
    async fn build_imagemap(
        &self,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Option<Node> {
        use crate::dom::node::{ElementKind, Node, NodeKind};
        use crate::pipeline::imagemap::{self, Shape};

        let ext_src = stt
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("source"))
            .and_then(|kv| kv.value.as_str())
            .unwrap_or_default()
            .to_string();
        let body_src = crate::pipeline::extension_handler::extract_ext_body(stt, &ext_src);
        let parsed = imagemap::parse_body(&body_src).ok()?;

        // The image as a block figure (`|none`) with formats suppressed.
        let img_wt = format!("[[{}|{}|none]]", parsed.image, parsed.options);
        let mut figure = self
            .render_inline_wikitext_node(&img_wt, source, frame, about_counter)
            .await?;
        if !matches!(figure.kind, NodeKind::Element(_)) {
            return None;
        }

        // `renderMedia(forceBlock=true)` appends `|none` to force the block
        // (figure) form, then strips the `mw-halign-none` it produced — unless
        // the source itself carried a `none` option (PHP's `$explicitNone`).
        let explicit_none = parsed.options.split('|').any(|o| o.trim() == "none");
        if !explicit_none && let Some(class) = figure.get_attr("class").map(str::to_string) {
            let kept: Vec<&str> = class
                .split_whitespace()
                .filter(|c| *c != "mw-halign-none")
                .collect();
            if kept.is_empty() {
                figure.attrs.retain(|a| a.key != "class");
            } else {
                figure.set_attr("class", kept.join(" "));
            }
        }

        // The rendered sizes decide the coordinate scale.
        let (thumb_w, thumb_h, file_w, file_h) = {
            let img = find_element(&figure, "img")?;
            let get = |k: &str| {
                img.get_attr(k)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0)
            };
            (
                get("width"),
                get("height"),
                get("data-file-width"),
                get("data-file-height"),
            )
        };
        let denominator = file_w + file_h;
        if denominator <= 0.0 || thumb_w + thumb_h <= 0.0 {
            return None;
        }
        let scale = (thumb_w + thumb_h) / denominator;

        // The media caption, if any (the imagemap's first line may carry one).
        let caption_text = find_element(&figure, "figcaption")
            .map(text_content)
            .unwrap_or_default()
            .trim()
            .to_string();
        let has_caption = !caption_text.is_empty();

        // One `<area>` per shape line; `default` becomes the image's own link.
        let mut areas: Vec<Node> = Vec::new();
        let mut default_link: Option<Node> = None;
        for line in &parsed.areas {
            let link_dom = self
                .render_inline_wikitext_node(&line.link, source, frame, about_counter)
                .await?;
            let a = find_anchor(&link_dom)?;
            let href = a.get_attr("href")?.to_string();
            let rel = a.get_attr("rel").unwrap_or_default().to_string();
            let external = rel.split_whitespace().any(|t| t == "mw:ExtLink");
            // `getExternalLinkAttribs` returns the module rels only, not `mw:ExtLink`.
            let ext_rel = if external {
                let joined = rel
                    .split_whitespace()
                    .filter(|t| !t.starts_with("mw:"))
                    .collect::<Vec<_>>()
                    .join(" ");
                (!joined.is_empty()).then_some(joined)
            } else {
                None
            };
            let ext_target = if external {
                a.get_attr("target").map(str::to_string)
            } else {
                None
            };
            let alt = text_content(a).trim().to_string();
            let coords: Vec<i64> = line
                .coords
                .iter()
                .map(|c| (c * scale).round() as i64)
                .collect();
            if line.shape == Shape::Default {
                let mut anchor = Node::element(ElementKind::Other("a".to_string()));
                anchor.set_attr("href", href);
                if external {
                    anchor.set_attr("class", "plainlinks");
                    if let Some(r) = &ext_rel {
                        anchor.set_attr("rel", r.clone());
                    }
                    if let Some(t) = &ext_target {
                        anchor.set_attr("target", t.clone());
                    }
                }
                default_link = Some(anchor);
            } else {
                areas.push(imagemap::area_node(
                    &href,
                    external,
                    ext_rel.as_deref(),
                    ext_target.as_deref(),
                    line.shape,
                    &coords,
                    &alt,
                ));
            }
        }

        // The `<map>`, named from the hash of its serialized areas, and the
        // image's `usemap`.
        if !areas.is_empty() {
            let mut map = Node::element(ElementKind::Other("map".to_string()));
            for area in areas {
                map.push_child(area);
            }
            let inner = crate::html::serialize::HtmlSerializer::new(
                crate::options::ParserOptions::for_page(""),
            )
            .serialize_children_to_string(&map)
            .ok()?;
            let name = imagemap::map_name(&inner);
            if let Some(img) = find_element_mut(&mut figure, "img") {
                img.set_attr("usemap", format!("#{name}"));
            }
            map.set_attr("name", name);
            // Directly after the image's wrapper (the figure's first child).
            figure.children.insert(1, map);
        }

        // `noresize`, and the `desc` placement class.
        {
            let existing = figure.get_attr("class").unwrap_or_default().to_string();
            let mut classes: Vec<String> =
                existing.split_whitespace().map(str::to_string).collect();
            classes.push("noresize".to_string());
            let is_thumb = figure
                .get_attr("typeof")
                .map(|t| t.split_whitespace().any(|x| x == "mw:File/Thumb"))
                .unwrap_or(false);
            if !is_thumb && let Some(suffix) = parsed.desc.class_suffix() {
                classes.push(format!("mw-ext-imagemap-desc-{suffix}"));
            }
            figure.set_attr("class", classes.join(" "));
            let ty = figure.get_attr("typeof").unwrap_or("mw:File").to_string();
            figure.set_attr("typeof", format!("{ty} mw:Extension/imagemap"));
        }

        // Replace the media anchor with the default link (or a `<span>`),
        // keeping the image.
        let mut anchor = figure.children.remove(0);
        let img = take_element(&mut anchor, "img")?;
        let mut wrapper =
            default_link.unwrap_or_else(|| Node::element(ElementKind::Other("span".to_string())));
        // A caption that is not shown as a `<figcaption>` becomes the wrapper's
        // `title` (PHP's `!hasVisibleCaption` branch).
        if has_caption {
            wrapper.set_attr("title", caption_text);
        }
        wrapper.push_child(img);
        figure.children.insert(0, wrapper);

        // The extension encapsulation: `about`, then `data-mw`.
        let about = stt
            .attribs
            .iter()
            .find(|kv| {
                kv.key.as_str() == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
            })
            .and_then(|kv| kv.value.as_str())
            .filter(|b| !b.is_empty())
            .map(|b| format!("#mwt{b}"))
            .unwrap_or_else(|| self.new_about_id(about_counter, "imagemap"));
        figure.set_attr("about", about);
        let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
        figure.set_attr(
            "data-mw",
            crate::pipeline::extension_handler::extension_data_mw(
                "imagemap",
                &attrs,
                Some(&body_src),
            ),
        );

        // The media's own `<figcaption>` is not part of an imagemap's output when
        // it carries no caption.
        if !has_caption {
            remove_element(&mut figure, "figcaption");
        }

        Some(figure)
    }

    /// Expand `<imagemap>` extension tokens in place, stashing each rendered
    /// `<figure>` behind a `mw:dom-fragment-token` placeholder (mirrors
    /// `expand_gallery`).
    async fn expand_imagemap(
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
            let Some(stt) = imagemap_target(&item) else {
                out.push(item);
                continue;
            };
            let stt = stt.clone();
            match self
                .build_imagemap(&stt, source, frame, about_counter)
                .await
            {
                Some(figure) => {
                    let mut frag = crate::dom::node::Node::document();
                    frag.push_child(figure);
                    let id = next_id.get();
                    next_id.set(id + 1);
                    fragments.insert(id, frag);
                    out.push(emit_dom_fragment_placeholder(&stt, id));
                }
                None => out.push(item),
            }
        }
        out
    }

    /// Expand `<syntaxhighlight>` extension tokens in place, stashing each
    /// wrapper behind a `mw:dom-fragment-token` placeholder (mirrors
    /// `expand_phonos`). The body is escaped text rather than Pygments output;
    /// see [`rustoid_extensions::syntaxhighlight`].
    fn expand_syntaxhighlight(
        &self,
        tokens: Vec<Item>,
        about_counter: &std::cell::Cell<usize>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        use crate::pipeline::extension_handler::{extension_data_mw, extension_kv_attrs};

        let mut out: Vec<Item> = Vec::new();
        for item in tokens {
            let Some(stt) = syntaxhighlight_target(&item) else {
                out.push(item);
                continue;
            };
            let stt = stt.clone();
            let about = stt
                .attribs
                .iter()
                .find(|kv| {
                    kv.key.as_str() == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
                })
                .and_then(|kv| kv.value.as_str())
                .filter(|b| !b.is_empty())
                .map(|b| format!("#mwt{b}"))
                .unwrap_or_else(|| self.new_about_id(about_counter, "syntaxhighlight"));
            let attrs = extension_kv_attrs(&stt);
            let pairs: Vec<(String, String)> = attrs
                .iter()
                .filter_map(|kv| {
                    Some((kv.key.as_str()?.to_string(), kv.value.as_str()?.to_string()))
                })
                .collect();
            let ext_src = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("source"))
                .and_then(|kv| kv.value.as_str())
                .unwrap_or_default()
                .to_string();
            // A self-closing tag has no body, and the service's `data-mw` omits
            // the key entirely; a tag pair keeps it even when empty.
            let has_body = stt
                .data_parsoid
                .ext_tag_offsets
                .as_ref()
                .and_then(|offsets| offsets.close_width)
                .unwrap_or(0)
                > 0;
            let body = has_body
                .then(|| crate::pipeline::extension_handler::extract_ext_body(&stt, &ext_src));
            let data_mw = extension_data_mw("syntaxhighlight", &attrs, body.as_deref());
            let request = rustoid_extensions::syntaxhighlight::Request {
                attrs: &pairs,
                body: body.as_deref(),
                about,
                data_mw,
            };
            match rustoid_extensions::syntaxhighlight::render(&request) {
                Some(node) => {
                    let mut frag = crate::dom::node::Node::document();
                    frag.push_child(node);
                    let id = next_id.get();
                    next_id.set(id + 1);
                    fragments.insert(id, frag);
                    out.push(emit_dom_fragment_placeholder(&stt, id));
                }
                None => out.push(item),
            }
        }
        out
    }

    /// Expand `<phonos>` extension tokens in place, stashing each rendered
    /// `<span class="ext-phonos">` behind a `mw:dom-fragment-token` placeholder
    /// (mirrors `expand_gallery`/`expand_imagemap`).
    async fn expand_phonos(
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
            let Some(stt) = phonos_target(&item) else {
                out.push(item);
                continue;
            };
            let stt = stt.clone();
            match self.build_phonos(&stt, source, frame, about_counter).await {
                Some(node) => {
                    let mut frag = crate::dom::node::Node::document();
                    frag.push_child(node);
                    let id = next_id.get();
                    next_id.set(id + 1);
                    fragments.insert(id, frag);
                    out.push(emit_dom_fragment_placeholder(&stt, id));
                }
                None => out.push(item),
            }
        }
        out
    }

    /// Build the `<span class="ext-phonos">` for one `<phonos>` tag. Port of
    /// `Phonos::renderPhonos` for the corpus's `<phonos file="…">` form: an
    /// OOUI `PhonosButton` inside the wrapper, then the attribution `<sup>`.
    async fn build_phonos(
        &self,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Option<Node> {
        use crate::pipeline::extension_handler::{extension_data_mw, extension_kv_attrs};
        let attrs: Vec<(String, String)> = extension_kv_attrs(stt)
            .into_iter()
            .filter_map(|kv| Some((kv.key.as_str()?.to_string(), kv.value.as_str()?.to_string())))
            .collect();
        let args =
            crate::pipeline::phonos::PhonosArgs::from_attrs(&attrs, self.config.language_code());
        if args.file.is_empty() {
            return None;
        }
        // `handleExistingFile` resolves the file (a redirect included) and reads
        // its mp3 transcode; a missing file throws there, so the tag is left
        // for the error path.
        let src = source?;
        let title = crate::title::Title::new(6, args.file.clone());
        let info = src.get_file_info(&title, None, None).await.ok().flatten()?;

        // The tag body is the button's visible label, parsed in full
        // (`renderPhonos`'s `recursiveTagParseFully`) and stripped of an outer
        // paragraph; it is empty for the plain `<phonos file="…"/>` form.
        let ext_src = stt
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("source"))
            .and_then(|kv| kv.value.as_str())
            .unwrap_or_default()
            .to_string();
        let body_src = crate::pipeline::extension_handler::extract_ext_body(stt, &ext_src);
        let label_wt = body_src.trim();
        let label = if label_wt.is_empty() {
            None
        } else {
            Some(
                Box::pin(self.process_fragment_body(label_wt, source, frame, about_counter, false))
                    .await,
            )
        };

        let mut node = crate::pipeline::phonos::render(
            &args,
            &info,
            label.as_ref(),
            self.config,
            &self.ooui,
            self.strip_data_parsoid.get(),
        );

        // The extension encapsulation: `typeof`, `about`, `data-mw`.
        let about = stt
            .attribs
            .iter()
            .find(|kv| {
                kv.key.as_str() == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
            })
            .and_then(|kv| kv.value.as_str())
            .filter(|b| !b.is_empty())
            .map(|b| format!("#mwt{b}"))
            .unwrap_or_else(|| self.new_about_id(about_counter, "phonos"));
        node.set_attr("typeof", "mw:Extension/phonos");
        node.set_attr("about", about);
        node.set_attr(
            "data-mw",
            extension_data_mw("phonos", &extension_kv_attrs(stt), Some(&body_src)),
        );
        Some(node)
    }

    /// Expand `<mapframe>` extension tokens in place, stashing each rendered
    /// Kartographer map behind a `mw:dom-fragment-token` placeholder (mirrors
    /// `expand_phonos`).
    #[allow(clippy::too_many_arguments)]
    async fn expand_mapframe(
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
            let Some(stt) = kartographer_target(&item) else {
                out.push(item);
                continue;
            };
            let stt = stt.clone();
            let node = match kartographer_tag(&stt) {
                "maplink" => self.build_maplink(&stt, source, frame, about_counter).await,
                _ => self.build_mapframe(&stt, about_counter),
            };
            match node {
                Some(node) => {
                    let mut frag = crate::dom::node::Node::document();
                    frag.push_child(node);
                    let id = next_id.get();
                    next_id.set(id + 1);
                    fragments.insert(id, frag);
                    out.push(emit_dom_fragment_placeholder(&stt, id));
                }
                None => out.push(item),
            }
        }
        out
    }

    /// Build the `<a>`/`<figure>` for one `<mapframe>` tag. Port of
    /// Kartographer's `ParsoidMapFrame::sourceToDom` plus the DOM processor's
    /// group-hash pass.
    fn build_mapframe(
        &self,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        about_counter: &std::cell::Cell<usize>,
    ) -> Option<Node> {
        use crate::pipeline::extension_handler::{extension_kv_attrs, extract_ext_body};
        let attrs: Vec<(String, String)> = extension_kv_attrs(stt)
            .into_iter()
            .filter_map(|kv| Some((kv.key.as_str()?.to_string(), kv.value.as_str()?.to_string())))
            .collect();
        let source = stt
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("source"))
            .and_then(|kv| kv.value.as_str())
            .unwrap_or_default();
        let body = extract_ext_body(stt, source);
        let about = stt
            .attribs
            .iter()
            .find(|kv| {
                kv.key.as_str() == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
            })
            .and_then(|kv| kv.value.as_str())
            .filter(|b| !b.is_empty())
            .map(|b| format!("#mwt{b}"))
            .unwrap_or_else(|| self.new_about_id(about_counter, "extension-id"));
        let page_title = self.page_title.borrow().replace(' ', "_");
        let server = self.config.server_url();
        // The server URL is protocol-relative on the live wiki (`//en.wikipedia.org`).
        let domain = server
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("//")
            .trim_end_matches('/');
        let request = rustoid_extensions::kartographer::MapFrameRequest {
            attrs: &attrs,
            body: &body,
            page_title: &page_title,
            revision_id: self.page_revision.get(),
            page_lang: self.config.language_code(),
            domain,
            about: &about,
        };
        rustoid_extensions::kartographer::render(&request).map(|rendered| rendered.node)
    }

    /// Build the `<a>` for one `<maplink>` tag (port of
    /// `ParsoidMapLink::sourceToDom`). The body's `title`/`description`
    /// properties are inline wikitext that Kartographer parses *before* hashing
    /// the group id, and marker counters are applied after that.
    async fn build_maplink(
        &self,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Option<Node> {
        use crate::pipeline::extension_handler::{extension_kv_attrs, extract_ext_body};
        let attrs: Vec<(String, String)> = extension_kv_attrs(stt)
            .into_iter()
            .filter_map(|kv| Some((kv.key.as_str()?.to_string(), kv.value.as_str()?.to_string())))
            .collect();
        let source_text = stt
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("source"))
            .and_then(|kv| kv.value.as_str())
            .unwrap_or_default();
        let body = extract_ext_body(stt, source_text);
        let about = stt
            .attribs
            .iter()
            .find(|kv| {
                kv.key.as_str() == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
            })
            .and_then(|kv| kv.value.as_str())
            .filter(|b| !b.is_empty())
            .map(|b| format!("#mwt{b}"))
            .unwrap_or_else(|| self.new_about_id(about_counter, "extension-id"));

        let mut geometries = rustoid_extensions::kartographer::parse_geometries(&body);
        let mut fallback_text: Option<String> = None;
        if let Some(geometries) = geometries.as_mut() {
            self.parse_geojson_wikitext(geometries, source, frame, about_counter)
                .await;
            let fallback = rustoid_extensions::kartographer::find_first_marker_symbol(geometries);
            // The DOM processor writes the counter over the label when the tag
            // has no `text` attribute; without a marker it uses the raw symbol.
            let counter = rustoid_extensions::kartographer::update_marker_counters(geometries);
            fallback_text = counter.or(fallback);
        }

        let text_attr = attrs.iter().find(|(k, _)| k == "text").map(|(_, v)| v);
        let text_node = match text_attr.map(|v| v.trim()) {
            Some("") => None,
            Some(text) if is_ascii_alnum(text) => {
                let mut doc = Node::document();
                doc.push_child(Node::text(text.to_string()));
                Some(doc)
            }
            Some(text) => Some(
                self.parse_inline_nodes(text, source, frame, about_counter)
                    .await,
            ),
            None => {
                if let Some(label) = &fallback_text {
                    let mut doc = Node::document();
                    doc.push_child(Node::text(label.clone()));
                    Some(doc)
                } else {
                    rustoid_extensions::kartographer::coordinate_span(&attrs).map(|span| {
                        let mut doc = Node::document();
                        doc.push_child(span);
                        doc
                    })
                }
            }
        };

        let request = rustoid_extensions::kartographer::MapLinkRequest {
            attrs: &attrs,
            body: &body,
            geometries: geometries.as_ref(),
            text: text_node.as_ref(),
            fallback_text: fallback_text.as_deref(),
            page_lang: self.config.language_code(),
            about: &about,
        };
        rustoid_extensions::kartographer::render_maplink(&request).map(|r| r.node)
    }

    /// Parse the GeoJSON `properties.title`/`description` wikitext the way
    /// `SimpleStyleParser::recursivelySanitizeAndParseWikitext` does, before the
    /// group id is hashed over the result.
    async fn parse_geojson_wikitext(
        &self,
        geometries: &mut serde_json::Value,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) {
        let mut strings: Vec<(Vec<GeojsonPath>, String)> = Vec::new();
        collect_geojson_properties(geometries, &mut Vec::new(), &mut strings);
        let mut rendered: std::collections::HashMap<Vec<GeojsonPath>, String> =
            std::collections::HashMap::new();
        for (path, text) in strings {
            let html = if wf_escape_wiki_text_changes(&text, self.config) {
                self.parse_inline_html(&text, source, frame, about_counter)
                    .await
            } else {
                text
            };
            rendered.insert(path, html);
        }
        apply_geojson_properties(geometries, &mut Vec::new(), &rendered);
    }

    /// Build an inline fragment from raw wikitext (mirrors `wikitextToDOM` with
    /// `context: inline`, `processInNewFrame`, `clearDSROffsets`).
    async fn parse_inline_nodes(
        &self,
        text: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Node {
        let mut tokens = match self.tokenize_inline(text) {
            Ok(tokens) => tokens,
            Err(_) => {
                let mut doc = Node::document();
                doc.push_child(Node::text(text.to_string()));
                return doc;
            }
        };
        let mut fragments: std::collections::HashMap<usize, Node> =
            std::collections::HashMap::new();
        let next_id = std::cell::Cell::new(0usize);
        tokens = self
            .expand_templates(frame, tokens, source, about_counter, false, false, text, 0)
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
        let mut frag = self.build_inline_fragment(tokens, &mut fragments, &next_id);
        if let Some(src) = source {
            crate::pipeline::add_media_info::run(&mut frag, src, self.config).await;
        }
        frag
    }

    /// Serialize [`Self::parse_inline_nodes`] the way `ParsoidExtensionAPI::
    /// domToHtml` does — `data-parsoid` intact, because Kartographer hashes the
    /// HTML string.
    async fn parse_inline_html(
        &self,
        text: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> String {
        let mut frag = self
            .parse_inline_nodes(text, source, frame, about_counter)
            .await;
        strip_literal_tag_src(&mut frag);
        let serializer =
            crate::html::serialize::HtmlSerializer::new(crate::options::ParserOptions {
                body_only: true,
                strip_data_parsoid: false,
                ..crate::options::ParserOptions::for_page("")
            });
        serializer
            .serialize(&frag)
            .unwrap_or_else(|_| text.to_string())
    }

    /// Expand `<templatedata>` extension tokens in place, stashing each rendered
    /// `<section class="mw-templatedata-doc-wrap">` behind a
    /// `mw:dom-fragment-token` placeholder (mirrors `expand_imagemap`).
    fn expand_templatedata(
        &self,
        tokens: Vec<Item>,
        about_counter: &std::cell::Cell<usize>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        let mut out: Vec<Item> = Vec::new();
        for item in tokens {
            let Some(stt) = templatedata_target(&item) else {
                out.push(item);
                continue;
            };
            let stt = stt.clone();
            match self.build_templatedata(&stt, about_counter) {
                Some(node) => {
                    let mut frag = crate::dom::node::Node::document();
                    frag.push_child(node);
                    let id = next_id.get();
                    next_id.set(id + 1);
                    fragments.insert(id, frag);
                    out.push(emit_dom_fragment_placeholder(&stt, id));
                }
                None => out.push(item),
            }
        }
        out
    }

    /// Build the `<section class="mw-templatedata-doc-wrap">` for one
    /// `<templatedata>` tag. Port of `TemplateData\Hooks::render`'s formatter
    /// path; the tag's JSON body is validated and normalized inside
    /// [`crate::pipeline::templatedata::render`].
    fn build_templatedata(
        &self,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        about_counter: &std::cell::Cell<usize>,
    ) -> Option<Node> {
        use crate::pipeline::extension_handler::{extension_data_mw, extension_kv_attrs};
        let ext_src = stt
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("source"))
            .and_then(|kv| kv.value.as_str())
            .unwrap_or_default()
            .to_string();
        let body_src = crate::pipeline::extension_handler::extract_ext_body(stt, &ext_src);

        // `$frame->getTitle()`: the subpage the tag sits in, stamped by
        // `stamp_templatedata_frame_title` when the body was expanded. Fall back
        // to the page title for a tag written directly on the page.
        let frame_title = stt
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some(TEMPLATEDATA_FRAME_TITLE_ATTR))
            .and_then(|kv| kv.value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| self.page_title.borrow().clone());

        let mut node = crate::pipeline::templatedata::render(
            &body_src,
            &frame_title,
            self.config.language_code(),
        );

        // The extension encapsulation: `typeof`, `about`, `data-mw`.
        let about = stt
            .attribs
            .iter()
            .find(|kv| {
                kv.key.as_str() == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
            })
            .and_then(|kv| kv.value.as_str())
            .filter(|b| !b.is_empty())
            .map(|b| format!("#mwt{b}"))
            .unwrap_or_else(|| self.new_about_id(about_counter, "templatedata"));
        node.set_attr("typeof", "mw:Extension/templatedata");
        node.set_attr("about", about);
        node.set_attr(
            "data-mw",
            extension_data_mw("templatedata", &extension_kv_attrs(stt), Some(&body_src)),
        );
        Some(node)
    }

    /// Expand `<inputbox>` extension tokens in place, stashing each rendered form
    /// behind a `mw:dom-fragment-token` placeholder (mirrors `expand_templatedata`).
    fn expand_inputbox(
        &self,
        tokens: Vec<Item>,
        about_counter: &std::cell::Cell<usize>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        let mut out: Vec<Item> = Vec::new();
        for item in tokens {
            let Some(stt) = inputbox_target(&item) else {
                out.push(item);
                continue;
            };
            let stt = stt.clone();
            match self.build_inputbox(&stt, about_counter, next_id) {
                Some(node) => {
                    let mut frag = crate::dom::node::Node::document();
                    frag.push_child(node);
                    let id = next_id.get();
                    next_id.set(id + 1);
                    fragments.insert(id, frag);
                    out.push(emit_dom_fragment_placeholder(&stt, id));
                }
                None => out.push(item),
            }
        }
        out
    }

    /// Build the `<div class="mw-inputbox-centered">` for one `<inputbox>` tag.
    /// Port of `InputBox::render`'s search forms; other types fall back to the
    /// generic `<extension>` (the caller leaves the token).
    fn build_inputbox(
        &self,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        about_counter: &std::cell::Cell<usize>,
        next_id: &std::cell::Cell<usize>,
    ) -> Option<Node> {
        use crate::pipeline::extension_handler::{extension_data_mw, extension_kv_attrs};
        let ext_src = stt
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("source"))
            .and_then(|kv| kv.value.as_str())
            .unwrap_or_default()
            .to_string();
        let body_src = crate::pipeline::extension_handler::extract_ext_body(stt, &ext_src);
        let options = crate::pipeline::inputbox::Options::parse(&body_src);

        // `wfRandom()`: `0.` + 12 digits, used for the checkbox ids. The mask in
        // the comparator ignores the value, so a deterministic one keeps rustoid's
        // own output reproducible.
        let seed = next_id.get() as u64;
        let rand_suffix = format!(
            "0.{:012}",
            seed.wrapping_mul(2_654_435_761).wrapping_add(12_345) % 1_000_000_000_000
        );
        let mut node = options.render(self.config, &rand_suffix)?;

        // The extension encapsulation: `typeof`, `about`, `data-mw`.
        let about = stt
            .attribs
            .iter()
            .find(|kv| {
                kv.key.as_str() == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
            })
            .and_then(|kv| kv.value.as_str())
            .filter(|b| !b.is_empty())
            .map(|b| format!("#mwt{b}"))
            .unwrap_or_else(|| self.new_about_id(about_counter, "inputbox"));
        node.set_attr("typeof", "mw:Extension/inputbox");
        node.set_attr("about", about);
        node.set_attr(
            "data-mw",
            extension_data_mw("inputbox", &extension_kv_attrs(stt), Some(&body_src)),
        );
        Some(node)
    }

    /// Resolve one `<templatestyles>` extension token into a placeholder, stashing
    /// the resolved stylesheet for the chunk's extension pass to number.
    ///
    /// The `about` id is deliberately *not* taken here — see
    /// [`Parser::number_style_placeholders`] — so the placeholder carries only a
    /// fragment id, and an expansion that is later rendered back to source spends
    /// no id at all.
    ///
    /// Returns the token unchanged when the tag cannot be resolved (no source, no
    /// page, no revision) — leaving it visible in the diff rather than silently
    /// emitting an empty stylesheet.
    async fn expand_one_templatestyles(
        &self,
        item: &Item,
        source: Option<&dyn DataSource>,
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

        let wrapper = attr("wrapper").map(|s| s.to_string());
        let css = crate::pipeline::templatestyles::render(&body, wrapper.as_deref());
        // Whether the tag had content, which decides whether the resolved `<style>`
        // records `body` in its `data-mw`. A literal `<templatestyles src="…"/>`
        // has none, while the `frame:extensionTag{name='templatestyles', …}` form
        // Scribunto emits is a tag pair with empty content — and the service keeps
        // the two apart in the served bytes. The tokenizer records the matched end
        // tag as `extTagOffsets`, which is exactly that distinction.
        let has_body = stt.data_parsoid.ext_tag_offsets.is_some();
        // The `about` id is not taken yet: the chunk's extension pass numbers the
        // placeholder (see [`Self::number_style_placeholders`]). Until then the
        // resolved stylesheet waits here, keyed by the fragment id the placeholder
        // carries — an entry a discarded placeholder never drains spends no id.
        let id = self.ext_next_id.get();
        self.ext_next_id.set(id + 1);
        self.pending_styles.borrow_mut().insert(
            id,
            PendingStyle {
                css,
                revid,
                src,
                wrapper,
                has_body,
            },
        );
        emit_style_placeholder(stt, id)
    }

    /// Assign `about` ids to the templatestyles placeholders in a chunk, in
    /// document order, and build the stashed `<style>` fragments.
    ///
    /// Parsoid's TT2 runs `TemplateHandler` over the whole chunk and only then
    /// `ExtensionHandler`, so a chunk's transclusions take their ids first and
    /// its extensions follow *in source order* — an id-allocating extension that
    /// precedes a `{{template}}` is still numbered after it. Resolving a
    /// `<templatestyles>` where the walk reaches it would spend the id before the
    /// later transclusion's, which the transform endpoint rejects:
    /// `<templatestyles src="Plainlist/styles.css"/>{{Center|b}}` is
    /// `#mwt2`/`#mwt1` on the service and was `#mwt1`/`#mwt2` here.
    ///
    /// A placeholder whose fragment id is not pending was already numbered by a
    /// nested chunk's pass (the fragment is stashed then), so it is skipped; a
    /// pending entry whose placeholder never reached a token stream is never
    /// drained and spends nothing.
    fn number_style_placeholders(&self, out: &mut [Item], about_counter: &std::cell::Cell<usize>) {
        // The ids numbered in *this* pass. A template that splices one parameter
        // value twice puts the same placeholder in the chunk twice; the second
        // copy is a new occurrence and must be numbered like one.
        let mut numbered = std::collections::HashSet::new();
        for item in out.iter_mut() {
            self.number_style_placeholder(item, about_counter, &mut numbered);
        }
    }

    /// Number one `<templatestyles>` placeholder in place, draining its pending
    /// stylesheet into `ext_fragments`. Returns whether `item` was such a
    /// placeholder, so the caller can number extensions and styles in one
    /// source-order pass (a reflist's `<templatestyles>` precedes its
    /// `<references>`, and the two ids must follow that order).
    fn number_style_placeholder(
        &self,
        item: &mut Item,
        about_counter: &std::cell::Cell<usize>,
        numbered_here: &mut std::collections::HashSet<usize>,
    ) -> bool {
        let Item::Tok(ParsoidToken::Tag(t)) = item else {
            return false;
        };
        if t.name != "style" {
            return false;
        }
        let is_fragment = t.attribs.iter().any(|kv| {
            kv.key.as_str() == Some("typeof") && kv.value.as_str() == Some("mw:DOMFragment")
        });
        if !is_fragment {
            return false;
        }
        let Some(id) = t
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("data-fragment-id"))
            .and_then(|kv| kv.value.as_str())
            .and_then(|v| v.parse::<usize>().ok())
        else {
            return false;
        };
        let pending = self.pending_styles.borrow_mut().remove(&id);
        let repeated_here = !numbered_here.insert(id);
        let Some(pending) = pending else {
            if !repeated_here {
                return true;
            }
            // The same placeholder, twice in one chunk: a template body that
            // uses one parameter value at two places. The first copy was
            // numbered above; this one is another occurrence, so it gets its
            // own copy of the stylesheet and its own `about`. `DedupeStyles`
            // then serves it as the `<link>` the service emits for a repeated
            // stylesheet (`Hydrogen`'s second `{{chem2|H}}` lost it, and every
            // id after it was one behind).
            let Some(existing) = self.ext_fragments.borrow().get(&id).cloned() else {
                return true;
            };
            let new_id = self.ext_next_id.get();
            self.ext_next_id.set(new_id + 1);
            let about = self.new_about_id(about_counter, "templatestyles");
            let mut node = existing;
            if let Some(style) = node.children.first_mut() {
                style.set_attr("about", about);
            }
            self.ext_fragments.borrow_mut().insert(new_id, node);
            for kv in t.attribs.iter_mut() {
                if kv.key.as_str() == Some("data-fragment-id") {
                    kv.value = crate::wikitext::tokens_v2::KeyValue::Str(new_id.to_string());
                }
            }
            return true;
        };
        let about = self.new_about_id(about_counter, "templatestyles");
        let node = crate::pipeline::templatestyles::style_node(
            &pending.css,
            pending.revid,
            &pending.src,
            pending.wrapper.as_deref(),
            &about,
            pending.has_body,
        );
        self.ext_fragments
            .borrow_mut()
            .insert(id, Node::document_with_child(node));
        true
    }

    /// Resolve one `<indicator>` extension token into a placeholder plus a
    /// stashed `<meta>` fragment.
    ///
    /// The `about` id is **not** taken here. Parsoid's TT2 numbers a chunk's
    /// extensions only after its templates (`TemplateHandler` runs over the
    /// whole chunk, then `ExtensionHandler`), and in source order among
    /// themselves — the rule [`Self::number_style_placeholders`] applies to
    /// `<templatestyles>`, and it holds for indicators too: on
    /// `<templatestyles…/><indicator…>` the service numbers the stylesheet
    /// `#mwt1` and the indicator `#mwt2`, while an inline spend numbered the
    /// indicator first. The id belongs to the position the placeholder ends up
    /// in, not the position the token is first *seen*: one spliced in from a
    /// template argument or a module's output is seen while the argument is
    /// expanded — before the callee's own stylesheets that precede it in the
    /// output. `Berlin`'s infobox `coordinates` argument took `#mwt7` there
    /// where the service numbers the infobox's first stylesheet `#mwt7` and
    /// the indicator `#mwt16`.
    ///
    /// The chunk post-pass ([`Self::number_indicator_placeholder`], alongside
    /// the stylesheet pass) gives the placeholder its id, and the tree builder
    /// carries it onto the `<meta>`. A chunk whose extensions are deferred
    /// (`style_defer`/`arg_expansion`) skips that pass, leaving the placeholder
    /// unnumbered for the chunk it lands in.
    ///
    /// Returns `None` when `item` is not an indicator token, so the caller can
    /// fall through to the other extension paths.
    fn expand_one_indicator(&self, item: &Item) -> Option<Vec<Item>> {
        let stt = indicator_target(item)?;
        let body = extension_body(stt);
        let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
        let node = Node::document_with_child(crate::pipeline::extension_handler::indicator_node(
            &body, &attrs,
        ));
        let id = self.ext_next_id.get();
        self.ext_next_id.set(id + 1);
        self.ext_fragments.borrow_mut().insert(id, node);
        Some(emit_indicator_placeholder(stt, id))
    }

    /// Number one `<indicator>` placeholder in place, spending an `about` id
    /// from the same source-order pass that numbers the chunk's
    /// `<templatestyles>` (see [`Self::expand_one_indicator`]).
    ///
    /// Parsoid's `ExtensionHandler::onDocumentFragment` sets the id it
    /// allocates as the `about` of every top-level node of the extension's
    /// output, so the `<meta>` an indicator renders to carries it. The
    /// placeholder is what reaches the output, and the tree builder carries its
    /// attributes (minus the fragment indirection) onto the element it inserts,
    /// so the id travels there. Returns whether `item` was such a placeholder.
    fn number_indicator_placeholder(
        &self,
        item: &mut Item,
        about_counter: &std::cell::Cell<usize>,
    ) -> bool {
        let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item else {
            return false;
        };
        if t.name != "mw:dom-fragment-token" {
            return false;
        }
        let is_indicator = t.attribs.iter().any(|kv| {
            kv.key.as_str() == Some("typeof") && kv.value.as_str() == Some("mw:Extension/indicator")
        });
        if !is_indicator {
            return false;
        }
        if t.attribs.iter().any(|kv| kv.key.as_str() == Some("about")) {
            return true;
        }
        let about = self.new_about_id(about_counter, "indicator");
        t.add_attribute_str("about", &about);
        true
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
    /// during expansion. Its placeholders are numbered here, in document order; a
    /// leftover tag is behind everything the expansion took, so this is the tail
    /// of the sequence either way.
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
            out.extend(self.expand_one_templatestyles(&item, source).await);
        }
        self.number_style_placeholders(&mut out, about_counter);
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
                ul.set_attr("about", self.new_about_id(about_counter, "gallery"));
            }
            let mut frag = crate::dom::node::Node::document();
            frag.push_child(ul);
            let id = next_id.get();
            next_id.set(id + 1);
            fragments.insert(id, frag);
            out.push(emit_dom_fragment_placeholder(stt, id));
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
        let tokens = self.render_links(tokens, &mut fragments, &next_id, context_title, None);
        let tokens = self.render_external_links(tokens, &mut fragments, &next_id, None);
        let tokens = self.render_behavior_switches(tokens);
        let tokens = self.render_language_variants(tokens);
        let (tokens, pre_fragments) = self.expand_wikitext_pre_sync(tokens, &next_id);
        fragments.extend(pre_fragments);
        let (tokens, wrapper_fragments) = self.expand_wrapper_tag_sync(tokens, &next_id);
        fragments.extend(wrapper_fragments);
        let (tokens, indicator_fragments) = self.expand_indicator_sync(tokens, &next_id);
        fragments.extend(indicator_fragments);
        let about_counter = std::cell::Cell::new(0usize);
        let tokens = self.expand_gallery_sync(tokens, &about_counter, &mut fragments, &next_id);
        let tokens = self.expand_syntaxhighlight(tokens, &about_counter, &mut fragments, &next_id);
        let stage = TreeBuilderStage::new(false);
        let mut ast =
            stage.to_ast_with_fragments(tokens, Some(wikitext), self.config, fragments, None);
        // Move expandable media-option fragments onto their containers before
        // `post_pwrap_transforms` unpacks placeholders into the tree.
        crate::pipeline::attr_mw_fragments::collect(&mut ast);
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
        crate::pipeline::p_wrap::run(&mut ast);
        // AddLinkAttributes now runs inside `post_pwrap_transforms`, between
        // tplwrap and `dom-unpack` (see that function).
        crate::pipeline::tree_builder_html::post_pwrap_transforms(
            &mut ast,
            &depths,
            Some(wikitext),
            Some(self.config),
        );
        crate::pipeline::cleanup::run(&mut ast);
        crate::pipeline::headings::gen_anchors(&mut ast);
        crate::pipeline::headings::dedupe_heading_ids(&mut ast);
        // Repair `<a>`-inside-`<a>` bad nesting created by DOM-fragment unpack.
        // (The sync path has no media resolution, so this runs after the other
        // DOM transforms here; the async `build_ast` runs it after `AddMediaInfo`.)
        crate::pipeline::unpack_dom_fragments::fix_bad_nesting(&mut ast);
        wrap_sections_in_ast(&mut ast, wrap_sections);
        // Serialize the expandable media-option fragments into `data-mw.attribs`
        // (no id pass here, so their nodes carry no ids).
        crate::pipeline::attr_mw_fragments::serialize_into_data_mw(
            &mut ast,
            self.strip_data_parsoid.get(),
        );
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
        *self.page_source.borrow_mut() = page_source.to_string();
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
        // The revision the page is being parsed at, which `{{REVISIONID}}`
        // answers from. Asked of the source rather than threaded from the caller,
        // because the wikitext and its revision come from one fetch; a source
        // that does not track revisions reports `None`, which is MediaWiki's
        // "no revision" and makes the magic word answer empty rather than
        // inventing an id.
        let page_revision = match source {
            Some(source) => source
                .get_page_with_revision(&title)
                .await
                .ok()
                .flatten()
                .and_then(|(_, revision)| revision),
            None => None,
        };
        self.page_revision.set(page_revision);
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

        // TT2 is per chunk: PHP's `TokenHandlerPipeline` runs `TemplateHandler`,
        // `ExtensionHandler`, and `AttributeExpander` over one block at a time
        // (PHP `ParserPipelineFactory::STAGES`), so every id a handler spends
        // belongs to its chunk. rustoid used to run these as global passes, which
        // put a media *caption's* nested expansion after the whole page's
        // templates: the caption lives in the link's `mw:maybeContent` KV, which
        // `expand_attributes` expands (`Zebro`: parsoid `#mwt36`, rustoid
        // `#mwt119`). `fragments`/`next_id` are one document-wide space, so they
        // persist across the chunks.
        //
        // Only these three interleave. `render_links` stays a global pass: its
        // own id allocators (a media option's templatestyles) were already late
        // before, and interleaving it dropped the first `{{citation}}`'s CS1
        // `<style>` (the placeholder is a top-level token that a per-chunk pass
        // reached before the fragment was resolved).
        let mut fragments: std::collections::HashMap<usize, crate::dom::node::Node> =
            std::collections::HashMap::new();
        // One fragment-id space for the whole document, exactly as PHP's
        // `Env::newFragmentId` provides: a link's tunnelled caption, a
        // stylesheet, and a gallery all draw from the same counter, so no two
        // placeholders can share an id and resolve to each other's content.
        // `expand_one_templatestyles` allocates from this same counter while
        // expansion is still running.
        let next_id = &self.ext_next_id;
        let mut chunked: Vec<Item> = Vec::new();
        for chunk in split_into_chunks(tokens) {
            let chunk = self
                .expand_templates(
                    &frame,
                    chunk,
                    source,
                    about_counter,
                    false,
                    false,
                    page_source,
                    0,
                )
                .await;
            // TT2 order: ExtensionHandler runs after TemplateHandler and before
            // AttributeExpander. Attribute values are sub-pipelines, so the
            // extension handler must reach inside them before attributes are
            // expanded.
            let chunk = crate::pipeline::extension_handler::expand_in_attributes(
                chunk,
                self.config,
                &mut fragments,
                next_id,
            );
            let chunk = self
                .expand_attributes(
                    &frame,
                    chunk,
                    source,
                    about_counter,
                    Some(page_source),
                    &mut fragments,
                    next_id,
                )
                .await;
            // A media option that is a token array earns its container a fresh
            // `about` id (`renderFile`'s `$hasExpandableOpt`), allocated where the
            // media sits. rustoid renders media in a global post-expansion pass,
            // so the counter there is past the whole page; stamp the id the media
            // would have been given onto the file link instead (see
            // `MW_ABOUT_BASE_ATTR`), and advance the counter past it.
            let mut chunk = chunk;
            {
                let base = about_counter.get();
                let media_ctx =
                    crate::pipeline::wiki_link_render::WikiLinkContext::new(self.config);
                let mut index = 0usize;
                for item in &mut chunk {
                    if let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item
                        && t.attribs.iter().any(|kv| {
                            kv.key.as_str()
                                == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
                        })
                    {
                        // Numbered in source order by the expansion pass
                        // (`stamp_rich_extension_about`); leave it alone.
                        continue;
                    }
                    let is_gallery = gallery_target(item).is_some()
                        || imagemap_target(item).is_some()
                        || phonos_target(item).is_some()
                        || templatedata_target(item).is_some()
                        || inputbox_target(item).is_some()
                        || kartographer_target(item).is_some();
                    let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item else {
                        continue;
                    };
                    let is_media = t.name == "wikilink"
                        && is_file_link_target(
                            t.attribs
                                .iter()
                                .find(|kv| kv.key.as_str() == Some("href"))
                                .and_then(|kv| kv.value.as_str())
                                .unwrap_or_default(),
                        )
                        && crate::pipeline::wiki_link_render::has_expandable_option(
                            &media_ctx,
                            &ParsoidToken::SelfclosingTag(t.clone()),
                        );
                    if is_media || is_gallery {
                        t.add_attribute_str(
                            crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR,
                            (base + 1 + index).to_string(),
                        );
                        index += 1;
                    }
                }
                // Those `about`s are real allocations at this point in the
                // document, ahead of the next chunk: advance the counter so a
                // later id (e.g. a following `<ref>`) continues after them.
                if index > 0 {
                    about_counter.set(base + index);
                }
            }
            chunked.extend(chunk);
        }
        let tokens = chunked;
        // `<templatestyles>` (and other pre-built sub-fragments) are stashed in
        // `ext_fragments` while expansion runs. `render_links` renders a media
        // *caption* through its own sub-pipeline, which resolves placeholders
        // against the fragment map, so fold the stash in before it runs (a later
        // fold at the end of `build_ast` would leave the caption's `<style>`
        // unresolved: `[[File:…|thumb|{{#invoke:list|…}}]]`).
        fragments.extend(std::mem::take(&mut *self.ext_fragments.borrow_mut()));
        let tokens = self.render_links(
            tokens,
            &mut fragments,
            next_id,
            Some(&title),
            Some(about_counter),
        );
        let tokens =
            self.render_external_links(tokens, &mut fragments, next_id, Some(about_counter));
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
        let tokens = self
            .expand_imagemap(
                tokens,
                source,
                &frame,
                about_counter,
                &mut fragments,
                next_id,
            )
            .await;
        let tokens = self
            .expand_phonos(
                tokens,
                source,
                &frame,
                about_counter,
                &mut fragments,
                next_id,
            )
            .await;
        let tokens = self.expand_syntaxhighlight(tokens, about_counter, &mut fragments, next_id);
        let tokens = self
            .expand_mapframe(
                tokens,
                source,
                &frame,
                about_counter,
                &mut fragments,
                next_id,
            )
            .await;
        let tokens = self.expand_templatedata(tokens, about_counter, &mut fragments, next_id);
        let tokens = self.expand_inputbox(tokens, about_counter, &mut fragments, next_id);
        // `<templatestyles>` is normally resolved during expansion, so its `.about`
        // id is already spent; anything left over here is a tag the inline pass
        // could not resolve (a missing sheet, or one produced after expansion).
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
        // Move expandable media-option fragments onto their containers (and out of
        // view) before `post_pwrap_transforms` unpacks placeholders into the tree.
        crate::pipeline::attr_mw_fragments::collect(&mut ast);
        // Capture transclusion marker depth map over the freshly-built DOM
        // (before p-wrapping restructures it), mirroring PHP's
        // `transclusionMetaTagDepthMap` recorded at tree-build time.
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
        // DOM-level p-wrapping runs before transclusion encapsulation (mirrors
        // PHP's `pwrap` … `tplwrap` order).
        crate::pipeline::p_wrap::run(&mut ast);
        // AddLinkAttributes now runs inside `post_pwrap_transforms` (see
        // `wikitext_to_ast`).
        crate::pipeline::tree_builder_html::post_pwrap_transforms(
            &mut ast,
            &depths,
            Some(page_source),
            Some(self.config),
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
        // It runs *after* Cite here (see below), because a note's stylesheet is
        // moved out of the stashed ref body into the reference list by Cite, and
        // Parsoid's DedupeStyles sees the final DOM.
        // CleanUp (`cleanup`): drop empty elements and do the final fix-ups.
        crate::pipeline::cleanup::run(&mut ast);
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
            let render_body = |r: &crate::ext::cite::Reference| -> Node {
                // A body expanded at the ref site (see `expand_templates`) is the
                // faithful one: its templates were expanded there, and its own
                // extension ids were spent in the right place. Only a body that
                // never reached that pass — one built outside expansion, or
                // deferred inside an `#invoke` argument — falls back to the
                // synchronous inline render, which expands nothing.
                if let Some(about) = &r.body_about
                    && let Some(node) = self.ref_bodies.borrow().get(about)
                {
                    return node.clone();
                }
                // The `about` may have been overwritten by encapsulation; the body
                // text survives it.
                if let Some(node) = self.ref_bodies_by_body.borrow().get(&r.body) {
                    return node.clone();
                }
                let mut fragments = note_fragments.borrow_mut();
                render_inline_fragment(
                    self.config,
                    self.tokenize(&r.body).unwrap_or_default(),
                    &mut fragments,
                    &next_note_id,
                    Some(&crate::title::TitleParser::parse(
                        &self.page_title.borrow(),
                        self.config,
                    )),
                )
            };
            // A `<ref>` nested in a transcluded note (a `#tag:ref` body) is rendered
            // by Cite into the note body, so it needs the body's stashed node by
            // wikitext — the same lookup `render_body` falls back to below. Only a
            // body parsed with `inTemplate` can hold one; Parsoid drops a nested
            // `<ref>` in any other body, so returning the node for one would let
            // Cite collect a ref the service never saw.
            let nested_body = |text: &str| -> Option<Node> {
                if !self.nested_ref_bodies.borrow().contains(text) {
                    return None;
                }
                self.ref_bodies_by_body.borrow().get(text).cloned()
            };
            // A `<references>` element's **list-defined** refs
            // (`{{reflist|refs=<ref name="A">…</ref>}}`), which Parsoid parses into
            // the element's children and Cite collects at its position. rustoid
            // keeps the element childless, so its body comes from the stash
            // instead (see [`Parser::references_bodies`]).
            let references_body =
                |text: &str| -> Option<Node> { self.references_bodies.borrow().get(text).cloned() };
            crate::ext::cite::run(
                &mut ast,
                &page_title_prefixed,
                &mut ids,
                &render_body,
                &nested_body,
                &references_body,
            );
            // Hand the advanced counter back: ids are allocated in document
            // order, so anything numbered after Cite must continue from here.
            about_counter.set(ids.about_counter());
        }
        // Cite splices the note bodies into the reference list *after* the
        // link-neighbour pass above, so their links never got their trails
        // (`[[glacier]]s` served as `<a>glacier</a>s`). Parsoid had already
        // parsed those bodies at extension time, which is why its pass saw
        // them. Running the pass again is safe: a link whose trail was already
        // merged has no adjacent text left.
        crate::pipeline::handle_link_neighbours::run(&mut ast, self.config);
        // DisplaySpace (`displayspace`): armor French spaces. PHP runs it as a
        // global DOM pass after extension post-processing (`extpp`) and before
        // `gen-anchors`, which is why it is here and not before Cite: a note's
        // body is spliced into the reference list by Cite, and only then does it
        // exist to armor.
        crate::pipeline::display_space::run(&mut ast);
        crate::pipeline::headings::gen_anchors(&mut ast);
        crate::pipeline::headings::dedupe_heading_ids(&mut ast);
        // AddRedLinks: resolve which wikilink targets exist, marking missing
        // ones as red links and redirect targets with `mw-redirect`. It runs
        // *after* Cite because a note's body only reaches the DOM when Cite
        // renders the reference list, and Parsoid's AddRedLinks sees the final
        // DOM: a `{{cite journal}}` note's `[[Doi (identifier)|doi]]` link
        // carries `mw-redirect` on the served page.
        //
        // `titles` are gathered from the final tree, so the notes' targets are
        // batch-checked with everything else.
        let mut titles = Vec::new();
        crate::pipeline::add_red_links::collect_wikilink_titles(&ast, &mut titles);
        if !titles.is_empty() {
            let page_info = match source {
                Some(source) => source.get_page_info(&titles).await.unwrap_or_default(),
                None => std::collections::HashMap::new(),
            };
            crate::pipeline::add_red_links::run(&mut ast, &page_info, &page_title_prefixed);
        }
        // DedupeStyles runs over the tree just made final by Cite: a note's
        // `<templatestyles>` is emitted full at its first occurrence and as a
        // `mw-deduplicated-inline-style` link afterwards.
        crate::pipeline::dedupe_styles::run(&mut ast);
        // PHP's `CleanUp` (which records `DISCARDABLE_DP`) runs *before*
        // `WrapSections`, so the section wrappers the next pass creates are not
        // marked discardable — and so take page-bundle ids even inside a
        // transclusion range. Running the marking after wrapping marked them,
        // and every id after the first section shifted down (`Template:Weather
        // box`: `mwCQ` where the service has `mwCg`).
        //
        // Every earlier pass keeps its `dp`: `wrap_sections` only reads a
        // heading's `stx`, and a heading is one of the nodes `discard_node`
        // deliberately keeps.
        crate::pipeline::cleanup::mark_discardable_data_parsoid(&mut ast);
        wrap_sections_in_ast(&mut ast, options.wrap_sections);
        // Core appends the table-of-contents placeholder to section 0 of a
        // main-namespace page that has enough headings and no `__TOC__`/
        // `__NOTOC__`. It needs the sections to already exist, so it runs here.
        crate::pipeline::auto_toc::insert(&mut ast, title.namespace_id == 0);
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
        // Serialize the expandable media-option fragments into `data-mw.attribs`
        // (after the id pass, which numbers them; without it in standalone mode,
        // where their nodes carry no ids).
        crate::pipeline::attr_mw_fragments::serialize_into_data_mw(
            &mut ast,
            self.strip_data_parsoid.get(),
        );
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
    ///
    /// A stashed extension token (a `<ref>`, `<nowiki>`, `#tag:…`) is carried
    /// the same way. The preprocessor hands a module a strip marker for every
    /// extension, so rendering the source back and re-tokenizing it in the
    /// module's output would both spell a tag the service stashed and spend a
    /// second id for a `#tag:ref` whose `about` the expansion already spent —
    /// the double-walk the `<indicator>` had (see `expand_one_indicator`). The
    /// marker keeps the token, which `substitute_strip_markers` splices back
    /// unchanged.
    ///
    /// The answer is the expansion's *text*, so a stale recorded source (one that
    /// still spells a `{{…}}` the frame expanded, like `Template:Div col`'s
    /// `<div class="div-col {{#ifeq:…}}" {{#if:…}}>`) is rebuilt from the token's
    /// live attributes — the module must be handed the expansion, not the
    /// construct. See [`crate::pipeline::lua_deferred::MissingSrc`].
    fn render_answer_markers(&self, items: &[Item]) -> String {
        self.render_markers(items, crate::pipeline::lua_deferred::MissingSrc::Html)
    }

    /// Render an `#invoke` argument value as the text Scribunto receives. Shares
    /// [`Self::render_answer_markers`]'s policy: Scribunto is handed the
    /// expansion's text either way, and a spelled element or rebuilt stale source
    /// comes back, when the module's output is re-parsed, as a keyed node with an
    /// id. Rebuilding instead is what keeps `Template:Infobox`'s `id="mwCA"`.
    fn render_argument_markers(&self, items: &[Item]) -> String {
        self.render_markers(items, crate::pipeline::lua_deferred::MissingSrc::Html)
    }

    /// Shared body of the two renderers: marker each placeholder (or extension)
    /// and hand the rest to the answer renderer under `missing_src`.
    fn render_markers(
        &self,
        items: &[Item],
        missing_src: crate::pipeline::lua_deferred::MissingSrc,
    ) -> String {
        let mut replaced: Vec<Item> = Vec::with_capacity(items.len());
        let mut i = 0;
        while i < items.len() {
            match answer_marker_span(items, i) {
                Some((end, tokens, tag)) => {
                    let marker = self.record_marker(&tag, tokens);
                    replaced.push(Item::Str(marker));
                    i = end;
                }
                None => {
                    // An extension nested in an attribute value — a template
                    // body's `title="{{#tag:nowiki|…}}"` — is stashed the same
                    // way, inside the value.
                    replaced.push(self.markerize_nested(&items[i]));
                    i += 1;
                }
            }
        }
        crate::pipeline::lua_deferred::render_answer_with(&replaced, missing_src)
    }

    /// Record `tokens` under a freshly numbered strip marker and return its text.
    fn record_marker(&self, tag: &str, tokens: Vec<Item>) -> String {
        let marker = strip_marker(tag, self.strip_markers.borrow().len());

        self.strip_markers
            .borrow_mut()
            .insert(marker.clone(), tokens);
        marker
    }

    /// The wikitext body for a `#tag` extension's content.
    ///
    /// Parsoid never hands an extension the *tokens* `#tag` collected: it
    /// serializes them back to wikitext and re-parses it (`TemplateHandler::
    /// expandTemplate`'s `preparePFragment` + `processTemplateSource`), replacing
    /// every piece with no wikitext form by a fragment marker
    /// (`PipelineUtils::pFragmentToParsoidFragmentMarkers`). rustoid's serializer
    /// dropped such a piece instead — `tag_content_source` cannot rebuild the
    /// whole list, and `tag_content_fallback_source` writes nothing for a token
    /// without a `src`. That is how a `<templatestyles>` resolved *before* the
    /// call (the placeholder a template argument's expansion left) vanished from
    /// `{{refn|name=A|{{cite web|…}}}}`, leaving `cite_note-A-1` with no
    /// stylesheet and its `about` id unspent.
    ///
    /// The marker is rustoid's own strip marker, recorded in
    /// [`Parser::strip_markers`]; `expand_templates`'s `substitute_strip_markers`
    /// splices the piece back once the body has been tokenized — the same path a
    /// module's `frame:extensionTag` answer takes. Only a placeholder (a
    /// `mw:DOMFragment` tag, or the `mw:dom-fragment-token` it wraps) is marked,
    /// with its matching end tag when it is a pair: a token that legitimately has
    /// no source in the body is *meant* to drop.
    fn tag_body_source(&self, content: &[Item]) -> String {
        if let Some(src) = tag_content_source(content) {
            return src;
        }
        let mut replaced: Vec<Item> = Vec::with_capacity(content.len());
        let mut i = 0;
        while i < content.len() {
            match dom_fragment_placeholder_at(content, i) {
                Some((end, tokens)) => {
                    let marker = self.record_marker("tag", tokens);
                    replaced.push(Item::Str(marker));
                    i = end;
                }
                None => {
                    replaced.push(content[i].clone());
                    i += 1;
                }
            }
        }
        crate::pipeline::parser_functions::tag_content_fallback_source(&replaced)
    }

    /// Replace an `extension` token nested in an attribute value with a strip
    /// marker, at any depth.
    ///
    /// `PPFrame_Hash::expand` stashes *every* extension tag the expansion holds,
    /// wherever it sits, so `title="{{#tag:nowiki|…}}"` reaches a module as
    /// `title="<UNIQ--nowiki-…-QINU>"`. rustoid spelled the tag's own source
    /// instead, which is both the wrong bytes and the wrong length — the module
    /// measures the answer into the navbox `argHash`.
    ///
    /// Only a *nested* extension is replaced: one at the answer's top level is
    /// [`answer_marker_span`]'s (when numbered) or is spelled by
    /// [`crate::pipeline::lua_deferred::render_answer_with`].
    fn markerize_nested(&self, item: &Item) -> Item {
        let Item::Tok(tok) = item else {
            return item.clone();
        };
        let attribs = tok.get_attribs();
        if !attribs.iter().any(kv_holds_tokens) {
            return item.clone();
        }
        let new_attribs: Vec<crate::wikitext::tokens_v2::KV> = attribs
            .iter()
            .map(|kv| crate::wikitext::tokens_v2::KV {
                key: self.markerize_value(&kv.key),
                value: self.markerize_value(&kv.value),
                src_offsets: kv.src_offsets.clone(),
                ksrc: kv.ksrc.clone(),
                vsrc: kv.vsrc.clone(),
            })
            .collect();
        let mut new_tok = tok.clone();
        new_tok.set_attribs(new_attribs);
        Item::Tok(new_tok)
    }

    /// [`Self::markerize_nested`] over a `KeyValue` that holds a token list.
    fn markerize_value(
        &self,
        kv: &crate::wikitext::tokens_v2::KeyValue,
    ) -> crate::wikitext::tokens_v2::KeyValue {
        use crate::wikitext::tokens_v2::KeyValue;
        let KeyValue::Tokens(items) = kv else {
            return kv.clone();
        };
        // A templatestyles placeholder can sit *inside* a value — a wikilink
        // label (`[[X|{{color|…}}]]`) carries it as a `<style typeof="mw:
        // DOMFragment">` tag pair. Carrying it as one strip marker keeps the
        // argument's measured length right (`Module:Navbox` sums them) and
        // lets `substitute_strip_markers` splice the stylesheet back.
        let mut out = Vec::with_capacity(items.len());
        let mut i = 0;
        while i < items.len() {
            match answer_marker_span(items, i) {
                Some((end, tokens, tag)) => {
                    out.push(Item::Str(self.record_marker(&tag, tokens)));
                    i = end;
                }
                None => {
                    out.push(self.markerize_extension(&items[i]));
                    i += 1;
                }
            }
        }
        KeyValue::Tokens(out)
    }

    /// Replace an `extension` token with a strip marker, or recurse into a
    /// token's attributes. Only reached from within an attribute value, where an
    /// extension is nested by construction.
    fn markerize_extension(&self, item: &Item) -> Item {
        if let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item
            && t.name == "extension"
        {
            let tag = extension_name(t).unwrap_or_else(|| "extension".to_string());
            return Item::Str(self.record_marker(&tag, vec![item.clone()]));
        }
        self.markerize_nested(item)
    }

    /// Splice remembered strip markers back into their placeholder tokens.
    ///
    /// The counterpart to [`Parser::render_answer_markers`]: a module's output
    /// is parsed as wikitext, so a marker that reaches a token stream becomes
    /// the fragment placeholder it stood for, exactly where the module left it.
    ///
    /// A marker may sit *inside* an attribute value — a module's
    /// `frame:expandTemplate('Tooltip')` answer spells the span's `title` as
    /// `title="<UNIQ--nowiki-…-QINU>"` (see [`Self::markerize_extension`]) — so
    /// the splice reaches into a token's attributes as well as its top level.
    fn substitute_strip_markers(&self, tokens: Vec<Item>) -> Vec<Item> {
        // Fast path: nothing carries a marker — neither a top-level text run nor
        // a token's attribute value — so the stream is returned untouched rather
        // than cloned token by token.
        if !items_carry_marker(&tokens) {
            return tokens;
        }
        self.splice_marker_runs(tokens)
            .into_iter()
            .map(|it| self.substitute_in_attribs(it))
            .collect()
    }

    /// Splice the *top-level* marker runs of `tokens`; the attribute recursion is
    /// [`Self::substitute_strip_markers`]'s.
    fn splice_marker_runs(&self, tokens: Vec<Item>) -> Vec<Item> {
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

    /// Resolve markers a token carries in its attributes, at any depth.
    fn substitute_in_attribs(&self, item: Item) -> Item {
        let Item::Tok(mut tok) = item else {
            return item;
        };
        if !tok.get_attribs().iter().any(kv_carries_marker) {
            return Item::Tok(tok);
        }
        let resolved = tok
            .get_attribs()
            .iter()
            .map(|kv| self.resolve_kv_markers(kv))
            .collect();
        tok.set_attribs(resolved);
        Item::Tok(tok)
    }

    /// [`Self::substitute_in_attribs`] for one attribute (either half).
    fn resolve_kv_markers(
        &self,
        kv: &crate::wikitext::tokens_v2::KV,
    ) -> crate::wikitext::tokens_v2::KV {
        let mut out = kv.clone();
        out.key = self.resolve_value_markers(&kv.key);
        out.value = self.resolve_value_markers(&kv.value);
        out
    }

    /// [`Self::substitute_in_attribs`] for a `KeyValue`. A plain string that
    /// holds a marker becomes a token list, so the marker can be spliced.
    fn resolve_value_markers(
        &self,
        value: &crate::wikitext::tokens_v2::KeyValue,
    ) -> crate::wikitext::tokens_v2::KeyValue {
        use crate::wikitext::tokens_v2::KeyValue;
        match value {
            KeyValue::Str(s) if s.contains('\u{7f}') => {
                KeyValue::Tokens(self.substitute_strip_markers(vec![Item::Str(s.clone())]))
            }
            KeyValue::Tokens(items) => {
                // A marker inside an HTML attribute is written with its `\"`
                // armoring entity-encoded (`&quot;`), so the tokenizer gives
                // the value a `mw:Entity` span in the middle of the marker and
                // `substitute_strip_markers` cannot see it whole. Flatten the
                // decoded text first when that text holds a marker.
                if let Some(flat) = flatten_attribute_markers(items)
                    && flat.contains('\u{7f}')
                {
                    return KeyValue::Tokens(self.substitute_strip_markers(vec![Item::Str(flat)]));
                }
                KeyValue::Tokens(self.substitute_strip_markers(items.clone()))
            }
            _ => value.clone(),
        }
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
        // Table-data-block depth inherited from the caller: a branch or body
        // expanded while a table is open keeps recognizing cell separators
        // (PHP's `tableDataBlock`).
        initial_table_depth: usize,
    ) -> Vec<Item> {
        let _nesting = self.enter_expansion();
        // A marker a module held and moved becomes the fragment placeholder it
        // stood for, in the position the module left it.
        let tokens = self.substitute_strip_markers(tokens);
        // PHP's TT2 (`TokenHandlerPipeline::processChunk`) processes the page
        // **one top-level block at a time** — the tokenizer's `processChunkily`
        // yields one block per chunk — and runs each transformer over the whole
        // chunk, so a chunk's `TemplateHandler` precedes its `ExtensionHandler`.
        // The id order therefore resets per block: a template after a ref on the
        // *same* line still takes the lower id (`<ref>a</ref>{{1x|b}}` numbers the
        // template `#mwt1`), while a template on a *later* line does not (Zebro's
        // `{{reflist}}` takes `#mwt122`, not `#mwt8`). The flat token stream
        // carries the boundary on every top-level `Nl`.
        //
        // `table_depth` is deliberately *not* chunk state: a table spans lines,
        // so it is carried across the chunks here.
        let mut result = Vec::with_capacity(tokens.len());
        let mut table_depth = initial_table_depth;
        // Extension ids are not spent while an argument whose extensions are
        // numbered where it lands in the output is being expanded. See
        // [`Parser::arg_expansion`] and [`Parser::style_defer`]. The flags are
        // constant across one `expand_templates` call, so this is read once.
        let gated = self.in_arg_expansion() || self.in_style_defer();
        let mut chunks: Vec<Vec<Item>> = Vec::new();
        let mut chunk: Vec<Item> = Vec::new();
        for item in tokens {
            if !chunk.is_empty() && matches!(&item, Item::Tok(ParsoidToken::Nl(_))) {
                chunks.push(std::mem::take(&mut chunk));
            }
            chunk.push(item);
        }
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        for chunk in chunks {
            let mut out = Vec::new();
            for item in chunk {
                track_table(&item, &mut table_depth);
                let Item::Tok(tok) = &item else {
                    out.push(item);
                    continue;
                };
                let ParsoidToken::SelfclosingTag(stt) = tok else {
                    out.push(item);
                    continue;
                };

                // A `<templatestyles>` is resolved here so the CSS is fetched while
                // the expansion is running, but its `about` id is *not* taken yet:
                // this chunk's transclusions are numbered during the walk below, and
                // Parsoid numbers a chunk's extensions only after them. The id is
                // assigned by the `number_style_placeholders` post-pass at the end of
                // the chunk.
                if templatestyles_target(&item).is_some() {
                    let emitted = self.expand_one_templatestyles(&item, source).await;
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
                    // Core `Parser::braceSubstitution`'s T2529: a *nested*
                    // template or parser-function expansion that begins with a
                    // table or list marker is treated as beginning a new line.
                    // The page's own calls are expanded by Parsoid with
                    // `sol => true` and take no such newline (T382464), but
                    // their output is still tokenized at start of line, so the
                    // leading marker is re-tokenized either way. Core's
                    // `$piece['lineStart']` is strict: `$text[$i - 1] === "\n"`,
                    // so a directive indented with spaces does **not** start a
                    // line and gets the newline (`Template:Quote box`'s style,
                    // Isaac Newton).
                    let arg_value = self.in_arg_expansion();
                    let nested = body || arg_value;
                    let expanded = t2529_force_line_start(
                        expanded,
                        t2529_at_line_start(
                            &ParsoidToken::SelfclosingTag(stt.clone()),
                            src_text,
                            nested,
                        ),
                        nested,
                        // A module argument is handed over as *text*, so the
                        // newline belongs in that text (like an attribute
                        // value) and the marker is not re-tokenized here.
                        self.in_attr_expansion() || arg_value,
                    );
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
                    // PHP wraps the token with `mw:Param` only when
                    // `wrapTemplates && expandTemplates` (`onTemplateArg`), and on a
                    // nested pipeline (`body`) both are false — the body pipeline is
                    // run with `inTemplate => true, expandTemplates => false`. This
                    // mirrors the *template* token's own `wrap` below it. Reading
                    // only the caller's `in_template` wrapped every `{{{…}}}` in a
                    // template body, spending an `about` id each: `Template:Infobox
                    // drug`'s arguments took `#mwt8`–`#mwt25`, and the
                    // `Module:Infobox/styles.css` behind them landed at `#mwt26`
                    // where the service has `#mwt8`.
                    let wrap = !body && !in_template && self.wrap_templates.get();
                    let about_id = if wrap {
                        self.new_about_id(about_counter, "templatearg")
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
                        table_depth,
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
                if let Some(emitted) = self.expand_one_indicator(&item) {
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

            // Extension ids are not spent while an argument whose extensions are
            // numbered where it lands in the output is being expanded. See
            // [`Parser::arg_expansion`] and [`Parser::style_defer`].
            if gated {
                result.extend(out);
                continue;
            }
            // TT2's `ExtensionHandler` numbers every extension token with
            // `$env->newAboutId()`, and `TokenHandlerPipeline::processChunk` runs each
            // transformer over the *whole* chunk — `TemplateHandler` then
            // `ExtensionHandler` — so the chunk's templates are expanded and
            // numbered first, the extensions after. The id has to be spent here,
            // inside the expansion, because a pass that runs once the tree is
            // built cannot reproduce that order: `Bicycle`'s infobox `<ref>` is
            // `#mwt11` in the service and `#mwt199` here, the 188 in between being
            // infobox rows the late Cite pass ran past before numbering the ref.
            //
            // A `<ref>` with content also has its body expanded here, before the
            // marker's id is taken, because that is the order `onExtension`
            // produces: `sourceToDom` parses the body (spending the ids of any
            // extension it contains) and only then does `onDocumentFragment`
            // allocate the wrapper's. The body is stashed for Cite's list
            // renderer to move into the note.
            //
            // Only the extensions whose output consumes the id are numbered here.
            // `<ref>`/`<references>` keep it on the `<extension>` element the Cite
            // pass later reads; `<pre>` keeps it on the `<pre>` element (see
            // `extension_handler::pre_items`), and `<section>` on the void `<link>`
            // it renders (`extension_handler::section_items`). `<nowiki>` is lean
            // markup with no `about` at all. The remaining extensions are rebuilt
            // from their rich `data-mw` attribs (`extension_kv_attrs`), which do
            // not carry a token attribute, so numbering them here would spend an id
            // the output never shows — a second, different drift. They are left to
            // a follow-up.
            //
            // A token that already carries an `about` — one the nested expansion
            // that produced it already numbered — is left alone.
            //
            // A `<templatestyles>` is numbered in the same source-order pass: a
            // chunk's extensions take ids in document order, so a reflist's
            // `<templatestyles>` (written before its `<references>`) takes the
            // lower id. (The `<references>` reaches this pass only because the
            // parser-function branch that produces it deferred its numbering —
            // see the `begin_style_defer` around the branch expansion.)
            let mut numbered_styles = std::collections::HashSet::new();
            for item in out.iter_mut() {
                if self.number_style_placeholder(item, about_counter, &mut numbered_styles) {
                    continue;
                }
                if self.number_indicator_placeholder(item, about_counter) {
                    continue;
                }
                if self.stamp_rich_extension_about(item, about_counter) {
                    continue;
                }
                self.number_extension_token(item, source, frame, about_counter)
                    .await;
            }
            result.extend(out);
        }
        result
    }

    /// Stamp the `about` id a rich-rendered extension (`<gallery>`, `<imagemap>`,
    /// `<phonos>`, `<templatedata>`, `<inputbox>`) would be given at its chunk
    /// position, in the same source-order pass that numbers `<templatestyles>`.
    ///
    /// The output is built in a later pass, so the id travels on the token
    /// (`MW_ABOUT_BASE_ATTR`); it has to be *chosen* here, because that later
    /// pass reads a counter already past the chunk's stylesheets and so cannot
    /// interleave: `Help:Introduction`'s `<inputbox>` sits between two
    /// stylesheets and took a later id than the second. Returns whether `item`
    /// was such a token.
    fn stamp_rich_extension_about(
        &self,
        item: &mut Item,
        about_counter: &std::cell::Cell<usize>,
    ) -> bool {
        let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item else {
            return false;
        };
        if !matches!(
            extension_token_name(t),
            Some("gallery")
                | Some("imagemap")
                | Some("phonos")
                | Some("templatedata")
                | Some("inputbox")
                | Some("mapframe")
                | Some("maplink")
                | Some("syntaxhighlight")
        ) {
            return false;
        }
        if t.attribs.iter().any(|kv| {
            kv.key.as_str() == Some(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR)
        }) {
            return false;
        }
        let about = self.new_about_id(about_counter, "extension-id");
        let id = about.trim_start_matches("#mwt").to_string();
        t.add_attribute_str(crate::pipeline::wiki_link_render::MW_ABOUT_BASE_ATTR, id);
        true
    }

    /// Number one extension token in place, if it is a kind that carries an
    /// `about` and does not have one yet. A `<ref>`'s body is expanded (and
    /// stashed for Cite) before the about is taken, which is PHP's order:
    /// `ExtensionHandler::onExtension` calls `sourceToDom` first and only then
    /// allocates the wrapper's id in `onDocumentFragment`.
    ///
    /// `<nowiki>` is excluded (lean markup, no `about`), and so are the
    /// extensions rebuilt from rich `data-mw` attribs: their about would spend
    /// an id the output never shows.
    async fn number_extension_token(
        &self,
        item: &mut Item,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) {
        let (body, is_ref, is_references, arg_value, tag_content) = {
            let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item else {
                return;
            };
            if t.name != "extension" {
                return;
            }
            let name = extension_name(t);
            if !matches!(
                name.as_deref(),
                Some("ref") | Some("references") | Some("pre") | Some("section")
            ) {
                return;
            }
            if t.attribs.iter().any(|kv| kv.key.as_str() == Some("about")) {
                return;
            }
            let is_ref = name.as_deref() == Some("ref");
            let is_references = name.as_deref() == Some("references");

            // PHP parses the body in a *fresh* pipeline (`sourceToDom` on the
            // extension body), so `inTemplate` is false and a template inside a
            // note is a top-level transclusion that is wrapped — even when the
            // ref itself sits in a template argument (`{{Infobox|data1=<ref>{{tl|A}}</ref>}}`
            // numbers `{{tl}}`; the body is not the argument's body).
            //
            // A `<references>` body is its **list-defined** `ref`s
            // (`{{reflist|refs=<ref name="A">…</ref>}}`), which
            // `ReferenceListTagHandler::sourceToDom` parses into the element's
            // children before Cite collects them — they are ordinary refs there,
            // so their bodies are rendered and stashed like any other.
            let body = if is_ref || is_references {
                extension_body(t)
            } else {
                String::new()
            };
            // A `#tag`-produced ref's body is that call's argument value, which
            // the preprocessor expands with `inTemplate`; Parsoid therefore does
            // not wrap a template inside it (`{{#tag:ref|{{mono|EA}}}}` serves a
            // bare `mw:Extension/templatestyles`, unlike a literal
            // `<ref>{{mono|EA}}</ref>`). See [`crate::wikitext::tokens_v2::TempData::in_arg_value`].
            let arg_value = t.data_parsoid.tmp.in_arg_value;
            let tag_content = t.data_parsoid.tmp.tag_content.clone();
            (body, is_ref, is_references, arg_value, tag_content)
        };
        // Pre-rendering a body costs a full expansion. Only a body that can spend
        // an `about` id of its own needs it: a template (which may emit a
        // `<templatestyles>`, an `<indicator>`, …) or a literal extension tag.
        // A body with neither renders identically through Cite's synchronous
        // inline fallback, and leaves the id sequence untouched, so the expensive
        // path is skipped for the plain-text and bare-URL notes that large pages
        // carry in bulk.
        // A `#tag` call keeps its content tokens (`TempData::tag_content`); the
        // `source` it also carries has already dropped anything with no wikitext
        // form, so the body is serialized again here where a marker can stand in
        // for such a piece. See [`Parser::tag_body_source`].
        let render_body = match &tag_content {
            Some(content) => self.tag_body_source(content),
            None => body.clone(),
        };
        let needs_expansion = render_body.contains("{{") || render_body.contains('<');
        let rendered = if needs_expansion {
            // The note body is a *fresh* pipeline: its templates are numbered
            // whether or not they are wrapped. Whether they are wrapped follows
            // the ref's own context (`wikitextToDOM` forwards
            // `inTemplate => $this->inTemplate()`): a top-level note's body is
            // wrapped, one reached from inside a transclusion is not — the same
            // "inside a transclusion" test as the phantom id below, and the
            // service's output shows it (`Zebra`'s top-level `cite journal` note
            // keeps `mw:Transclusion mw:Extension/templatestyles`, a note inside
            // an Infobox argument does not). See [`Parser::wrap_templates`].
            let _unwrapped =
                (is_ref && self.expansion_nesting.get() > 1).then(|| self.expand_unwrapped());
            let mut body = Box::pin(self.process_fragment_body(
                &render_body,
                source,
                frame,
                about_counter,
                // A `#tag` body is the call's argument value, expanded with
                // `inTemplate`: its templates take no `about` of their own and
                // are not wrapped. `arg_value` carries that (the ref token is
                // marked by `tag_extension_token`). A `<ref>` nested in such a
                // body keeps its own body's transclusion ids, though: Parsoid
                // allocates them (they show as gaps) even where it emits no
                // wrapper, which the `nest > 1` `expand_unwrapped` path above
                // reproduces by suppressing only `wrap_templates`.
                /* in_template */
                arg_value,
            ))
            .await;
            // The note body reaches the final document when Cite builds the
            // reference list, i.e. *after* the page-level CleanUp pass. Parsoid's
            // global cleanup therefore sees it; trim its whitespace here so
            // `* {{cite…}}` loses the list marker's space. The rest of the pass
            // must wait for the real splice: an anchor whose content is still an
            // unpacked DOM-fragment placeholder would look empty.
            crate::pipeline::cleanup::trim_whitespace_in_subtree(&mut body);
            Some(body)
        } else {
            None
        };
        let about = self.new_about_id(about_counter, "extension-id");
        // A top-level `#tag`-produced ref makes the live Cite extension spend more
        // discarded ids when it picks the note for the reference list: one for the
        // note itself, plus one for every `mw:Extension/*` wrapper in the note it
        // splices in — including the wrappers of a nested `<ref>`'s own note, which
        // is spliced in the same pass. rustoid renders the note once and keeps the
        // node, so it has to spend the extra ids itself; see
        // [`Parser::count_note_extensions`]. Measured against the online oracle on
        // `x{{#tag:ref|BODY|name=o}}y\n\n{{reflist}}`, the count is exactly
        // `1 + <extension wrappers in BODY, following nested refs>`. A `#tag` call
        // *inside a template* is a different case and is left to the
        // `expansion_nesting > 1` block below (Sundial is a page of such refs and
        // does not spend the extra). A literal `<ref>` spends nothing here.
        if is_ref && arg_value && self.expansion_nesting.get() <= 1 {
            let extra = 1 + rendered
                .as_ref()
                .map_or(0, |n| self.count_note_extensions(n));
            for _ in 0..extra {
                let _ = self.new_about_id(about_counter, "extension-id");
            }
        }
        // A `<ref>` rendered from *inside a transclusion* makes the live Cite
        // extension spend a second, discarded `about` id, immediately after its
        // own. The live service shows it as a gap in the id sequence (`{{Geo|
        // ref=<ref>…</ref>}}` numbers the transclusion, the ref, a phantom id,
        // then the references list), and every later element is shifted by it.
        // A standalone Parsoid does not reproduce the spend, but the online
        // oracle is the target. [`Parser::expansion_nesting`] is what "inside a
        // transclusion" means here: a top-level ref sits at depth 1 and spends
        // nothing extra, one reached through any nested expansion spends one.
        if is_ref && self.expansion_nesting.get() > 1 {
            let _ = self.new_about_id(about_counter, "extension-id");
            // Each wrapper in the note's rendered body spends a further discarded
            // id — see [`Parser::count_note_extensions`] for the count and why it
            // follows a nested `<ref>`'s own note. This is the same rule the
            // top-level `#tag` case above applies; the two only differ in the
            // outer id they spend.
            if let Some(node) = rendered.as_ref() {
                for _ in 0..self.count_note_extensions(node) {
                    let _ = self.new_about_id(about_counter, "extension-id");
                }
            }
        }
        if let Item::Tok(ParsoidToken::SelfclosingTag(t)) = item {
            t.add_attribute_str("about", &about);
        }
        if let Some(node) = rendered {
            if is_references {
                // The definitions live in the `<references>` element's children;
                // Cite's collect walk reads them from here (see
                // [`Parser::references_bodies`]).
                self.references_bodies
                    .borrow_mut()
                    .insert(body.clone(), node.clone());
            }
            self.ref_bodies.borrow_mut().insert(about, node.clone());
            if !body.is_empty() {
                // A body parsed with `inTemplate` may hold a nested `<ref>`; see
                // [`Parser::nested_ref_bodies`].
                if arg_value {
                    self.nested_ref_bodies.borrow_mut().insert(body.clone());
                }
                self.ref_bodies_by_body
                    .borrow_mut()
                    .insert(body.clone(), node);
            }
        }
    }

    /// Extension wrappers in a rendered note body, *plus* those in the notes of
    /// any `<ref>` nested inside it.
    ///
    /// A ref's note is re-numbered when Cite splices it back into the page, and a
    /// nested `<ref>`'s own note is spliced in the same pass — so its wrappers are
    /// re-numbered too. A nested ref appears in the outer body's render as an
    /// `mw:Extension` placeholder, counted by [`is_note_extension`]; its note body
    /// lives in `ref_bodies`, keyed by that `about`, and is followed here. See
    /// [`Parser::number_extension_token`].
    fn count_note_extensions(&self, node: &Node) -> usize {
        let mut count = usize::from(is_note_extension(node));
        // A node whose `about` is a stashed note is a `<ref>`; its note is spliced
        // into the reference list alongside this one.
        let nested = node
            .get_attr("about")
            .and_then(|about| self.ref_bodies.borrow().get(about).cloned());
        if let Some(body) = nested {
            count += self.count_note_extensions(&body);
        }
        count
            + node
                .children
                .iter()
                .map(|c| self.count_note_extensions(c))
                .sum::<usize>()
    }

    /// The `mw:Transclusion` wrapper a literal `subst:` invocation renders as.
    ///
    /// `Parser::braceSubstitution` keeps a plain `subst:` literal outside PST —
    /// `virtualBracketedImplode( '{{', '|', '}}', … )` for the title and the
    /// arguments — and Parsoid serves the result as a transclusion whose *body is
    /// the call as written*. The live transform endpoint answers
    /// `A {{subst:X}} B` with
    ///
    /// ```text
    /// <span typeof="mw:Transclusion"
    ///       data-mw='…"target":{"wt":"subst:X","href":"./Template:Subst:X"}…'
    ///   >{{subst:X}}</span>
    /// ```
    ///
    /// so the body is the invocation source and the target names
    /// `Template:Subst:X` even though no such page exists. A `safesubst:` always
    /// expands, and the tokenizer has already stripped that prefix.
    ///
    /// [`Parser::expand_template_token`] calls this when it resolves such a
    /// target. [`Parser::reparse_bail_source`] calls it too, because a link bail
    /// re-tokenizes a source reconstructed from *expanded* text and a
    /// `{{subst:…}}` is the one invocation that survives that — it renders as its
    /// own source, and rebuilding it needs no fetch, so it is synchronous.
    #[allow(clippy::too_many_arguments)]
    fn encapsulate_subst_literal(
        &self,
        token: &ParsoidToken,
        // The invocation's source, which is the wrapper's body.
        src: &str,
        name: &str,
        title: &crate::title::Title,
        params: &crate::pipeline::parser_functions::Params,
        about_id: String,
        wrap: bool,
        // The text the parameters' source ranges index, for `data-mw`'s `wt`.
        source: &str,
    ) -> Vec<Item> {
        let body = vec![Item::Str(src.to_string())];
        if !wrap {
            return body;
        }
        let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
        let mut info = template_info_from(None, Some(name), vec![]);
        // `data-mw.target.wt` is the target as written (`subst:X`), not the page
        // it resolves to (`Template:Subst:X`), while `href` is that page.
        info.target_wt = raw_template_target(src).map(|t| t.trim_start().to_string());
        info.href = Some(crate::title::make_link(title, self.config));
        info.param_infos =
            crate::pipeline::template_encapsulator::prepare_tpl_param_infos(params, source);
        encap.encap_tokens(body, &info)
    }

    /// The `mw:Transclusion` wrapper a *missing* template renders as: a redlink to
    /// the page it named, with `data-mw` naming that call.
    ///
    /// `data-mw.target.wt` is the target **as written** and `href` is its link
    /// (`./Template:Foo`, fragment and all). Building both from the resolved DB
    /// key left `"wt":""` — no target at all — and an `href` without the `./`
    /// prefix, so every redlinked template on a page carried a wrong `data-mw`.
    #[allow(clippy::too_many_arguments)]
    fn missing_template_items(
        &self,
        name: &str,
        target_wt: &str,
        title: &crate::title::Title,
        params: &crate::pipeline::parser_functions::Params,
        about_id: String,
        token: &ParsoidToken,
        wrap: bool,
        page_source: &str,
    ) -> Vec<Item> {
        let link = crate::pipeline::template_handler::template_to_wikilink(name, self.config);
        if !wrap {
            return vec![link];
        }
        let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
        let mut info = template_info_from(None, None, vec![]);
        info.target_wt = Some(target_wt.to_string());
        info.href = Some(crate::title::make_link(title, self.config));
        info.param_infos =
            crate::pipeline::template_encapsulator::prepare_tpl_param_infos(params, page_source);
        encap.encap_tokens(vec![link], &info)
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
        let wrap = !body && !in_tpl && self.wrap_templates.get();

        // The id is taken whenever Parsoid processes the token, i.e. outside a
        // PHP-pre-expanded body. PHP's `TemplateEncapsulator` allocates one in
        // its constructor for every template token it handles, wrapped or not,
        // but on a *page* Parsoid never sees a body's nesting: the core
        // preprocessor expands it first, so a `{{…}}` written in a template's
        // source must not advance the counter. `body` (and `in_tpl` for exactly
        // the argument-value tokens spliced in the same way) carries that
        // context. Taking an id for these instead put every transclusion after a
        // busy template far ahead of the service: on `List of sovereign states`
        // the Redirect2 hatnote was `#mwt11` where the service has `#mwt2`,
        // because `Template:Short description`'s own expansion had consumed
        // 2…10. A fresh `<ref>` body is the opposite case: Parsoid *does* expand
        // it, so its templates spend ids even though `wrap_templates` suppresses
        // their wrappers — `take_id` deliberately ignores that flag.
        //
        // A variable or parser function takes its own id inside
        // `TemplateHandler::process`, which is a separate path.
        //
        // The suppression is `arg_value || body`, not `in_tpl || body`: PHP
        // constructs a `TemplateEncapsulator` (which allocates the id in its
        // constructor) for every `template` token Parsoid processes, whatever
        // `wrapTemplates` says, while core *pre-expanded* content is never seen
        // by Parsoid at all. A module's output is the former: it is re-parsed at
        // the caller's level, so a `{{…}}` that survives there spends an id even
        // though nothing is wrapped. `arg_value` is the former too — the value
        // is flattened to text for a module or a parser function and never
        // reaches Parsoid as tokens.
        let take_id = || {
            if arg_value || body {
                String::new()
            } else {
                self.new_about_id(about_counter, "template")
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

        // `Parser::braceSubstitution`'s SUBST block is handled in the
        // `ResolvedTarget::Template` arm below, where the target's title is
        // known: a plain `subst:` keeps the invocation literally *as a
        // transclusion of it*.

        // The target may hold a nested template
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

        // `Parser::braceSubstitution`'s SUBST block. A plain `subst:` is not
        // performed while a page is *rendered* (only a save substitutes it), so
        // the invocation is kept as written — and kept as a *transclusion of the
        // literal call*, which is what the live transform endpoint serves:
        //
        //   A {{subst:REVISIONUSER}} B
        //     → <span typeof="mw:Transclusion" data-mw='…"wt":"subst:REVISIONUSER"…'
        //         data-parsoid='{"pi":[[]],…}'>{{subst:REVISIONUSER}}</span>
        //
        // so the body is the call's own source and `data-mw`/`data-parsoid` name
        // `Template:Subst:X`. (A `safesubst:` always expands, and the tokenizer
        // has already stripped that prefix.) This replaced a bare literal: the
        // text matched, but a page that *renders* one lost the wrapper, and a
        // link bail that re-tokenized it resurrected a `<template>` element.
        if let Some(ResolvedTarget::Template { name, title }) = &resolved
            && crate::wikitext::tokenizer_v2::subst_is_literal(&target_str)
        {
            let src = stt
                .data_parsoid
                .src
                .clone()
                .unwrap_or_else(|| format!("{{{{{target_str}}}}}"));
            let subst_id = take_id();
            let produced = self.encapsulate_subst_literal(
                tok, &src, name, title, &params, subst_id, wrap, src_text,
            );
            for e in &produced {
                track_table(e, table_depth);
            }
            out.extend(produced);
            return out;
        }
        // The parser function's name, lower-cased, when this call is one. The
        // `#tag` and general argument-expansion passes below key off it.
        let pf_name = match &resolved {
            Some(ResolvedTarget::ParserFunction { name, .. }) => Some(name.to_ascii_lowercase()),
            _ => None,
        };
        // `#tag`'s arguments are expanded *eagerly*, unlike a template's. Core's
        // `CoreParserFunctions::tagObj` runs the inner (second) argument and every
        // named attribute's name and value through `$frame->expand` before it
        // builds the tag, because the tag's content and attributes are *strings*:
        // nothing re-expands them later. A template's arguments are the opposite —
        // they stay unexpanded until the body reads them — which is why this is
        // scoped to `#tag` rather than folded into `attribute_transform_manager`.
        //
        // Without it, `{{#tag:indicator|<body>|name=<expr>}}` recorded the
        // unexpanded `{{{…}}}`/`{{…}}` in its `data-mw`: `Template:Top icon`'s
        // indicator name came out `" "` and its `extsrc` still spelled
        // `{{{image|…}}}` (the first difference on Grand Theft Auto V and, once
        // its other breaks were cleared, on COVID-19 pandemic).
        let attribs = if pf_name.as_deref() == Some("tag") {
            self.expand_tag_args(frame, attribs, source, about_counter, src_text)
                .await
        } else {
            attribs
        };
        // Every *other* parser function reads its value arguments as strings, so
        // they are expanded in the calling frame before the call runs (see
        // [`Self::expand_parser_function_args`]). `#tag` did its own pass above,
        // and `#invoke` expands its arguments lazily inside Scribunto.
        let attribs = match pf_name.as_deref() {
            Some(name) if name != "tag" && name != "invoke" => {
                self.expand_parser_function_args(
                    frame,
                    name,
                    attribs,
                    source,
                    about_counter,
                    src_text,
                )
                .await
            }
            _ => attribs,
        };
        // Whether this call's result is a *string* the token stream re-tokenizes,
        // rather than the branch's own tokens. See
        // [`TemplateHandler::expands_branch_to_text`]; the `_` arm below is where
        // it takes effect.
        let branch_is_text = matches!(
            &resolved,
            Some(ResolvedTarget::ParserFunction { name, .. })
                if TemplateHandler::expands_branch_to_text(name)
        );
        // A `#property` answer is plain text that may be a bare URL, which only
        // the tokenizer turns into a link; there is no block-syntax trigger to
        // key on, so the branch always re-tokenizes.
        let force_branch_retokenize = matches!(
            &resolved,
            Some(ResolvedTarget::ParserFunction { name, .. })
                if name.eq_ignore_ascii_case("property")
        );
        // `#switch` keeps its branch's *tokens* (core returns them with
        // `PPFrame::RECOVER_ORIG`), so the text-branch marking above must not run.
        // A branch that carries a table start still needs it re-tokenized, though:
        // the value was tokenized inline as an argument, and
        // `Template:Largest cities` builds its whole table inside a `#switch`
        // branch, so without this the `{|`/`|-`/`!` lines stayed literal text.
        let retokenize_branch_table = matches!(
            &resolved,
            Some(ResolvedTarget::ParserFunction { name, .. })
                if name.eq_ignore_ascii_case("switch")
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
        // `#property` reads Wikidata, so its answer is also a fetch. Its `from`
        // argument has been expanded by now, which is what makes the entity id
        // known here.
        if let Some(ResolvedTarget::ParserFunction { name, pf_arg, .. }) = &resolved
            && name.eq_ignore_ascii_case("property")
        {
            let params = crate::pipeline::parser_functions::Params::new(attribs.clone());
            self.prime_property(pf_arg, &params, source).await;
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
                // Scribunto's `#invoke` is not an ordinary parser function:
                // everything after the colon is its argument list, and the
                // tokenizer has already split that on `|`. `params.args[0]` is
                // the target (`#invoke:Module`), the rest are the arguments.
                //
                // Only the *function name* is expanded up front. Scribunto does
                // exactly that — `$functionName = trim( $frame->expand( $args[1] ) )`
                // in `invokeHook` — because it has to pick the entry point before
                // the module runs. Every other argument is handed over
                // **unexpanded** and expanded when the module first reads it, so
                // the templates inside one spend their `about` ids at that moment,
                // not before the module has run. See
                // [`crate::pipeline::lua_deferred::ArgSlot`].
                let function = match params.args.get(1) {
                    Some(kv) => {
                        let expanded = self
                            .expand_invoke_args(
                                std::slice::from_ref(kv),
                                frame,
                                source,
                                about_counter,
                                body,
                                src_text,
                            )
                            .await;
                        expanded
                            .first()
                            .map(expanded_arg_pair)
                            .map(|(_, value)| value)
                            .unwrap_or_default()
                    }
                    None => String::new(),
                };
                // The module's arguments, raw. `raw_pairs` preserves the call as
                // written for the one path that cannot run it (no data source,
                // so nothing can be expanded).
                let raw_call_args: Vec<crate::wikitext::tokens_v2::KV> = params
                    .args
                    .get(2..)
                    .map(<[crate::wikitext::tokens_v2::KV]>::to_vec)
                    .unwrap_or_default();
                let raw_pairs: Vec<(Option<String>, String)> = raw_call_args
                    .iter()
                    .map(|kv| {
                        let name = crate::wikitext::token_utils::key_value_to_string(&kv.key)
                            .trim()
                            .to_string();
                        let value = kv_value_source(kv);
                        if name.is_empty() {
                            (None, value)
                        } else {
                            (Some(name), value)
                        }
                    })
                    .collect();
                // Scribunto's `frame:getParent()` is the frame of the *calling
                // template*: a `{{#invoke:}}` inside a template hands the module
                // that template's own arguments. They are read the same lazy way,
                // so they are passed raw as well; the parent's list has no
                // `#invoke:` target, so every entry is an argument — including the
                // first, which `{{see Wiktionary|…}}` hands to `Module:Hatnote`.
                let raw_parent_args: Vec<crate::wikitext::tokens_v2::KV> =
                    frame.args().args.clone();
                let Some(call) =
                    crate::lua::invoke::Invoke::from_parts(pf_arg, &function, raw_pairs)
                else {
                    return vec![Item::Str(format!("{{{{#invoke:{pf_arg}}}}}"))];
                };
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
                        raw_call_args,
                        raw_parent_args,
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
                        // The expansion's own pipeline only exists when Parsoid
                        // (not core) processes this token; a body's nesting is
                        // already part of the caller's expansion.
                        body,
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
                //
                // Parsoid's `TemplateEncapsulator` allocates its `about` id in
                // the constructor, *before* the target is resolved, so an
                // un-resolvable target still spends one; the id is never
                // attached to an element and shows only as a gap in the
                // sequence. `Module:Citation/CS1` writes its maintenance
                // message as `{{[[Template:Cite web|cite web]]}}`, which is
                // exactly this case (see `t5` in the reduction notes): without
                // the spend, every id after such a citation is one too low.
                let _ = take_id();
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
                    *table_depth,
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
                    &self.page_context(),
                    &self.ifexist.borrow(),
                    vec![expanded_item],
                    wrap,
                    // The id is spent whenever Parsoid expands the token, i.e.
                    // outside a PHP-pre-expanded body — even where `wrap` is
                    // suppressed for a fresh `<ref>` body. See
                    // [`Parser::wrap_templates`].
                    !in_tpl && !body,
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
                // Expansion happens *after* the wrapper is built,
                // so the wrapper's own id still precedes its children's, as on
                // the service.
                //
                // The branch is spliced into the *enclosing* chunk, so any
                // extension it produces must be numbered there, in document
                // order, not by this nested expansion's own post-pass. Numbering
                // it here put `Template:Reflist`'s `<references>` (from
                // `#tag:references` inside an `#if`) before the reflist's
                // `<templatestyles>`, which precedes it in the source. The frame
                // is the same — a parser function expands its branch in the
                // caller's frame — so deferring loses nothing.
                let expanded = {
                    let _defer = self.begin_style_defer();
                    Box::pin(self.expand_templates(
                        frame,
                        produced,
                        source,
                        about_counter,
                        in_tpl,
                        body,
                        src_text,
                        *table_depth,
                    ))
                    .await
                };
                let mut expanded = expanded;
                if branch_is_text {
                    // The branch answers a *string* in core, which the parser
                    // re-tokenizes: table/list syntax it produces must be
                    // recognized, and its wikitext targets were resolved before
                    // any attribute pass saw them. Re-tokenize the plain-text
                    // case so a `{{!}}`-born `|` becomes a real cell, then flag
                    // what is left — see
                    // [`TemplateHandler::expands_branch_to_text`] and
                    // [`crate::wikitext::tokens_v2::TempData::in_text_branch`].
                    expanded = re_tokenize_text_branch(
                        expanded,
                        self.config.extension_tags(),
                        BranchSyntax::Cells,
                        *table_depth > 0,
                        branch_starts_line(tok, src_text),
                        force_branch_retokenize,
                    );
                    mark_in_text_branch(&mut expanded);
                } else if retokenize_branch_table {
                    expanded = re_tokenize_text_branch(
                        expanded,
                        self.config.extension_tags(),
                        BranchSyntax::TableOnly,
                        *table_depth > 0,
                        // T2529 puts an expansion that starts with `{|` on a
                        // line of its own even when the call is mid-line.
                        true,
                        false,
                    );
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
        src_text: &str,
    ) -> Vec<crate::wikitext::tokens_v2::KV> {
        let Some(target) = attribs.first_mut() else {
            return attribs;
        };
        let crate::wikitext::tokens_v2::KeyValue::Tokens(items) = &target.key else {
            return attribs;
        };
        // A template may sit directly in the target (`{{ {{T}} }}`) or nested
        // inside a token-array attribute of one of the target's tokens
        // (`{{urlencode:[[User:{{T}}]]}}`, whose `wikilink` carries the `{{T}}` in
        // its `href`). PHP's `Frame::expand` walks the whole tree, so both must be
        // expanded before the target is resolved and stringified.
        if !target_holds_template(items) {
            return attribs;
        }
        let items = items.clone();
        // A `{{{…}}}` in the target is *substituted*, not wrapped: it belongs to
        // the template's name expression, and the service never emits an
        // `mw:Param` marker for one — PHP's `expandFirstAttribute` expands the
        // target through `AttributeTransformManager`, whose `$frame->expand`
        // substitutes the reference. Leaving it to `expand_templates` wrapped it
        // and spent an `about` id the service does not: `Template:Str ≥ len`'s
        // whole body is the five-brace `safesubst` idiom, so each of its two
        // target references was numbered where the service has the infobox
        // stylesheet.
        let items = frame.expand(&items);
        // The target is flattened to a *string* that becomes the template's
        // name, so nothing in it is wrapped: PHP's `expandFirstAttribute` runs
        // the chunk through `AttributeTransformManager`, and the service emits
        // no `mw:Transclusion` span for an inner call there.
        // `{{{{#ifeq:…}}|x}}` is the case that pins it — the inner `#ifeq`
        // answers the target, and wrapping it spent an `about` id the service
        // does not (the outer transclusion took `#mwt2` instead of `#mwt1`)
        // and put a span inside the name.
        let expanded = Box::pin(self.expand_templates(
            frame,
            items,
            source,
            about_counter,
            in_template,
            /* body */ true,
            src_text,
            0,
        ))
        .await;
        // The top-level pass reaches only the target's own tokens; one nested in
        // a token-array attribute survives it, so the attribute pass runs too.
        let expanded = Box::pin(self.expand_attrib_templates(
            frame,
            expanded,
            source,
            about_counter,
            in_template,
            /* body */ true,
            src_text,
        ))
        .await;
        target.key = crate::wikitext::tokens_v2::KeyValue::Tokens(expanded);
        attribs
    }

    /// Expand a `#tag` call's arguments the way core's `tagObj` does.
    ///
    /// `CoreParserFunctions::tagObj` runs the inner (second) argument and every
    /// named attribute's name and value through `$frame->expand`, so the tag is
    /// built from expanded strings. The target (argument zero) is already done by
    /// [`Self::expand_target_templates`]. A positional argument is the tag's
    /// inner content; `tagObj` expands it too, because the content is a string
    /// that nothing re-expands later — leaving it alone recorded
    /// `Template:Top icon`'s indicator body as the literal
    /// `[[File:{{{image|{{{imagename|…}}}}}}|…]]` instead of the resolved file.
    #[allow(clippy::too_many_arguments)]
    async fn expand_tag_args(
        &self,
        frame: &Frame,
        attribs: Vec<crate::wikitext::tokens_v2::KV>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        src_text: &str,
    ) -> Vec<crate::wikitext::tokens_v2::KV> {
        use crate::wikitext::tokens_v2::KeyValue;

        let mut out = attribs;
        // The tag's name (`ref` in `#tag:ref`). A `#tag:ref`'s content is the
        // reference *body*, which the Cite extension renders from the recorded
        // source; expanding it here is redundant, and on `India` it made the
        // render take minutes — the body's templates were expanded a second
        // time. The served `<ref>` records only `body.id`, no `extsrc`, so
        // nothing observable changes.
        let tag_name = out
            .first()
            .map(|kv| crate::wikitext::token_utils::key_value_to_string(&kv.key))
            .unwrap_or_default();
        let expand_content = !tag_name
            .rsplit(':')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("ref");

        for (i, kv) in out.iter_mut().enumerate() {
            if i == 0 {
                // The target, already expanded.
                continue;
            }
            // An empty key is the tag's inner content, a non-empty one a named
            // attribute (`name=value`).
            let key_empty = matches!(&kv.key, KeyValue::Str(s) if s.is_empty());
            if key_empty {
                if expand_content {
                    self.expand_tag_field(frame, &mut kv.value, source, about_counter, src_text)
                        .await;
                }
                continue;
            }
            self.expand_tag_field(frame, &mut kv.key, source, about_counter, src_text)
                .await;
            self.expand_tag_field(frame, &mut kv.value, source, about_counter, src_text)
                .await;
        }
        out
    }

    /// Expand one `#tag` argument field — the inner value, or an attribute's name
    /// or value — to the tokens the frame produces for it.
    ///
    /// The argument references belong to the *calling* frame; the nested templates
    /// then expand in a child frame with no arguments of its own, mirroring
    /// [`Self::expand_invoke_args`]. Expansion runs in template context
    /// (`in_template => true`) so nothing is wrapped in `mw:Transclusion` markers:
    /// the field is about to be flattened to a string, and a wrapper there would
    /// both leak its own source (`{{PAGENAME}}`) into the text and spend an `about`
    /// id the service does not spend.
    #[allow(clippy::too_many_arguments)]
    async fn expand_tag_field(
        &self,
        frame: &Frame,
        field: &mut crate::wikitext::tokens_v2::KeyValue,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        src_text: &str,
    ) {
        let child = frame.new_child(frame.title().clone(), vec![]);
        self.expand_field_tokens(&child, field, source, about_counter, src_text)
            .await;
    }

    /// Expand a parser function's argument values in the **calling** frame.
    ///
    /// Core's `ParserFunctions` implementations read each argument they use with
    /// `$frame->expand( … )` — the `#time` date, `#ifeq`'s second comparison, every
    /// `#switch` case name — and that call expands the argument's wikitext, nested
    /// parser functions and templates alike, through the caller's frame. rustoid's
    /// [`Frame::expand`] is synchronous and only substitutes `{{{…}}}`, so a nested
    /// `{{…}}` in an argument reached the function as its own source. In
    /// `Template:Film date`'s
    /// `{{#ifexpr: {{#time:U|{{{1}}}-{{#if:…}}…}} …}}` the `#time` was handed the
    /// literal `1986-{{#if:…}}-01`, answered `Error: Invalid time.`, and the film
    /// infobox lost the `#ifexpr` cell's `id="mwCQ"`.
    ///
    /// A function's *branch* arguments are left alone: the caller re-expands a
    /// returned branch in this same frame after the call, so expanding it here
    /// would be redundant and would discard the tokens standalone Parsoid keeps
    /// (the fixtures pin a `#switch` branch retaining a templated link target).
    #[allow(clippy::too_many_arguments)]
    async fn expand_parser_function_args(
        &self,
        frame: &Frame,
        name: &str,
        attribs: Vec<crate::wikitext::tokens_v2::KV>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        src_text: &str,
    ) -> Vec<crate::wikitext::tokens_v2::KV> {
        use crate::wikitext::tokens_v2::KeyValue;

        let mut out = attribs;
        for (i, kv) in out.iter_mut().enumerate() {
            if i == 0 {
                // The target arg, `#name: first argument`, is already expanded by
                // [`Self::expand_target_templates`].
                continue;
            }
            let positional = matches!(&kv.key, KeyValue::Str(s) if s.is_empty());
            if TemplateHandler::arg_is_branch(name, i, positional) {
                continue;
            }
            // Expand both halves: a named argument's name is its case for
            // `#switch` (`|case=result`) and is ignored elsewhere, and `{{{…}}}`
            // inside a value was already substituted by the attribute pass.
            self.expand_field_tokens(frame, &mut kv.key, source, about_counter, src_text)
                .await;
            self.expand_field_tokens(frame, &mut kv.value, source, about_counter, src_text)
                .await;
        }
        out
    }

    /// Expand one field (a `KeyValue`'s item list) to the tokens `frame` produces
    /// for it, resolving nested `{{…}}` and templated targets.
    ///
    /// Shared by `#tag`'s arguments and a parser function's argument values: both
    /// expand in template context with no wrapper (`in_template => false`,
    /// `body => true`) because the field is about to be flattened to a string. A
    /// `mw:Transclusion` wrapper there would both leak its own source into the
    /// text and spend an `about` id the service does not.
    async fn expand_field_tokens(
        &self,
        frame: &Frame,
        field: &mut crate::wikitext::tokens_v2::KeyValue,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        src_text: &str,
    ) {
        use crate::wikitext::tokens_v2::KeyValue;

        let KeyValue::Tokens(items) = field else {
            return;
        };
        let found = expandable_content(items);
        if !found.template && !found.arg {
            return;
        }
        let substituted = if found.arg {
            frame.expand(items)
        } else {
            items.clone()
        };
        let expanded = Box::pin(self.expand_templates(
            frame,
            substituted,
            source,
            about_counter,
            // The field is flattened to a *string*, so magic words must expand to
            // their text (`{{!}}` -> `|`) rather than to the in-template `<td>`
            // form, and nothing may be wrapped in `mw:Transclusion` markers — a
            // wrapper there both leaks its own source into the text and spends an
            // `about` id the service does not. `body` is what suppresses the
            // wrapper without turning on the template-context magic-word forms.
            /* in_template */
            false,
            /* body */ true,
            src_text,
            0,
        ))
        .await;
        // A target the expansion left nested — `[[{{PAGENAME}}]]` — is resolved
        // here, because nothing else looks at these tokens: they are about to be
        // flattened to the string the tag or function carries.
        let expanded = Box::pin(self.expand_attrib_templates(
            frame,
            expanded,
            source,
            about_counter,
            /* in_template */ false,
            /* body */ true,
            src_text,
        ))
        .await;
        *field = KeyValue::Tokens(expanded);
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
        self.expand_attributes_at(
            frame,
            tokens,
            source,
            about_counter,
            page_source,
            fragments,
            next_id,
            0,
        )
        .await
    }

    /// The body of [`Self::expand_attributes`], carrying the sub-pipeline depth.
    ///
    /// A value that expands to markup with *its own* templated attributes (a
    /// `Template:Legend` inside a media caption) needs the attribute expander run
    /// over the expansion, not only over the page's top level: Parsoid's
    /// `AttributeTransformManager` expands a value with `attrExpansion` and
    /// `buildExpandedAttrs` runs it through a pipeline that includes the
    /// `AttributeExpander`. `depth` bounds the recursion.
    #[allow(clippy::too_many_arguments)]
    async fn expand_attributes_at(
        &self,
        frame: &Frame,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        page_source: Option<&str>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
        depth: u8,
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
            //
            // A token that came out of a template body (`synthesized`) has its
            // attribute values expanded the way that body's pipeline did: with
            // `inTemplate = true`, so a template in the value is *not* wrapped in
            // its own `mw:Transclusion` markers. Parsoid gets the same result
            // because the preprocessor expanded the body before Parsoid saw it;
            // rustoid emulates that by not spending an `about` id here (a
            // category link whose target is `{{{pagetype|{{pagetype}}}…}}`
            // otherwise burned one id per page — see `Template:Short
            // description`).
            let in_tpl = tok.data_parsoid().is_some_and(|dp| dp.tmp.synthesized);
            let mut expanded_attrs: Vec<KV> = Vec::with_capacity(attribs.len());
            for kv in &attribs {
                let new_key = if let KeyValue::Tokens(toks) = &kv.key {
                    let expanded = self
                        .expand_attr_tokens(
                            frame,
                            toks.clone(),
                            source,
                            about_counter,
                            page_source,
                            fragments,
                            next_id,
                            in_tpl,
                            depth,
                        )
                        .await;
                    crate::pipeline::attribute_transform_manager::items_to_key_value(expanded)
                } else {
                    kv.key.clone()
                };
                let new_value = if let KeyValue::Tokens(toks) = &kv.value {
                    let expanded = self
                        .expand_attr_tokens(
                            frame,
                            toks.clone(),
                            source,
                            about_counter,
                            page_source,
                            fragments,
                            next_id,
                            in_tpl,
                            depth,
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
                &mut |kv, ck| self.value_to_dom_html(kv, ck, fragments, next_id),
                page_source,
            );
            out.extend(result);
        }
        out
    }

    /// Expand one attribute key/value token array the way Parsoid's attribute
    /// sub-pipeline does: templates, then the extension handler over the value's
    /// attributes, then the attribute expander over what the expansion produced.
    /// The last step is what reaches a `Template:Legend`'s templated `class`/
    /// `style` when the legend sits inside a caption.
    #[allow(clippy::too_many_arguments)]
    async fn expand_attr_tokens(
        &self,
        frame: &Frame,
        toks: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        page_source: Option<&str>,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &std::cell::Cell<usize>,
        in_tpl: bool,
        depth: u8,
    ) -> Vec<Item> {
        // An `extension` token the value already carries keeps rustoid's existing
        // handling: `expand_in_attributes` on the enclosing page chunk expands
        // one nested in a *tag's* attribute, and a link target or a media caption
        // reads the raw token directly. One that only appears *after* the
        // expansion below is a `{{#tag:…}}` result. rustoid's TT2 sub-pipeline for
        // an attribute value only approximates PHP's `Frame::expand` (which runs
        // the whole pipeline, ExtensionHandler included), so that token is
        // expanded here — otherwise the tree builder flattens it to its raw
        // `src` (`<nowiki>A</nowiki>` where the service serves `A`).
        let had_extension = chunk_has_extension_token(&toks);
        let expanded = {
            let _attr = self.begin_attr_expansion();
            self.expand_templates(
                frame,
                toks,
                source,
                about_counter,
                in_tpl,
                in_tpl,
                page_source.unwrap_or(""),
                0,
            )
            .await
        };
        // The tokens already in the value keep their page-absolute `tsr`: PHP's
        // `Frame::expand` runs the chunk through the expansion pipeline, which
        // only *replaces* template/variable tokens and clears the `tsr` of what
        // they produce (`processTemplateTokens`). A blanket `clear_tsr` over the
        // whole value would also wipe a `mw-quote` token's `tsr`, leaving the
        // extlink `[url ''italic'']` with an `<i>` that has no `dsr`.
        let expanded = if !had_extension && chunk_has_extension_token(&expanded) {
            crate::pipeline::extension_handler::run(expanded, self.config, fragments, next_id)
        } else {
            expanded
        };
        // TT2 order inside the value's sub-pipeline: ExtensionHandler before
        // AttributeExpander.
        let expanded = crate::pipeline::extension_handler::expand_in_attributes(
            expanded,
            self.config,
            fragments,
            next_id,
        );
        if depth < MAX_ATTR_EXPANSION_DEPTH && has_token_attributes(&expanded) {
            Box::pin(self.expand_attributes_at(
                frame,
                expanded,
                source,
                about_counter,
                page_source,
                fragments,
                next_id,
                depth + 1,
            ))
            .await
        } else {
            expanded
        }
    }

    /// The interface message a `{{MediaWiki:…}}` transclusion names.
    ///
    /// MediaWiki resolves a MediaWiki-namespace target through `MessageCache`,
    /// not through the page table: `MediaWiki:Toc` has no page and still answers
    /// "Contents". A name that is not a message at all (`MediaWiki:Sitename`)
    /// answers `None`, which lets the caller's page fetch fail and the
    /// transclusion render as a redlink — the service's own answer.
    ///
    /// Message keys are case-insensitive in their first character only, and use
    /// underscores for spaces (`Title::getDBkey`).
    async fn mediawiki_message(
        &self,
        title: &crate::title::Title,
        source: &dyn DataSource,
    ) -> Option<String> {
        let namespace = self.config.canonical_namespace_id("MediaWiki")?;
        if title.namespace_id != namespace {
            return None;
        }
        let mut chars = title.text.chars();
        let first = chars.next()?;
        let key = format!("{}{}", first.to_ascii_lowercase(), chars.as_str()).replace(' ', "_");
        source
            .get_message(self.config.language_code(), &key)
            .await
            .ok()
            .flatten()
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
        // Whether this call sits in a *template body* expanded by core. Such a
        // body has no pipeline of its own on the Parsoid side.
        body: bool,
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
            self.module_scope.get() > 0,
        ) {
            return err;
        }

        // Without a data source, a template becomes a redlink.
        let Some(src) = source else {
            return self.missing_template_items(
                name,
                target_wt,
                title,
                params,
                about_id,
                token,
                wrap,
                page_source,
            );
        };

        // A `{{MediaWiki:…}}` transclusion reads an *interface message*, not a
        // page body. `MediaWiki:Toc` exists only as a system message, yet
        // `{{MediaWiki:Toc}}` renders "Contents"; a name that is not a message
        // (`MediaWiki:Sitename`) stays a redlink, and no `get_message` answer
        // produces one. `MessageCache` is the authority — it already reflects an
        // on-wiki override — so a message miss falls back to the page table.
        let message_src = self.mediawiki_message(title, src).await;
        let fetched = match message_src {
            Some(text) => Some(text),
            None => src.get_template(title).await.ok().flatten(),
        };
        let Some(template_src) = fetched else {
            return self.missing_template_items(
                name,
                target_wt,
                title,
                params,
                about_id,
                token,
                wrap,
                page_source,
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
                return self.missing_template_items(
                    name,
                    target_wt,
                    title,
                    params,
                    about_id,
                    token,
                    wrap,
                    page_source,
                );
            }
            Redirected::NotARedirect => (template_src, title.clone()),
        };

        // A redirect is followed *before* the child frame is created, and the loop
        // check above ran with the *called* title. The target must be checked too:
        // `Template:Toc` redirects to `Template:Compact TOC`, whose body
        // transcludes `{{MediaWiki:Toc}}`, so without this check a template could
        // re-enter itself through its own redirect and expand forever.
        if let Some(err) = crate::pipeline::template_handler::enforce_template_constraints(
            self.config,
            frame,
            &title.get_prefixed_text(),
            &title,
            MAX_TEMPLATE_DEPTH,
            self.module_scope.get() > 0,
        ) {
            return err;
        }

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
        // Production Parsoid expands templates through MediaWiki's preprocessor
        // (`Env::$nativeTemplateExpansion` is set only for parser tests), and the
        // preprocessor evaluates a call's argument values in the *calling* frame,
        // before the callee's frame exists. See
        // [`Self::expand_template_arg_values`] for why that is observable.
        self.expand_template_arg_values(
            frame,
            &mut child_args,
            source,
            about_counter,
            in_template,
            page_source,
        )
        .await;
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
            0,
        ))
        .await;
        // A `<templatedata>` in the body records the frame's title for its
        // `mw:edittemplatedata` marker (`$frame->getTitle()` — the `/doc` subpage
        // of the documented template). Only walked when the body has the tag.
        let expanded = if template_src.contains("<templatedata") {
            stamp_templatedata_frame_title(
                expanded,
                &child_frame.title().full_text_with_config(self.config),
            )
        } else {
            expanded
        };

        // A comment in a template body is dropped. MediaWiki's preprocessor
        // strips comments out of a template's expanded source before it is
        // tokenized (`Preprocessor_Hash::preprocessToObj`), so
        // `Template:Short description`'s `<!-- Start tracking -->` markers never
        // reach the reader while a *page*'s comments survive. A comment that
        // sits alone on its line — only whitespace around it, between newlines —
        // additionally takes the line's trailing newline with it
        // (`// To avoid leaving blank lines ... trim leading and trailing spaces
        // and the trailing newline`, gated on `$wsStart > 0`), which is why the
        // same guard is what makes them survive on a *page*. The tokenizer has
        // already bundled exactly such a run into one `EmptyLine`, so that whole
        // token goes when a newline precedes it, and only its comments go
        // otherwise (the line is not eaten at the very start of the source).
        let mut out: Vec<Item> = Vec::with_capacity(expanded.len());
        for item in expanded {
            match item {
                Item::Tok(ParsoidToken::Comment(_)) => continue,
                Item::Tok(ParsoidToken::EmptyLine(t))
                    if t.tokens
                        .iter()
                        .any(|t| matches!(t, ParsoidToken::Comment(_))) =>
                {
                    if ends_with_newline(out.last()) {
                        continue;
                    }
                    out.extend(
                        t.tokens
                            .into_iter()
                            .filter(|t| !matches!(t, ParsoidToken::Comment(_)))
                            .map(Item::Tok),
                    );
                }
                other => out.push(other),
            }
        }
        let mut expanded = out;
        strip_link_content_comments(&mut expanded);
        // A body's tags are not the page's: `expand_attributes` must not mark
        // their attributes `mw:ExpandedAttrs`. See [`mark_synthesized`].
        mark_synthesized(&mut expanded);

        // Parsoid runs the attribute expander inside the expansion's *own*
        // pipeline (`processTemplateSource`) and then overwrites the `about` it
        // set with the transclusion wrapper's. The id it spent stays in the
        // sequence, though, *before* the outer chunk's next expansion
        // (`France`'s Art paragraph: `Template:Abbreviation`'s
        // `title="{{#tag:nowiki|…}}"`, ref 319). rustoid's page-level pass runs
        // after the whole chunk, so reserve the ids here and let that pass
        // consume them. Only an expansion Parsoid actually processes has such a
        // pipeline: a nested template was expanded by core, and its output is
        // part of this expansion.
        if !body {
            for item in expanded.iter_mut() {
                let Item::Tok(tok) = item else { continue };
                if tok.get_attribute_v("about").is_some()
                    || !has_expandable_dom_fragment(tok, self.config)
                {
                    continue;
                }
                let id = self.new_about_id(about_counter, "expanded-attrs");
                if let Some(dp) = tok.data_parsoid_mut() {
                    dp.tmp.pending_expanded_attrs = Some(id);
                }
            }
            // `WikiLinkHandler::renderCategory` numbers a category link whose
            // sort key is generated (`count($content) !== 1`), in the same
            // pipeline — so the id lands after the attribute expander's, and
            // before the caller's next token. The page-level link pass runs
            // after the whole chunk, which would put it after everything the
            // caller does. An expansion core pre-processed (`body`) has no such
            // pipeline and spends nothing here.
            crate::pipeline::wiki_link_render::stamp_generated_category_ids(
                &mut crate::pipeline::wiki_link_render::WikiLinkContext::new(self.config),
                &mut expanded,
                about_counter,
            );
        }

        if !wrap {
            // Nested/extension-content context: no `mw:Transclusion` wrapping.
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
        // The resolved call. Its `function` is expanded; its arguments are only
        // used to echo the call when nothing can be expanded at all.
        call: &crate::lua::invoke::Invoke,
        // The token's *own* arguments, unexpanded. `data-mw` records the call as
        // written, so a nested `#invoke` in an argument stays literal there even
        // though the module receives its expansion.
        raw_params: &crate::pipeline::parser_functions::Params,
        about_id: String,
        token: &ParsoidToken,
        // PHP's `wrapTemplates` for *this* call, and whether the call is being
        // expanded as part of a template body — the latter is what the nested
        // pipeline that runs the module's output inherits.
        wrap: bool,
        body: bool,
        // The wikitext of the chunk that holds this call. It is *not* the page
        // source: a `#invoke` inside a template expands its arguments against
        // the template body, while `getCurrentTitle():getContent()` must answer
        // with the page (see `Parser::page_source`).
        src_text: &str,
        // The call's own arguments and the calling frame's, both raw. Scribunto
        // expands an argument when the module first reads it, so the module is
        // handed lazy [`ArgSlot`](crate::pipeline::lua_deferred::ArgSlot)s that
        // index into these lists; the host expands the one that was read.
        raw_call_args: Vec<crate::wikitext::tokens_v2::KV>,
        raw_parent_args: Vec<crate::wikitext::tokens_v2::KV>,
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
        // modules read through `frame:getParent()`.
        let lazy_parent = lazy_args(&raw_parent_args, ArgSource::Parent);
        let parent_title = frame.title().full_text();
        let lazy_call = lazy_args(&raw_call_args, ArgSource::Call);
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
            parent_args: lazy_parent,
            parent_title: Some(parent_title),
            has_parent: true,
            page_source: self.page_source.borrow().clone(),
            page_title: Some(self.page_title.borrow().clone()),
            // Seed the frame with every title already resolved this render, so
            // `preload_titles` does not re-fetch them; see `title_facts`.
            titles: self.title_facts.borrow().clone(),
            ..Default::default()
        };

        // A module may call back into the parser (`frame:expandTemplate`,
        // `frame:preprocess`) or read an argument that has not been expanded yet.
        // Neither can happen inside Lua — the pipeline is async — so each one is
        // deferred and the module re-run with the answer available; see
        // [`crate::pipeline::lua_deferred`].
        //
        // The closure is `Fn`, not `FnOnce`: the answer is expanded once per
        // distinct request, and `invoke` may call it several times.
        // The module's output is wikitext, not HTML: templates it returns are
        // expanded by the caller. A failure (missing module, Lua error) becomes
        // MediaWiki's script-error markup, so a broken call reads as broken
        // rather than as silently absent text.
        let output = match crate::lua::invoke::invoke(
            call,
            &lazy_call,
            src,
            site,
            &frame.title().full_text(),
            ctx,
            |request| {
                // Bind the references first, so the `move` block below copies
                // them rather than consuming the values they point at: the
                // closure is `Fn`, and `about_id` in particular is owned.
                let raw_call_args = &raw_call_args;
                let raw_parent_args = &raw_parent_args;
                let about_id = &about_id;
                async move {
                    match request {
                        // One or more `frame.args` reads. Expanding each in the
                        // order given is what puts the argument templates' ids
                        // where the module asked for them.
                        crate::pipeline::lua_deferred::FrameRequest::ExpandArgs { slots } => {
                            let mut out = Vec::with_capacity(slots.len());
                            for slot in &slots {
                                let kvs = match slot.source {
                                    ArgSource::Call => raw_call_args,
                                    ArgSource::Parent => raw_parent_args,
                                };
                                // A parent argument's `{{{…}}}` was expanded in the
                                // frame that called the template, so that is where it
                                // is resolved here. Using the template's own frame made
                                // a value that names its own parameter expand into
                                // itself: `{{Parameter names example|subheader2={{{subheader2}}}<br/>…}}`
                                // grew a copy of the value on every pass.
                                let expand_frame = match slot.source {
                                    ArgSource::Call => frame,
                                    ArgSource::Parent => frame.parent().unwrap_or(frame),
                                };
                                let text = match kvs.get(slot.index) {
                                    Some(kv) => {
                                        self.expand_invoke_arg_text(
                                            kv,
                                            expand_frame,
                                            source,
                                            about_counter,
                                            body,
                                            src_text,
                                        )
                                        .await
                                    }
                                    None => String::new(),
                                };

                                out.push((slot.key(), text));
                            }
                            out
                        }
                        other => {
                            let key = other.key();
                            let text = self
                                .expand_lua_request(
                                    source,
                                    frame,
                                    other,
                                    about_id.clone(),
                                    token,
                                    about_counter,
                                )
                                .await;
                            vec![(key, text)]
                        }
                    }
                }
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

        // A module whose output begins with a table (`{|`) is spliced into the
        // caller right after the transclusion's marker span, which is not the
        // start of a line — so the table is not at SOL. Parsoid inserts a newline
        // to force it there (T2529: "for *#:; (lists) and {| (table start),
        // newlines are added"), and the inserted newline is what lets a
        // transclusion that follows merge with the ones before it (see
        // ONLINE-PARITY.md). A module that is already at SOL is left alone.
        let mut items = items;
        if output.starts_with("{|") {
            let mut nl = crate::wikitext::tokens_v2::NlTk::new(
                crate::wikitext::tokens_v2::SourceRange::new(0, 0),
            );
            nl.data_parsoid.tmp.synthetic_sol_newline = true;
            items.insert(0, Item::Tok(ParsoidToken::Nl(nl)));
        }

        // The module's output is expanded in a child of the calling frame. This
        // frame carries *no* arguments: a `{{{…}}}` in the output is literal
        // wikitext to the service — the preprocessor has already run, and its
        // re-parse is a parser pass, not a preprocessor one — so expanding it
        // against the `#invoke` call's arguments resolved a value that named its
        // own parameter into itself. `Template:Infobox`'s documentation hands
        // `subheader2={{{subheader2}}}<br/>......` to `{{Parameter names
        // example}}`, which passes it to `frame.expandTemplate`; the service
        // serves the one literal `{{{subheader2}}}`, while rustoid re-expanded it
        // three deep (`…<br/>......<br/>......<br/>......`).
        //
        // `frame:getParent()` is a separate path: `expand_invoke` reads the
        // calling frame's raw arguments for it (see `raw_parent_args`), so a
        // nested `#invoke` in the output still sees them.
        let child = frame.new_child(frame.title().clone(), Vec::new());
        // A module's output is wikitext in the calling frame's context, and a
        // template it echoes (`Module:Clade` emits nested `{{clade}}` calls) is
        // expanded with the title-chain loop check stood down: production
        // Parsoid's preprocessor passes `ignoreLoop => true`, and the recursion
        // is bounded by depth instead. See [`Parser::module_scope`].
        let _module_scope = self.enter_module_scope();
        // The document's counter, not a fresh one. A module's output can carry a
        // `<templatestyles>` — `Module:Infobox` emits one through
        // `frame:extensionTag` — and that stylesheet's `about` id belongs to the
        // page's sequence. A fresh counter started it at 1, so the `<style>`
        // reused the id the enclosing `#invoke` wrapper had just been given.
        //
        // The output is *expanded output*, not a template body: the core
        // preprocessor expands the template (and its `#invoke`) before Parsoid
        // ever sees it, and Parsoid then tokenizes the resulting wikitext. A
        // `{{…}}` that survives in it — `Module:Citation/CS1` writes its
        // maintenance message as `{{[[Template:Cite web|cite web]]}}` — is
        // therefore a token Parsoid processes, so it spends an `about` id even
        // when the target does not resolve (`take_id` keys off `body`, not the
        // wrapping flag). Nothing in the chunk is *wrapped*, though: Parsoid
        // runs the expansion with `expandTemplates => false` / `inTemplate =>
        // true`, and a `{{{…}}}` it contains is pre-processed text, not a new
        // `mw:Param` range. Wrapping them put bogus `templatearg` parts into the
        // enclosing transclusion's compound `data-mw` (`Template:Infobox`'s
        // documentation).
        let mut expanded = Box::pin(self.expand_templates(
            &child,
            items,
            source,
            about_counter,
            /* in_template */ true,
            /* body */ false,
            /* src_text */ "",
            0,
        ))
        .await;
        // A module's output is not the page's own wikitext: `expand_attributes`
        // must not mark its tags `mw:ExpandedAttrs`. See [`mark_synthesized`].
        mark_synthesized(&mut expanded);
        mark_from_module(&mut expanded);

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
    /// Used for the one argument Scribunto expands eagerly — the *function
    /// name*, so the entry point can be picked before the module runs — and, one
    /// at a time, for the arguments a module actually reads (see
    /// [`Self::expand_invoke_arg_text`]). Scribunto receives expanded text, so a
    /// nested template or `#invoke` in an argument is substituted: the tokenizer
    /// keeps such a value as tokens holding an unexpanded `template` token, which
    /// stringifies to nothing useful — that is why the unexpanded form answered
    /// 17 where the service answers 3.
    ///
    /// The caller passes only the arguments to expand, so the two shapes cannot
    /// be confused: the `#invoke` call's own list starts after the target, while
    /// a **parent** frame's list has no target and is passed whole.
    /// Expand a template call's argument *values* in the calling frame.
    ///
    /// Production Parsoid expands templates through MediaWiki's preprocessor
    /// (`Env::$nativeTemplateExpansion` says so: it is "only used during parser
    /// tests; in production, template expansion is done via MediaWiki's legacy
    /// preprocessor"), and the preprocessor evaluates a call's argument values in
    /// the *calling* frame, before the callee's frame exists. Two observable
    /// consequences, both checked against the live transform endpoint:
    ///
    /// - `{{Main other|{{Main other|X}}}}` renders `X`. Expanding the inner call
    ///   from inside the callee's frame trips `loopAndDepthCheck` instead, which
    ///   is what rustoid served — `Template loop detected: Template:Main other`
    ///   in the middle of `Polio vaccine`, because `Template:Short description`
    ///   ends with `{{Main other|{{SDcat|…}}}}` and `Template:Infobox drug` wraps
    ///   its own short-description call in `{{Main other|…}}`.
    /// - A module's `frame:getParent().args` arrives already expanded. That is why
    ///   the `<div class="plainlist">` a module echoes back carries no `about`
    ///   and costs no extra `#mwt` ids: the service hands it markup, not wikitext
    ///   that re-expands into a fresh transclusion.
    ///
    /// The helper frame is titled with the *page*, not the caller: the value's
    /// templates are expanded in the caller's scope, and a child frame cloning the
    /// caller's title would make an argument that calls the caller's own template
    /// look like a loop. PHP's equivalent frame has a null title, which can never
    /// match.
    ///
    /// It carries the *caller's arguments*, though — that is the other half of
    /// "expanded in the caller's scope", and a nested `#invoke` reads them as
    /// `frame:getParent().args`. The preprocessor expands a template's argument
    /// values in `$this->parent` (`PPTemplateFrame_Hash::getNumberedArgument`),
    /// so a `{{#invoke:…}}` written in an argument sees the *call site's* frame:
    /// `Template:Infobox person`'s `Born` row is
    /// `{{br separated entries|2={{#invoke:person date|birth}}}}`, and `person
    /// date` reads `birth_date` from the frame the infobox was called in. An
    /// empty child frame made that lookup miss and the whole row empty.
    ///
    /// Unlike [`Self::expand_invoke_args`], extension ids **are** spent here —
    /// but not at expansion time: the guard defers them to the post-pass of the
    /// chunk the value is spliced into, which is its document position. Numbering
    /// them at expansion time put a value's stylesheets before the extensions the
    /// *callee* emits, which are earlier in the output: `Template:Infobox
    /// military conflict`'s args carry `{{multiple image}}`, so Multiple image's
    /// stylesheet took `#mwt12` and the infobox's own `#mwt25`, where the service
    /// has the infobox's at `#mwt12` and Multiple image's at `#mwt14`.
    async fn expand_template_arg_values(
        &self,
        frame: &Frame,
        args: &mut [crate::wikitext::tokens_v2::KV],
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
        page_source: &str,
    ) {
        // The value's extensions are numbered where the value lands in the
        // output, not here — see the note above and [`Parser::style_defer`].
        let _guard = self.begin_style_defer();
        for kv in args.iter_mut() {
            // Both halves of a call are expanded in the caller's frame. Core's
            // preprocessor expands an argument *name* too, which templates use
            // for computed names: `Template:Multi-sport event sidebar` writes
            // `| {{#ifeq:…|yes|list1title|title}} = …`.
            if let crate::wikitext::tokens_v2::KeyValue::Tokens(items) = &kv.key {
                let expanded = self
                    .expand_template_arg_tokens(
                        frame,
                        items.clone(),
                        source,
                        about_counter,
                        in_template,
                        page_source,
                    )
                    .await;
                kv.key = crate::pipeline::attribute_transform_manager::items_to_key_value(expanded);
            }
            let crate::wikitext::tokens_v2::KeyValue::Tokens(items) = &kv.value else {
                continue;
            };
            let expanded = self
                .expand_template_arg_tokens(
                    frame,
                    items.clone(),
                    source,
                    about_counter,
                    in_template,
                    page_source,
                )
                .await;
            kv.value = crate::wikitext::tokens_v2::KeyValue::Tokens(expanded);
        }
    }

    /// Expand one template call argument (key or value) in the caller's frame.
    ///
    /// MediaWiki's preprocessor resolves the include directives, `{{{…}}}`
    /// references and nested templates before the callee's body is tokenized.
    async fn expand_template_arg_tokens(
        &self,
        frame: &Frame,
        mut items: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
        page_source: &str,
    ) -> Vec<Item> {
        // MediaWiki's preprocessor resolves the include directives in an
        // argument value before the callee's body is tokenized: the value
        // `A<noinclude>B</noinclude>C` reaches the body as `ABC`, the
        // `includeonly` form as `AC`, and the `onlyinclude` form as `ABC`.
        // (`Preprocessor_Hash` includes or skips a directive's content and
        // drops the marker; the page's own top-level markers stay, which is
        // why this belongs to argument values and not to the page.)
        strip_include_directive_metas(&mut items);
        let found = expandable_content(&items);
        if !found.template && !found.arg {
            return items;
        }
        let substituted = if found.arg {
            frame.expand(&items)
        } else {
            items
        };
        let child = frame.new_child(frame.root_title().clone(), frame.args().args.clone());
        let expanded = Box::pin(self.expand_templates(
            &child,
            substituted,
            source,
            about_counter,
            in_template,
            /* body */ true,
            page_source,
            0,
        ))
        .await;
        Box::pin(self.expand_attrib_templates(
            &child,
            expanded,
            source,
            about_counter,
            in_template,
            true,
            page_source,
        ))
        .await
    }

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

        // Number no extensions while these arguments expand: Scribunto hands the
        // module expanded *text*, and the extensions in it are numbered where the
        // module's output places them, not here. The token stream below is
        // rendered back to text by the caller, so an id spent here is spent on a
        // token the output never shows.
        let _guard = self.begin_arg_expansion();

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
            // The argument reference is substituted from the **calling** frame, as
            // MediaWiki does: the value was written in the calling template. The
            // child below inherits that frame's arguments so that a nested
            // `#invoke` in the value sees them through `frame:getParent()` —
            // `{{Further|…}}`'s body is returned verbatim by `Module:Unsubst`,
            // and its inner `#invoke:labelled list hatnote` reads the enclosing
            // template's arguments to learn which pages to name.
            let mut substituted = if has_arg_ref {
                frame.expand(items)
            } else {
                items.clone()
            };
            // The value is flattened to the string the module receives, so the
            // templates in it are expanded by the preprocessor and never reach
            // Parsoid as tokens; mark them so `expand_template_token` does not
            // spend `about` ids for them.
            mark_arg_value_tokens(&mut substituted);
            // The text this argument keeps to when the expansion below cannot be
            // rendered exactly (see [`argument_value_or_source`]). It has to be
            // taken from the tokens *now*, before they are expanded: the recorded
            // source range can be stale, because the tokenizer already substituted
            // any `{{{…}}}` the value contained. `Template:Infobox sport` passes
            // `data3={{{nicknames|{{{nickname|}}}}}}`, whose tokens are
            // `{{Plainlist|…}}` while its range still spells the reference — and a
            // reference re-expanded in the *module's* frame has no `nickname`, so it
            // collapsed to nothing and the infobox row rendered blank.
            if let Some(text) = argument_value_text(&substituted) {
                kv.vsrc = Some(text);
            }
            let child = frame.new_child(frame.title().clone(), frame.args().args.clone());
            let expanded = Box::pin(self.expand_templates(
                &child,
                substituted,
                source,
                about_counter,
                /* in_template */ true,
                body,
                src_text,
                0,
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

    /// Expand one lazy `frame.args` argument to the text a module receives.
    ///
    /// Called when the module first reads the key, which is when Scribunto
    /// expands it — so the templates inside spend their `about` ids here rather
    /// than before the module ran. The text is exactly what the eager path used
    /// to produce for the same argument ([`expanded_arg_pair`]), so the value a
    /// module sees is unchanged; only *when* it is produced differs.
    #[allow(clippy::too_many_arguments)]
    async fn expand_invoke_arg_text(
        &self,
        kv: &crate::wikitext::tokens_v2::KV,
        frame: &Frame,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        body: bool,
        src_text: &str,
    ) -> String {
        let expanded = self
            .expand_invoke_args(
                std::slice::from_ref(kv),
                frame,
                source,
                about_counter,
                body,
                src_text,
            )
            .await;
        let Some(mut kv) = expanded.into_iter().next() else {
            return String::new();
        };
        if let crate::wikitext::tokens_v2::KeyValue::Tokens(items) = &kv.value
            && argument_value_text(items).is_none()
        {
            // The exact renderer declined a construct it cannot spell — a
            // generated element, a `mw:DOMFragment` placeholder (a tunnelled
            // `<templatestyles>`, whose content is stashed), or a `{|` table.
            // The module answer renderer is the right answer for all of them: it
            // writes the source of what has one, rebuilds a `{|` table and a
            // `[[…]]` from their token data, and turns a placeholder into a
            // `UNIQ…QINU` strip marker that `substitute_strip_markers` splices
            // back when the module's output is re-parsed.
            //
            // The renderer drops a token that has no source at all, and that is
            // right: Scribunto is handed the expansion's *text*, so a node the
            // pipeline *synthesised* is not in it. `Template:Legend` is the case
            // that pins it — its `{{#if:}}` leaves an auto-inserted `</span>`
            // with no source, and spelling it produced an unbalanced tag the tree
            // builder then marked `mw:Placeholder/StrippedTag`.
            //
            // A live element with no source *is* spelled, and a stale source is
            // rebuilt from the token's expanded attributes: see
            // [`Self::render_argument_markers`].
            //
            // Falling back to the recorded source instead hands the module the
            // *unexpanded* call — which `Module:Clade` string-matches against
            // `{|class="clade"`, so it stripped its own outer class and wrapped
            // the wrong div — and the module's output then re-expands the call,
            // numbering its extensions in a nested pipeline where the service
            // numbers the expansion's own tokens in place.
            kv.value =
                crate::wikitext::tokens_v2::KeyValue::Str(self.render_argument_markers(items));
        }
        expanded_arg_pair(&kv).1
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
    ///
    /// The pass recurses: an attribute value can hold a token with attributes of
    /// its own (a `wikilink`'s display text is an attribute value, and that text
    /// can be a `<span title="{{…}}">`), and `expand_templates` only walks a
    /// chunk's top level. PHP's `Frame::expand` walks the whole tree in one call,
    /// so recursing here is what keeps the two equivalent.
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
            // Mirrors PHP's `AttributeExpander::processComplexAttributes`: an
            // `extlink`/`urllink` whose `href` holds a template token is flagged,
            // and `render_answer` then *rebuilds* it rather than writing back its
            // stale `src`. rustoid tokenizes a template body before its `{{{…}}}`
            // are substituted, so that `src` spells `{{{1}}}` where the service's
            // (preprocessed before tokenizing) spells the expansion.
            let href_holds_template = matches!(tok.get_name(), "extlink" | "urllink")
                && attribs.iter().any(|kv| {
                    kv.key.as_str() == Some("href") && matches!(kv.value, KeyValue::Tokens(_))
                });
            let mut expanded_attrs: Vec<KV> = Vec::with_capacity(attribs.len());
            for kv in attribs {
                // Whether the key was written as a construct rather than a plain
                // name. PHP gates the reparse on the *original* key being an array
                // (`is_array( $oldA->k )`); checking the expanded one instead
                // missed `{{#if:…|id="toc"}}`, which collapses to the single
                // string `id="toc"` and has to be reparsed into an attribute.
                let key_was_tokens = matches!(kv.key, KeyValue::Tokens(_));
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
                            0,
                        )
                        .await;
                    // A token in an attribute value can carry attributes of its
                    // own, and `expand_templates` only walks the *top level* of a
                    // chunk, so a template nested one attribute deeper survives
                    // unless this pass recurses. PHP's `Frame::expand` walks the
                    // whole node tree in one call; rustoid splits that into the
                    // top-level pass and this attribute pass, so the attribute
                    // pass has to recurse to match it. `Template:Dead link` is the
                    // case that pins it: its `<span title="{{#if:…}}">` is the
                    // *display text* of a `wikilink`, i.e. a span inside the
                    // link's content attribute, so the `#if` is two attribute
                    // levels down and stayed unexpanded — leaving a literal
                    // `{{{date|}}}` to be resolved later in the root frame, which
                    // has no `date`.
                    let expanded = Box::pin(self.expand_attrib_templates(
                        frame,
                        expanded,
                        source,
                        about_counter,
                        in_template,
                        body,
                        src_text,
                    ))
                    .await;
                    *field =
                        crate::pipeline::attribute_transform_manager::items_to_key_value(expanded);
                }
                // A templated key can expand into whole attributes —
                // `Template:Div col`'s `<div … {{#if:…|style="…"}}>` — which the
                // page-level `expand_attributes` pass resolves in
                // `build_expanded_attrs`' reparse-KV step. This pass has to do the
                // same, or the generated attribute stays a key that merely spells
                // it (and the module is handed a tag without it).
                let reparsed = (new_kv.value.is_empty() && key_was_tokens)
                    .then(|| {
                        crate::pipeline::attribute_expander::reparse_attr_key_string(
                            &crate::wikitext::token_utils::tokens_to_string(
                                &crate::pipeline::attribute_transform_manager::key_value_to_items(
                                    &new_kv.key,
                                ),
                            ),
                        )
                    })
                    .flatten();
                match reparsed {
                    Some(clean) => expanded_attrs.extend(clean),
                    None => expanded_attrs.push(new_kv),
                }
            }
            let mut new_tok = tok.clone();
            new_tok.set_attribs(expanded_attrs);
            if href_holds_template && let Some(dp) = new_tok.data_parsoid_mut() {
                dp.tmp.link_contains_template = Some(true);
            }
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

        let (items, text) =
            crate::pipeline::lua_deferred::request_tokens(&request, self.config.extension_tags());
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
        // A `frame:expandTemplate`/`preprocess` answer is *wikitext*: Scribunto
        // returns the preprocessor's expansion, extensions and all, and the
        // caller re-parses it — so an extension's `about` id belongs to that
        // parse, not here (Parsoid's `ExtensionHandler` numbers it there, in
        // document order). Deferring the answer's extensions is what stops the
        // ids a stringified answer would otherwise spend.
        //
        // A `#tag` call (`frame:extensionTag` lowers to it) is the same story as
        // its input suggests: Parsoid's `pf_tag` returns a plain `<tag>` token,
        // which the pipeline that re-parses the module's output numbers where the
        // module left it, in document order — *not* at the call. So a
        // `#tag:templatestyles` answer is deferred like any other; the resolved
        // stylesheet is carried to the module's output by a strip marker and
        // numbered there, in the chunk's document order. The `indicator` answer
        // is a placeholder too (see [`Self::expand_one_indicator`]), so it is
        // deferred by the same rule: numbering it at the call put it *before* a
        // stylesheet the module emitted earlier, shifting every later id.
        let defer = !matches!(
            request,
            crate::pipeline::lua_deferred::FrameRequest::CallParserFunction { .. }
        ) || is_deferred_extension_tag(&request);
        let _answer_gate = defer.then(|| self.begin_style_defer());
        // The answer is the preprocessor's expansion (a string the module
        // returns), so any `{{…}}` left in it was already expanded by core or
        // did not resolve. `body` marks it as content Parsoid does not see as
        // tokens, which suppresses the `about` id a template token would
        // otherwise spend here: the two `{{…}}`s `Module:Infobox` feeds through
        // `frame:expandTemplate` were two ids ahead of the service (`France`'s
        // first infobox stylesheet), and the attribute pass below spent five
        // more on `Zebra`.
        let expanded = Box::pin(self.expand_templates(
            &child,
            items,
            Some(source),
            about_counter,
            /* in_template */ true,
            /* body */ true,
            &text,
            0,
        ))
        .await;
        // The body's *attributes* are expanded one pass later on a page
        // (`expand_attributes` at the end of `build_ast`), but the module is
        // handed the body's wikitext now, so resolve them here.
        // `Template:Stack` writes its class as
        // `class="stack mw-stack {{#switch:…}}"`, and the preprocessor's
        // `$frame->expand` hands the module the resolved `stack-clear-right`;
        // left unexpanded, the module returned the switch and the answer
        // re-parsed it with the argument gone (`stack-right`).
        let expanded = Box::pin(self.expand_attrib_templates(
            &child,
            expanded,
            Some(source),
            about_counter,
            /* in_template */ true,
            /* body */ true,
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
        // Markers, not the plain renderer: a `frame:expandTemplate` answer that
        // carries an extension — `Module:Infobox` builds the infobox with
        // `frame:expandTemplate('stack begin')`, and `Template:Stack` opens with a
        // literal `<templatestyles>` — has a fragment placeholder inside it, and
        // `render_answer` has no textual form for one and drops it. The fragment
        // Was the fragment spent (`Stack/styles.css` `#mwt13`) and then lost, and the output
        // re-expanded the template's source, building a second fragment at
        // `#mwt64`. The marker keeps the first.
        self.render_answer_markers(&encapped)
    }
}

/// MediaWiki's strip-marker format — `Parser::MARKER_PREFIX`/`MARKER_SUFFIX`:
/// `\x7f'"`UNIQ--<tag>-<8 hex>-QINU`"'\x7f`.
///
/// The `'"`` characters are MediaWiki's, not decoration: a marker smuggled into
/// an attribute must be escaped, or it could break out of the attribute. They
/// also make the marker six bytes longer, which is observable wherever the
/// marker text is *measured* rather than resolved — `Module:Navbox` sums its
/// arguments' lengths into the navbox `id`'s `argHash`, and an argument holding
/// a `{{Nobold}}` (which carries a `Nobold/styles.css` marker) then counted six
/// bytes short.
fn strip_marker(tag: &str, id: usize) -> String {
    format!("\u{7f}'\"`UNIQ--{tag}-{id:08X}-QINU`\"'\u{7f}")
}

/// The `mw:DOMFragment` placeholder starting at `items[i]`, with the index just
/// past it: a self-closing `mw:dom-fragment-token`, a self-closing
/// `mw:DOMFragment` element, or a `mw:DOMFragment` element with the immediately
/// following matching end tag.
///
/// Used by [`Parser::tag_body_source`] to keep such a placeholder alive through
/// the `#tag` content's wikitext round-trip. Anything else — including an
/// `mw:DOMFragment` tag whose pair is not adjacent — is left to the plain
/// serializer, so the change stays confined to the shapes an expansion emits.
fn dom_fragment_placeholder_at(items: &[Item], i: usize) -> Option<(usize, Vec<Item>)> {
    let Item::Tok(tok) = &items[i] else {
        return None;
    };
    let is_placeholder = |t: &crate::wikitext::tokens_v2::ParsoidToken| {
        let typeof_ = t
            .get_attribs()
            .iter()
            .find(|kv| kv.key.as_str() == Some("typeof"))
            .and_then(|kv| kv.value.as_str());
        match t {
            crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(tk)
                if tk.name == "mw:dom-fragment-token" =>
            {
                true
            }
            _ => typeof_.is_some_and(|ty| ty.split_whitespace().any(|v| v == "mw:DOMFragment")),
        }
    };
    if !is_placeholder(tok) {
        return None;
    }
    if matches!(
        tok,
        crate::wikitext::tokens_v2::ParsoidToken::SelfclosingTag(_)
    ) {
        return Some((i + 1, vec![items[i].clone()]));
    }
    let crate::wikitext::tokens_v2::ParsoidToken::Tag(open) = tok else {
        return None;
    };
    match items.get(i + 1) {
        Some(Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::EndTag(end)))
            if end.name == open.name =>
        {
            Some((i + 2, items[i..=i + 1].to_vec()))
        }
        _ => None,
    }
}

/// Whether an attribute key or value holds a token list.
fn kv_holds_tokens(kv: &crate::wikitext::tokens_v2::KV) -> bool {
    use crate::wikitext::tokens_v2::KeyValue;
    matches!(kv.key, KeyValue::Tokens(_)) || matches!(kv.value, KeyValue::Tokens(_))
}

/// Whether any item carries a strip marker — a top-level text run or, at any
/// depth, a token's attribute value.
fn items_carry_marker(items: &[Item]) -> bool {
    items.iter().any(|it| match it {
        Item::Str(s) => s.contains('\u{7f}'),
        Item::Tok(t) => t.get_attribs().iter().any(kv_carries_marker),
    })
}

/// Whether either half of an attribute carries a strip marker.
fn kv_carries_marker(kv: &crate::wikitext::tokens_v2::KV) -> bool {
    value_carries_marker(&kv.key) || value_carries_marker(&kv.value)
}

/// Whether an attribute key or value carries a strip marker.
fn value_carries_marker(value: &crate::wikitext::tokens_v2::KeyValue) -> bool {
    use crate::wikitext::tokens_v2::KeyValue;
    match value {
        KeyValue::Str(s) => s.contains('\u{7f}'),
        KeyValue::Tokens(items) => items_carry_marker(items),
    }
}

/// Flatten an attribute value's `Str` and `mw:Entity` spans to the text HTML
/// decoding would produce, or `None` if it holds any other token.
///
/// `mw.html` entity-encodes the quotes inside a strip marker, so the tokenizer
/// sees `&quot;` in the middle of the marker and emits an entity span. The
/// decoded text is what `substitute_strip_markers` must search for the marker.
fn flatten_attribute_markers(items: &[Item]) -> Option<String> {
    use crate::wikitext::token_utils::{is_entity_span_token, skip_entity_decoded_text};

    let mut out = String::new();
    let mut i = 0;
    while i < items.len() {
        match &items[i] {
            Item::Str(s) => out.push_str(s),
            Item::Tok(tok) if is_entity_span_token(tok) => {
                out.push_str(tok.data_parsoid()?.src_content.as_deref()?);
                if skip_entity_decoded_text(tok, items.get(i + 1)) {
                    i += 1;
                }
                if matches!(items.get(i + 1), Some(Item::Tok(ParsoidToken::EndTag(e))) if e.name == "span")
                {
                    i += 1;
                }
            }
            _ => return None,
        }
        i += 1;
    }
    Some(out)
}

/// Whether a deferred request is a `frame:extensionTag('templatestyles', …)` —
/// lowered to `#tag` with `templatestyles` as its first argument.
///
/// Only that tag's answer is a `mw:DOMFragment` placeholder the module's output
/// can carry, so the id it spends waits for the position the module left it in.
/// See [`Parser::expand_lua_request`].
fn is_deferred_extension_tag(request: &crate::pipeline::lua_deferred::FrameRequest) -> bool {
    let crate::pipeline::lua_deferred::FrameRequest::CallParserFunction { name, args } = request
    else {
        return false;
    };
    name == "#tag"
        && args.first().is_some_and(|a| {
            a.name.is_none() && (a.value == "templatestyles" || a.value == "indicator")
        })
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
    // A value the caller re-rendered — the `#invoke` argument path substitutes and
    // expands the value, so the recorded range no longer describes it. Recording
    // the re-rendered text is what keeps this fallback from handing the module a
    // stale reference; see `expand_invoke_args`.
    if let Some(vsrc) = kv.vsrc.as_deref()
        && !vsrc.is_empty()
    {
        return vsrc.to_string();
    }
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
        // A named argument's value is trimmed before the module sees it, which
        // MediaWiki's preprocessor does for every frame's arguments
        // (`getNamedArgument` runs `trim()`, `getNumberedArgument` does not).
        // Without it `Template:Infobox OS`'s `{{#invoke:Unsubst||$B=\n{{Main
        // other|…}}}}` carries a leading newline in `$B`, and left in place it
        // survives into the module's return value and renders as a stray
        // `<span about=…> </span>` the service does not have.
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

/// Drop the include-directive markers of a template argument value, keeping the
/// content between them. `<includeonly>`'s content is already swallowed into
/// its marker's `data-mw.src` by the tokenizer, so removing the marker removes
/// the content with it — exactly the preprocessor's `includeonly` behaviour.
fn strip_include_directive_metas(items: &mut Vec<Item>) {
    items.retain(|it| {
        !matches!(it, Item::Tok(t)
            if crate::pipeline::template_handler::is_includes_meta(t))
    });
}

/// Whether a token chunk ends at the start of a new line — a preceding `Nl`
/// token or text whose last character is a newline. Used by the template-body
/// comment strip to tell a comment-only line (eaten) from one at the source's
/// very start (kept).
fn ends_with_newline(last: Option<&Item>) -> bool {
    match last {
        Some(Item::Tok(ParsoidToken::Nl(_))) => true,
        Some(Item::Str(s)) => s.ends_with('\n'),
        _ => false,
    }
}

/// Strip comments nested in a link's content.
///
/// PHP's `processTemplateTokens` filters a body's token chunk *after*
/// `ExternalLinkHandler`/`WikiLinkHandler` have spliced a link's content into
/// that chunk, so `[http://… <!--c--> text]` and `[[Foo<!--c-->]]` lose the
/// comment alongside the body's own top-level ones. rustoid renders links in a
/// later pass, so the content is still nested in the link token's `mw:content`
/// / `mw:maybeContent` KV, out of the top-level filter's reach; remove it here.
///
/// File links are left alone: their `mw:maybeContent` parts are media options
/// and a caption, rendered through their own fragment pipeline.
fn strip_link_content_comments(items: &mut [Item]) {
    for item in items.iter_mut() {
        let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
            continue;
        };
        let key = match stt.name.as_str() {
            "extlink" => "mw:content",
            "wikilink" => {
                let href = stt
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))
                    .and_then(|kv| kv.value.as_str())
                    .unwrap_or_default();
                if is_file_link_target(href) {
                    continue;
                }
                "mw:maybeContent"
            }
            _ => continue,
        };
        for kv in stt
            .attribs
            .iter_mut()
            .filter(|kv| kv.key.as_str() == Some(key))
        {
            if let crate::wikitext::tokens_v2::KeyValue::Tokens(toks) = &mut kv.value {
                toks.retain(|it| !matches!(it, Item::Tok(ParsoidToken::Comment(_))));
            }
        }
    }
}

/// Mark the top-level tags of a chunk that came out of expanding a template body
/// or a module output.
///
/// `expand_attributes` runs once, at the page level, over a stream into which
/// every transclusion has already been flattened, so it cannot tell a page's own
/// tag from a body's. The `mw:ExpandedAttrs` marking belongs to the page chunk
/// only — Parsoid's `AttributeExpander` runs per chunk and a body's `inTemplate`
/// suppresses it — so the fact is carried on the token. See
/// [`crate::wikitext::tokens_v2::TempData::synthesized`].
/// Whether one of the token's attribute values carries a piece Parsoid's
/// `AttributeExpander` expands in the expansion's own pipeline — a
/// `mw:DOMFragment` placeholder or an extension token (`#tag:nowiki`).
///
/// Those are the attributes rustoid's page-level pass marks even on a
/// synthesized (template-output) token, so they are also the ones whose `about`
/// id Parsoid spends before the wrapper exists.
fn has_expandable_dom_fragment(tok: &ParsoidToken, config: &dyn SiteConfig) -> bool {
    tok.get_attribs().iter().any(|kv| {
        // `mw:maybeContent` is a wikilink/media link's *content*, not an HTML
        // attribute; the DOM fragment inside it is the link text, not an
        // expanded attribute (`ISO 3166-1 alpha-2`'s `{{mono}}` in a link).
        if kv
            .key
            .as_str()
            .is_some_and(|k| k.starts_with("mw:") || k == "data-mw" || k == "data-parsoid")
        {
            return false;
        }
        let crate::wikitext::tokens_v2::KeyValue::Tokens(items) = &kv.value else {
            return false;
        };
        items.iter().any(|item| match item {
            Item::Tok(t) => {
                t.get_name() == "extension"
                    || t.get_attribute_v("typeof")
                        .is_some_and(|ty| ty.starts_with("mw:DOMFragment"))
                    || is_extension_tag_call(t, config)
            }
            Item::Str(_) => false,
        })
    })
}

/// Whether a template token in an attribute value is a `{{#tag:<extension>|…}}`
/// call.
///
/// Such a call is nowiki-protected (or otherwise tunnelled through a strip
/// marker) by the core preprocessor, so it survives into Parsoid's token stream
/// and `AttributeExpander` expands it into a DOM fragment. A plain
/// `title={{Foo}}` is expanded to text by core and does *not* leave a token, so
/// it must not spend an id here.
fn is_extension_tag_call(tok: &ParsoidToken, config: &dyn SiteConfig) -> bool {
    if tok.get_name() != "template" {
        return false;
    }
    let src = tok
        .data_parsoid()
        .and_then(|d| d.src.as_deref())
        .unwrap_or("")
        .trim_start();
    let Some(rest) = src.strip_prefix("{{#tag:") else {
        return false;
    };
    let target = rest.split(['|', '}']).next().unwrap_or("").trim();
    config
        .extension_tags()
        .iter()
        .any(|e| e.eq_ignore_ascii_case(target))
}

fn mark_synthesized(items: &mut [Item]) {
    for_each_token_mut(items, &mut |tok| {
        if let Some(dp) = tok.data_parsoid_mut() {
            dp.tmp.synthesized = true;
        }
    });
}

/// Mark a module's (`#invoke`) output. Core renders that output as HTML, so its
/// link targets are NFC-composed (see `TempData::from_module`); a template
/// body's output is wikitext that Parsoid parses and is not marked here.
fn mark_from_module(items: &mut [Item]) {
    for item in items.iter_mut() {
        if let Item::Tok(tok) = item
            && let Some(dp) = tok.data_parsoid_mut()
        {
            dp.tmp.from_module = true;
        }
    }
}

/// How many `#ifexist` titles one render may resolve.
///
/// MediaWiki caps expensive parser functions at 500 per page and `#ifexist` is
/// one of them. The bound is on *distinct* titles here because rustoid answers
/// each title once and reuses the answer, which is a stricter budget than
/// MediaWiki's per-call count and therefore never exceeds it.
const MAX_IFEXIST_TITLES: usize = 500;

/// How many property values one `#property` call may fetch labels for.
///
/// A statement list is short in practice; the bound keeps a hostile entity from
/// turning one call into an unbounded crawl of Wikidata.
const MAX_PROPERTY_LABELS: usize = 32;

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

/// Build the lazy `frame.args` view of a raw argument list.
///
/// Scribunto expands an argument when the module first reads it, so the frame is
/// handed [`Arg::Lazy`](crate::lua::engine::Arg::Lazy) slots that index into the
/// raw list rather than expanded text. `source` says which list, because the
/// `#invoke` call's own arguments and the calling frame's are expanded through
/// the same code but cached separately.
///
/// The name is the tokenizer's, not re-derived from the value: only the
/// tokenizer can tell a `=` that names an argument from one inside a value. An
/// empty name is a positional argument, whose Lua key is its number.
fn lazy_args(
    args: &[crate::wikitext::tokens_v2::KV],
    source: ArgSource,
) -> Vec<crate::lua::engine::Arg> {
    use crate::lua::engine::Arg;
    use crate::wikitext::token_utils::key_value_to_string;

    args.iter()
        .enumerate()
        .map(|(index, kv)| {
            let key = key_value_to_string(&kv.key);
            let trimmed = key.trim();
            let name = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            Arg::Lazy(ArgSlot {
                source,
                index,
                name,
                raw: kv_value_source(kv),
            })
        })
        .collect()
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
/// - comments — dropped, as MediaWiki's preprocessor strips them before a
///   module sees an argument. A newline is *not* dropped: it is kept, because
///   the argument is expanded wikitext and the preprocessor preserves line
///   breaks (a navbox `above` value is a multi-line list).
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
/// - `extension` — **declined**. The preprocessor stashes an extension tag as a
///   strip marker, so a module receives `<UNIQ--ref-…-QINU>` and never the tag's
///   source; returning `None` hands the value to the marker renderer, which
///   keeps the token under a marker `substitute_strip_markers` splices back.
/// - an `mw:Entity` span (`&#32;`, `&nbsp;`) — the entity *as written*, i.e. the
///   span's `src`. Its decoded character and its end tag are skipped: the source
///   stands for all three, and re-parsing the module's output rebuilds the same
///   span. `Template:Redirect-several` is the case that pins it — its link list
///   separates terms with `&#32;and`/`&#44;&#32;and`, and without this arm the
///   whole value declined and fell back to a source range whose `{{{1|}}}` no
///   longer resolves in the module's frame, so the hatnote read "other terms".
/// - `template`/`template3` — the call's own source, from the token's `src`. A
///   value the tokenizer left with an unexpanded call inside it is handed to the
///   module as that call, so the module's output re-expands it where the module
///   put it.
/// - a live HTML element — start tag, content and end tag are separate items and
///   each is written as it was, which is the wikitext a module receives for one.
///   `Template:Nowrap` is the case that pins it: `{{nowrap|SEE}}` reaches a
///   module as `<span class="nowrap">SEE</span>`. Declining the `<span>` made the
///   whole value fall back to its *source range* — which the frame's substitution
///   had already made stale, so the module was handed the raw
///   `{{{1|{{{text|{{{content}}}}}}}}}` the argument was written as.
///
/// Anything else returns `None`, so the value keeps to its source range rather
/// than losing the construct.
fn argument_value_text(items: &[Item]) -> Option<String> {
    use crate::wikitext::tokens_v2::ParsoidToken;
    let mut out = String::new();
    let mut i = 0;
    while i < items.len() {
        match &items[i] {
            Item::Str(s) => out.push_str(s),
            // Newlines are dropped in *attribute* context (`tokensToString`),
            // but a module's argument is expanded wikitext and MediaWiki's
            // preprocessor keeps the newline: `|above=\n* a\n* b` reaches
            // `Module:Navbox` with the line break intact, which is what makes
            // the value a two-item list. Dropping it here merged the items into
            // one (`<li>a* b</li>`), so every navbox `above`/`below`/list lost
            // its list structure.
            Item::Tok(ParsoidToken::Nl(_)) => out.push('\n'),
            // A comment is rendered as its own markup so the whole-line rule in
            // [`strip_html_comments`] can see where it sat; its content is never
            // part of the value.
            Item::Tok(ParsoidToken::Comment(_)) => out.push_str("<!--c-->"),
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "mw-quote" => {
                let value = t.attribs.iter().find(|kv| kv.key.as_str() == Some("value"));
                out.push_str(
                    &value
                        .and_then(|kv| key_value_text(&kv.value))
                        .unwrap_or_else(|| "''".to_string()),
                );
            }
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "extension" => {
                // An extension tag is *stashed* as a strip marker by the
                // preprocessor, so a module is handed `<UNIQ--ref-…-QINU>`, never
                // the tag's source. Spelling the source leaked the tag's body —
                // and any comma or "and" in it — into the module's view:
                // `{{#invoke:Detect singular|pluralize|<ref>a, b</ref>|plural=s}}`
                // answered `s` where the service answers the empty string.
                // Declining hands the value to the marker renderer, which keeps
                // the token under a marker `substitute_strip_markers` splices back.
                return None;
            }
            // A template or parser-function call left unexpanded. Its source is the
            // call, and a module is handed that text so its own output re-expands it
            // where the module placed it. This is how `Template:Infobox sport`'s
            // `data3={{{nicknames|{{{nickname|}}}}}}` arrives: the tokenizer already
            // substituted the reference, so the value's tokens hold
            // `{{Plainlist|…}}` while the recorded source range still spans the
            // *unsubstituted* `{{{…}}}` — which, re-expanded in the module's own
            // frame, collapses to its empty default.
            Item::Tok(ParsoidToken::SelfclosingTag(t))
                if t.name == "template" || t.name == "template3" =>
            {
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
            // A bracketed external link: `[href content]`. A module's output
            // produces this token without a `src`, and declining on it dropped
            // the whole link from a re-serialized argument (`{{Refn|{{cite…}}}}`
            // lost the citation title).
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "extlink" => {
                let href = t
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))?;
                out.push('[');
                out.push_str(&key_value_text(&href.value)?);
                if let Some(content) = t
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("mw:content"))
                {
                    out.push(' ');
                    out.push_str(&key_value_text(&content.value)?);
                }
                out.push(']');
            }
            Item::Tok(ParsoidToken::Tag(t))
                if (t.name == "td" || t.name == "th") && cell_opener_source(t).is_some() =>
            {
                out.push_str(&cell_opener_source(t).unwrap_or_default());
            }
            // A `listItem` token: its `bullets` attribute holds the `*`/`#`/`;`
            // marker, and the item content follows as separate items. A module
            // argument that contains a list — `Module:Navbox`'s nested
            // `|list1 = * A` — arrives as these, with no `src` to read (the token
            // was built during expansion), so declining on it dropped the whole
            // value to its stale source range and the nested navbox came back as
            // the literal call `{{Navbox|child…}}`. Mirrors `tokens_to_string`.
            Item::Tok(ParsoidToken::Tag(t)) if t.name == "listItem" => {
                if let Some(bullets) = t
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("bullets"))
                    .and_then(|kv| kv.value.as_str())
                {
                    out.push_str(bullets);
                }
            }
            Item::Tok(tok) if crate::wikitext::token_utils::is_entity_span_token(tok) => {
                out.push_str(&token_source_for_module(tok)?);
                // Mirrors PHP's `$i += 2` — skip the decoded character and the
                // end tag, which the entity source stands in for.
                if matches!(items.get(i + 2), Some(Item::Tok(ParsoidToken::EndTag(e))) if e.name == "span")
                {
                    i += 2;
                }
            }
            // A live HTML element: the start tag, the content (which follows as
            // separate items) and the end tag are each written as they were.
            //
            // A start tag whose source still spells an attribute *construct*
            // (`<div {{#switch:…}}>`) is rebuilt from its expanded attributes
            // instead. The construct generated the attribute and the source cannot
            // be patched — its offsets are chunk-relative and the reparsed
            // attribute loses them — so left as written it reaches the module
            // unexpanded, and the module's output re-expands it in the *root*
            // frame, where a `{{{…}}}` it names no longer resolves
            // (`Template:Wikisource`'s `lang`).
            Item::Tok(tok @ ParsoidToken::Tag(_)) => {
                let src = token_source_for_module(tok)?;
                out.push_str(
                    &crate::pipeline::lua_deferred::stale_tag_html(
                        tok,
                        &src,
                        crate::pipeline::lua_deferred::MissingSrc::Html,
                    )
                    .unwrap_or(src),
                );
            }
            Item::Tok(tok @ ParsoidToken::EndTag(_)) => {
                out.push_str(&token_source_for_module(tok)?)
            }
            // A bare URL the tokenizer turned into a `urllink` has no `src` when
            // it came out of a *module's* output (only a page-source one is
            // stamped), and its wikitext is exactly its `href`. Reading only `src`
            // dropped it, so `Module:Citation/CS1`'s
            // `url={{#invoke:London Gazette util|url_param|…}}` came back as a
            // citation with no link where the wiki serves one.
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "urllink" => {
                let href = t
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))?;
                out.push_str(&key_value_text(&href.value)?);
            }
            // Every other self-closing token — `<br>`, an `extlink`, a behavior
            // switch like `__DATE__` — reaches the module as its own source, so a
            // value made of one is not a reason to decline.
            Item::Tok(tok @ ParsoidToken::SelfclosingTag(_)) => {
                out.push_str(&token_source_for_module(tok)?);
            }
            _ => return None,
        }
        i += 1;
    }
    Some(strip_html_comments(&out))
}

/// A token's own source with any substituted attribute written back.
///
/// `data_parsoid.src` is the source *as written*, but the token's attributes are
/// the expanded ones — a module is handed the expansion (see
/// [`crate::wikitext::token_utils::rewrite_expanded_attrs`]), so an attribute the
/// template's frame substituted would otherwise reach the module spelled the old
/// way. `Template:Fossil range/bar`'s `style` carries
/// `{{period color|{{{1}}}}}` and `{{#expr:…{{period start|{{{1}}}}}…}}`, and its
/// caller's frame substitutes `{{{1}}}` before the value is flattened; without
/// this the bar reaches a module as the unexpanded reference, and the module's
/// own `#iferror` check answers an error instead of the computed colour and
/// offsets. Falls back to the source as written when there is nothing to
/// rewrite.
fn token_source_for_module(tok: &ParsoidToken) -> Option<String> {
    let src = tok.data_parsoid()?.src.as_deref()?;
    Some(
        crate::wikitext::token_utils::rewrite_expanded_attrs(src, tok.get_attribs())
            .unwrap_or_else(|| src.to_string()),
    )
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

/// The wikitext a table-cell opener token stands for.
///
/// A `td`/`th` token in a text-returning branch is a *cell opener*, not
/// content: PHP's preprocessed source spells it as literal text (`||`, or
/// `| class="…" |`). The tokenizer records the opener in its TSR, but a token
/// that came out of a template expansion loses it (`Template:!!`, whose whole
/// source is `||`), so the opener is rebuilt from `startTagSrc` + `attrSrc` +
/// `attrSepSrc` — the same fields PHP's `buildTableTokens` writes. A `{{!}}`
/// magic marker has none of them set and stands for a single `|`. A literal
/// HTML `<td>` is not a wikitext cell and returns `None`.
fn cell_opener_source(t: &crate::wikitext::tokens_v2::TagTk) -> Option<String> {
    if t.name != "td" && t.name != "th" {
        return None;
    }
    let dp = &t.data_parsoid;
    if dp.stx.as_deref() == Some("html") {
        return None;
    }
    if let Some((Some(start), Some(source))) =
        dp.tsr.as_ref().map(|r| (r.start, r.source.as_deref()))
    {
        // The cell's TSR spans only the opener and its attribute box, never the
        // content, so the slice is exactly the wikitext the token stands for.
        if let Some(src) = source.get(start..dp.tsr.as_ref()?.end) {
            return Some(src.to_string());
        }
    }
    let row_syntax = dp.stx.as_deref() == Some("row");
    let default_opener = match (t.name.as_str(), row_syntax) {
        ("th", true) => "!!",
        ("th", false) => "!",
        (_, true) => "||",
        (_, false) => "|",
    };
    let attr_src = dp.tmp.attr_src.as_deref().unwrap_or("");
    let mut out = dp
        .start_tag_src
        .clone()
        .unwrap_or_else(|| default_opener.to_string());
    out.push_str(attr_src);
    if attr_src.is_empty() {
        // No attribute box: only a separator the tokenizer recorded is part of
        // the opener (`||` from a template body), not the default one.
        if let Some(sep) = &dp.attr_sep_src {
            out.push_str(sep);
        }
    } else {
        // A non-empty box is always followed by its separating pipe.
        out.push_str(dp.attr_sep_src.as_deref().unwrap_or("|"));
    }
    Some(out)
}

/// `tag_content_source` for the *source* of a `#tag` call.
///
/// A resolved DOM-fragment placeholder has no wikitext form. Parsoid records it
/// as its fragment marker (`PipelineUtils::pFragmentToParsoidFragmentMarkers`),
/// which is what the service keeps in an `extsrc` — for example the template
/// stylesheet a module emitted before an indicator's content. `tag_body_source`
/// deliberately does not use this: a body is re-parsed, so its placeholder must
/// stay alive under a strip marker.
pub(crate) fn tag_source_with_fragments(items: &[Item]) -> String {
    let mut replaced: Vec<Item> = Vec::with_capacity(items.len());
    let mut i = 0;
    while i < items.len() {
        match dom_fragment_placeholder_at(items, i) {
            Some((end, tokens)) => {
                let Some(id) = tokens.iter().find_map(fragment_id) else {
                    // A placeholder without an id cannot be named; keep the
                    // fallback's behaviour rather than inventing one.
                    replaced.extend_from_slice(&items[i..end]);
                    i = end;
                    continue;
                };
                replaced.push(Item::Str(format!("{{{{#parsoid\u{0}fragment:{id}}}}}")));
                i = end;
            }
            None => {
                replaced.push(items[i].clone());
                i += 1;
            }
        }
    }
    tag_content_source(&replaced).unwrap_or_else(|| {
        crate::pipeline::parser_functions::tag_content_fallback_source(&replaced)
    })
}

/// The `data-fragment-id` a DOM-fragment placeholder carries.
fn fragment_id(item: &Item) -> Option<usize> {
    let Item::Tok(tok) = item else {
        return None;
    };
    tok.get_attribs()
        .iter()
        .find(|kv| kv.key.as_str() == Some("data-fragment-id"))
        .and_then(|kv| kv.value.as_str())
        .and_then(|v| v.parse().ok())
}

/// Render `#tag` content tokens back to the wikitext the tag records as its
/// `source`/`extsrc`.
///
/// Core's `$frame->expand` returns the *expanded* text, but a token's
/// `data_parsoid.src` is the source as written, and that is stale once an
/// argument reference inside it — a link target, typically — has been
/// substituted. So the text is regenerated from the token's own fields, the way
/// [`argument_value_text`] does for a module argument. Two differences make this
/// its own function rather than a call to that one:
///
/// - newlines are **kept**: the tag content is wikitext, and `#tag:pre`'s
///   `extsrc` carries them while a Scribunto argument drops them;
/// - a token this cannot render makes the *whole* content fall back to the
///   recorded source (the caller's `unwrap_or_else`), so a construct it does not
///   know is never silently dropped.
pub(crate) fn tag_content_source(items: &[Item]) -> Option<String> {
    use crate::wikitext::tokens_v2::ParsoidToken;
    let mut out = String::new();
    let mut i = 0;
    while i < items.len() {
        match &items[i] {
            Item::Str(s) => out.push_str(s),
            // A comment is stripped by the preprocessor; a newline is content.
            Item::Tok(ParsoidToken::Comment(_)) => out.push_str("<!--c-->"),
            Item::Tok(ParsoidToken::Nl(_)) => out.push('\n'),
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
            // An `mw:Entity` span is spelled out by its `src`; the decoded text
            // it is followed by — and the `</span>` — carry no source of their
            // own (see `skip_entity_decoded_text`). Without this the stringify
            // fallback emitted the `src` *and* the decoded text, so a `&nbsp;` in
            // a `#tag` body became `&nbsp;` plus a literal NBSP: the doubled
            // `p.&nbsp;54` that `Module:Footnotes` hands `frame:extensionTag`.
            Item::Tok(tok) if crate::wikitext::token_utils::is_entity_span_token(tok) => {
                if let Some(src) = tok.data_parsoid().and_then(|dp| dp.src.as_deref()) {
                    out.push_str(src);
                }
                if crate::wikitext::token_utils::skip_entity_decoded_text(tok, items.get(i + 1)) {
                    i += 1;
                }
                if matches!(items.get(i + 1), Some(Item::Tok(ParsoidToken::EndTag(e))) if e.name == "span")
                {
                    i += 1;
                }
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
            // A bracketed external link: `[href content]`. A module's output
            // produces this token without a `src`, and declining on it dropped
            // the whole link from a re-serialized `#tag` body (`{{Refn|{{cite…}}}}`
            // lost the citation title).
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "extlink" => {
                let href = t
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))?;
                out.push('[');
                out.push_str(&key_value_text(&href.value)?);
                if let Some(content) = t
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("mw:content"))
                {
                    out.push(' ');
                    out.push_str(&key_value_text(&content.value)?);
                }
                out.push(']');
            }
            Item::Tok(ParsoidToken::Tag(t))
                if (t.name == "td" || t.name == "th") && cell_opener_source(t).is_some() =>
            {
                out.push_str(&cell_opener_source(t).unwrap_or_default());
            }
            // A live HTML element: the start tag, the content (which follows as
            // separate items) and the end tag are each written as they were.
            // Declining on them made the fallback drop any following token that
            // has no `src` of its own — a module's `<span>[[link]]</span>`
            // stringifies to an empty span instead of a link.
            Item::Tok(tok @ ParsoidToken::Tag(_)) => {
                out.push_str(&token_source_for_module(tok)?);
            }
            Item::Tok(tok @ ParsoidToken::EndTag(_)) => {
                out.push_str(&token_source_for_module(tok)?);
            }
            _ => return None,
        }
        i += 1;
    }
    Some(strip_html_comments(&out))
}

/// Strip HTML comments, as the preprocessor does before a template body is
/// expanded.
///
/// A comment inside a `#tag` argument is not a `Comment` token: the tokenizer
/// keeps it as text inside the argument value (`Template:Top icon`'s file spec
/// carries `<!--\n-->` between its parameters), so `$frame->expand` on the
/// service returns the content with the comments gone and the surrounding
/// whitespace kept. An unclosed `<!--` swallows the rest, which is what the
/// preprocessor does with one.
///
/// A comment that sits on a line of its own — preceded by a newline and
/// followed by one, with only spaces or tabs around it — takes that whole line
/// with it: `\n  <!--c-->  \n` becomes `\n`. MediaWiki's preprocessor does the
/// same, which is why `{{Reflist|refs=…}}`'s `extsrc` keeps one newline fewer
/// than the source between two refs separated by a commented-out section
/// header. A comment at the very start of the content has no preceding newline
/// and is removed alone.
pub(crate) fn strip_html_comments(s: &str) -> String {
    if !s.contains("<!--") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        // The text between the last newline and the comment: a line of its own
        // only when it is blank.
        let line_start = out.rfind('\n').map(|n| n + 1);
        let blank_before =
            line_start.is_some_and(|from| out[from..].chars().all(|c| c == ' ' || c == '\t'));
        let mut skip_newline = false;
        let after = match rest[start + 4..].find("-->") {
            Some(end) => {
                let after = &rest[start + 4 + end + 3..];
                // A following line break, reached over spaces/tabs only.
                let trimmed = after.trim_start_matches([' ', '\t']);
                skip_newline = trimmed.starts_with('\n');
                after
            }
            // An unclosed comment swallows the rest.
            None => "",
        };
        // A comment on a line of its own takes that line's break with it; any
        // other comment leaves the break alone.
        if blank_before && skip_newline {
            // Drop the horizontal whitespace the comment's line began with.
            if let Some(from) = line_start {
                out.truncate(from);
            }
            rest = after
                .trim_start_matches([' ', '\t'])
                .strip_prefix('\n')
                .unwrap_or(after);
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
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

    /// A comment nested in a link's content is stripped (the body chunk filter
    /// runs after the link handlers splice it in); a plain attribute value keeps
    /// it, and a file link's options are left alone.
    #[test]
    fn strip_link_content_comments_drops_content_comments_only() {
        use crate::wikitext::tokens_v2::{CommentTk, KV, KeyValue, ParsoidToken, SelfclosingTagTk};

        let comment = || {
            Item::Tok(ParsoidToken::Comment(CommentTk::new(
                "c",
                Default::default(),
            )))
        };
        let s = |t: &str| Item::Str(t.to_string());
        let kv = |key: &str, value: KeyValue| KV {
            key: KeyValue::Str(key.to_string()),
            value,
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };

        let mut ext = SelfclosingTagTk::new("extlink", vec![], Default::default());
        ext.attribs
            .push(kv("href", KeyValue::Str("http://x".into())));
        ext.attribs
            .push(kv("mw:content", KeyValue::Tokens(vec![comment(), s("y")])));

        let mut wl = SelfclosingTagTk::new("wikilink", vec![], Default::default());
        wl.attribs.push(kv("href", KeyValue::Str("Foo".into())));
        wl.attribs.push(kv(
            "mw:maybeContent",
            KeyValue::Tokens(vec![comment(), s("ar")]),
        ));

        let mut file = SelfclosingTagTk::new("wikilink", vec![], Default::default());
        file.attribs
            .push(kv("href", KeyValue::Str("File:X.jpg".into())));
        file.attribs.push(kv(
            "mw:maybeContent",
            KeyValue::Tokens(vec![comment(), s("cap")]),
        ));

        let mut items = vec![
            comment(), // a body-level comment is *not* this function's job
            Item::Tok(ParsoidToken::SelfclosingTag(ext)),
            Item::Tok(ParsoidToken::SelfclosingTag(wl)),
            Item::Tok(ParsoidToken::SelfclosingTag(file)),
        ];
        strip_link_content_comments(&mut items);

        let content = |it: &Item| {
            match it {
            Item::Tok(ParsoidToken::SelfclosingTag(t)) => t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("mw:content"))
                .or_else(|| {
                    t.attribs
                        .iter()
                        .find(|kv| kv.key.as_str() == Some("mw:maybeContent"))
                })
                .map(|kv| matches!(kv.value, KeyValue::Tokens(ref v) if v.iter().any(|i| matches!(i, Item::Tok(ParsoidToken::Comment(_))))))
                .unwrap_or(false),
            _ => false,
        }
        };
        // The body-level comment is untouched; the extlink and plain wikilink
        // lost theirs; the file link kept its.
        assert!(matches!(items[0], Item::Tok(ParsoidToken::Comment(_))));
        assert!(!content(&items[1]));
        assert!(!content(&items[2]));
        assert!(content(&items[3]));
    }

    /// A rendered `<ref>` body's discarded-id count is the number of extension
    /// wrappers in it, with lean `<nowiki>` excluded. See
    /// [`Parser::count_note_extensions`].
    #[test]
    fn count_note_extensions_counts_wrappers() {
        use crate::dom::node::{ElementKind, Node};

        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let ext = |typeof_: &str| {
            let mut n = Node::element(ElementKind::Other("style".to_string()));
            n.set_attr("typeof", typeof_);
            n
        };
        let mut body = Node::document();
        // A plain element and a text run contribute nothing.
        body.push_child(Node::element(ElementKind::Span));
        body.push_child(Node::text("x"));
        // A templatestyles and a `<pre>` (with a nested one) each count.
        body.push_child(ext("mw:Extension/templatestyles"));
        let mut pre = ext("mw:Extension/pre");
        pre.push_child(ext("mw:Extension/templatestyles"));
        body.push_child(pre);
        // `<nowiki>` is lean markup and is not counted.
        body.push_child(ext("mw:Extension/nowiki"));
        assert_eq!(parser.count_note_extensions(&body), 3);
    }

    /// A nested `<ref>`'s own note is spliced in with the enclosing note, so its
    /// wrappers are re-numbered too. [`Parser::count_note_extensions`] must follow
    /// the placeholder (`about` → stashed body in `ref_bodies`) into that note,
    /// and must count the placeholder itself (it is still an `mw:Extension`
    /// marker, and it too is re-numbered). Lean `<nowiki>` is excluded.
    #[test]
    fn count_note_extensions_follows_a_nested_ref_note() {
        use crate::dom::node::{ElementKind, Node};

        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let ext = |ty: &str, about: &str| {
            let mut n = Node::element(ElementKind::Other("style".to_string()));
            n.set_attr("typeof", ty);
            n.set_attr("about", about);
            n
        };

        // The enclosing note's render: a templatestyles plus the nested ref, which
        // is still an `mw:Extension` placeholder at this point.
        let mut outer = Node::document();
        outer.push_child(ext("mw:Extension/templatestyles", "#mwt2"));
        let mut nested = Node::element(ElementKind::Other("extension".to_string()));
        nested.set_attr("typeof", "mw:Extension");
        nested.set_attr("about", "#mwt5");
        outer.push_child(nested);

        // The nested ref's note, stashed under its `about`.
        let mut note = Node::document();
        note.push_child(ext("mw:Extension/templatestyles", "#mwt7"));
        parser
            .ref_bodies
            .borrow_mut()
            .insert("#mwt5".to_string(), note);

        // Both stylesheets and the placeholder itself.
        assert_eq!(parser.count_note_extensions(&outer), 3);
    }

    #[test]
    fn test_has_token_attributes() {
        use crate::wikitext::tokens_v2::{KeyValue, SelfclosingTagTk};

        // A plain-string attribute and a token-array attribute.
        let tok = |value: KeyValue| {
            let mut stt = SelfclosingTagTk::new(
                "span",
                vec![],
                crate::wikitext::tokens_v2::DataParsoid::default(),
            );
            stt.attribs.push(crate::wikitext::tokens_v2::KV {
                key: KeyValue::Str("style".to_string()),
                value,
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            });
            Item::Tok(ParsoidToken::SelfclosingTag(stt))
        };

        assert!(!has_token_attributes(&[tok(KeyValue::Str(
            "x".to_string()
        ))]));
        assert!(has_token_attributes(&[tok(KeyValue::Tokens(vec![
            Item::Str("x".to_string())
        ]))]));
        // A bare string item never counts.
        assert!(!has_token_attributes(&[Item::Str("x".to_string())]));
    }

    /// A marker a module returned inside an attribute is spliced back there:
    /// [`Parser::render_answer_markers`] spells a nested `#tag` as a strip marker
    /// (`title="<UNIQ--nowiki-…-QINU>"`), so the splice has to reach into the
    /// attribute value, not only a top-level text run.
    #[test]
    fn substitute_strip_markers_reaches_inside_an_attribute() {
        use crate::wikitext::tokens_v2::{DataParsoid, KV, KeyValue, SelfclosingTagTk, TagTk};

        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        // The nested extension the marker stands for — a nowiki.
        let mut nowiki = SelfclosingTagTk::new("extension", vec![], DataParsoid::default());
        nowiki.add_attribute_str("name", "nowiki");
        let marker = parser.record_marker(
            "nowiki",
            vec![Item::Tok(ParsoidToken::SelfclosingTag(nowiki))],
        );

        // `title="<marker>"`, as a module's output re-tokenizes.
        let span = TagTk::new(
            "span",
            vec![KV {
                key: KeyValue::Str("title".to_string()),
                value: KeyValue::Str(marker),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            }],
            DataParsoid::default(),
        );
        let out = parser.substitute_strip_markers(vec![Item::Tok(ParsoidToken::Tag(span))]);

        let Item::Tok(ParsoidToken::Tag(t)) = &out[0] else {
            panic!("expected a span tag, got {:?}", out[0]);
        };
        let title = t
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("title"))
            .expect("the span keeps its title");
        match &title.value {
            KeyValue::Tokens(items) => {
                assert_eq!(items.len(), 1);
                assert!(
                    matches!(&items[0], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "extension")
                );
            }
            other => panic!("expected the marker resolved to tokens, got {other:?}"),
        }
    }

    /// A `#tag` body that holds an expanded placeholder keeps it: the wikitext
    /// round-trip cannot spell a `mw:DOMFragment`, so
    /// [`Parser::tag_body_source`] replaces it with a strip marker that
    /// `substitute_strip_markers` splices back. Without it the placeholder — a
    /// `<templatestyles>` a template argument resolved before the call — was
    /// dropped from `{{refn|name=A|{{cite web|…}}}}`.
    #[test]
    fn tag_body_source_marks_a_dom_fragment_placeholder() {
        use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, TagTk};

        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        // The `<style typeof="mw:DOMFragment">` + `</style>` pair a resolved
        // `<templatestyles>` is reached through, with no `src` of its own.
        let mut open = TagTk::new("style", vec![], DataParsoid::default());
        open.add_attribute_str("typeof", "mw:DOMFragment");
        open.add_attribute_str("data-fragment-id", "0");
        let content = vec![
            Item::Tok(ParsoidToken::Tag(open)),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "style",
                vec![],
                DataParsoid::default(),
            ))),
        ];

        let source = parser.tag_body_source(&content);
        assert!(
            source.contains('\u{7f}'),
            "placeholder not marked: {source:?}"
        );
        assert!(!source.contains("<style"), "serialized instead: {source:?}");

        // The marker resolves back to the very pair, in place.
        let out = parser.substitute_strip_markers(vec![Item::Str(source)]);
        assert_eq!(out.len(), 2, "got {out:?}");
        let Item::Tok(ParsoidToken::Tag(t)) = &out[0] else {
            panic!("expected the style tag, got {:?}", out[0]);
        };
        assert_eq!(t.name, "style");
        assert!(
            t.attribs
                .iter()
                .any(|kv| kv.key.as_str() == Some("data-fragment-id")
                    && kv.value.as_str() == Some("0")),
            "the placeholder keeps its fragment id: {t:?}"
        );
        assert!(
            matches!(&out[1], Item::Tok(ParsoidToken::EndTag(e)) if e.name == "style"),
            "expected the matching end tag, got {:?}",
            out[1]
        );
    }

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
    fn an_argument_value_renders_a_live_element_from_its_source() {
        use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, TagTk};

        // A live element is written as it was: `{{nowrap|SEE}}` reaches a module
        // as `<span class="nowrap">SEE</span>`. The substitution that produced it
        // left the value's recorded *source range* spelling the unexpanded
        // `{{{1|…}}}`, so the tag has to render itself rather than fall back to
        // that stale range.
        let mut span = TagTk::new("span", vec![], DataParsoid::default());
        span.data_parsoid.src = Some("<span class=\"nowrap\">".to_string());
        let mut end = EndTagTk::new("span", vec![], DataParsoid::default());
        end.data_parsoid.src = Some("</span>".to_string());
        let items = vec![
            Item::Tok(ParsoidToken::Tag(span)),
            Item::Str("SEE".to_string()),
            Item::Tok(ParsoidToken::EndTag(end)),
        ];
        assert_eq!(
            argument_value_text(&items).as_deref(),
            Some("<span class=\"nowrap\">SEE</span>")
        );
    }

    /// A live element whose attribute a template frame substituted must render
    /// the *expanded* value, not the source as written: the tag's `src` still
    /// spells `{{{1}}}` inside `Template:Fossil range/bar`'s `style`, and a module
    /// handed that reaches its `#iferror` check as an error instead of the
    /// computed colour. Mirrors `render_answer` in `lua_deferred`, which writes
    /// substituted attributes back for the same reason.
    #[test]
    fn an_argument_value_writes_a_substituted_attribute_back() {
        use crate::wikitext::tokens_v2::{DataParsoid, KV, KeyValue, TagTk};
        let written = r"background-color:{{period color|{{{1}}}}};";
        let expanded = "background-color:rgb(127,160,86);";
        let style = KV {
            key: KeyValue::Str("style".to_string()),
            value: KeyValue::Str(expanded.to_string()),
            src_offsets: None,
            ksrc: Some("style".to_string()),
            vsrc: Some(written.to_string()),
        };
        let mut div = TagTk::new("div", vec![style], DataParsoid::default());
        div.data_parsoid.src = Some(format!(r#"<div style="{written}">"#));
        let items = vec![Item::Tok(ParsoidToken::Tag(div))];
        assert_eq!(
            argument_value_text(&items).as_deref(),
            Some(r#"<div style="background-color:rgb(127,160,86);">"#)
        );
    }

    /// A `listItem` carries its marker in `bullets` and has no `src`, so the
    /// renderer must read the attribute rather than decline. A module argument
    /// that holds a list — the nested navbox's `|list1 = * A` — arrives this way,
    /// and declining dropped the whole value to a stale source range, which made
    /// the nested `{{Navbox|child…}}` come back as literal text and loop.
    #[test]
    fn an_argument_value_renders_a_list_item_from_its_bullets() {
        use crate::wikitext::tokens_v2::{DataParsoid, KV, KeyValue, TagTk};
        let bullets = KV {
            key: KeyValue::Str("bullets".to_string()),
            value: KeyValue::Str("*".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        let item = TagTk::new("listItem", vec![bullets], DataParsoid::default());
        let items = vec![
            Item::Tok(ParsoidToken::Tag(item)),
            Item::Str(" A".to_string()),
        ];
        assert_eq!(argument_value_text(&items).as_deref(), Some("* A"));
    }

    /// A value the exact renderer declines but that holds a **table** is rendered
    /// like a module answer: the `{|` table is rebuilt from its token data and a
    /// `mw:DOMFragment` placeholder becomes a `UNIQ…QINU` strip marker (so the
    /// module's output re-splices it). `Module:Clade`'s
    /// `1={{clade|1=a|2=b}}` is the case that needs it — the module
    /// string-matches the value for `{|class="clade"`.
    #[test]
    fn render_answer_markers_carries_a_table_and_a_placeholder() {
        use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, KV, KeyValue, TagTk};
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let attr = |k: &str, v: &str| KV {
            key: KeyValue::Str(k.to_string()),
            value: KeyValue::Str(v.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        let style = TagTk::new(
            "style",
            vec![
                attr("typeof", "mw:DOMFragment"),
                attr("data-fragment-id", "3"),
            ],
            DataParsoid::default(),
        );
        let items = vec![
            Item::Tok(ParsoidToken::Tag(TagTk::new(
                "table",
                vec![],
                DataParsoid::default(),
            ))),
            Item::Tok(ParsoidToken::Tag(style)),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "style",
                vec![],
                DataParsoid::default(),
            ))),
            Item::Str("a".to_string()),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "table",
                vec![],
                DataParsoid::default(),
            ))),
        ];
        let s = parser.render_answer_markers(&items);
        assert!(s.contains("{|"), "got: {s}");
        assert!(s.contains("|}"), "got: {s}");
        assert!(s.contains("UNIQ--templatestyles-"), "got: {s}");
        // The markers carry MediaWiki's `'"`` armoring around `UNIQ-`/`-QINU`,
        // which makes them 42 bytes rather than 36 — a length a module can
        // *measure* (Module:Navbox's `argHash`). See [`strip_marker`].
        assert!(
            s.contains("\u{7f}'\"`UNIQ--templatestyles-00000000-QINU`\"'\u{7f}"),
            "got: {s}"
        );
        assert!(s.contains('a'), "got: {s}");
    }

    /// Any other self-closing token is handed to the module as its own source:
    /// a bare `urllink` is a citation's `url` (declining it made
    /// `Module:Citation` render the wrong URL), and a `br`/behavior switch is
    /// likewise representable.
    #[test]
    fn an_argument_value_renders_a_self_closing_token_from_its_source() {
        use crate::wikitext::tokens_v2::{DataParsoid, SelfclosingTagTk};
        let mut br = SelfclosingTagTk::new("br", vec![], DataParsoid::default());
        br.data_parsoid.src = Some("<br>".to_string());
        let items = vec![Item::Tok(ParsoidToken::SelfclosingTag(br))];
        assert_eq!(argument_value_text(&items).as_deref(), Some("<br>"));
    }

    #[test]
    fn an_argument_value_the_renderer_cannot_represent_is_declined() {
        use crate::wikitext::tokens_v2::{DataParsoid, TagTk};
        // A token with no source of its own has no faithful text, so the renderer
        // must return `None` and let the caller keep to the source range rather
        // than drop the token.
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
        assert!(html.contains("<b"), "got: {html}");
        assert!(html.contains("bold"), "got: {html}");
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
        let html = parser
            .value_to_dom_html(&kv, "title", &mut fragments, &next_id)
            .0;
        assert_eq!(html, "color:red", "got: {html:?}");
    }

    #[test]
    fn test_wikitext_to_html_bold() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("'''bold'''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<b"), "got: {html}");
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
        assert!(html.contains("<i"), "got: {html}");
        assert!(html.contains("italic"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_bold_italic() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("'''''both'''''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<b"), "got: {html}");
        assert!(html.contains("<i"), "got: {html}");
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
        assert!(html.contains("<i"), "got: {html}");
        assert!(html.contains("foo'"), "got: {html}");
        assert!(html.contains("<b"), "got: {html}");
        assert!(html.contains("bar"), "got: {html}");
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
    async fn test_dynamic_template_argument_names_are_expanded() {
        use crate::mock::MockDataSource;

        // MediaWiki's preprocessor expands an argument *name* in the caller's
        // frame. `Template:Multi-sport event sidebar` relies on it:
        // `| {{#ifeq:…|yes|list1title|title}} = …`.
        let source = MockDataSource::new();
        source.add_template("Template:Dyn", "{{{title|missing}}}/{{{content1|empty}}}");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        let html = parser
            .wikitext_to_html_expanded(
                "{{Dyn|{{#ifeq:no|yes|list1title|title}}=Shown|{{#ifeq:no|yes|list1|content1}}=Body}}",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        assert!(html.contains("Shown"), "got: {html}");
        assert!(html.contains("Body"), "got: {html}");
        assert!(!html.contains("missing"), "got: {html}");
        assert!(!html.contains("empty"), "got: {html}");
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

    /// Core `Parser::braceSubstitution`'s T2529: a template whose expansion
    /// begins with a list/table marker and does not start a line gets a newline
    /// in front, so the marker is seen at SOL. The newline goes outside the
    /// transclusion's wrapper marker.
    #[test]
    fn test_t2529_force_line_start() {
        let marker = Item::Tok(ParsoidToken::SelfclosingTag(
            crate::wikitext::tokens_v2::SelfclosingTagTk::new(
                "meta",
                vec![crate::wikitext::tokens_v2::KV {
                    key: crate::wikitext::tokens_v2::KeyValue::Str("typeof".to_string()),
                    value: crate::wikitext::tokens_v2::KeyValue::Str("mw:Transclusion".to_string()),
                    src_offsets: None,
                    ksrc: None,
                    vsrc: None,
                }],
                crate::wikitext::tokens_v2::DataParsoid::default(),
            ),
        ));
        // A nested call that did not start a line: newline first, then the
        // re-tokenized content.
        let out = t2529_force_line_start(
            vec![marker.clone(), Item::Str("* a".to_string())],
            Some(false),
            true,
            false,
        );
        assert!(
            matches!(out[0], Item::Tok(ParsoidToken::Nl(_))),
            "newline first"
        );
        assert!(
            !matches!(out[1], Item::Str(_)),
            "`* a` re-tokenized into list syntax: {:?}",
            out[1]
        );
        // Parsoid expands the page's own calls at `sol => true`, so the marker
        // is re-tokenized but no newline is inserted (T382464).
        let out = t2529_force_line_start(
            vec![marker.clone(), Item::Str("* a".to_string())],
            Some(false),
            false,
            false,
        );
        assert!(!matches!(out[0], Item::Tok(ParsoidToken::Nl(_))));
        assert!(!matches!(out[0], Item::Str(_)), "re-tokenized at SOL");
        // At a line start the marker is still re-tokenized (Parsoid tokenizes
        // the expansion at SOL), just without a newline; content that is not a
        // marker is unchanged.
        assert!(!matches!(
            t2529_force_line_start(vec![Item::Str("* a".to_string())], Some(true), true, false)[0],
            Item::Str(_)
        ));
        assert!(matches!(
            t2529_force_line_start(
                vec![marker.clone(), Item::Str("plain".to_string())],
                Some(false),
                true,
                false
            )[0],
            Item::Tok(ParsoidToken::SelfclosingTag(_))
        ));
        // Inside an attribute value the newline is text (PHP splices it into the
        // value string) and the marker stays text: block syntax is not parsed
        // there.
        let out = t2529_force_line_start(
            vec![marker, Item::Str(";".to_string())],
            Some(false),
            true,
            true,
        );
        assert!(matches!(&out[0], Item::Str(s) if s == "\n"));
        assert!(matches!(out.last(), Some(Item::Str(s)) if s == ";"));
    }

    /// A table start (`{|`) is a token with no `src`, so the marker is read from
    /// `start_tag_src`; and a token with no source range at all (synthesised,
    /// e.g. from a module's output) is assumed to start its line rather than
    /// given an invented newline. Getting the second wrong added a stray `\n`
    /// before `Zebra`'s infobox table while fixing the one in `Hydrogen`.
    #[test]
    fn test_t2529_reads_a_table_marker_and_ignores_synthesised_tokens() {
        let dp = crate::wikitext::tokens_v2::DataParsoid {
            start_tag_src: Some("{|".to_string()),
            ..Default::default()
        };
        let table = Item::Tok(ParsoidToken::Tag(crate::wikitext::tokens_v2::TagTk::new(
            "table",
            vec![],
            dp,
        )));
        let out = t2529_force_line_start(vec![table.clone()], Some(false), true, false);
        assert!(
            matches!(out[0], Item::Tok(ParsoidToken::Nl(_))),
            "a table start not at SOL gets a newline"
        );

        let synthesized =
            ParsoidToken::SelfclosingTag(crate::wikitext::tokens_v2::SelfclosingTagTk::new(
                "template",
                vec![],
                crate::wikitext::tokens_v2::DataParsoid::default(),
            ));
        assert_eq!(t2529_at_line_start(&synthesized, "", true), None);
    }

    /// A construct that generates a whole attribute (`<div {{#switch:…}}>`) has
    /// no attribute-level source to rewrite, so a tag whose source still spells
    /// one is rebuilt from its expanded attributes before a module sees it. Left
    /// stale, the construct reaches the module unexpanded and is then re-expanded
    /// in the *root* frame, where the `{{{…}}}` it names no longer resolves.
    #[tokio::test]
    async fn a_generated_attribute_reaches_a_module_expanded() {
        use crate::mock::MockDataSource;

        let config = MockSiteConfig::new();
        let source = MockDataSource::new();
        source.add_module(
            "Module:Echo",
            "return { main = function(frame) return frame.args[1] end }",
        );
        source.add_template(
            "Template:Inner",
            "<div {{#switch:{{{lang}}}|en=|#default=lang=\"{{{lang}}}\"}} style=\"x\">y</div>",
        );
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html_expanded(
                "{{#invoke:Echo|main|{{Inner|lang=fr}}}}",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        assert!(
            html.contains("lang=\"fr\""),
            "the generated attribute reaches the module expanded: {html}"
        );
        assert!(
            !html.contains("{{{lang}}}"),
            "the unexpanded reference must not survive: {html}"
        );
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

        // The reset is per *block*, not per line of the same paragraph: the
        // tokenizer yields one chunk per block line, so a template on the next
        // line is numbered after the previous line's extensions. This is the
        // shape that put `Zebro`'s `{{reflist}}` at `#mwt122` rather than `#mwt8`.
        let next_line = render("<ref name=\"r\">b</ref>{{1x|a}}\n{{1x|c}}").await;
        assert!(
            next_line.contains(r##"<span about="#mwt3""##),
            "the next line's template is numbered after the previous block: {next_line}"
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

    /// An unresolvable template target still spends an `about` id.
    ///
    /// PHP's `TemplateHandler::onTemplate` constructs the `TemplateEncapsulator`
    /// (whose constructor calls `Env::newAboutId`) *before* the target is
    /// resolved, so `{{[[Foo|bar]]}}` — which only renders back as literal text —
    /// leaves a gap in the sequence. `Module:Citation/CS1` writes its maintenance
    /// message in exactly that shape, so missing the spend put every later
    /// transclusion one id low (`France`'s Buchenwald note).
    #[tokio::test]
    async fn an_unresolvable_template_target_still_spends_an_about_id() {
        let source = crate::mock::MockDataSource::new();
        source.add_template("Template:Real", "real");
        let config = crate::mock::MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html_expanded(
                "a{{[[Foo|bar]]}}b{{Real}}c",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        // The target renders back as literal `{{` + link + `}}` …
        assert!(html.contains("{{"), "got: {html}");
        // … but the spent id moves the following transclusion to #mwt2.
        assert!(html.contains(r##"about="#mwt2""##), "got: {html}");
        assert!(!html.contains(r##"about="#mwt1""##), "got: {html}");
    }

    /// An attribute that Parsoid's `AttributeExpander` expands inside a
    /// template's own pipeline spends its `about` id there, before the outer
    /// chunk continues — even though the transclusion wrapper overwrites it.
    /// rustoid reserves that id in `expand_one_template` so the page-level
    /// attribute pass keeps the position (`France`, ref 319).
    #[tokio::test]
    async fn an_attribute_expanded_in_a_template_body_spends_its_id_early() {
        let source = crate::mock::MockDataSource::new();
        source.add_template(
            "Template:Abbr",
            r#"<abbr title="{{#tag:nowiki|x}}">y</abbr>"#,
        );
        source.add_template("Template:After", "z");
        let config = crate::mock::MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html_expanded(
                "{{Abbr}} {{After}}",
                &source,
                &ParserOptions::for_page("Test"),
            )
            .await
            .unwrap();
        // `Abbr` is #mwt1; its expanded `title` spends #mwt2 (and is then
        // overwritten by the wrapper), so `After` is #mwt3.
        assert!(html.contains(r##"about="#mwt1""##), "got: {html}");
        assert!(html.contains(r##"about="#mwt3""##), "got: {html}");
    }

    /// `tag_content_source` must rebuild a link from its expanded fields (the
    /// recorded `data_parsoid.src` is stale once the target is substituted), drop
    /// comments the way the preprocessor does, and keep newlines — the three
    /// things that make a `#tag` `extsrc` match the service.
    #[test]
    fn tag_content_source_rebuilds_a_link_and_drops_comments_but_keeps_newlines() {
        use crate::wikitext::tokens_v2::{KV, KeyValue, NlTk, ParsoidToken, SelfclosingTagTk};

        let kv = |k: &str, v: &str| KV {
            key: KeyValue::Str(k.to_string()),
            value: KeyValue::Str(v.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        // `[[File:X.svg|20px<!--c--> ]]` with the href already substituted and the
        // comment held as text inside the content value, which is what the
        // tokenizer produces for a comment in a link.
        let mut link = SelfclosingTagTk::new("wikilink", vec![], Default::default());
        link.attribs.push(kv("href", "File:X.svg"));
        link.attribs.push(kv("mw:maybeContent", "20px<!--c--> "));
        let items = vec![
            Item::Tok(ParsoidToken::SelfclosingTag(link)),
            Item::Tok(ParsoidToken::Nl(NlTk {
                data_parsoid: Default::default(),
            })),
        ];
        assert_eq!(
            tag_content_source(&items),
            Some("[[File:X.svg|20px ]]\n".to_string())
        );
        // A token it cannot render makes the whole content fall back, so the
        // caller keeps the recorded source rather than losing the construct.
        assert_eq!(
            tag_content_source(&[Item::Tok(ParsoidToken::SelfclosingTag(
                SelfclosingTagTk::new("template", vec![], Default::default())
            ))]),
            None
        );
    }

    /// A resolved DOM-fragment placeholder in a `#tag` source is written as
    /// Parsoid's fragment marker, which is what the service keeps in an
    /// `extsrc`; the body renderer (not this function) keeps it alive instead.
    #[test]
    fn tag_source_names_a_fragment_placeholder() {
        use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, KeyValue, ParsoidToken, TagTk};

        let attr = |k: &str, v: &str| crate::wikitext::tokens_v2::KV {
            key: KeyValue::Str(k.to_string()),
            value: KeyValue::Str(v.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        let style = TagTk::new(
            "style",
            vec![
                attr("typeof", "mw:DOMFragment"),
                attr("data-fragment-id", "7"),
            ],
            DataParsoid::default(),
        );
        let items = vec![
            Item::Str("a".to_string()),
            Item::Tok(ParsoidToken::Tag(style)),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "style",
                vec![],
                DataParsoid::default(),
            ))),
            Item::Str("b".to_string()),
        ];
        assert_eq!(
            tag_source_with_fragments(&items),
            "a{{#parsoid\u{0}fragment:7}}b"
        );
        // Without a placeholder the two serializers agree.
        let plain = vec![Item::Str("a".to_string()), Item::Str("b".to_string())];
        assert_eq!(tag_source_with_fragments(&plain), "ab");
    }

    /// A bare `mw:dom-fragment-token`'s strip marker is named after the extension
    /// it stands for, as the preprocessor names it. `Module:Coordinates.coordinsert`
    /// matches `UNIQ--indicator-…-QINU` to replace the indicator a coord call
    /// emitted; naming every bare placeholder `gallery` left the original
    /// indicator in place, its `extsrc` unmodified (`Berlin`).
    #[test]
    fn a_bare_fragment_placeholder_is_named_for_its_extension() {
        use crate::wikitext::tokens_v2::{DataParsoid, KeyValue, SelfclosingTagTk};

        let placeholder = |attrs: &[(&str, &str)]| {
            let mut tok =
                SelfclosingTagTk::new("mw:dom-fragment-token", vec![], DataParsoid::default());
            for (k, v) in attrs {
                tok.attribs.push(crate::wikitext::tokens_v2::KV {
                    key: KeyValue::Str(k.to_string()),
                    value: KeyValue::Str(v.to_string()),
                    src_offsets: None,
                    ksrc: None,
                    vsrc: None,
                });
            }
            tok
        };
        let name = |attrs: &[(&str, &str)]| {
            let items = vec![Item::Tok(ParsoidToken::SelfclosingTag(placeholder(attrs)))];
            placeholder_span(&items, 0)
                .expect("a placeholder spans one item")
                .2
        };

        assert_eq!(name(&[("typeof", "mw:Extension/indicator")]), "indicator");
        // A tunnelled fragment with no type of its own is a gallery.
        assert_eq!(name(&[("data-fragment-id", "7")]), "gallery");
    }

    /// A live HTML element in a `#tag` body is written from its source, and a
    /// wikitext link that follows it keeps its content even though the link
    /// token has no `src` of its own (it came from a module's string).
    #[test]
    fn tag_content_source_keeps_a_link_inside_a_live_element() {
        use crate::wikitext::tokens_v2::{
            DataParsoid, EndTagTk, KeyValue, ParsoidToken, SelfclosingTagTk, TagTk,
        };

        let kv = |k: &str, v: &str| crate::wikitext::tokens_v2::KV {
            key: KeyValue::Str(k.to_string()),
            value: KeyValue::Str(v.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        let mut span = TagTk::new(
            "span",
            vec![],
            DataParsoid {
                src: Some("<span class=\"IPA nowrap\">".to_string()),
                ..Default::default()
            },
        );
        span.add_attribute_str("class", "IPA nowrap");
        let mut link = SelfclosingTagTk::new("wikilink", vec![], Default::default());
        link.attribs.push(kv("href", "Help:IPA/English"));
        link.attribs.push(kv("mw:maybeContent", "/x/"));
        let end = EndTagTk::new(
            "span",
            vec![],
            DataParsoid {
                src: Some("</span>".to_string()),
                ..Default::default()
            },
        );
        let items = vec![
            Item::Tok(ParsoidToken::Tag(span)),
            Item::Tok(ParsoidToken::SelfclosingTag(link)),
            Item::Tok(ParsoidToken::EndTag(end)),
        ];
        assert_eq!(
            tag_content_source(&items),
            Some("<span class=\"IPA nowrap\">[[Help:IPA/English|/x/]]</span>".to_string())
        );
    }

    /// A branch whose content is a tag followed by table lines (`<div>` then a
    /// `{|`/`!`/`|` block) must have each *text run* re-tokenized, not only an
    /// all-text branch. `Template:Largest cities` builds its table this way.
    #[test]
    fn re_tokenize_text_branch_parses_a_table_after_a_tag() {
        use crate::wikitext::tokens_v2::{EndTagTk, NlTk, TagTk};

        let s = |t: &str| Item::Str(t.to_string());
        let items = vec![
            Item::Tok(ParsoidToken::Tag(TagTk::new(
                "div",
                vec![],
                Default::default(),
            ))),
            Item::Tok(ParsoidToken::Nl(NlTk {
                data_parsoid: Default::default(),
            })),
            s("{|\n! H1\n|-\n| a\n|}\n"),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "div",
                vec![],
                Default::default(),
            ))),
        ];
        let out = re_tokenize_text_branch(items, &[], BranchSyntax::Cells, false, true, false);
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "table")),
            "the table start should be tokenized: {out:?}"
        );
        assert!(
            !out.iter()
                .any(|it| matches!(it, Item::Str(s) if s.contains("{|"))),
            "the literal table-start text should be gone: {out:?}"
        );
    }
}
