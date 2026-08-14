//! WikiLinkHandler (static helpers) — faithful port of the self-contained
//! static methods from PHP Parsoid's `src/Wt2Html/TT/WikiLinkHandler.php`.
//!
//! These helpers (`hrefParts`, `buildLinkAttrs`) are pure functions used by
//! both the WikiLinkHandler itself and the ExternalLinkHandler to build link
//! attributes and extract link text.

use crate::wikitext::tokens_v2::{KV, KeyValue};

/// Split a wikilink href into (prefix, title) at the first colon.
/// Mirrors PHP's `WikiLinkHandler::hrefParts`.
pub fn href_parts(str_: &str) -> Option<(&str, &str)> {
    match str_.find(':') {
        Some(idx) => Some((&str_[..idx], &str_[idx + 1..])),
        None => None,
    }
}

/// The result of `build_link_attrs`.
pub struct LinkAttrs {
    pub attribs: Vec<KV>,
    pub content_kvs: Vec<KV>,
    pub has_rdfa_type: bool,
}

/// Build link attributes from a token's attribute list. Extracts `about` and
/// `typeof` (and optionally the link-text KVs), combines them with the given
/// `rdfa_type` and `link_attrs`, in that order.
///
/// Mirrors PHP's `WikiLinkHandler::buildLinkAttrs`.
pub fn build_link_attrs(
    attrs: &[KV],
    get_link_text: bool,
    rdfa_type: Option<&str>,
    link_attrs: Option<&[KV]>,
) -> LinkAttrs {
    let mut new_attrs: Vec<KV> = Vec::new();
    let mut link_text_kvs: Vec<KV> = Vec::new();
    let mut about: Option<String> = None;
    let mut rdfa_type = rdfa_type.map(|s| s.to_string());

    // Single pass: fetch about, typeof, and (optionally) link text.
    for kv in attrs {
        let k = kv.key.as_str();
        let v = kv.value.as_str();

        if get_link_text && k == Some("mw:maybeContent") {
            link_text_kvs.push(kv.clone());
        } else if let Some(k) = k
            && !k.is_empty()
        {
            if k.trim() == "typeof" {
                match (&rdfa_type, v) {
                    (Some(existing), Some(v)) => {
                        rdfa_type = Some(format!("{existing} {v}"));
                    }
                    (None, v) => {
                        rdfa_type = v.map(|s| s.to_string());
                    }
                    (Some(_), None) => {}
                }
            } else if k.trim() == "about" {
                about = v.map(|s| s.to_string());
            }
        }
    }

    if let Some(rt) = &rdfa_type {
        new_attrs.push(string_kv("typeof", rt));
    }

    if let Some(about) = &about {
        new_attrs.push(string_kv("about", about));
    }

    if let Some(link_attrs) = link_attrs {
        new_attrs.extend(link_attrs.iter().cloned());
    }

    LinkAttrs {
        attribs: new_attrs,
        content_kvs: link_text_kvs,
        has_rdfa_type: rdfa_type.is_some(),
    }
}

/// Create a simple string KV.
pub fn string_kv(key: &str, value: &str) -> KV {
    KV {
        key: KeyValue::Str(key.to_string()),
        value: KeyValue::Str(value.to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_href_parts() {
        assert_eq!(href_parts("en:Foo"), Some(("en", "Foo")));
        assert_eq!(href_parts("Foo"), None);
    }

    #[test]
    fn test_build_link_attrs_extracts_about_typeof() {
        let attrs = vec![
            KV {
                key: KeyValue::Str("about".to_string()),
                value: KeyValue::Str("#mwt1".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str("typeof".to_string()),
                value: KeyValue::Str("mw:Transclusion".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ];
        let link_attrs = vec![string_kv("rel", "mw:WikiLink")];
        let result = build_link_attrs(&attrs, false, None, Some(&link_attrs));

        // Order: typeof, about, then link attrs.
        assert_eq!(result.attribs.len(), 3);
        assert_eq!(result.attribs[0].key.as_str(), Some("typeof"));
        assert_eq!(result.attribs[0].value.as_str(), Some("mw:Transclusion"));
        assert_eq!(result.attribs[1].key.as_str(), Some("about"));
        assert_eq!(result.attribs[2].key.as_str(), Some("rel"));
    }

    #[test]
    fn test_build_link_attrs_detects_rdfa_type() {
        let link_attrs = vec![string_kv("rel", "mw:ExtLink")];
        let result = build_link_attrs(&[], false, Some("mw:ExtLink"), Some(&link_attrs));
        assert!(result.has_rdfa_type);
        // rdfa_type is combined, then about (none), then link_attrs.
        assert_eq!(result.attribs[0].key.as_str(), Some("typeof"));
        assert_eq!(result.attribs[0].value.as_str(), Some("mw:ExtLink"));
    }
}
