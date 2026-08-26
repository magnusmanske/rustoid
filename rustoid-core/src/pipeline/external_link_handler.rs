//! ExternalLinkHandler — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/ExternalLinkHandler.php`.
//!
//! Converts `urllink` (auto-linked URLs) and `extlink` (bracketed `[url ...]`)
//! self-closing tokens into `<a rel="mw:ExtLink">` tag sequences.
//!
//! This module covers the non-templated common case; template wrapping
//! (`getTemplateInfo`/`wrapReturn`) is layered on once the
//! `DataMw`/`TemplateInfo`/DOM-fragment infrastructure is available.

use crate::pipeline::wiki_link_handler::{build_link_attrs, string_kv};
use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, Item, ParsoidToken, TagTk};

/// Is this file extension an image extension? Mirrors
/// `ExternalLinkHandler::imageExtensions`.
fn image_extensions(ext: &str) -> bool {
    matches!(
        ext,
        "avif" | "gif" | "jpeg" | "jpg" | "png" | "svg" | "webp"
    )
}

/// Check whether an href refers to an external image (allowed by config).
/// Mirrors `ExternalLinkHandler::hasImageLink`.
pub fn has_image_link(href: &str, allowed_prefixes: &[String]) -> bool {
    let lower = href.to_lowercase();
    let is_http = lower.starts_with("http://") || lower.starts_with("https://");

    // Split on '.', check the final part is an image extension.
    let bits: Vec<&str> = href.split('.').collect();
    let has_image_extension = bits.len() > 1
        && bits
            .last()
            .map(|e| image_extensions(e.to_ascii_lowercase().as_str()))
            .unwrap_or(false);

    if !has_image_extension || !is_http {
        return false;
    }

    // true if some allowed prefix matches ("" means allow all).
    allowed_prefixes
        .iter()
        .any(|p| p.is_empty() || href.starts_with(p))
}

/// Handler for a `urllink` token (an auto-linked bare URL). Mirrors
/// `ExternalLinkHandler::onUrlLink`.
pub fn on_url_link(
    token: &ParsoidToken,
    content_href: &str,
    clean: impl Fn(&str) -> Option<String>,
) -> Option<Vec<Item>> {
    let data_parsoid = token.data_parsoid().cloned().unwrap_or_default();

    if has_image_link_generic(content_href) {
        // Render as an external image.
        let alt = content_href.rsplit('/').next().unwrap_or(content_href);
        let tag_attrs = [
            string_kv("src", content_href),
            string_kv("alt", alt),
            string_kv("rel", "mw:externalImage"),
        ];
        let result = build_link_attrs(token.get_attribs(), false, None, Some(&tag_attrs));
        let mut img =
            crate::wikitext::tokens_v2::SelfclosingTagTk::new("img", result.attribs, data_parsoid);
        img.data_parsoid.stx = Some("url".to_string());
        return Some(vec![Item::Tok(ParsoidToken::SelfclosingTag(img))]);
    }

    // Render as a plain external link.
    let link_attrs = [string_kv("rel", "mw:ExtLink")];
    let result = build_link_attrs(token.get_attribs(), false, None, Some(&link_attrs));

    let href = clean(content_href)?;

    let mut dp = data_parsoid;
    dp.stx = Some("url".to_string());

    let mut a_tag = TagTk::new("a", result.attribs, dp);
    a_tag.add_attribute_str("href", &href);
    // Auto-link text is the (cleaned) href.
    let text = href.clone();

    Some(vec![
        Item::Tok(ParsoidToken::Tag(a_tag)),
        Item::Str(text),
        Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "a",
            vec![],
            DataParsoid::default(),
        ))),
    ])
}

/// Helper for image detection with a permissive (allow-all) prefix set.
fn has_image_link_generic(href: &str) -> bool {
    has_image_link(href, &[])
}

/// Handler for an `extlink` token (a bracketed `[url ...]`). Mirrors
/// `ExternalLinkHandler::onExtLink` for the non-templated case.
pub fn on_ext_link(
    token: &ParsoidToken,
    clean: impl Fn(&str) -> Option<String>,
    relative_link_prefix: &str,
) -> Option<Vec<Item>> {
    let orig_href = token.get_attribute_v("href")?.to_string();
    // `mw:content` may be a plain string, or (after the AttributeExpander has
    // expanded any embedded templates) a token array carrying `mw:Transclusion`
    // markers / `template` tokens. Render it faithfully rather than collapsing
    // it back to a string.
    let content_items: Vec<Item> = {
        let kv = token.get_attribute_kv("mw:content");
        match kv.map(|kv| &kv.value) {
            Some(crate::wikitext::tokens_v2::KeyValue::Str(s)) => vec![Item::Str(s.clone())],
            Some(crate::wikitext::tokens_v2::KeyValue::Tokens(t)) => t.clone(),
            None => vec![],
        }
    };

    let data_parsoid = token.data_parsoid().cloned().unwrap_or_default();

    // Magic links (`RFC`/`PMID`/`ISBN`) are `extlink` tokens carrying a
    // `typeof` attribute (`mw:ExtLink/RFC`, `mw:WikiLink/ISBN`, ...). They
    // render as a plain `<a>` whose `rel`/`href` are derived from the type.
    let magic_link_type = token
        .get_attribute_v("typeof")
        .and_then(parse_magic_link_type);

    if let Some(magic_link_type) = magic_link_type {
        let is_isbn = magic_link_type == "ISBN";
        let new_href = if is_isbn {
            format!("{relative_link_prefix}{orig_href}")
        } else {
            orig_href.clone()
        };
        let new_rel = if is_isbn { "mw:WikiLink" } else { "mw:ExtLink" };

        // Remove the magic `typeof` from the token's attributes, then build a
        // fresh href/rel pair (mirrors PHP's `removeAttribute('typeof')` then
        // `buildLinkAttrs`).
        let mut remaining = token.get_attribs().to_vec();
        remaining.retain(|kv| kv.key.as_str() != Some("typeof"));
        let link_attrs = [string_kv("href", &new_href), string_kv("rel", new_rel)];
        let result = build_link_attrs(&remaining, false, None, Some(&link_attrs));

        let a_tag = TagTk::new("a", result.attribs, data_parsoid);

        let mut out = vec![Item::Tok(ParsoidToken::Tag(a_tag))];
        out.extend(content_items);
        out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "a",
            vec![],
            DataParsoid::default(),
        ))));
        return Some(out);
    }

    // href is a plain string from our tokenizer, so no template wrapping.
    let href = clean(&orig_href)?;

    // If the content is a single plain-text URL (and an image), render `<img>`.
    let rendered_content: Vec<Item> = if let [Item::Str(s)] = content_items.as_slice()
        && has_image_link(s, &[])
    {
        let alt = s.rsplit('/').next().unwrap_or(s);
        let mut img = crate::wikitext::tokens_v2::SelfclosingTagTk::new(
            "img",
            vec![string_kv("src", s), string_kv("alt", alt)],
            DataParsoid::default(),
        );
        img.data_parsoid.stx = Some("url".to_string());
        vec![Item::Tok(ParsoidToken::SelfclosingTag(img))]
    } else {
        content_items
    };

    let link_attrs = [string_kv("rel", "mw:ExtLink")];
    let result = build_link_attrs(token.get_attribs(), false, None, Some(&link_attrs));

    // A bracketed `[url text]` extlink keeps its original `data-parsoid` (it is
    // *not* `stx: 'url'`, which is reserved for auto-linked bare URLs in
    // `onUrlLink`). Mirrors PHP `ExternalLinkHandler::onExtLink`.
    let mut a_tag = TagTk::new("a", result.attribs, data_parsoid);
    a_tag.add_attribute_str("href", &href);
    // Record source/normalized href shadow attrs for round-tripping. The `<a>`
    // *width* for `ComputeDSR` is derived from `tmp.extLinkContentOffsets` (set
    // by the tokenizer), not from these.
    a_tag.data_parsoid.set_sa("href", &orig_href);
    a_tag.data_parsoid.set_a("href", &href);

    let mut out = vec![Item::Tok(ParsoidToken::Tag(a_tag))];
    out.extend(rendered_content);
    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        "a",
        vec![],
        DataParsoid::default(),
    ))));
    Some(out)
}

/// Parse a magic-link `typeof` value into the link type (`RFC`, `PMID`, or
/// `ISBN`), or `None` if it is not a magic-link type. Mirrors PHP's
/// `TokenUtils::matchTypeOf($token, '#^mw:(Ext|Wiki)Link/(ISBN|RFC|PMID)$#')`.
fn parse_magic_link_type(typeof_val: &str) -> Option<&'static str> {
    for ty in typeof_val.split_whitespace() {
        match ty {
            "mw:ExtLink/RFC" => return Some("RFC"),
            "mw:ExtLink/PMID" => return Some("PMID"),
            "mw:ExtLink/ISBN" | "mw:WikiLink/ISBN" => return Some("ISBN"),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::{KeyValue, SelfclosingTagTk};

    fn id_clean(href: &str) -> Option<String> {
        // A permissive clean (no protocol filtering).
        Some(href.to_string())
    }

    fn extlink_token(href: &str, content: &str) -> ParsoidToken {
        let mut tk = SelfclosingTagTk::new("extlink", vec![], DataParsoid::default());
        tk.attribs.push(crate::wikitext::tokens_v2::KV {
            key: KeyValue::Str("href".to_string()),
            value: KeyValue::Str(href.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
        tk.attribs.push(crate::wikitext::tokens_v2::KV {
            key: KeyValue::Str("mw:content".to_string()),
            value: KeyValue::Str(content.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
        ParsoidToken::SelfclosingTag(tk)
    }

    #[test]
    fn test_extlink_renders_a_tag() {
        let token = extlink_token("http://example.com", "link");
        let out = on_ext_link(&token, id_clean, "./").unwrap();

        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "a"));
        assert!(matches!(out.last(), Some(Item::Tok(ParsoidToken::EndTag(t))) if t.name == "a"));

        if let Item::Tok(ParsoidToken::Tag(t)) = &out[0] {
            assert_eq!(
                t.attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))
                    .and_then(|kv| kv.value.as_str()),
                Some("http://example.com")
            );
        }
        // Content "link" should be present.
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "link"))
        );
    }

    #[test]
    fn test_has_image_link() {
        // A single empty-string prefix means "allow all external images" (PHP
        // represents $wgAllowExternalImagesFrom with ['']).
        assert!(has_image_link(
            "http://example.com/foo.png",
            &["".to_string()]
        ));
        assert!(!has_image_link(
            "http://example.com/foo.txt",
            &["".to_string()]
        ));
        // No allowed prefixes means no external images.
        assert!(!has_image_link("http://example.com/foo.png", &[]));
    }
}
