//! Token utilities — faithful port of PHP Parsoid's `src/Utils/TokenUtils.php`.
//!
//! These helpers query token properties and manipulate token collections.
//! They are shared across TokenTransform handlers.

use crate::wikitext::consts;
use crate::wikitext::tokens_v2::{Item, KeyValue, ParsoidToken, SelfclosingTagTk};

/// Is this name a wikitext block tag?
pub fn is_wikitext_block_tag(name: &str) -> bool {
    consts::wikitext_block_elems().contains(name)
}

/// In the legacy parser, these block tags open block-tag scope.
pub fn tag_opens_block_scope(name: &str) -> bool {
    consts::block_elems().contains(name) || consts::always_block_elems().contains(name)
}

/// In the legacy parser, these block tags close block-tag scope.
pub fn tag_closes_block_scope(name: &str) -> bool {
    consts::anti_block_elems().contains(name) || consts::never_block_elems().contains(name)
}

/// Is this token an HTML tag (i.e., came from literal HTML in wikitext)?
/// Mirrors PHP `TokenUtils::isHTMLTag`: `stx === 'html'`.
pub fn is_html_tag(token: &ParsoidToken) -> bool {
    if let Some(dp) = token.data_parsoid() {
        return dp.stx.as_deref() == Some("html");
    }
    false
}

/// Determine whether the token matches the given `typeof` attribute value
/// (exact match). Mirrors `TokenUtils::hasTypeOf`.
pub fn has_type_of(token: &ParsoidToken, expected: &str) -> bool {
    token.get_attribute_v("typeof") == Some(expected)
}

/// Determine whether the token's `typeof` attribute matches a regex.
/// For the common prefixes we need, match against a prefix pattern.
/// Mirrors `TokenUtils::matchTypeOf` for the specific patterns used.
pub fn match_type_of(token: &ParsoidToken, pattern: &str) -> Option<String> {
    let v = token.get_attribute_v("typeof")?;
    for ty in v.split_whitespace() {
        if matches_type(ty, pattern) {
            return Some(ty.to_string());
        }
    }
    None
}

/// Match a single typeof value against a PHP-style regex pattern.
/// Supports the common patterns: `#^mw:Transclusion/End#`, `#^mw:Transclusion$#`,
/// `#^mw:ExtLink/#`, etc.
fn matches_type(value: &str, pattern: &str) -> bool {
    // Strip PHP regex delimiters (#...#) and anchors (^, $).
    let re = pattern.trim_start_matches('#').trim_end_matches('#');
    let anchored_start = re.starts_with('^');
    let anchored_end = re.ends_with('$');
    // Strip anchors in sequence (^ first, then $).
    let mut inner = re;
    if anchored_start {
        inner = inner.strip_prefix('^').unwrap_or(inner);
    }
    if anchored_end {
        inner = inner.strip_suffix('$').unwrap_or(inner);
    }

    match (anchored_start, anchored_end) {
        (true, true) => value == inner,
        (true, false) => value.starts_with(inner),
        (false, true) => value.ends_with(inner),
        (false, false) => value.contains(re),
    }
}

/// Is this a template token (template/template3/templatearg)?
pub fn is_template_token(token: &ParsoidToken) -> bool {
    matches!(
        token,
        ParsoidToken::SelfclosingTag(t)
            if t.name == "template" || t.name == "template3" || t.name == "templatearg"
    )
}

/// Is this a template arg token?
pub fn is_template_arg_token(token: &ParsoidToken) -> bool {
    matches!(token, ParsoidToken::SelfclosingTag(t) if t.name == "templatearg")
}

/// Is this an extension token?
pub fn is_extension_token(token: &ParsoidToken) -> bool {
    matches!(token, ParsoidToken::SelfclosingTag(t) if t.name == "extension")
}

/// Is this token a behavior switch?
pub fn is_behavior_switch(token: &ParsoidToken) -> bool {
    match token {
        ParsoidToken::SelfclosingTag(t) if t.name == "behavior-switch" => true,
        ParsoidToken::SelfclosingTag(t) if t.name == "meta" => t
            .attribs
            .iter()
            .any(|kv| kv.key.as_str() == Some("property")),
        _ => false,
    }
}

/// Is this token sol-transparent? Mirrors `TokenUtils::isSolTransparent`.
pub fn is_sol_transparent(token: &Item) -> bool {
    match token {
        Item::Str(s) => !s.is_empty() && s.chars().all(|c| c == ' ' || c == '\t'),
        Item::Tok(t) => match t {
            ParsoidToken::EmptyLine(_) | ParsoidToken::Comment(_) => true,
            ParsoidToken::SelfclosingTag(tk) => {
                // Behavior switches and meta tokens are sol-transparent.
                is_behavior_switch(t) || tk.name == "meta"
            }
            _ => false,
        },
    }
}

/// Does this token represent an HTML entity span (`<span typeof="mw:Entity">`)?
pub fn is_entity_span_token(token: &ParsoidToken) -> bool {
    matches!(token, ParsoidToken::Tag(t) if t.name == "span" && has_type_of(token, "mw:Entity"))
}

/// Flatten/convert a token array into a string.
/// Mirrors `TokenUtils::tokensToString` (non-strict mode without opts).
pub fn tokens_to_string(tokens: &[Item]) -> String {
    let mut out = String::new();
    for token in tokens {
        match token {
            Item::Str(s) => out.push_str(s),
            Item::Tok(t) => match t {
                // Strip comments and newlines.
                ParsoidToken::Comment(_) | ParsoidToken::Nl(_) => {}
                ParsoidToken::Tag(tk) if tk.name == "listItem" => {
                    // Append the bullets source.
                    if let Some(bullets) = tk
                        .attribs
                        .iter()
                        .find(|kv| kv.key.as_str() == Some("bullets"))
                        .and_then(|kv| kv.value.as_str())
                    {
                        out.push_str(bullets);
                    }
                }
                // Reconstruct an `extension` token (e.g. `<nowiki>`) back to its
                // source wikitext (mirrors `tokensToString` reconstructing the
                // token's `src`), so a nowiki inside an option/caption survives
                // stringification instead of collapsing to nothing.
                ParsoidToken::SelfclosingTag(tk) if tk.name == "extension" => {
                    if let Some(src) = tk.data_parsoid.src.as_deref() {
                        out.push_str(src);
                    }
                }
                _ => {}
            },
        }
    }
    out
}

/// Convert a key-value's value (string or tokens) to a string.
pub fn key_value_to_string(kv: &KeyValue) -> String {
    match kv {
        KeyValue::Str(s) => s.clone(),
        KeyValue::Tokens(items) => tokens_to_string(items),
    }
}

/// Create an `mw:IndentPreWS` meta token (used by PreHandler).
pub fn new_indent_pre_ws() -> ParsoidToken {
    let mut tk = SelfclosingTagTk::new("meta", vec![], Default::default());
    tk.attribs.push(crate::wikitext::tokens_v2::KV {
        key: KeyValue::Str("typeof".to_string()),
        value: KeyValue::Str("mw:IndentPreWS".to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });
    ParsoidToken::SelfclosingTag(tk)
}

/// Is this token a `listItem` TagTk?
pub fn is_list_item_token(token: &ParsoidToken) -> bool {
    matches!(token, ParsoidToken::Tag(t) if t.name == "listItem")
}

/// Get the `bullets` attribute value of a listItem token as chars.
pub fn get_bullets(token: &ParsoidToken) -> Vec<char> {
    match token {
        ParsoidToken::Tag(t) => t
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("bullets"))
            .and_then(|kv| kv.value.as_str())
            .map(|s| s.chars().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}
