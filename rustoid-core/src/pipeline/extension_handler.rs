//! Extension-tag handling for built-in extensions (a faithful port of PHP
//! Parsoid's `Ext/Nowiki/Nowiki.php`, driven by the token-level `extension`
//! token produced by the tokenizer's `maybe_extension_tag` rule).
//!
//! The tokenizer collapses a matching extension tag pair (`<nowiki>…</nowiki>`,
//! `<pre>…</pre>`, …) into a single `SelfclosingTagTk('extension', …)` carrying
//! `typeof`, `name`, `source`, and `options`. This handler expands the built-in
//! `<nowiki>` extension into its DOM equivalent: a `<span typeof="mw:Nowiki">`
//! whose children are escaped plain text, with any decoded HTML entities wrapped
//! in `<span typeof="mw:Entity">` (mirroring the `htmlentity` tokenizer rule).

use crate::wikitext::tokenizer_v2::decode_wt_entities;
use crate::wikitext::tokens_v2::{
    DataParsoid, EndTagTk, Item, ParsoidToken, SelfclosingTagTk, TagTk,
};

/// Expand a single `extension` self-closing token into its token sequence.
///
/// Returns `None` if `token` is not an `extension` token (or an unhandled
/// extension), in which case the caller should leave it unchanged. Mirrors the
/// dispatch in PHP's `ExtensionHandler` (which delegates to the module
/// registered for the tag name).
fn expand_extension(
    token: &SelfclosingTagTk,
    config: &dyn crate::traits::SiteConfig,
    fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
    next_id: &mut usize,
) -> Option<Vec<Item>> {
    if token.name != "extension" {
        return None;
    }
    let name = attr_str(token, "name")?;
    match name {
        "nowiki" => Some(nowiki_items(token)),
        "pre" => Some(pre_items(token, config)),
        "i18ntag" | "i18nattr" => Some(i18n_items(token)),
        "pwraptest" => Some(pwraptest_fragment_items(token, fragments, next_id)),
        // Other built-in extension tags (gallery, …) are not yet handled.
        _ => None,
    }
}

/// Read a string-valued attribute from a self-closing tag.
fn attr_str<'t>(token: &'t SelfclosingTagTk, name: &str) -> Option<&'t str> {
    token
        .attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some(name))
        .and_then(|kv| kv.value.as_str())
}

/// Build the token sequence for a `<nowiki>` extension. Faithful port of
/// `Nowiki::sourceToDom` (without the DOM indirection): the raw body text is
/// split on entity references, with decodable entities wrapped in
/// `<span typeof="mw:Entity">`, all inside a `<span typeof="mw:Nowiki">`.
fn nowiki_items(token: &SelfclosingTagTk) -> Vec<Item> {
    let source = attr_str(token, "source").unwrap_or_default().to_string();
    let body = extract_ext_body(token, &source);

    let parts = split_entities(&body);

    let mut out: Vec<Item> = Vec::new();

    // `<span typeof="mw:Nowiki">` (always emitted, as in PHP `Nowiki::sourceToDom`).
    let mut span_dp = token.data_parsoid.clone();
    span_dp.src = None;
    span_dp.src_content = None;
    let mut span = TagTk::new("span", vec![], span_dp);
    span.add_attribute_str("typeof", "mw:Nowiki");
    out.push(Item::Tok(ParsoidToken::Tag(span)));

    // Emit the body parts: raw text for even indices, decoded entity spans
    // for decodable odd indices, and raw text for undecodable odd indices.
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 1 {
            let decoded = decode_wt_entities(part);
            if decoded != *part {
                // `<span typeof="mw:Entity">` + decoded text + `</span>`.
                let ep = DataParsoid {
                    src: Some((*part).to_string()),
                    src_content: Some(decoded.clone()),
                    ..DataParsoid::default()
                };
                let mut entity_span = TagTk::new("span", vec![], ep);
                entity_span.add_attribute_str("typeof", "mw:Entity");
                out.push(Item::Tok(ParsoidToken::Tag(entity_span)));
                out.push(Item::Str(decoded));
                out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                    "span",
                    vec![],
                    DataParsoid::default(),
                ))));
                continue;
            }
            // Fall through: an undecodable entity stays as plain raw text.
        }
        if !part.is_empty() {
            out.push(Item::Str((*part).to_string()));
        }
    }

    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        "span",
        vec![],
        DataParsoid::default(),
    ))));
    out
}

/// Build the token sequence for an `<i18ntag>` or `<i18nattr>` extension.
/// Faithful port of `ParserTests\I18nTag::sourceToDom` (and the generic
/// `ExtensionHandler` encapsulation that wraps its fragment).
///
/// - `<i18ntag>` → `<span typeof="mw:I18n mw:Extension/i18ntag" …>` (empty),
///   with `data-mw-i18n` holding the span info in the page-content language.
/// - `<i18nattr>` → `<span typeof="mw:LocalizedAttrs mw:Extension/i18nattr">`
///   wrapping the body text, with `data-mw-i18n` holding the localized
///   attribute info in the interface language.
fn i18n_items(token: &SelfclosingTagTk) -> Vec<Item> {
    let source = attr_str(token, "source").unwrap_or_default().to_string();
    let body = extract_ext_body(token, &source);
    let tag_name = attr_str(token, "name").unwrap_or("i18ntag").to_string();

    // Recover the parsed start-tag attributes (for `<i18nattr>`, the `message`
    // attribute carries the localization key).
    let attrs = extension_kv_attrs(token);

    let mut dp = token.data_parsoid.clone();
    dp.src = None;
    dp.src_content = None;
    dp.ext_tag_offsets = None;

    let mut span = TagTk::new("span", vec![], dp);

    if tag_name == "i18ntag" {
        // `<span typeof="mw:I18n mw:Extension/i18ntag">` with span info in the
        // page-content language (mirrors `createPageContentI18nFragment`).
        span.add_attribute_str("typeof", "mw:I18n mw:Extension/i18ntag");
        let key = body.trim();
        span.add_attribute_str(
            "data-mw-i18n",
            format!("{{\"/\":{{\"lang\":\"x-page\",\"key\":\"{key}\"}}}}"),
        );
        // Empty span: no children.
        vec![
            Item::Tok(ParsoidToken::Tag(span)),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "span",
                vec![],
                DataParsoid::default(),
            ))),
        ]
    } else {
        // `<span typeof="mw:LocalizedAttrs mw:Extension/i18nattr">…body…</span>`
        // with the `message` attribute localized in the interface language
        // (mirrors `addInterfaceI18nAttribute`).
        span.add_attribute_str("typeof", "mw:LocalizedAttrs mw:Extension/i18nattr");
        let key = attrs
            .iter()
            .find(|kv| kv.key.as_str() == Some("message"))
            .and_then(|kv| kv.value.as_str())
            .unwrap_or_default();
        span.add_attribute_str(
            "data-mw-i18n",
            format!("{{\"message\":{{\"lang\":\"x-user\",\"key\":\"{key}\"}}}}"),
        );
        vec![
            Item::Tok(ParsoidToken::Tag(span)),
            Item::Str(body),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "span",
                vec![],
                DataParsoid::default(),
            ))),
        ]
    }
}

/// Build the `mw:dom-fragment-token` sequence for a `<pwraptest>` parser-test
/// extension. Faithful port of `ParserHook::sourceToDom`'s `pwraptest` case
/// combined with the generic extension encapsulation
/// (`ExtensionHandler::onDocumentFragment`).
///
/// `pwraptest` always produces the DOM `<!--CMT--><style>p{}</style>` regardless
/// of its content (mirrors `$extApi->htmlToDom( '<!--CMT--><style>p{}</style>' )`).
/// `PipelineUtils::addSpanWrappers` then wraps the comment in the encapsulation
/// `<span typeof="mw:Extension/pwraptest">`, leaving the `<style>` metadata element
/// as a sibling. The whole fragment is tunneled through a `mw:dom-fragment-token`
/// placeholder so its content bypasses token-level p-wrapping.
fn pwraptest_fragment_items(
    token: &SelfclosingTagTk,
    fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
    next_id: &mut usize,
) -> Vec<Item> {
    use crate::dom::node::{ElementKind, Node};
    use crate::wikitext::tokens_v2::KeyValue;

    // Build the encapsulation span: `<span typeof="mw:Extension/pwraptest">`
    // wrapping the comment, with the `<style>` element as a sibling.
    let mut span = Node::element(ElementKind::Span);
    span.set_attr("typeof", "mw:Extension/pwraptest");
    span.push_child(Node::comment("CMT"));

    let mut style = Node::element(ElementKind::Other("style".to_string()));
    style.push_child(Node::text("p{}"));

    let mut frag = Node::document();
    frag.push_child(span);
    frag.push_child(style);

    let id = *next_id;
    *next_id += 1;
    fragments.insert(id, frag);

    // Emit an `mw:dom-fragment-token` placeholder carrying the fragment id.
    let mut dp = token.data_parsoid.clone();
    dp.src = None;
    dp.src_content = None;
    dp.ext_tag_offsets = None;
    let mut frag_tok = SelfclosingTagTk::new("mw:dom-fragment-token", vec![], dp);
    frag_tok.attribs.push(crate::wikitext::tokens_v2::KV {
        key: KeyValue::Str("data-fragment-id".to_string()),
        value: KeyValue::Str(id.to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });

    vec![Item::Tok(ParsoidToken::SelfclosingTag(frag_tok))]
}

/// Extract an extension tag's body source (the text between the opening and
/// closing tags). Mirrors PHP's `Utils::extractExtBody` / `stripTags`: strip
/// `openWidth` leading and `closeWidth` trailing bytes from `source`.
pub fn extract_ext_body(token: &SelfclosingTagTk, source: &str) -> String {
    let Some(offsets) = &token.data_parsoid.ext_tag_offsets else {
        return String::new();
    };
    let start = offsets.open_width.unwrap_or(0).min(source.len());
    let end = source
        .len()
        .saturating_sub(offsets.close_width.unwrap_or(0));
    if end <= start {
        return String::new();
    }
    source[start..end].to_string()
}

/// Build the token sequence for a `<pre>` extension. Faithful port of
/// `Ext/Pre/Pre.php::sourceToDom`: a `<pre>` element carrying the sanitized
/// start-tag attributes, whose single text node is the raw content with:
///   * `<nowiki>…</nowiki>` wrappers stripped,
///   * a single leading newline stripped (legacy parser parity), and
///   * wikitext entities decoded without `mw:Entity` spans.
///
/// Unlike `<nowiki>` (which is special-cased to lean `mw:Nowiki` markup), the
/// `<pre>` extension gets the generic extension encapsulation: a
/// `typeof="mw:Extension/pre"` type and a `data-mw` blob carrying the tag
/// name, sanitized attributes, and raw body source (mirrors
/// `ExtensionHandler::onDocumentFragment`).
fn pre_items(token: &SelfclosingTagTk, _config: &dyn crate::traits::SiteConfig) -> Vec<Item> {
    let source = attr_str(token, "source").unwrap_or_default().to_string();
    let mut body = extract_ext_body(token, &source);

    // Recover the parsed start-tag attributes (including the `format` option).
    let attrs: Vec<crate::wikitext::tokens_v2::KV> = extension_kv_attrs(token);
    let sanitized = crate::sanitizer::sanitize_tag_attrs("pre", attrs, |_proto| true);

    // `dataParsoid.stx = 'html'` (the `<pre>` element came from literal HTML).
    let mut dp = token.data_parsoid.clone();
    dp.src = None;
    dp.src_content = None;
    dp.ext_tag_offsets = None;
    dp.stx = Some("html".to_string());

    let mut pre = TagTk::new("pre", sanitized, dp);
    pre.data_mw = None;
    pre.add_attribute_str("typeof", "mw:Extension/pre");

    let open = Item::Tok(ParsoidToken::Tag(pre));
    let close = Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        "pre",
        vec![],
        DataParsoid::default(),
    )));

    // Strip `<nowiki>…</nowiki>` wrappers (mirrors the `preg_replace` in
    // `Pre::sourceToDom`).
    body = strip_nowiki_wrappers(&body);

    // Strip a single leading newline (legacy PHP parser parity).
    if let Some(stripped) = body.strip_prefix('\n') {
        body = stripped.to_string();
    }

    // Decode wikitext entities (no `mw:Entity` spans for `<pre>`).
    let decoded = decode_wt_entities_all(&body);

    vec![open, Item::Str(decoded), close]
}

/// Recover the parsed start-tag attributes from an extension token's `data-mw`
/// rich attribs (set by the tokenizer's `extension_data_mw`).
pub fn extension_kv_attrs(token: &SelfclosingTagTk) -> Vec<crate::wikitext::tokens_v2::KV> {
    let Some(data_mw) = &token.data_mw else {
        return Vec::new();
    };
    data_mw
        .attribs
        .iter()
        .map(|a| crate::wikitext::tokens_v2::KV {
            key: crate::wikitext::tokens_v2::KeyValue::Str(
                a.key.as_str().unwrap_or_default().to_string(),
            ),
            value: crate::wikitext::tokens_v2::KeyValue::Str(
                a.value.as_str().unwrap_or_default().to_string(),
            ),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        })
        .collect()
}

/// Strip `<nowiki>…</nowiki>` wrappers from `<pre>` content (mirrors the
/// `preg_replace('/<nowiki\s*>(.*?)<\/nowiki\s*>/s', '$1', …)` in `Pre`).
/// This is a non-greedy, case-sensitive global replacement: each `<nowiki>` is
/// paired with the nearest `</nowiki>`, both discarded, and the intervening
/// content kept.
fn strip_nowiki_wrappers(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(open) = find_tag(rest, "<nowiki") {
        out.push_str(&rest[..open]);
        // Content after the `<nowiki` prefix.
        let after_open = &rest[open + "<nowiki".len()..];
        // Skip optional whitespace then the required `>` to close the tag.
        let trimmed = after_open.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let Some(body_start_rest) = trimmed.strip_prefix('>') else {
            // Not a well-formed `<nowiki>` tag: leave it literal and stop.
            out.push_str("<nowiki");
            rest = after_open;
            break;
        };
        // Find the nearest closing `</nowiki>`.
        match find_tag(body_start_rest, "</nowiki") {
            Some(close) => {
                let inner = &body_start_rest[..close];
                out.push_str(inner);
                let after_close = &body_start_rest[close + "</nowiki".len()..];
                let after_close = after_close.trim_start_matches(|c: char| c.is_ascii_whitespace());
                rest = after_close.strip_prefix('>').unwrap_or(after_close);
            }
            None => {
                // No closing tag: emit the original `<nowiki>` literally and stop.
                out.push_str("<nowiki");
                out.push_str(after_open);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Find the next occurrence of the literal `tag` (a `<tag`/`</tag` prefix), as
/// a byte offset into `s`.
fn find_tag(s: &str, tag: &str) -> Option<usize> {
    s.find(tag)
}

/// Decode all wikitext entities in a string in one pass (mirrors
/// `Utils::decodeWtEntities`, which is `decodeCharReferences` over the whole
/// string). Unlike the tokenizer's per-entity `htmlentity` rule, this produces
/// plain decoded text with no `mw:Entity` wrappers.
fn decode_wt_entities_all(text: &str) -> String {
    let parts = split_entities(text);
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 1 {
            out.push_str(&decode_wt_entities(part));
        } else {
            out.push_str(part);
        }
    }
    out
}

/// Split a string on wikitext entity references (`&[#0-9a-zA-Z]+;`), retaining
/// the delimiters (mirrors `preg_split(/(&[#0-9a-zA-Z]+;)/, …,
/// PREG_SPLIT_DELIM_CAPTURE)`).
fn split_entities(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';') else {
            break;
        };
        let body = &after[..semi];
        // The entity body must be non-empty and match `[#0-9a-zA-Z]+`.
        if !body.is_empty() && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '#') {
            parts.push(&rest[..amp]);
            parts.push(&rest[amp..amp + 2 + semi]);
            rest = &rest[amp + 2 + semi..];
        } else {
            parts.push(&rest[..=amp]);
            rest = &rest[amp + 1..];
        }
    }
    parts.push(rest);
    parts
}

/// Expand extension tokens in a token stream. Runs before tree building,
/// replacing `extension` self-closing tokens with their DOM token sequences.
/// Faithful to the TT3 extension-handler stage in PHP Parsoid.
///
/// Generic extensions whose content must bypass token-level p-wrapping (i.e.
/// whose PHP `sourceToDom` returns an HTML DOM fragment — `pwraptest`, `style`,
/// `divtag`, …) are instead built as pre-built sub-`Node` fragments, registered
/// in `fragments`, and referenced via an `mw:dom-fragment-token` placeholder
/// (mirrors `PipelineUtils::tunnelDOMThroughTokens`).
pub fn run(
    tokens: Vec<Item>,
    config: &dyn crate::traits::SiteConfig,
    fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
    next_id: &mut usize,
) -> Vec<Item> {
    let mut out = Vec::with_capacity(tokens.len());
    for item in tokens {
        match &item {
            Item::Tok(ParsoidToken::SelfclosingTag(t)) => {
                if let Some(expanded) = expand_extension(t, config, fragments, next_id) {
                    out.extend(expanded);
                } else {
                    out.push(item);
                }
            }
            _ => out.push(item),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::{DataMw, KV, KeyValue};

    #[test]
    fn test_split_entities() {
        assert_eq!(split_entities("a&rarr;b"), vec!["a", "&rarr;", "b"]);
        assert_eq!(split_entities("no entities"), vec!["no entities"]);
        assert_eq!(split_entities("&amp;"), vec!["", "&amp;", ""]);
    }

    #[test]
    fn test_extract_ext_body() {
        let dp = DataParsoid {
            ext_tag_offsets: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(0),
                end: Some(22),
                open_width: Some(8),
                close_width: Some(9),
            }),
            ..DataParsoid::default()
        };
        let mut tok = SelfclosingTagTk::new("extension", vec![], dp);
        tok.add_attribute_str("name", "nowiki");
        tok.add_attribute_str("source", "<nowiki>hello</nowiki>");

        let source = "<nowiki>hello</nowiki>";
        assert_eq!(extract_ext_body(&tok, source), "hello");
    }

    #[test]
    fn test_strip_nowiki_wrappers() {
        // Simple wrapper is stripped.
        assert_eq!(strip_nowiki_wrappers("<nowiki>a</nowiki>"), "a");
        // No closing tag: left literal.
        assert_eq!(strip_nowiki_wrappers("<nowiki>"), "<nowiki>");
        // Nested nowikis (T15238): `<nowiki><nowiki></nowiki>Foo<nowiki></nowiki></nowiki>`
        // collapses to `<nowiki>Foo</nowiki>` under non-greedy matching.
        assert_eq!(
            strip_nowiki_wrappers("<nowiki><nowiki></nowiki>Foo<nowiki></nowiki></nowiki>"),
            "<nowiki>Foo</nowiki>"
        );
    }

    // Build an `extension` token for a `<nowiki>` body (for `nowiki_items` tests).
    fn nowiki_token(body: &str) -> SelfclosingTagTk {
        let full = format!("<nowiki>{body}</nowiki>");
        let dp = DataParsoid {
            ext_tag_offsets: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(0),
                end: Some(full.len()),
                open_width: Some("<nowiki>".len()),
                close_width: Some("</nowiki>".len()),
            }),
            ..DataParsoid::default()
        };
        let mut tok = SelfclosingTagTk::new("extension", vec![], dp);
        tok.add_attribute_str("name", "nowiki");
        tok.add_attribute_str("source", &full);
        tok
    }

    #[test]
    fn test_nowiki_items_plain_text_wraps() {
        // Plain text (no decodable entities) still gets a `mw:Nowiki` wrapper
        // (matching PHP `Nowiki::sourceToDom`).
        let items = nowiki_items(&nowiki_token("</pre>"));
        let has_nowiki = items.iter().any(|it| {
            matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "span"
                && t.attribs.iter().any(|kv| kv.key.as_str() == Some("typeof") && kv.value.as_str() == Some("mw:Nowiki")))
        });
        assert!(has_nowiki, "expected mw:Nowiki span in {items:?}");
        assert!(
            items
                .iter()
                .any(|it| matches!(it, Item::Str(s) if s == "</pre>"))
        );
    }

    #[test]
    fn test_nowiki_items_entity_keeps_wrapper() {
        // A decodable entity keeps the `mw:Nowiki` wrapper and adds an
        // `mw:Entity` span for the decoded content.
        let items = nowiki_items(&nowiki_token("&rarr;"));
        let has_nowiki = items.iter().any(|it| {
            matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "span"
                && t.attribs.iter().any(|kv| kv.key.as_str() == Some("typeof") && kv.value.as_str() == Some("mw:Nowiki")))
        });
        assert!(has_nowiki, "expected mw:Nowiki span in {items:?}");
        let has_entity = items.iter().any(|it| {
            matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "span"
                && t.attribs.iter().any(|kv| kv.key.as_str() == Some("typeof") && kv.value.as_str() == Some("mw:Entity")))
        });
        assert!(has_entity, "expected mw:Entity span in {items:?}");
        assert!(
            items
                .iter()
                .any(|it| matches!(it, Item::Str(s) if s == "\u{2192}")),
            "expected decoded arrow in {items:?}"
        );
    }

    // Build an `extension` token for an i18n tag body (with optional parsed
    // start-tag attributes for `<i18nattr>`).
    fn i18n_token(name: &str, body: &str, attrs: Vec<KV>) -> SelfclosingTagTk {
        let full = format!("<{name}>{body}</{name}>");
        let dp = DataParsoid {
            ext_tag_offsets: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(0),
                end: Some(full.len()),
                open_width: Some(format!("<{name}>").len()),
                close_width: Some(format!("</{name}>").len()),
            }),
            ..DataParsoid::default()
        };
        let mut tok = SelfclosingTagTk::new("extension", vec![], dp);
        tok.add_attribute_str("name", name);
        tok.add_attribute_str("source", &full);
        // Store the parsed attrs as rich `data-mw` attribs (mirrors the
        // tokenizer's `extension_data_mw`).
        tok.data_mw = Some(DataMw {
            parts: vec![],
            src: None,
            attribs: attrs
                .iter()
                .map(|kv| {
                    crate::wikitext::tokens_v2::DataMwAttrib::new(
                        crate::wikitext::tokens_v2::DataMwValue::Str(
                            kv.key.as_str().unwrap_or_default().to_string(),
                        ),
                        crate::wikitext::tokens_v2::DataMwValue::Str(
                            kv.value.as_str().unwrap_or_default().to_string(),
                        ),
                    )
                })
                .collect(),
        });
        tok
    }

    fn kv(key: &str, value: &str) -> KV {
        KV {
            key: KeyValue::Str(key.to_string()),
            value: KeyValue::Str(value.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        }
    }

    #[test]
    fn test_i18n_items_i18ntag() {
        let items = i18n_items(&i18n_token("i18ntag", "message.key", vec![]));
        let tag = items
            .iter()
            .find_map(|it| match it {
                Item::Tok(ParsoidToken::Tag(t)) if t.name == "span" => Some(t),
                _ => None,
            })
            .expect("span tag");
        assert_eq!(
            tag.attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str()),
            Some("mw:I18n mw:Extension/i18ntag")
        );
        assert_eq!(
            tag.attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("data-mw-i18n"))
                .and_then(|kv| kv.value.as_str()),
            Some("{\"/\":{\"lang\":\"x-page\",\"key\":\"message.key\"}}")
        );
    }

    #[test]
    fn test_i18n_items_i18nattr() {
        let items = i18n_items(&i18n_token(
            "i18nattr",
            "some text",
            vec![kv("message", "message.key")],
        ));
        let tag = items
            .iter()
            .find_map(|it| match it {
                Item::Tok(ParsoidToken::Tag(t)) if t.name == "span" => Some(t),
                _ => None,
            })
            .expect("span tag");
        assert_eq!(
            tag.attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str()),
            Some("mw:LocalizedAttrs mw:Extension/i18nattr")
        );
        assert_eq!(
            tag.attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("data-mw-i18n"))
                .and_then(|kv| kv.value.as_str()),
            Some("{\"message\":{\"lang\":\"x-user\",\"key\":\"message.key\"}}")
        );
        assert!(
            items
                .iter()
                .any(|it| matches!(it, Item::Str(s) if s == "some text"))
        );
    }

    #[test]
    fn test_pwraptest_items() {
        // `<pwraptest />` always produces `<!--CMT--><style>p{}</style>`, wrapped
        // by `addSpanWrappers` into `<span typeof="mw:Extension/pwraptest">
        // <!--CMT--></span><style>p{}</style>`, and tunneled through a
        // `mw:dom-fragment-token` placeholder (mirrors `ParserHook::sourceToDom`).
        let mut tok = SelfclosingTagTk::new("extension", vec![], DataParsoid::default());
        tok.add_attribute_str("name", "pwraptest");
        let mut fragments = std::collections::HashMap::new();
        let mut next_id = 0usize;
        let items = pwraptest_fragment_items(&tok, &mut fragments, &mut next_id);

        // The output is a single `mw:dom-fragment-token` placeholder.
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "mw:dom-fragment-token"),
            "expected fragment token: {items:?}"
        );
        // The fragment id resolves to the pre-built fragment.
        let id = match &items[0] {
            Item::Tok(ParsoidToken::SelfclosingTag(t)) => t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("data-fragment-id"))
                .and_then(|kv| kv.value.as_str())
                .and_then(|s| s.parse::<usize>().ok())
                .expect("data-fragment-id"),
            _ => panic!("expected fragment token"),
        };
        let frag = fragments.get(&id).expect("fragment");
        // The fragment has a span (typeof mw:Extension/pwraptest) wrapping a
        // comment, plus a style sibling with text `p{}`.
        assert_eq!(frag.children.len(), 2);
        let span = &frag.children[0];
        assert_eq!(span.get_attr("typeof"), Some("mw:Extension/pwraptest"));
        assert!(
            span.children
                .iter()
                .any(|c| matches!(&c.kind, crate::dom::node::NodeKind::Comment(c) if c == "CMT"))
        );
        let style = &frag.children[1];
        assert_eq!(
            style.get_attr("typeof"),
            None,
            "style should have no typeof"
        );
        assert!(
            style
                .children
                .iter()
                .any(|c| matches!(&c.kind, crate::dom::node::NodeKind::Text(t) if t == "p{}"))
        );
        assert_eq!(next_id, 1);
    }
}
