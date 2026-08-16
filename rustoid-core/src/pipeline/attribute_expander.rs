//! AttributeExpander — faithful port of the pure static helpers from PHP
//! Parsoid's `src/Wt2Html/TT/AttributeExpander.php`.
//!
//! These helpers process token arrays that result from attribute expansion:
//! - `nl_tk_index` — locate the first newline token outside an include
//!   directive (used to split multi-attribute template output across lines).
//! - `strip_meta_tags` — remove transclusion/param/include/annotation meta
//!   markers from an expanded token chunk and report whether generated
//!   content was present.
//! - `tpl_toks_to_string` — convert a preprocessed-token chunk back to plain
//!   wikitext.
//!
//! The full `buildExpandedAttrs`/`processComplexAttributes` orchestrator still
//! depends on `PipelineUtils::expandAttrValuesToDOM` and `DataMwAttrib`, which
//! are layered in once the value-to-DOM pipeline is ported.

use crate::wikitext::tokens_v2::{Item, ParsoidToken};

/// The `typeof` regexp identifying the meta markers that isolate
/// transclusion/param/language-variant/include/annotation content, mirroring
/// PHP's `AttributeExpander::META_TYPE_MATCHER`.
const META_TYPE_PREFIXES: &[&str] = &[
    "mw:LanguageVariant",
    "mw:Transclusion",
    "mw:Param",
    "mw:Includes/",
    "mw:Annotation/",
];

/// Is this `typeof` string a start/end meta marker for encapsulated content?
fn is_meta_type(ty: &str) -> bool {
    META_TYPE_PREFIXES.iter().any(|p| ty.starts_with(p))
}

/// The `typeof` regexp for include directives, mirroring PHP's `$includeRE`.
fn is_include_type(ty: &str) -> bool {
    ty.starts_with("mw:Includes/")
}

/// Locate the index of the first newline token outside an include directive.
/// Mirrors `AttributeExpander::nlTkIndex`.
///
/// Returns `-1` if no such newline exists (or `nl_tk_okay` short-circuits the
/// check).
pub fn nl_tk_index(tokens: &[Item], nl_tk_okay: bool) -> isize {
    if nl_tk_okay {
        return -1;
    }

    let mut in_include = false;
    for (i, token) in tokens.iter().enumerate() {
        if let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = token
            && let Some(ty) = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str())
            && is_include_type(ty)
        {
            in_include = !ty.ends_with("/End");
        }
        if !in_include && matches!(token, Item::Tok(ParsoidToken::Nl(_))) {
            return i as isize;
        }
    }
    -1
}

/// The result of `strip_meta_tags`.
#[derive(Debug, Clone, PartialEq)]
pub struct StripMetaTagsResult {
    /// Whether the chunk contained generated (template/extension) content.
    pub has_generated_content: bool,
    /// Whether an image/link/table-cell terminator was seen.
    pub cell_attr_terminator_seen: bool,
    /// Annotation types encountered.
    pub annotation_types: Vec<String>,
    /// The stripped token chunk.
    pub value: Vec<Item>,
}

/// Strip all meta markers introduced by transclusions/params/includes, and
/// return the content. Mirrors `AttributeExpander::stripMetaTags`.
pub fn strip_meta_tags(tokens: &[Item], wrap_templates: bool) -> StripMetaTagsResult {
    let mut buf = Vec::new();
    let mut has_generated_content = false;
    let mut cell_attr_terminator_seen = false;
    let mut annotation_types = Vec::new();

    for t in tokens {
        if let Item::Tok(tok) = t {
            if matches!(tok, ParsoidToken::Tag(_) | ParsoidToken::SelfclosingTag(_)) {
                let type_of = tok.get_attribute_v("typeof");
                let rel = tok.get_attribute_v("rel");

                // DOM fragments indicate generated attribute content.
                if has_dom_fragment_type(tok) {
                    has_generated_content = true;
                }

                // Images/links terminate table-cell attribute processing.
                if is_cell_attr_terminator(type_of, rel) {
                    cell_attr_terminator_seen = true;
                }

                if wrap_templates {
                    if let Some(ty) = type_of
                        && is_meta_type(ty)
                    {
                        if !ty.ends_with("/End") {
                            has_generated_content = true;
                        }
                        if let Some(ann) = annotation_type_from(ty) {
                            annotation_types.push(ann);
                        }
                    } else {
                        buf.push(t.clone());
                        continue;
                    }
                }

                // Keep non-meta tokens.
                if tok.get_name() != "meta" {
                    buf.push(t.clone());
                }
            } else {
                buf.push(t.clone());
            }
        } else {
            buf.push(t.clone());
        }
    }

    StripMetaTagsResult {
        has_generated_content,
        cell_attr_terminator_seen,
        annotation_types,
        value: buf,
    }
}

fn has_dom_fragment_type(tok: &ParsoidToken) -> bool {
    tok.get_attribute_v("typeof")
        .is_some_and(|ty| ty.starts_with("mw:DOMFragment"))
}

fn is_cell_attr_terminator(type_of: Option<&str>, rel: Option<&str>) -> bool {
    let is_file = type_of.is_some_and(|ty| {
        let ty = format!(" {ty} ");
        ty.contains(" mw:File") || ty.contains(" mw:File/")
    });
    let is_wikilink_construct = rel.is_some_and(|r| {
        [
            "mw:WikiLink",
            "mw:MediaLink",
            "mw:PageProp/Category",
            "mw:PageProp/Language",
        ]
        .iter()
        .any(|prefix| r.starts_with(prefix) || r == *prefix || r.contains(prefix))
    });
    is_file || is_wikilink_construct
}

/// Extract the annotation type from a `mw:Annotation/<type>` `typeof` value.
fn annotation_type_from(ty: &str) -> Option<String> {
    if let Some(rest) = ty.strip_prefix("mw:Annotation/") {
        let base = rest.strip_suffix("/End").unwrap_or(rest);
        Some(base.to_string())
    } else {
        None
    }
}

/// Convert a preprocessed token chunk back to plain wikitext. Mirrors
/// `AttributeExpander::tplToksToString`.
pub fn tpl_toks_to_string(tokens: &[Item]) -> String {
    let mut buf = String::new();
    for t in tokens {
        match t {
            Item::Str(s) => buf.push_str(s),
            Item::Tok(tok) => {
                if let Some(src) = tok.data_parsoid().and_then(|d| d.src.clone()) {
                    buf.push_str(&src);
                }
            }
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::{NlTk, SelfclosingTagTk};

    fn meta_token(type_of: &str) -> Item {
        let mut stt = SelfclosingTagTk::new(
            "meta",
            vec![],
            crate::wikitext::tokens_v2::DataParsoid::default(),
        );
        stt.add_attribute_str("typeof", type_of);
        Item::Tok(ParsoidToken::SelfclosingTag(stt))
    }

    #[test]
    fn test_nl_tk_index_noop() {
        let tokens = vec![Item::Str("a".to_string())];
        assert_eq!(nl_tk_index(&tokens, true), -1);
    }

    #[test]
    fn test_nl_tk_index_finds_newline() {
        let tokens = vec![
            Item::Str("a".to_string()),
            Item::Tok(ParsoidToken::Nl(NlTk::new(
                crate::wikitext::tokens_v2::SourceRange::new(0, 1),
            ))),
            Item::Str("b".to_string()),
        ];
        assert_eq!(nl_tk_index(&tokens, false), 1);
    }

    #[test]
    fn test_strip_meta_tags() {
        let tokens = vec![
            meta_token("mw:Transclusion"),
            Item::Str("content".to_string()),
            meta_token("mw:Transclusion/End"),
        ];

        let result = strip_meta_tags(&tokens, true);
        assert!(result.has_generated_content);
        assert_eq!(result.value, vec![Item::Str("content".to_string())]);
    }

    #[test]
    fn test_tpl_toks_to_string() {
        let tokens = vec![Item::Str("foo".to_string()), Item::Str("bar".to_string())];
        assert_eq!(tpl_toks_to_string(&tokens), "foobar");
    }
}
