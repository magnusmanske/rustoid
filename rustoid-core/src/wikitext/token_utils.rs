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

/// Clear the `tsr` on every token in a chunk, and on tokens nested inside its
/// attributes.
///
/// Mirrors PHP's `TemplateHandler::processTemplateTokens`, which `unset`s `tsr`
/// on everything a template / parser function / variable expansion produced: a
/// token from an expanded *sub-source* carries a source range relative to that
/// source, not the page, so leaving it set lets the encapsulation range plan
/// read it as a bogus page offset. On `Israel` a `{{PAGENAME}}` expanded inside
/// a wikilink target (`Template:Wikiatlas` is `[[commons:Atlas of
/// {{PAGENAME}}|…]]`) kept `tsr = (0, 12)`; the range plan took it for a
/// top-level transclusion starting at page offset 0 and folded the page tail
/// into the `External links` list's `data-mw`, trapping the `==References==`
/// section inside an attribute.
pub fn clear_tsr(items: &mut [Item]) {
    for item in items.iter_mut() {
        clear_tsr_item(item);
    }
}

fn clear_tsr_item(item: &mut Item) {
    let Item::Tok(tok) = item else {
        return;
    };
    if let Some(dp) = tok.data_parsoid_mut() {
        dp.tsr = None;
    }
    if let Some(attribs) = tok.attribs_mut() {
        for kv in attribs.iter_mut() {
            clear_tsr_value(&mut kv.key);
            clear_tsr_value(&mut kv.value);
        }
    }
}

fn clear_tsr_value(value: &mut KeyValue) {
    if let KeyValue::Tokens(items) = value {
        clear_tsr(items);
    }
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

/// Options for [`tokens_to_string`]. Mirrors the `$opts` array of PHP's
/// `TokenUtils::tokensToString`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokensToStringOpts<'a> {
    /// Resolve `mw:DOMFragment` placeholder tokens to their fragment's text
    /// content (mirrors the `unpackDOMFragments` option). PHP notes the correct
    /// thing would be the fragment's *innerHTML*, but `<translate>`/`<nowiki>`
    /// are the expected contents and `textContent` suffices for them — which is
    /// exactly why a `<nowiki>` in an attribute value flattens to its literal
    /// text (T280115).
    pub unpack_dom_fragments: bool,
    /// The stashed DOM fragments, keyed by fragment id.
    pub fragments: Option<&'a std::collections::HashMap<usize, crate::dom::node::Node>>,
}

/// Flatten/convert a token array into a string.
/// Mirrors `TokenUtils::tokensToString` (non-strict mode without opts).
pub fn tokens_to_string(tokens: &[Item]) -> String {
    tokens_to_string_with_opts(tokens, TokensToStringOpts::default())
}

/// Render a single item to its source text.
///
/// The one-item form of [`tokens_to_string`], for callers that already have the
/// items apart and would otherwise allocate a slice to borrow.
pub fn item_to_string(item: &Item) -> String {
    tokens_to_string(std::slice::from_ref(item))
}

/// [`tokens_to_string`] with explicit options.
pub fn tokens_to_string_with_opts(tokens: &[Item], opts: TokensToStringOpts<'_>) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
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
                // Reconstruct a `wikilink` token as `[[target]]`.
                //
                // A wikilink token is normally replaced by a real `<a>` before
                // this is reached, so the arm exists for the one place a token is
                // *handed to a module* rather than rendered: a missing template's
                // answer is the wikitext `[[:Title]]`, and `frame:expandTemplate`
                // must give the module exactly that source. Without the arm the
                // token stringified to nothing, and the redlink disappeared.
                ParsoidToken::SelfclosingTag(tk) if tk.name == "wikilink" => {
                    let href = tk
                        .attribs
                        .iter()
                        .find(|kv| kv.key.as_str() == Some("href"))
                        .and_then(|kv| kv.value.as_str())
                        .unwrap_or_default();
                    out.push_str("[[");
                    out.push_str(href);
                    out.push_str("]]");
                }
                // Resolve a DOM-fragment placeholder to its fragment's text
                // content. A `TagTk` is followed by its `EndTagTk`, which is
                // skipped (mirrors PHP's `$i += 1` + the "tag should be followed
                // by endtag" invariant).
                ParsoidToken::Tag(tk)
                    if opts.unpack_dom_fragments
                        && has_dom_fragment_attr(&tk.attribs)
                        && opts.fragments.is_some() =>
                {
                    if let Some(fid) = fragment_id(&tk.attribs) {
                        let frag = opts.fragments.and_then(|m| m.get(&fid));
                        out.push_str(&dom_fragment_text(frag));
                    }
                    i += 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    out
}

/// Does an attribute list carry a `typeof` beginning `mw:DOMFragment`?
fn has_dom_fragment_attr(attribs: &[crate::wikitext::tokens_v2::KV]) -> bool {
    attribs.iter().any(|kv| {
        kv.key.as_str() == Some("typeof")
            && kv
                .value
                .as_str()
                .is_some_and(|ty| ty.starts_with("mw:DOMFragment"))
    })
}

/// The `data-fragment-id` of a DOM-fragment placeholder token.
fn fragment_id(attribs: &[crate::wikitext::tokens_v2::KV]) -> Option<usize> {
    attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("data-fragment-id"))
        .and_then(|kv| kv.value.as_str())
        .and_then(|s| s.parse().ok())
}

/// The text content of a stashed DOM fragment (`Node::textContent`).
fn dom_fragment_text(fragment: Option<&crate::dom::node::Node>) -> String {
    fn collect(node: &crate::dom::node::Node, out: &mut String) {
        match &node.kind {
            crate::dom::node::NodeKind::Text(t) => out.push_str(t),
            _ => {
                for child in &node.children {
                    collect(child, out);
                }
            }
        }
    }
    let Some(frag) = fragment else {
        return String::new();
    };
    let mut out = String::new();
    for child in &frag.children {
        collect(child, &mut out);
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

/// Like [`tokens_to_string`], but retains newlines: a `Nl` token is emitted as
/// its source (a single `\n`), rather than stripped. Mirrors `tokensToString`
/// with `['retainNLs' => true]`, used where a `format="wikitext"` body must
/// round-trip its newlines.
pub fn tokens_to_string_with_nls(tokens: &[Item]) -> String {
    let mut out = String::new();
    for token in tokens {
        match token {
            Item::Str(s) => out.push_str(s),
            Item::Tok(t) => match t {
                ParsoidToken::Comment(_) => {}
                ParsoidToken::Nl(_) => out.push('\n'),
                ParsoidToken::Tag(tk) if tk.name == "listItem" => {
                    if let Some(bullets) = tk
                        .attribs
                        .iter()
                        .find(|kv| kv.key.as_str() == Some("bullets"))
                        .and_then(|kv| kv.value.as_str())
                    {
                        out.push_str(bullets);
                    }
                }
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

/// Stringify a chunk back to the *source* it came from, keeping tags.
///
/// [`tokens_to_string`] deliberately drops HTML tags, which is right for its
/// Parsoid callers and wrong for one: the text a module receives from
/// `frame:expandTemplate`. That text is wikitext, so `{{1x|<b>x</b>}}` must
/// answer with `<b>x</b>` — eight characters, as the live service reports —
/// and a stringifier that drops the tag answers with nothing at all.
///
/// Reconstructing from each token's recorded `src` is what makes that exact:
/// the tokenizer already knows the bytes, so there is no need to re-derive a
/// tag's spelling (attribute quoting, spacing, self-closing) from its parsed
/// form and risk differing. A token with no recorded source contributes its
/// name only when it has one, so nothing is invented.
pub fn tokens_to_source(tokens: &[Item]) -> String {
    let mut out = String::new();
    for token in tokens {
        match token {
            Item::Str(s) => out.push_str(s),
            Item::Tok(t) => {
                // A comment is stripped by the preprocessor before a module sees
                // anything, so it is not part of the answer.
                if matches!(t, ParsoidToken::Comment(_)) {
                    continue;
                }
                if let Some(src) = t.data_parsoid().and_then(|dp| dp.src.as_deref()) {
                    out.push_str(src);
                } else if let Some(src) = table_token_wikitext(t) {
                    out.push_str(&src);
                }
            }
        }
    }
    out
}

/// The wikitext spelling of a table-structure token (`table`/`tr`/`caption`/
/// `td`/`th`), rebuilt from its `dataParsoid`.
///
/// The tokenizer records a table token's spelling in pieces — `start_tag_src`
/// for the `{|`/`|-`/`|`/`!` marker, `tmp.attr_src` for the attribute box,
/// `attr_sep_src` for a non-default separator, `end_tag_src` for `|}` — and never
/// in `src` (which is set only on transclusion/extension tokens). A renderer that
/// reconstructs wikitext from `src` therefore drops the whole table: a
/// `frame:expandTemplate` answer loses every `{|` and cell, so a template that
/// builds a table (every taxobox, infobox and navbox) hands the module flat text
/// instead. Mirrors PHP `TokenStreamPatcher::convertNonHTMLTokenToString`.
///
/// The attribute box is rebuilt from the token's *expanded* attributes, not from
/// `tmp.attr_src`: that field holds the source as written, still carrying the
/// templates the expansion substituted (`{{{colour}}}` in a taxobox), and the
/// module must receive the answer rather than the template. A table's `|}` uses
/// `end_tag_src`; other end tags carry no wikitext and answer `None`.
pub fn table_token_wikitext(tok: &ParsoidToken) -> Option<String> {
    let (name, dp, is_end) = match tok {
        ParsoidToken::Tag(t) => (t.name.as_str(), &t.data_parsoid, false),
        ParsoidToken::EndTag(t) => (t.name.as_str(), &t.data_parsoid, true),
        _ => return None,
    };
    let default_marker = match (name, is_end) {
        ("table", false) => "{|",
        ("table", true) => "|}",
        ("tr", false) => "|-",
        ("caption", false) => "|+",
        ("caption", true) => return Some(String::new()),
        ("td", false) if dp.stx.as_deref() == Some("row") => "||",
        ("td", false) => "|",
        ("th", false) if dp.stx.as_deref() == Some("row") => "!!",
        ("th", false) => "!",
        _ => return None,
    };
    let marker = if is_end {
        dp.end_tag_src.as_deref().unwrap_or(default_marker)
    } else {
        dp.start_tag_src.as_deref().unwrap_or(default_marker)
    };
    let mut out = marker.to_string();
    // The attribute box from the *expanded* attributes, not `attr_src`: the box
    // source still holds the templates the expansion substituted (`{{{colour}}}`
    // in a taxobox), and the module must receive the answer, not the template.
    let attrs = tok.get_attribs();
    if attrs.is_empty() {
        if let Some(sep) = dp.attr_sep_src.as_deref() {
            out.push_str(sep);
        }
    } else {
        for kv in attrs {
            out.push(' ');
            out.push_str(&key_value_source_text(&kv.key));
            out.push_str("=\"");
            out.push_str(&key_value_source_text(&kv.value));
            out.push('"');
        }
        if matches!(name, "caption" | "td" | "th") {
            out.push_str(dp.attr_sep_src.as_deref().unwrap_or("|"));
        }
    }
    Some(out)
}

/// A token's own source with substituted attributes written back.
///
/// `data_parsoid.src` is the source *as written*, while the token's attributes
/// are the *expanded* ones. A module receives the expansion's wikitext —
/// `frame:expandTemplate` hands back what the preprocessor's `$frame->expand`
/// produced — so a substituted attribute would otherwise reach the module
/// spelled the old way: `class="x{{{foo|MISSING}}}"` where the service answers
/// `class="xBAR"`.
///
/// Each attribute carries the source it was written as (`ksrc`/`vsrc`), so the
/// rewrite is a text replacement and every other byte of the tag — quoting,
/// spacing, attributes that were not templated — is kept. `None` means the
/// source was declined (the old text is not unique within the tag, or an
/// attribute was expanded without a recorded source), and the caller should use
/// the source as it stands rather than guess.
pub fn rewrite_expanded_attrs(
    src: &str,
    attribs: &[crate::wikitext::tokens_v2::KV],
) -> Option<String> {
    let mut out = src.to_string();
    let mut patched = false;
    for kv in attribs {
        for (old_src, new_src) in [
            (kv.ksrc.as_deref(), key_value_source_text(&kv.key)),
            (kv.vsrc.as_deref(), key_value_source_text(&kv.value)),
        ] {
            let Some(old_src) = old_src.filter(|s| !s.is_empty()) else {
                continue;
            };
            if new_src == old_src {
                continue;
            }
            let Some(pos) = out.find(old_src) else {
                continue;
            };
            if out[pos + old_src.len()..].contains(old_src) {
                // Present twice in one tag: which occurrence is the attribute's
                // is not knowable here, and guessing could corrupt an unrelated
                // value, so the whole rewrite is declined.
                return None;
            }
            out.replace_range(pos..pos + old_src.len(), &new_src);
            patched = true;
        }
    }
    patched.then_some(out)
}

/// The source text an attribute key or value now stands for.
fn key_value_source_text(value: &KeyValue) -> String {
    match value {
        KeyValue::Str(s) => s.clone(),
        KeyValue::Tokens(items) => tokens_to_source(items),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::KV;

    fn kv(value: KeyValue, vsrc: Option<&str>) -> KV {
        KV {
            key: KeyValue::Str("class".to_string()),
            value,
            src_offsets: None,
            ksrc: None,
            vsrc: vsrc.map(str::to_string),
        }
    }

    #[test]
    fn rewrite_expanded_attrs_writes_a_substituted_value_back() {
        let src = r#"<div class="x{{{foo|MISSING}}}">"#;
        let attribs = vec![kv(
            KeyValue::Tokens(vec![
                Item::Str("x".to_string()),
                Item::Str("BAR".to_string()),
            ]),
            Some("x{{{foo|MISSING}}}"),
        )];
        assert_eq!(
            rewrite_expanded_attrs(src, &attribs).as_deref(),
            Some(r#"<div class="xBAR">"#)
        );
    }

    #[test]
    fn rewrite_expanded_attrs_declines_when_the_old_text_is_not_unique() {
        let src = r#"<div class="a{{b}}" id="a{{b}}">"#;
        let attribs = vec![kv(KeyValue::Str("aBAR".to_string()), Some("a{{b}}"))];
        // The old text appears twice in the tag, so the rewrite is declined
        // rather than corrupting the unrelated value.
        assert_eq!(rewrite_expanded_attrs(src, &attribs), None);
    }

    #[test]
    fn rewrite_expanded_attrs_is_a_no_op_without_a_change() {
        let src = r#"<div class="a">"#;
        let attribs = vec![kv(KeyValue::Str("a".to_string()), Some("a"))];
        assert_eq!(rewrite_expanded_attrs(src, &attribs), None);
    }
}
