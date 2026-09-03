//! LinkHandlerUtils — faithful port of PHP Parsoid's
//! `src/Html2Wt/LinkHandlerUtils.php`.
//!
//! Serializes link markup (`[[…]]`, `[…]`, autolinks, magic links, media).
//! This module is being ported bottom-up: the `LinkData` structure and the
//! self-contained leaf helpers land first; the `getLinkRoundTripData` /
//! `serializeAsWikiLink` / `serializeAsExtLink` / `linkHandler` orchestrators
//! follow as their `data-mw` / shadow-attribute dependencies land.

use crate::html::dom_handler::DomHandler;
use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::env::SerializerEnv;
use crate::html::serializer_state::SerializerState;
use crate::html::wts_utils::ShadowInfo;

/// The resolved round-trip data for a link (mirrors PHP's `$rtData`/`$linkData`
/// `stdClass` produced by `getLinkRoundTripData`).
#[derive(Debug, Clone, Default)]
pub struct LinkData {
    /// `mw:WikiLink`, `mw:ExtLink`, `mw:MediaLink`, `mw:PageProp/…`, or null.
    pub link_type: Option<String>,
    /// The original href (from `getHref`).
    pub orig_href: String,
    /// The href with `./`/`../` prefixes stripped.
    pub href: String,
    /// Shadow info for the `href` attribute (`{ value, modified, fromsrc }`).
    pub target: ShadowInfo,
    /// Link tail source (`dp->tail`).
    pub tail: String,
    /// Link prefix source (`dp->prefix`).
    pub prefix: String,
    /// Plain-text content string (when the content is representable as such).
    pub content_string: Option<String>,
    /// The content node (a `NodeId`) when the content has element children.
    pub content_node: Option<NodeId>,
    /// Is this an interwiki link?
    pub is_interwiki: bool,
    /// Is this an interlanguage link?
    pub is_interwiki_lang: bool,
    /// Is this a local (same-wiki) link?
    pub is_local: bool,
    /// Is this a redirect?
    pub is_redirect: bool,
}

/// Split a link content string into its content/prefix/tail parts, using the
/// `DataParsoid` `prefix`/`tail`. Faithful to `splitLinkContentString`.
pub fn split_link_content_string(
    content_string: &str,
    prefix: &str,
    tail: &str,
) -> (String, String, String) {
    let mut content = content_string.to_string();
    let tail_len = tail.len();
    if tail_len > 0 && content.ends_with(tail) {
        let n = content.len() - tail_len;
        content.truncate(n);
    }
    let prefix_len = prefix.len();
    if prefix_len > 0 && content.starts_with(prefix) {
        content = content[prefix_len..].to_string();
    }
    (content, prefix.to_string(), tail.to_string())
}

/// `normalizeIWP` — strip a leading `:` and lower-case + trim an interwiki prefix.
pub fn normalize_iwp(s: &str) -> String {
    s.trim().trim_start_matches(':').to_lowercase()
}

/// `getHref` — the `href` attribute (protocol-less absolute URLs are left as-is;
/// the interwiki base-resolution in PHP is too config-dependent to port here and
/// returns the raw `href`).
pub fn get_href(tree: &DomTree, node: NodeId) -> String {
    tree.node(node).get_attr("href").unwrap_or("").to_string()
}

/// `getContentString` — the plain text content of a node, if representable as
/// such (all children are text or `mw:DisplaySpace`). Faithful to
/// `LinkHandlerUtils::getContentString` (diff markers are ignored).
pub fn get_content_string(tree: &DomTree, node: NodeId) -> Option<String> {
    // PHP: `!$node->hasChildNodes()` → null.
    let mut child = tree.first_child(node)?;
    let mut out = String::new();
    loop {
        match &tree.node(child).kind {
            crate::dom::node::NodeKind::Text(t) => out.push_str(t),
            crate::dom::node::NodeKind::Element(_) => {
                // `mw:DisplaySpace` → ' '; anything else is not plain text.
                if crate::html::dom_utils::has_type_of(tree.node(child), "mw:DisplaySpace") {
                    out.push(' ');
                } else {
                    return None;
                }
            }
            crate::dom::node::NodeKind::Comment(_) => {
                // Diff markers are ignored (selser; `is_diff_marker` is stubbed
                // false, so comments here are ordinary comments → not plain text).
                return None;
            }
            _ => return None,
        }
        match tree.next_sibling(child) {
            Some(next) => child = next,
            None => break,
        }
    }
    Some(out)
}

/// `escapeExtLinkURL` — percent-encode already-encoded, but not wikitext-safe,
/// characters in an external URL. Faithful to `LinkHandlerUtils::escapeExtLinkURL`
/// (the trailing `-{` / IPv6-bracket entity-decode cases are approximated).
pub fn escape_ext_link_url(url_str: &str) -> String {
    let mut out = String::with_capacity(url_str.len());
    for c in url_str.chars() {
        // Encode characters that would terminate/merge with wikitext ext-link
        // syntax: the `EXT_LINK_URL_CLASS` negation minus `[`, `]`, `<`, `>`,
        // `"`, spaces, and control chars.
        if matches!(
            c,
            '[' | ']' | '<' | '>' | '"' | ' ' | '\u{00}'..='\u{20}' | '\u{7F}' | '\u{00A0}'
                | '\u{1680}' | '\u{180E}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}'
                | '\u{3000}'
        ) || (c == '-' && url_str.contains("{"))
        {
            out.push_str(&entity_encode_char(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// Entity-encode a single char as `&#xNN;` (PHP's `Utils::entityEncodeAll`).
fn entity_encode_char(c: char) -> String {
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    let mut out = String::with_capacity(8);
    for b in s.bytes() {
        out.push_str(&format!("&#x{b:02X};"));
    }
    out
}

/// `addColonEscape` — prepend a `:` to a wikilink target when it is a category/
/// file link, an interlanguage link, or a slash-prefixed subpage (so it isn't
/// confused with those constructs). Faithful to `LinkHandlerUtils::addColonEscape`
/// (the `namespaceHasSubpages` check uses the context namespace conservatively).
pub fn add_colon_escape(env: &SerializerEnv, link_target: &str, link_data: &LinkData) -> String {
    let is_wikilink = link_data.link_type.as_deref() == Some("mw:WikiLink");
    if !is_wikilink || link_target.starts_with(':') {
        return link_target.to_string();
    }

    let link_title = env.make_title_from_text(link_target);
    let category_ns = env.get_site_config().canonical_namespace_id("category");
    let file_ns = env.get_site_config().canonical_namespace_id("file");
    let is_category_or_file =
        Some(link_title.namespace_id) == category_ns || Some(link_title.namespace_id) == file_ns;

    let has_subpages = true; // enwiki Main namespace has subpages (approximation).
    let is_slash_prefixed =
        link_target.starts_with('/') && link_data.href.starts_with('/') && has_subpages;

    if is_category_or_file || is_slash_prefixed || link_data.is_interwiki_lang {
        format!(":{link_target}")
    } else {
        link_target.to_string()
    }
}

/// `getLinkRoundTripData` — resolve everything the link serializer needs to
/// serialize a node as a link. Faithful to `LinkHandlerUtils::getLinkRoundTripData`
/// (the `mw:MediaLink` resource branch, the interwiki conversion block, and the
/// `isURLLink`/magic-link content checks are ported; the localized magic-link
/// ISBN branch is deferred).
pub fn get_link_round_trip_data(env: &SerializerEnv, tree: &DomTree, node: NodeId) -> LinkData {
    let dp = tree.node(node).dp.clone().unwrap_or_default();

    let mut data = LinkData {
        tail: dp.tail.clone().unwrap_or_default(),
        prefix: dp.prefix.clone().unwrap_or_default(),
        ..Default::default()
    };

    // Figure out the type of the link from the `rel` attribute.
    if let Some(rel) = tree.node(node).get_attr("rel")
        && let Some(ty) = match_link_rel(rel)
    {
        data.link_type = Some(ty.clone());
    }

    // Default type if nothing else set, and not a media element.
    if data.link_type.is_none() && crate::html::dom_utils::select_media_elt(tree, node).is_none() {
        data.link_type = Some("mw:ExtLink".to_string());
    }

    // Get href; strip leading `./`/`../` prefixes for the canonical form.
    data.orig_href = get_href(tree, node);
    data.href = strip_leading_dot(data.orig_href.clone());

    // WikiLinks should be relative; fix up absolute WikiLinks to ExtLinks.
    if data.link_type.as_deref() == Some("mw:WikiLink")
        && (data.href.starts_with("//")
            || data.href.starts_with("http://")
            || data.href.starts_with("https://")
            || data.orig_href.starts_with('/'))
    {
        data.link_type = Some("mw:ExtLink".to_string());
    }

    // The serialized target (shadow info) for `href`.
    data.target = crate::html::wts_utils::get_attribute_shadow_info(tree.node(node), "href");

    // Get the content string or (when not plain text) the content node.
    data.content_string = get_content_string(tree, node);
    let has_children = tree.first_child(node).is_some();
    if data.content_string.is_none() && has_children {
        data.content_node = Some(node);
    }

    // Redirect links.
    if data.link_type.as_deref() == Some("mw:PageProp/redirect")
        && data.content_string.is_none()
        && !has_children
    {
        data.is_redirect = true;
        data.prefix = dp.src.clone().unwrap_or_else(|| "#REDIRECT ".to_string());
    }

    // mw:MediaLink is authoritative; interwiki matches are not made for it.
    if data.link_type.as_deref() == Some("mw:MediaLink") {
        let resource =
            crate::html::wts_utils::get_attribute_shadow_info(tree.node(node), "resource");
        if resource.value.is_empty() {
            // Non-parsoid HTML: reconstruct resource from the href (File:filename).
            let file_name = data.orig_href.rsplit('/').next().unwrap_or("");
            data.target = ShadowInfo {
                value: format!("File:{file_name}"),
                modified: false,
                fromsrc: false,
            };
        } else {
            data.target = resource;
        }
        data.href = strip_leading_dot(data.target.value.clone());
        return data;
    }

    // Interwiki matching and conversion.
    if let Some((interwiki_key, interwiki_target)) =
        env.get_site_config().interwiki_matcher(&data.orig_href)
    {
        // External link that is really an interwiki link: convert it.
        if data.link_type.as_deref() == Some("mw:ExtLink") {
            data.link_type = Some("mw:WikiLink".to_string());
        }
        data.is_interwiki = true;
        // Is it a language link / local link?
        let iw_info = env
            .get_site_config()
            .interwiki_map_no_namespaces()
            .into_iter()
            .find(|(k, _)| normalize_iwp(k) == normalize_iwp(&interwiki_key));
        if let Some((_, info)) = iw_info {
            data.is_interwiki_lang = info.language.is_some();
            data.is_local = info.localinterwiki == Some(true);
        }
        data.target = ShadowInfo {
            value: interwiki_target,
            modified: !data.target.fromsrc,
            fromsrc: false,
        };
    }

    data
}

/// Match the `rel` attribute's first recognized `mw:(WikiLink|ExtLink|MediaLink|PageProp)…`
/// token, returning the full matched token (including subtype).
fn match_link_rel(rel: &str) -> Option<String> {
    for token in rel.split(' ').filter(|s| !s.is_empty()) {
        if token.starts_with("mw:WikiLink")
            || token.starts_with("mw:ExtLink")
            || token.starts_with("mw:MediaLink")
            || token.starts_with("mw:PageProp")
        {
            return Some(token.to_string());
        }
    }
    None
}

/// Strip leading `./`/`../` prefix sequences (PHP's `#^(../)+#`).
fn strip_leading_dot(s: String) -> String {
    let mut i = 0;
    loop {
        if s[i..].starts_with("./") {
            i += 2;
        } else if s[i..].starts_with("../") {
            i += 3;
        } else {
            break;
        }
    }
    s[i..].to_string()
}

/// `hasAutoUrlTerminatingChars` — does the URL end with a character that would
/// terminate a free external link? (The legacy parser's `makeFreeExternalLink`
/// set, approximated with the trailing-punctuation + URL-class terminators.)
fn has_auto_url_terminating_chars(url: &str) -> bool {
    match url.chars().last() {
        Some(c) => {
            matches!(c, ',' | ';' | '\\' | '.' | ':' | '!' | '?')
                || matches!(c, '[' | ']' | '<' | '>' | '"' | ' ')
        }
        None => false,
    }
}

/// `isURLLink` — can this link be serialized as a bare auto-URL link (with the
/// content being exactly the cleaned URL)? Faithful to `LinkHandlerUtils::isURLLink`
/// (the `cleanUrl` comparison uses `crate::sanitizer::clean_url`).
pub fn is_url_link(
    env: &SerializerEnv,
    tree: &DomTree,
    node: NodeId,
    link_data: &LinkData,
) -> bool {
    let target = &link_data.target;
    let content_str = get_content_string(tree, node);

    let Some(content_str) = content_str else {
        return false;
    };
    if content_str.is_empty() {
        return false;
    }

    let is_valid_protocol = |s: &str| env.get_site_config().has_valid_protocol(s);
    let clean_content =
        crate::sanitizer::clean_url(&content_str, "", is_valid_protocol).unwrap_or_default();
    let clean_href = crate::sanitizer::clean_url(&get_href(tree, node), "", is_valid_protocol)
        .unwrap_or_default();

    (target.value == clean_content || clean_href == clean_content)
        && !content_str.starts_with("//")
        && env.get_site_config().has_valid_protocol(&content_str)
        && !has_auto_url_terminating_chars(&content_str)
}

/// `serializeAsExtLink` — serialize a node as an external link (`[…]`, auto-URL,
/// or hash-only internal `[[#…]]`). Faithful to `LinkHandlerUtils::serializeAsExtLink`
/// (the `isURLLink` fast-path and the auto-numbered/anchor link forms).
pub fn serialize_as_ext_link(
    state: &mut SerializerState,
    tree: &DomTree,
    env: &SerializerEnv,
    node: NodeId,
    link_data: &LinkData,
) {
    let target = &link_data.target;
    let mut url_str = target.value.clone();
    if target.modified || !target.fromsrc {
        url_str = escape_ext_link_url(&url_str);
    }

    if is_url_link(env, tree, node, link_data) {
        let ct = crate::html::constrained_text::ConstrainedText::auto_url_link(url_str, node);
        state.push_to_curr_line(ct);
        state.on_sol = false;
        state.at_start_of_output = false;
        return;
    }

    let pure_hash_match = url_str.starts_with('#');

    // Serialize the content with the link-specific escaping handler.
    let content_str = if pure_hash_match {
        crate::html::serializer_state::SerializerState::serialize_link_children_to_string(
            state,
            tree,
            node,
            Some(Box::new(move |_s, text, _o, _t| {
                crate::html::wikitext_escape_handlers::wikilink_handler(text)
            })),
        )
    } else {
        crate::html::serializer_state::SerializerState::serialize_link_children_to_string(
            state,
            tree,
            node,
            Some(Box::new(move |_s, text, _o, _t| {
                crate::html::wikitext_escape_handlers::a_handler(text)
            })),
        )
    };

    let link_text = if pure_hash_match {
        format!(
            "[[{url_str}{}{}]]",
            if content_str.is_empty() { "" } else { "|" },
            content_str
        )
    } else {
        format!(
            "[{url_str}{}{}]",
            if content_str.is_empty() { "" } else { " " },
            content_str
        )
    };

    let ct = if pure_hash_match {
        crate::html::constrained_text::ConstrainedText::wiki_link(
            link_text, node, false, None, None,
        )
    } else {
        crate::html::constrained_text::ConstrainedText::ext_link(link_text, node)
    };
    state.push_to_curr_line(ct);
}

/// `isSimpleWikiLink` — can the link be serialized as a pipeless `[[Foo]]` (i.e.
/// the content string matches the target)? Faithful to
/// `LinkHandlerUtils::isSimpleWikiLink` (the `normalizedTitleKey`/
/// `resolveTitle` comparisons are approximated with exact/underscore-normalized
/// string equality).
pub fn is_simple_wiki_link(
    _env: &SerializerEnv,
    dp: &crate::wikitext::tokens_v2::DataParsoid,
    target: &ShadowInfo,
    link_data: &LinkData,
) -> bool {
    let Some(content_string) = link_data.content_string.as_ref() else {
        return false;
    };

    // Would need to pipe for any modified/non-preserved/minimal-piped content.
    if (target.modified || (dp.stx.as_deref() != Some("piped")))
        && !content_string.starts_with("./")
    {
        // Strip colon-escapes and leading `./`/(spaces before) from the target.
        let mut stripped = target.value.trim_start();
        stripped = stripped.strip_prefix(':').unwrap_or(stripped);
        stripped = stripped.strip_prefix("./").unwrap_or(stripped);
        // rtrim spaces (moved out of links by DOMNormalizer).
        let stripped = stripped.trim_end();

        let decoded_target = crate::html::wts_utils::decode_wt_entities_all(stripped);

        // Normalize content string and decoded target before comparison.
        let content_norm = content_string.replace('_', " ");
        let decoded_norm = decoded_target.replace('_', " ");

        // See if the (normalized) content matches the target, either directly
        // or wrapped in forward slashes (relative-link stripping).
        return content_norm == decoded_norm
            || format!("/{content_norm}/") == decoded_norm
            || content_norm == crate::util::decode_uri_component(&link_data.href);
    }

    false
}

/// `serializeAsWikiLink` — serialize a node as a wikilink (`[[…]]`), handling
/// redirects, simple links, auto-URL links, piped links, category links, and the
/// invalid-link fallback. Faithful to `LinkHandlerUtils::serializeAsWikiLink`
/// (the category sort-key and template-affected sort-key handling are deferred).
pub fn serialize_as_wiki_link(
    state: &mut SerializerState,
    tree: &DomTree,
    env: &SerializerEnv,
    node: NodeId,
    link_data: &LinkData,
) {
    let dp = tree.node(node).dp.clone().unwrap_or_default();
    let mut target = link_data.target.clone();
    let mut is_piped = false;
    let mut content_src: Option<String> = None;
    let mut needs_escaping = true;

    // Decode any link that did not come from source.
    if !target.fromsrc {
        let value = target.value.clone();
        let decoded = match value.find('#') {
            Some(pos) => format!(
                "{}{}",
                crate::util::decode_uri_component(&value[..pos]),
                &value[pos..]
            ),
            None => crate::util::decode_uri_component(&value),
        };
        target.value = decoded;
    }

    // `link_target` is always assigned in a non-early-return branch below; the
    // `None` seed is required to satisfy definite-assignment without an unsafe
    // or sentinel value (mirrors PHP's two-phase `$linkTarget = …` pattern).
    #[allow(unused_assignments)]
    let mut link_target: Option<String> = None;
    let mut escaped_tgt: Option<crate::html::wikitext_escape_handlers::EscapeLinkTargetResult> =
        None;

    if link_data.is_redirect {
        if target.modified || !target.fromsrc {
            // Strip leading `./`/`../`, replace `_`→` `, then escape.
            let mut lt = target.value.clone();
            lt = strip_leading_dot(lt);
            lt = lt.replace('_', " ");
            let escaped = crate::html::wikitext_escape_handlers::escape_link_target(env, &lt);
            escaped_tgt = Some(escaped);
            link_target = escaped_tgt.as_ref().map(|e| e.link_target.clone());
        } else {
            link_target = Some(target.value.clone());
        }
    } else if is_simple_wiki_link(env, &dp, &target, link_data) {
        // Simple case: `[[Foo]]`.
        link_target = Some(target.value.trim_start_matches("./").to_string());
    } else if is_url_link(env, tree, node, link_data) {
        let ct = crate::html::constrained_text::ConstrainedText::auto_url_link(
            target.value.clone(),
            node,
        );
        state.push_to_curr_line(ct);
        state.on_sol = false;
        state.at_start_of_output = false;
        return;
    } else {
        // Emit piped wikilink syntax.
        is_piped = true;

        if let Some(content_node) = link_data.content_node {
            let cs =
                crate::html::serializer_state::SerializerState::serialize_link_children_to_string(
                    state,
                    tree,
                    content_node,
                    Some(Box::new(move |_s, text, _o, _t| {
                        crate::html::wikitext_escape_handlers::wikilink_handler(text)
                    })),
                );
            content_src = Some(cs);
            needs_escaping = false;
        } else {
            content_src = link_data.content_string.clone();
            needs_escaping = true;
        }

        if content_src.as_deref() == Some("")
            && link_data.link_type.as_deref() != Some("mw:PageProp/Category")
        {
            // Protect empty link content from the PST pipe trick.
            content_src = Some("<nowiki/>".to_string());
            needs_escaping = false;
        }

        if target.modified || !target.fromsrc {
            let lt = target.value.clone();
            let escaped = crate::html::wikitext_escape_handlers::escape_link_target(env, &lt);
            escaped_tgt = Some(escaped);
            link_target = escaped_tgt.as_ref().map(|e| e.link_target.clone());
        } else {
            link_target = Some(target.value.clone());
        }
    }

    let link_target = link_target.unwrap_or_default();
    let escaped_tgt = escaped_tgt;
    // Redirect handling: buffer the redirect text if not at start-of-output.
    if link_data.is_redirect {
        if state.redirect_text.is_some() {
            return;
        }
        let so_far = format!("{}{}", state.out, state.curr_line.text);
        if !so_far.trim().is_empty() {
            state.redirect_text = Some(format!("{}[[{}]]", link_data.prefix, link_target));
            return;
        }
        state.redirect_text = Some("unbuffered".to_string());
    }

    if let Some(escaped) = &escaped_tgt
        && escaped.invalid_link
    {
        // Invalid link target: omit the link and serialize just the content.
        let piped_text = if is_piped {
            content_src.clone().unwrap_or_default()
        } else {
            link_target.clone()
        };
        state.needs_escaping = needs_escaping;
        let text = format!("{}{}{}", link_data.prefix, piped_text, link_data.tail);
        state.emit_chunk(text, node, tree);
    } else {
        let pipe = dp.first_pipe_src.clone().unwrap_or_else(|| "|".to_string());
        let piped_text = if is_piped && needs_escaping {
            format!(
                "{}{}",
                pipe,
                crate::html::wikitext_escape_handlers::escape_link_content(
                    state,
                    tree,
                    content_src.as_deref().unwrap_or(""),
                    false,
                    node,
                    false,
                )
            )
        } else if is_piped {
            format!("{}{}", pipe, content_src.unwrap_or_default())
        } else {
            String::new()
        };

        if is_piped {
            state.single_line_context.disable();
        }

        let wt = format!(
            "{}[[{}{}]]{}",
            link_data.prefix, link_target, piped_text, link_data.tail
        );
        let trail = env
            .get_site_config()
            .link_trail_regex()
            .and_then(|t| regex::Regex::new(t).ok());
        let no_trails = link_data
            .link_type
            .as_deref()
            .is_some_and(|t| t.starts_with("mw:PageProp/") || t == "mw:MediaLink");
        let greedy = !no_trails && !wt.ends_with(']');
        let ct = crate::html::constrained_text::ConstrainedText::wiki_link(
            wt, node, greedy, None, trail,
        );
        state.push_to_curr_line(ct);

        if is_piped {
            state.single_line_context.pop();
        }
    }
}

/// `linkHandler` — the top-level link dispatcher. Faithful to
/// `LinkHandlerUtils::linkHandler`: resolve round-trip data, handle magic links,
/// and dispatch to `serializeAsWikiLink`/`serializeAsExtLink` based on the link
/// type. (The complex-link / figure fallback is simplified to an extlink.)
pub fn link_handler(
    state: &mut SerializerState,
    tree: &DomTree,
    env: &SerializerEnv,
    node: NodeId,
) {
    let link_data = get_link_round_trip_data(env, tree, node);
    let link_type = link_data.link_type.clone();

    // Magic-link detection (RFC/PMID/ISBN).
    let orig_href = link_data.orig_href.clone();
    if let Some(matched) = env
        .get_site_config()
        .ext_resource_url_pattern_match(&crate::util::decode_uri_component(&orig_href))
    {
        // Round-trip PMIDs as interwikis if that's how they were originally.
        let pmid_as_iw = matched.0 == "PMID"
            && link_type.as_deref() == Some("mw:WikiLink")
            && crate::html::dom_utils::has_rel(tree.node(node), "mw:WikiLink/Interwiki");
        if !pmid_as_iw {
            let content_str =
                crate::html::serializer_state::SerializerState::serialize_link_children_to_string(
                    state,
                    tree,
                    node,
                    Some(Box::new(move |_s, text, _o, _t| {
                        crate::html::wikitext_escape_handlers::a_handler(text)
                    })),
                );
            let serialized =
                env.get_site_config()
                    .make_ext_resource_url(&matched, &orig_href, &content_str);
            if !serialized.starts_with('[') {
                let ct =
                    crate::html::constrained_text::ConstrainedText::magic_link(serialized, node);
                state.push_to_curr_line(ct);
                return;
            }
        }
    }

    if let Some(link_type) = &link_type {
        // [[..]] links (normal, category, redirect, lang) — except images.
        let is_wiki = link_type == "mw:WikiLink"
            || link_type == "mw:MediaLink"
            || (link_type.starts_with("mw:PageProp/"));
        if is_wiki {
            serialize_as_wiki_link(state, tree, env, node, &link_data);
            return;
        }
        if link_type == "mw:ExtLink" {
            serialize_as_ext_link(state, tree, env, node, &link_data);
            return;
        }
    }

    // No type/target info. Detect a basic-HTML figure: `<a><img/></a>` (or
    // `<a><audio/|video/></a>`), where the link wraps the media element.
    // Faithful to the `$isFigure` check in `LinkHandlerUtils::linkHandler`.
    let media = media_child_of_link(tree, node);
    if let Some(media_elt) = media {
        // `new MediaStructure($media, $node)` → container is the media elt
        // itself (no figure/span wrapper), the `<a>` is the link elt.
        let ms = crate::html::media_structure::MediaStructure {
            container_elt: media_elt,
            link_elt: Some(node),
            media_elt,
            caption_elt: None,
        };
        figure_handler(state, tree, env, node, Some(ms));
        return;
    }

    // No type/target info: serialize as a plain external link with escaped href.
    let href_str = escape_ext_link_url(&link_data.orig_href);
    let content_str =
        crate::html::serializer_state::SerializerState::serialize_link_children_to_string(
            state,
            tree,
            node,
            Some(Box::new(move |_s, text, _o, _t| {
                crate::html::wikitext_escape_handlers::a_handler(text)
            })),
        );
    let chunk = if href_str.is_empty() {
        content_str
    } else {
        format!("[{href_str} {content_str}]")
    };
    let ct = crate::html::constrained_text::ConstrainedText::ext_link(chunk, node);
    state.push_to_curr_line(ct);
}

/// `figureHandler` — serialize a `<figure>`/media node. Faithful to
/// `LinkHandlerUtils::figureHandler`: parse the `MediaStructure` and delegate to
/// `figure_to_constrained_text` (falling back to literal HTML on failure).
pub fn figure_handler(
    state: &mut SerializerState,
    tree: &DomTree,
    env: &SerializerEnv,
    node: NodeId,
    ms: Option<crate::html::media_structure::MediaStructure>,
) {
    let Some(ms) = ms else {
        let mut fallback = crate::html::handlers::FallbackHTMLHandler;
        fallback.handle(tree, node, state);
        return;
    };
    let ct = figure_to_constrained_text(state, tree, env, node, &ms);
    state.push_to_curr_line(ct);
}

/// The media element wrapped directly by a link element (the `<a><img/>`
/// basic-HTML figure shape). Returns the media `NodeId` when `node` is an
/// `<a>`/`<span>` whose first non-separator child is an `img`/`audio`/`video`.
/// Faithful to `DOMUtils::selectMediaElt` + the `$isFigure` parent check in
/// `LinkHandlerUtils::linkHandler`.
fn media_child_of_link(tree: &DomTree, node: NodeId) -> Option<NodeId> {
    let name = crate::html::dom_utils::node_name(tree.node(node));
    if name != "a" && name != "span" {
        return None;
    }
    let media = crate::html::dom_tree::first_non_sep_child(tree, node)?;
    let mname = crate::html::dom_utils::node_name(tree.node(media));
    if matches!(mname.as_str(), "img" | "audio" | "video" | "span") {
        Some(media)
    } else {
        None
    }
}

/// Shadow info for a media attribute — the `{ value, modified, fromsrc,
/// fromDataMW }` tuple PHP's `serializedImageAttrVal`/`serializedAttrVal` produce.
/// For basic (non-data-parsoid/html import) HTML all flags are `false` and
/// `value` is the raw attribute value (or `None` when absent).
#[derive(Debug, Clone, Default)]
struct Shadow {
    value: Option<String>,
    #[allow(dead_code)]
    modified: bool,
    fromsrc: bool,
    #[allow(dead_code)]
    from_data_mw: bool,
}

/// Read an attribute as shadow info (basic-HTML semantics: no data-parsoid, so
/// `value` is the attribute value and everything else is `false`).
fn attribute_shadow(tree: &DomTree, node: NodeId, key: &str) -> Shadow {
    Shadow {
        value: tree.node(node).get_attr(key).map(str::to_string),
        ..Default::default()
    }
}

/// The wikitext aliases for a canonical magic-word key (e.g. `img_link`),
/// preferring the suggested alias when present. Faithful to
/// `$mwAliases[$alias]` in `figureToConstrainedText`.
fn mw_aliases<'a>(
    env: &'a SerializerEnv,
    alias: &str,
) -> Option<&'a crate::traits::MagicWordEntry> {
    env.get_site_config().magic_words().get(alias)
}

/// A single assembled media option (`[[File:resource|opt…]]` argument) in the
/// internal `$nopts` order, before alias/ordering resolution. `ak` holds the
/// alias(es) (string form, possibly multi-alias preserved from data-mw);
/// `v` is the substituted `$1` value, `ck` the canonical key.
struct Nopt {
    #[allow(dead_code)]
    ck: String,
    ak: String,
    v: Option<String>,
}

/// `figureToConstrainedText` — assemble the `[[File:resource|…]]` wikitext for a
/// media element. Faithful port of `LinkHandlerUtils::figureToConstrainedText`.
pub fn figure_to_constrained_text(
    state: &mut SerializerState,
    tree: &DomTree,
    env: &SerializerEnv,
    _node: NodeId,
    ms: &crate::html::media_structure::MediaStructure,
) -> crate::html::constrained_text::ConstrainedText {
    let outer_elt = ms.container_elt;
    let link_elt = ms.link_elt;
    let media_elt = ms.media_elt;
    let caption_elt = ms.caption_elt;
    let format = crate::html::wts_utils::get_media_format(tree.node(outer_elt));
    let is_img = crate::html::dom_utils::node_name(tree.node(media_elt)) == "img";

    // Try to identify the local title for this image (from `resource`, else
    // reconstruct from `src`, stripping the `.`/`..` relative prefix).
    let mut resource: Shadow = attribute_shadow(tree, media_elt, "resource");
    if resource.value.is_none() {
        let src = tree.node(media_elt).get_attr("src").unwrap_or("");
        if src.is_empty() {
            // No `resource`/`src`: nothing to serialize (PHP returns null).
            return crate::html::constrained_text::ConstrainedText::cast("", outer_elt);
        }
        // External image link (the `https?://` case) — emit as an autolink.
        if src.starts_with("https:") || src.starts_with("http:") {
            return crate::html::constrained_text::ConstrainedText::auto_url_link(src, outer_elt);
        }
        resource.value = Some(src.to_string());
    }
    if let Some(v) = &mut resource.value
        && !resource.fromsrc
    {
        *v = strip_dot_prefix(v);
    }
    let resource_value = resource.value.clone().unwrap_or_default();

    // Reconstruct the caption.
    let caption = caption_elt.map(|c| {
        crate::html::serializer_state::SerializerState::serialize_caption_children_to_string(
            state,
            tree,
            c,
            Some(Box::new(move |_s, text, _o, _t| {
                crate::html::wikitext_escape_handlers::media_option_handler(text)
            })),
        )
    });

    // Identify the link target.
    let mut link: Option<Shadow> = None;
    if let Some(l) = link_elt
        && tree.node(l).get_attr("href").is_some()
    {
        let mut lk = attribute_shadow(tree, l, "href");
        if !lk.fromsrc {
            // Strip page/lang parameters from the href.
            let stripped = strip_page_lang(tree.node(l).get_attr("href").unwrap_or(""));
            if stripped == tree.node(media_elt).get_attr("resource").unwrap_or("") {
                // default link: same place as resource
                lk = resource.clone();
            }
            if let Some(v) = &mut lk.value {
                *v = strip_dot_prefix(v);
            }
        }
        link = Some(lk);
    }
    if link.is_none() {
        // Otherwise, just try and get it from `href` on the outer elt.
        let h = attribute_shadow(tree, outer_elt, "href");
        if h.value.is_some() {
            link = Some(h);
        }
    }

    // Fetch the alt / lang / muted / loop.
    let alt = attribute_shadow(tree, media_elt, "alt");
    let lang = attribute_shadow(tree, media_elt, "lang");
    let muted = attribute_shadow(tree, media_elt, "muted");
    let loop_attr = attribute_shadow(tree, media_elt, "loop");

    // Determine whether an explicit `link=` is needed.
    let mut link_cond = is_img;
    if link_cond
        && let Some(lk) = &link
        && let Some(lv) = &lk.value
    {
        let link_title = env.normalized_title_key(&crate::util::decode_uri_component(lv), true);
        let resource_title =
            env.normalized_title_key(&crate::util::decode_uri_component(&resource_value), true);
        if *lv == resource_value || (link_title.is_some() && link_title == resource_title) {
            link_cond = false;
        }
    }

    let alt_cond = alt.value.is_some() && is_img;

    // Assemble the initial set of options (link, alt, lang, muted, loop).
    let mut nopts: Vec<Nopt> = Vec::new();
    {
        let mut push_simple = |ck: &str, shadow: &Shadow, cond: bool, alias: &str| {
            if !cond {
                return;
            }
            let ak = aliases_first(env, alias);
            if shadow.fromsrc {
                let v = shadow.value.clone().unwrap_or_default();
                nopts.push(Nopt {
                    ck: ck.to_string(),
                    ak: v,
                    v: None,
                });
            } else {
                let mut value = shadow.value.clone().unwrap_or_default();
                if ck == "link" || ck == "alt" {
                    value = crate::html::wikitext_escape_handlers::escape_link_content(
                        state, tree, &value, false, outer_elt, true,
                    );
                }
                nopts.push(Nopt {
                    ck: ck.to_string(),
                    ak,
                    v: Some(value),
                });
            }
        };
        push_simple("link", &link.unwrap_or_default(), link_cond, "img_link");
        push_simple("alt", &alt, alt_cond, "img_alt");
        push_simple("lang", &lang, lang.value.is_some(), "img_lang");
        push_simple("muted", &muted, muted.value.is_some(), "timedmedia_muted");
        push_simple(
            "loop",
            &loop_attr,
            loop_attr.value.is_some(),
            "timedmedia_loop",
        );
    }

    // Handle the class-derived options (halign/valign/border + extra classes).
    let classes: Vec<String> = tree
        .node(outer_elt)
        .get_attr("class")
        .map(|c| c.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let mut extra: Vec<String> = Vec::new();
    for c in &classes {
        match c.as_str() {
            "mw-halign-none" | "mw-halign-right" | "mw-halign-left" | "mw-halign-center" => {
                let val = &c[10..]; // strip mw-halign-
                let ak = aliases_first(env, &format!("img_{val}"));
                nopts.push(Nopt {
                    ck: val.to_string(),
                    ak,
                    v: None,
                });
            }
            "mw-valign-top"
            | "mw-valign-middle"
            | "mw-valign-baseline"
            | "mw-valign-sub"
            | "mw-valign-super"
            | "mw-valign-text-top"
            | "mw-valign-bottom"
            | "mw-valign-text-bottom" => {
                let val = c[10..].replace('-', "_");
                let ak = aliases_first(env, &format!("img_{val}"));
                nopts.push(Nopt {
                    ck: val,
                    ak,
                    v: None,
                });
            }
            "mw-image-border" => {
                let ak = aliases_first(env, "img_border");
                nopts.push(Nopt {
                    ck: "border".to_string(),
                    ak,
                    v: None,
                });
            }
            "mw-default-size" | "mw-default-audio-height" => { /* handled below */ }
            _ => extra.push(c.clone()),
        }
    }
    if !extra.is_empty() {
        let ak = aliases_first(env, "img_class");
        nopts.push(Nopt {
            ck: "class".to_string(),
            ak,
            v: Some(extra.join(" ")),
        });
    }

    // Format option (from `typeof` suffix).
    let has_manualthumb = false; // no data-mw in basic HTML
    match format.as_str() {
        "Thumb" => {
            let ak = aliases_first(env, "img_thumbnail");
            nopts.push(Nopt {
                ck: "thumbnail".to_string(),
                ak,
                v: None,
            });
        }
        "Frame" => {
            let ak = aliases_first(env, "img_framed");
            nopts.push(Nopt {
                ck: "framed".to_string(),
                ak,
                v: None,
            });
        }
        "Frameless" => {
            let ak = aliases_first(env, "img_frameless");
            nopts.push(Nopt {
                ck: "frameless".to_string(),
                ak,
                v: None,
            });
        }
        _ => {}
    }

    // Size options.
    let is_redlink = ms.is_red_link(tree);
    let wh = attribute_shadow(
        tree,
        media_elt,
        if is_redlink { "data-height" } else { "height" },
    );
    let ww = attribute_shadow(
        tree,
        media_elt,
        if is_redlink { "data-width" } else { "width" },
    );

    let size_unmodified = ww.from_data_mw || (!ww.modified && !wh.modified);
    let has_default_size = classes.iter().any(|c| c == "mw-default-size");
    let is_audio = crate::html::dom_utils::node_name(tree.node(media_elt)) == "audio";
    let has_default_audio_height = classes.iter().any(|c| c == "mw-default-audio-height");

    if !has_default_size && format != "Frame" && !has_manualthumb {
        let size_string = String::new(); // no data-mw optList to recover from
        if size_unmodified && !size_string.is_empty() {
            // preserve original width/height string (n/a in basic HTML)
        } else {
            let mut bbox: Option<i64> = None;
            if let Some(v) = &ww.value
                && let Ok(n) = leading_int(v)
            {
                bbox = Some(n);
            }
            if let Some(v) = &wh.value
                && let Ok(h) = leading_int(v)
                && !(is_audio && has_default_audio_height)
                && bbox.is_none_or(|b| h > b)
            {
                bbox = Some(h);
            }
            if let Some(bbox) = bbox {
                let ak = aliases_first(env, "img_width");
                nopts.push(Nopt {
                    ck: "width".to_string(),
                    ak,
                    v: Some(format!("{bbox}x{bbox}")),
                });
            }
        }
    }

    // Put the caption last, by default.
    if let Some(caption) = &caption
        && !caption.is_empty()
    {
        nopts.push(Nopt {
            ck: "caption".to_string(),
            ak: caption.clone(),
            v: None,
        });
    }

    // Emit all the options in order.
    let mut wikitext = format!("[[{resource_value}");
    for o in &nopts {
        wikitext.push('|');
        if let Some(v) = &o.v {
            wikitext.push_str(&o.ak.replace("$1", v));
        } else {
            wikitext.push_str(&o.ak);
        }
    }
    wikitext.push_str("]]");

    crate::html::constrained_text::ConstrainedText::wiki_link(
        wikitext,
        outer_elt,
        false,
        None,
        env.get_site_config()
            .link_trail_regex()
            .and_then(|t| regex::Regex::new(t).ok()),
    )
}

/// Strip the `./`/`../` relative-prefix segments from a title string, faithful
/// to `preg_replace('#^(\\.\\.?/)+#', '', $value, 1)`.
fn strip_dot_prefix(s: &str) -> String {
    let mut rest = s;
    loop {
        let mut changed = false;
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
            changed = true;
        } else if let Some(r) = rest.strip_prefix("../") {
            rest = r;
            changed = true;
        }
        if !changed {
            break;
        }
    }
    rest.to_string()
}

/// Strip a trailing `?page=NNN` or `?lang=xx` query from an href, faithful to
/// `preg_replace('#[?]((?:page=\\d+)|(?:lang=[a-z]+(?:-[a-z]+)*))$#Di', '', $href)`.
fn strip_page_lang(href: &str) -> String {
    if let Some(pos) = href.rfind('?') {
        let query = &href[pos + 1..];
        let is_page = query
            .strip_prefix("page=")
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()));
        let is_lang = query.strip_prefix("lang=").is_some_and(|seg| {
            !seg.is_empty()
                && seg
                    .split('-')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase()))
        });
        if is_page || is_lang {
            return href[..pos].to_string();
        }
    }
    href.to_string()
}

/// The first alias for a canonical magic-word key (the preferred English form),
/// or a bare fallback when the key is not configured. Faithful to
/// `$mwAliases[$alias][0]` (with `img_width` falling back to `$1px`).
fn aliases_first(env: &SerializerEnv, alias: &str) -> String {
    if let Some(entry) = mw_aliases(env, alias)
        && let Some(first) = entry.aliases.first()
    {
        return first.clone();
    }
    // Fallback aliases so unconfigured wikis still produce sane output.
    match alias {
        "img_link" => "link=$1".to_string(),
        "img_alt" => "alt=$1".to_string(),
        "img_lang" => "lang=$1".to_string(),
        "img_width" => "$1px".to_string(),
        "img_class" => "class=$1".to_string(),
        "timedmedia_muted" => "muted".to_string(),
        "timedmedia_loop" => "loop".to_string(),
        other => other.to_string(),
    }
}

/// The leading decimal integer of a size string (e.g. `"100"` or `"10px"` → 100).
/// Faithful to `preg_match('/^\\d+/', $value)` + `intval`.
fn leading_int(s: &str) -> Result<i64, ()> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        Err(())
    } else {
        digits.parse::<i64>().map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_split_link_content_string() {
        let (c, p, t) = split_link_content_string("prefixFOOtail", "prefix", "tail");
        assert_eq!(c, "FOO");
        assert_eq!(p, "prefix");
        assert_eq!(t, "tail");
        // No prefix/tail → unchanged.
        let (c, _, _) = split_link_content_string("FOO", "", "");
        assert_eq!(c, "FOO");
    }

    #[test]
    fn test_normalize_iwp() {
        assert_eq!(normalize_iwp(":EN"), "en");
        assert_eq!(normalize_iwp("W"), "w");
    }

    #[test]
    fn test_get_content_string() {
        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.push_child(Node::text("hello"));
        let tree = DomTree::new(a);
        // Walk the text child of the `<a>` root.
        assert_eq!(
            get_content_string(&tree, tree.root()),
            Some("hello".to_string())
        );
    }

    #[test]
    fn test_escape_ext_link_url() {
        // `[` and `]` become their entity-encoded form (protected chars).
        let out = escape_ext_link_url("http://a/[b]");
        assert!(out.contains("&#x5B;"));
        assert!(out.contains("&#x5D;"));
    }

    #[test]
    fn test_add_colon_escape_category() {
        let config = crate::mock::MockSiteConfig::new();
        let ctitle = crate::title::Title::new_main("Test");
        let env = SerializerEnv::new(&config, &ctitle);
        let ld = LinkData {
            link_type: Some("mw:WikiLink".to_string()),
            ..Default::default()
        };
        assert_eq!(
            add_colon_escape(&env, "Category:People", &ld),
            ":Category:People"
        );
        // Non-category wikilink target is left alone.
        assert_eq!(add_colon_escape(&env, "Foo", &ld), "Foo");
    }

    #[test]
    fn test_get_link_round_trip_data_wikilink() {
        let config = crate::mock::MockSiteConfig::new();
        let ctitle = crate::title::Title::new_main("Test");
        let env = SerializerEnv::new(&config, &ctitle);

        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("rel", "mw:WikiLink");
        a.set_attr("href", "./Foo_Bar");
        a.push_child(Node::text("Foo Bar"));
        let tree = DomTree::new(a);
        let a_id = tree.root();

        let data = get_link_round_trip_data(&env, &tree, a_id);
        assert_eq!(data.link_type.as_deref(), Some("mw:WikiLink"));
        assert_eq!(data.href, "Foo_Bar");
        assert_eq!(data.content_string.as_deref(), Some("Foo Bar"));
    }

    #[test]
    fn test_serialize_as_ext_link() {
        let config = crate::mock::MockSiteConfig::new();
        let ctitle = crate::title::Title::new_main("Test");
        let env = SerializerEnv::new(&config, &ctitle);

        // A bracketed extlink `[https://example.com content]`.
        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("rel", "mw:ExtLink");
        a.set_attr("href", "https://example.com");
        a.push_child(Node::text("content"));
        let tree = DomTree::new(a);
        let a_id = tree.root();

        let data = get_link_round_trip_data(&env, &tree, a_id);
        let mut state = SerializerState::new();
        serialize_as_ext_link(&mut state, &tree, &env, a_id, &data);
        state.flush_line();
        assert_eq!(state.out, "[https://example.com content]");
    }

    #[test]
    fn test_is_url_link() {
        let config = crate::mock::MockSiteConfig::new();
        let ctitle = crate::title::Title::new_main("Test");
        let env = SerializerEnv::new(&config, &ctitle);

        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("rel", "mw:ExtLink");
        a.set_attr("href", "https://example.com");
        a.push_child(Node::text("https://example.com"));
        let tree = DomTree::new(a);
        let a_id = tree.root();
        let data = get_link_round_trip_data(&env, &tree, a_id);
        assert!(is_url_link(&env, &tree, a_id, &data));
    }

    #[test]
    fn test_serialize_as_wiki_link_simple() {
        let config = crate::mock::MockSiteConfig::new();
        let ctitle = crate::title::Title::new_main("Test");
        let env = SerializerEnv::new(&config, &ctitle);

        // <a rel="mw:WikiLink" href="./Foo">Foo</a> → [[Foo]].
        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("rel", "mw:WikiLink");
        a.set_attr("href", "./Foo");
        a.push_child(Node::text("Foo"));
        let tree = DomTree::new(a);
        let a_id = tree.root();
        let data = get_link_round_trip_data(&env, &tree, a_id);

        let mut state = SerializerState::new();
        serialize_as_wiki_link(&mut state, &tree, &env, a_id, &data);
        state.flush_line();
        assert_eq!(state.out, "[[Foo]]");
    }

    #[test]
    fn test_link_handler_wikilink() {
        let config = crate::mock::MockSiteConfig::new();
        let ctitle = crate::title::Title::new_main("Test");
        let env = SerializerEnv::new(&config, &ctitle);

        // A simple wikilink round-trips via `link_handler`.
        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("rel", "mw:WikiLink");
        a.set_attr("href", "./Foo");
        a.push_child(Node::text("Foo"));
        let tree = DomTree::new(a);
        let a_id = tree.root();

        let mut state = SerializerState::new();
        link_handler(&mut state, &tree, &env, a_id);
        state.flush_line();
        assert_eq!(state.out, "[[Foo]]");
    }
}
