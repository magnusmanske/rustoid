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
fn expand_extension(token: &SelfclosingTagTk) -> Option<Vec<Item>> {
    if token.name != "extension" {
        return None;
    }
    let name = attr_str(token, "name")?;
    match name {
        "nowiki" => Some(nowiki_items(token)),
        "pre" => Some(pre_items(token)),
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
/// escaped and split on entity references, with decodable entities wrapped in
/// `<span typeof="mw:Entity">`.
fn nowiki_items(token: &SelfclosingTagTk) -> Vec<Item> {
    let source = attr_str(token, "source").unwrap_or_default().to_string();
    let body = extract_ext_body(token, &source);

    let mut out: Vec<Item> = Vec::new();

    // `<span typeof="mw:Nowiki">`
    let mut span_dp = token.data_parsoid.clone();
    span_dp.src = None;
    span_dp.src_content = None;
    let mut span = TagTk::new("span", vec![], span_dp);
    span.add_attribute_str("typeof", "mw:Nowiki");
    out.push(Item::Tok(ParsoidToken::Tag(span)));

    // Split on entity references, mirroring
    // `preg_split(/(&[#0-9a-zA-Z]+;)/, …, PREG_SPLIT_DELIM_CAPTURE)`, then emit
    // raw text nodes (the serializer HTML-escapes them).
    let parts = split_entities(&body);
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 1 {
            // An entity reference (odd index): decode it; wrap valid entities.
            let decoded = decode_wt_entities(part);
            if decoded != *part {
                // `<span typeof="mw:Entity">` + decoded text + `</span>`,
                // carrying the raw source `src` and decoded `srcContent`.
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

/// Extract an extension tag's body source (the text between the opening and
/// closing tags). Mirrors PHP's `Utils::extractExtBody` / `stripTags`: strip
/// `openWidth` leading and `closeWidth` trailing bytes from `source`.
fn extract_ext_body(token: &SelfclosingTagTk, source: &str) -> String {
    let Some(offsets) = &token.data_parsoid.ext_tag_offsets else {
        return String::new();
    };
    let start = offsets.open_width.min(source.len());
    let end = source.len().saturating_sub(offsets.close_width);
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
fn pre_items(token: &SelfclosingTagTk) -> Vec<Item> {
    let source = attr_str(token, "source").unwrap_or_default().to_string();
    let mut body = extract_ext_body(token, &source);

    // Recover the parsed start-tag attributes (including the `format` option).
    let attrs: Vec<crate::wikitext::tokens_v2::KV> = extension_kv_attrs(token);
    let format = attrs
        .iter()
        .find(|kv| kv.key.as_str() == Some("format"))
        .and_then(|kv| kv.value.as_str())
        .unwrap_or("")
        .to_string();
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

    if format == "wikitext" {
        // `format="wikitext"`: parse the body as inline wikitext (mirrors
        // `Pre::sourceToDom`'s `extTagToDOM` branch with `context: 'inline'`),
        // so `'''bold'''` becomes `<b>bold</b>` etc.
        let items = wikitext_body_items(&body);
        let mut out = vec![open];
        out.extend(items);
        out.push(close);
        return out;
    }

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

/// Tokenize a `<pre format="wikitext">` body as inline wikitext and run the
/// quote transformer, so `''`/`'''` are converted to `<i>`/`<b>`. This is the
/// `context: 'inline'` sub-parse of PHP's `extTagToDOM` for the common
/// bold/italic case (wikilinks, lists, tables, and headings inside `pre` are
/// not yet wired).
fn wikitext_body_items(body: &str) -> Vec<Item> {
    use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};
    use crate::wikitext::tokens_v2::Either;

    let options = TokenizerOptions {
        inline_context: true,
        sol: false,
        ext_tags: vec!["nowiki".to_string(), "pre".to_string()],
        ..TokenizerOptions::default()
    };
    let mut tokenizer = PegTokenizer::new(body, &options);
    let Ok(chunks) = tokenizer.tokenize() else {
        return vec![Item::Str(body.to_string())];
    };
    let items: Vec<Item> = chunks
        .into_iter()
        .map(|e| match e {
            Either::Left(s) => Item::Str(s),
            Either::Right(t) => Item::Tok(t),
        })
        .collect();
    // The quote transformer flushes pending quotes only on a newline or EOF
    // token; the tokenizer does not emit a trailing one, so append a synthetic
    // EOF, transform, then drop it again.
    let mut with_eof = items;
    with_eof.push(Item::Tok(ParsoidToken::Eof(
        crate::wikitext::tokens_v2::EOFTk,
    )));
    let mut out = crate::pipeline::quote_transformer_v2::QuoteTransformer::transform(with_eof);
    if matches!(out.last(), Some(Item::Tok(ParsoidToken::Eof(_)))) {
        out.pop();
    }
    out
}

/// Recover the parsed start-tag attributes from an extension token's `data-mw`
/// rich attribs (set by the tokenizer's `extension_data_mw`).
fn extension_kv_attrs(token: &SelfclosingTagTk) -> Vec<crate::wikitext::tokens_v2::KV> {
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
pub fn run(tokens: Vec<Item>) -> Vec<Item> {
    let mut out = Vec::with_capacity(tokens.len());
    for item in tokens {
        match &item {
            Item::Tok(ParsoidToken::SelfclosingTag(t)) => {
                if let Some(expanded) = expand_extension(t) {
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
                start: 0,
                end: 22,
                open_width: 8,
                close_width: 9,
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
}
