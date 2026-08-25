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

use crate::wikitext::tokens_v2::{Item, KV, KeyValue, ParsoidToken};

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

/// Serialize a list of rich attributes into the `data-mw.attribs` JSON
/// array. Mirrors PHP's `DataMwAttrib::toJsonArray` (an array of `[k, v]`
/// pairs).
pub fn serialize_data_mw_attribs(attribs: &[crate::wikitext::tokens_v2::DataMwAttrib]) -> String {
    let array: Vec<serde_json::Value> = attribs
        .iter()
        .map(|attr| {
            serde_json::Value::Array(vec![attr_to_json(&attr.key), attr_to_json(&attr.value)])
        })
        .collect();
    serde_json::Value::Array(array).to_string()
}

fn attr_to_json(value: &crate::wikitext::tokens_v2::DataMwValue) -> serde_json::Value {
    use crate::wikitext::tokens_v2::DataMwValue as V;
    match value {
        V::Str(s) => serde_json::Value::String(s.clone()),
        V::Object { txt, html } => {
            let mut obj = serde_json::Map::new();
            if let Some(t) = txt {
                obj.insert("txt".to_string(), serde_json::Value::String(t.clone()));
            }
            if let Some(h) = html {
                obj.insert("html".to_string(), serde_json::Value::String(h.clone()));
            }
            serde_json::Value::Object(obj)
        }
    }
}

/// The result of `split_tokens`.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitTokensResult {
    /// Hoisted transclusion start-meta tokens (empty if none).
    pub meta_tokens: Vec<Item>,
    /// Tokens before the first newline (after any start-meta hoisted out).
    pub pre_nl_buf: Vec<Item>,
    /// Tokens from the newline onward.
    pub post_nl_buf: Vec<Item>,
}

/// Split a token array around the first newline token, hoisting any
/// transclusion/param/language-variant/include/annotation start-meta from the
/// first line to before the whole chunk. Mirrors
/// `AttributeExpander::splitTokens`.
///
/// `token_tsr_start` is the enclosing token's source-range start (for
/// computing `unwrappedWT` and `firstWikitextNode`).
pub fn split_tokens(
    tokens: &[Item],
    nl_tk_pos: isize,
    wrap_templates: bool,
    token_tsr_start: usize,
    token_name: &str,
    token_stx: Option<&str>,
) -> SplitTokensResult {
    let mut pre_nl_buf = Vec::new();
    let mut post_nl_buf = Vec::new();
    let mut start_meta: Option<Item> = None;
    let mut start_meta_index: Option<usize> = None;

    for (i, t) in tokens.iter().enumerate() {
        if i as isize == nl_tk_pos {
            post_nl_buf = tokens[i..].to_vec();
            break;
        }

        if wrap_templates
            && let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = t
            && let Some(ty) = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str())
            && is_meta_type(ty)
            && !ty.ends_with("/End")
        {
            start_meta = Some(t.clone());
            start_meta_index = Some(i);
        }

        pre_nl_buf.push(t.clone());
    }

    if let Some(meta) = start_meta {
        if pre_nl_buf.len() == 1 {
            // Nothing to do (all content is after the newline).
            return SplitTokensResult {
                meta_tokens: Vec::new(),
                pre_nl_buf: Vec::new(),
                post_nl_buf: tokens.to_vec(),
            };
        }

        // Clear the start-meta from pre_nl_buf and hoist it.
        if let Some(idx) = start_meta_index {
            pre_nl_buf[idx] = Item::Str(String::new());
        }

        // Build the hoisted meta token with updated data-parsoid.
        let mut meta_tokens = Vec::new();
        if let Item::Tok(ParsoidToken::SelfclosingTag(mut m)) = meta {
            // `unwrappedWT` should be the wikitext between the enclosing token
            // start and the start-meta; computing it needs the source text,
            // which isn't threaded through this helper yet.
            m.data_parsoid.tmp.unwrapped_wt = Some(String::new());
            m.data_parsoid.tmp.first_wikitext_node = token_stx
                .map(|stx| format!("{}_{}", token_name.to_uppercase(), stx))
                .or_else(|| Some(token_name.to_uppercase()));
            m.data_parsoid.tsr = m
                .data_parsoid
                .tsr
                .map(|tsr| crate::wikitext::tokens_v2::SourceRange::new(token_tsr_start, tsr.end));
            meta_tokens.push(Item::Tok(ParsoidToken::SelfclosingTag(m)));
        }

        SplitTokensResult {
            meta_tokens,
            pre_nl_buf,
            post_nl_buf,
        }
    } else {
        SplitTokensResult {
            meta_tokens: Vec::new(),
            pre_nl_buf: tokens.to_vec(),
            post_nl_buf: Vec::new(),
        }
    }
}

/// Allocate the next transclusion `about` id (mirrors `Env::newAboutId`).
pub fn new_about_id(counter: &std::cell::Cell<usize>) -> String {
    let id = counter.get() + 1;
    counter.set(id);
    format!("#mwt{id}")
}

/// Expand a token's already-expanded attribute KVs into its final attributes,
/// handling the reparse-KV-string and (scenario 1) mixed-content cases, and
/// marking templated attributes with `mw:ExpandedAttrs`. Mirrors PHP
/// `AttributeExpander::buildExpandedAttrs`.
///
/// Returns `metaTokens ++ [token] ++ postNLToks` (hoisted transclusion markers
/// before the token and, for scenario 1, content moved after it).
#[allow(clippy::too_many_lines)]
pub fn build_expanded_attrs(
    mut token: ParsoidToken,
    old_attrs: &[KV],
    expanded_attrs: Vec<KV>,
    about_counter: &std::cell::Cell<usize>,
    in_template: bool,
) -> Vec<Item> {
    use super::attribute_transform_manager::{items_to_key_value, key_value_to_items};
    use crate::wikitext::token_utils::{is_html_tag, tokens_to_string};

    let wrap_templates = !in_template;
    let token_name = token.get_name().to_string();
    let nl_tk_okay = is_html_tag(&token) || (token_name != "table" && token_name != "tr");

    let mut meta_tokens: Vec<Item> = Vec::new();
    let mut post_nl_toks: Vec<Item> = Vec::new();
    let mut new_attrs: Option<Vec<KV>> = None;
    let mut should_mark_expanded = false;

    for (i, old_a) in old_attrs.iter().enumerate() {
        let mut expanded_a = expanded_attrs[i].clone();
        // Preserve the key/value source and offsets on the expanded attribute.
        if expanded_a.key != old_a.key || expanded_a.value != old_a.value {
            expanded_a.ksrc = old_a.ksrc.clone();
            expanded_a.vsrc = old_a.vsrc.clone();
            expanded_a.src_offsets = old_a.src_offsets.clone();
        }

        let mut expanded_k = expanded_a.key.clone();
        let expanded_v = expanded_a.value.clone();
        let orig_k = old_a.key.clone();
        let _orig_v = old_a.value.clone();

        let mut reparsed_kv = false;
        let mut key_uses_mixed_attr_content_tpl = false;
        let mut val_uses_mixed_attr_content_tpl = false;
        let mut key_generated = false;
        let mut val_generated = false;

        // Expand a templated attribute key.
        if matches!(expanded_k, KeyValue::Tokens(_)) {
            let mut expanded_k_items = key_value_to_items(&expanded_k);
            let nl_tk_pos = nl_tk_index(&expanded_k_items, nl_tk_okay);
            if nl_tk_pos != -1 {
                // Scenario 1: split the expanded key around the newline, hoisting
                // the transclusion start-meta and moving post-newline content after
                // the token.
                key_uses_mixed_attr_content_tpl = true;
                let split = split_tokens(
                    &expanded_k_items,
                    nl_tk_pos,
                    wrap_templates,
                    0,
                    &token_name,
                    token.data_parsoid().and_then(|d| d.stx.as_deref()),
                );
                expanded_k_items = split.pre_nl_buf;
                post_nl_toks = split.post_nl_buf;
                meta_tokens = split.meta_tokens;
                if expanded_a.src_offsets.is_some() {
                    expanded_a.src_offsets = None;
                    expanded_a.ksrc = None;
                }
            } else {
                // Scenario 2: strip meta markers from the expanded key.
                let stripped = strip_meta_tags(&expanded_k_items, wrap_templates);
                key_generated = stripped.has_generated_content;
                expanded_k_items = stripped.value;
            }
            expanded_a.key = items_to_key_value(expanded_k_items.clone());

            // Reparse-KV-string: a template that generates one or more `k=v`
            // strings is retokenized to recover the individual attributes.
            if expanded_a.value.is_empty() {
                let k_str = tokens_to_string(&expanded_k_items).trim().to_string();
                if k_str.contains('=') {
                    let kvs = crate::wikitext::tokenizer_v2::tokenize_as_attributes(&k_str);
                    if !kvs.is_empty() {
                        // At this point templates should have been expanded;
                        // any leftovers are converted to plain strings.
                        let clean_kvs: Vec<KV> = kvs
                            .into_iter()
                            .map(|kv| KV {
                                key: items_to_key_value(vec![Item::Str(tpl_toks_to_string(
                                    &key_value_to_items(&kv.key),
                                ))]),
                                value: items_to_key_value(vec![Item::Str(tpl_toks_to_string(
                                    &key_value_to_items(&kv.value),
                                ))]),
                                src_offsets: None,
                                ksrc: None,
                                vsrc: None,
                            })
                            .collect();
                        expanded_k = clean_kvs[0].key.clone();
                        expanded_a.key = clean_kvs[0].key.clone();
                        reparsed_kv = true;
                        if new_attrs.is_none() {
                            new_attrs = Some(if i == 0 {
                                Vec::new()
                            } else {
                                expanded_attrs[..i].to_vec()
                            });
                        }
                        if let Some(na) = new_attrs.as_mut() {
                            na.extend(clean_kvs);
                        }
                    }
                }
            }
        }

        // Expand a templated attribute value (when the key is a plain string).
        if let KeyValue::Str(s) = &expanded_k
            && !s.starts_with("mw:")
            && matches!(old_a.value, KeyValue::Tokens(_))
        {
            let mut expanded_v_items = key_value_to_items(&expanded_v);
            let nl_tk_pos = nl_tk_index(&expanded_v_items, nl_tk_okay);
            if nl_tk_pos != -1 {
                val_uses_mixed_attr_content_tpl = true;
                let split = split_tokens(
                    &expanded_v_items,
                    nl_tk_pos,
                    wrap_templates,
                    0,
                    &token_name,
                    token.data_parsoid().and_then(|d| d.stx.as_deref()),
                );
                expanded_v_items = split.pre_nl_buf;
                post_nl_toks = split.post_nl_buf;
                meta_tokens = split.meta_tokens;
                if expanded_a.src_offsets.is_some() {
                    expanded_a.src_offsets = None;
                    expanded_a.vsrc = None;
                }
            } else {
                let stripped = strip_meta_tags(&expanded_v_items, wrap_templates);
                val_generated = stripped.has_generated_content;
                expanded_v_items = stripped.value;
            }
            expanded_a.value = items_to_key_value(expanded_v_items);
        }

        let _ = (
            orig_k,
            key_uses_mixed_attr_content_tpl,
            val_uses_mixed_attr_content_tpl,
        );

        should_mark_expanded |=
            key_generated || val_generated || (reparsed_kv && !meta_tokens.is_empty());

        if let Some(na) = new_attrs.as_mut()
            && !reparsed_kv
        {
            na.push(expanded_a);
        }
    }

    if let Some(na) = new_attrs {
        token.set_attribs(na);
    } else {
        token.set_attribs(expanded_attrs);
    }

    // Mark the token as having expanded attributes, unless it already carries
    // an `about` (an existing transclusion/extension wrapping).
    if token.get_attribute_v("about").is_none() && should_mark_expanded {
        let about_id = new_about_id(about_counter);
        token.set_attribute("about", &about_id);
        token.add_space_separated_attribute("typeof", "mw:ExpandedAttrs");
    }

    let mut out = meta_tokens;
    out.push(Item::Tok(token));
    out.extend(post_nl_toks);
    out
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

    #[test]
    fn test_serialize_data_mw_attribs() {
        use crate::wikitext::tokens_v2::{DataMwAttrib, DataMwValue};

        let attribs = vec![DataMwAttrib::new(
            DataMwValue::Str("style".to_string()),
            DataMwValue::Str("color:red".to_string()),
        )];

        let json = serialize_data_mw_attribs(&attribs);
        assert_eq!(json, "[[\"style\",\"color:red\"]]");
    }

    #[test]
    fn test_split_tokens_no_meta() {
        let tokens = vec![
            Item::Str("a".to_string()),
            Item::Tok(ParsoidToken::Nl(NlTk::new(
                crate::wikitext::tokens_v2::SourceRange::new(0, 1),
            ))),
            Item::Str("b".to_string()),
        ];
        // No start-meta: PHP returns the whole token array as `preNLBuf`.
        let result = split_tokens(&tokens, 1, true, 0, "table", None);
        assert!(result.meta_tokens.is_empty());
        assert_eq!(result.pre_nl_buf, tokens);
        assert!(result.post_nl_buf.is_empty());
    }

    #[test]
    fn test_split_tokens_hoists_meta() {
        // A transclusion meta followed by content then a newline.
        let tokens = vec![
            meta_token("mw:Transclusion"),
            Item::Str("a".to_string()),
            Item::Tok(ParsoidToken::Nl(NlTk::new(
                crate::wikitext::tokens_v2::SourceRange::new(0, 1),
            ))),
            Item::Str("b".to_string()),
        ];
        let result = split_tokens(&tokens, 2, true, 0, "table", None);
        assert_eq!(result.meta_tokens.len(), 1);
        // The meta is hoisted; pre_nl_buf has "" (cleared meta) + "a".
        assert_eq!(
            result.pre_nl_buf,
            vec![Item::Str(String::new()), Item::Str("a".to_string())]
        );
        assert_eq!(result.post_nl_buf.len(), 2);
    }
}
