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
        // Other built-in extension tags (pre, gallery, …) are not yet handled.
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
}
