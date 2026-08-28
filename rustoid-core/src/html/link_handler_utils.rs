//! LinkHandlerUtils — faithful port of PHP Parsoid's
//! `src/Html2Wt/LinkHandlerUtils.php`.
//!
//! Serializes link markup (`[[…]]`, `[…]`, autolinks, magic links, media).
//! This module is being ported bottom-up: the `LinkData` structure and the
//! self-contained leaf helpers land first; the `getLinkRoundTripData` /
//! `serializeAsWikiLink` / `serializeAsExtLink` / `linkHandler` orchestrators
//! follow as their `data-mw` / shadow-attribute dependencies land.

use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::env::SerializerEnv;
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
}
