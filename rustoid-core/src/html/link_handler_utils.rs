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
            let lt = target.value.clone();
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

/// `figureToConstrainedText` — assemble the `[[File:resource|…]]` wikitext for a
/// media element. Faithful to `LinkHandlerUtils::figureToConstrainedText` for the
/// core resource + caption + basic format options; the localized media-option
/// aliases (`img_*`/`timedmedia_*` magic words) and `data-mw` option list are
/// deferred (they need per-wiki magic-word config not yet on the `SiteConfig`).
pub fn figure_to_constrained_text(
    state: &mut SerializerState,
    tree: &DomTree,
    env: &SerializerEnv,
    _node: NodeId,
    ms: &crate::html::media_structure::MediaStructure,
) -> crate::html::constrained_text::ConstrainedText {
    let outer_elt = ms.container_elt;
    let media_elt = ms.media_elt;

    // The resource (file title): from `resource`, or fp/File:filename from `src`.
    let resource = ms
        .get_resource(tree)
        .or_else(|| {
            tree.node(media_elt)
                .get_attr("src")
                .map(|s| format!("File:{}", s.rsplit('/').next().unwrap_or("")))
        })
        .unwrap_or_default();

    // The caption text (from the figcaption element).
    let caption = ms
        .caption_elt
        .map(|c| {
            crate::html::serializer_state::SerializerState::serialize_caption_children_to_string(
                state,
                tree,
                c,
                Some(Box::new(move |_s, text, _o, _t| {
                    crate::html::wikitext_escape_handlers::media_option_handler(text)
                })),
            )
        })
        .unwrap_or_default();

    // Assemble `[[File:resource|caption]]` (or just `[[File:resource]]`).
    let wt = if caption.is_empty() {
        format!("[[{resource}]]")
    } else {
        format!("[[{resource}|{caption}]]")
    };

    crate::html::constrained_text::ConstrainedText::wiki_link(
        wt,
        outer_elt,
        false,
        None,
        env.get_site_config()
            .link_trail_regex()
            .and_then(|t| regex::Regex::new(t).ok()),
    )
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
