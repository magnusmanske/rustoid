//! AddLinkAttributes — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Handlers/AddLinkAttributes.php`.
//!
//! Adds classes and extra attributes to external links (`rel="mw:ExtLink"` →
//! `class="external text|free|autonumber"` and `rel="nofollow"`) and interwiki
//! links (`rel="mw:WikiLink/Interwiki"` → `class="extiw"`), after tree building
//! and before serialization.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::traits::SiteConfig;

/// Add link attributes to every `<a>` element in the subtree rooted at `node`.
///
/// Mirrors the `DTState`-driven traversal in PHP that runs
/// `AddLinkAttributes::handler` over each `<a>` element. The simplified AST
/// keeps the link's `rel` as a plain attribute on the node, so this pass
/// mutates those attributes directly.
pub fn run(node: &mut Node, config: &dyn SiteConfig) {
    // Recurse first (PHP's DOMTraverser visits children after the handler).
    for child in &mut node.children {
        run(child, config);
    }
    if let NodeKind::Element(kind) = &node.kind {
        match kind {
            ElementKind::ExtLink => add_external_link_attrs(node, config),
            ElementKind::Wikilink => add_wikilink_attrs(node),
            _ => {}
        }
    }
}

fn rel_tokens(node: &Node) -> Vec<String> {
    node.get_attr("rel")
        .map(|r| r.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

fn has_rel(node: &Node, token: &str) -> bool {
    rel_tokens(node).iter().any(|t| t == token)
}

/// Add a token to the node's `rel` attribute (space-separated, de-duplicated).
pub(crate) fn add_rel(node: &mut Node, token: &str) {
    let mut tokens = rel_tokens(node);
    if !tokens.iter().any(|t| t == token) {
        tokens.push(token.to_string());
    }
    node.set_attr("rel", tokens.join(" "));
}

/// Add a token to the node's `class` attribute, preserving existing classes.
pub(crate) fn add_class(node: &mut Node, class: &str) {
    let mut classes: Vec<String> = node
        .get_attr("class")
        .map(|c| c.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if !classes.iter().any(|c| c == class) {
        classes.push(class.to_string());
    }
    node.set_attr("class", classes.join(" "));
}

/// Extract the `stx` field from a node's serialized `data-parsoid` JSON blob.
/// Returns `None` when the node has no `data-parsoid` or no `stx` field.
fn data_parsoid_stx(node: &Node) -> Option<String> {
    let dp = node.data_parsoid.as_deref()?;
    // The serializer wraps primitive values; `stx` appears as `"stx":"..."`.
    let marker = "\"stx\":\"";
    let start = dp.find(marker)? + marker.len();
    let rest = &dp[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// `AddLinkAttributes::handler` for an external (`rel="mw:ExtLink"`) link.
fn add_external_link_attrs(node: &mut Node, config: &dyn SiteConfig) {
    if !has_rel(node, "mw:ExtLink") {
        return;
    }

    // Class assignment mirrors PHP: unbracketed URL links (`stx: 'url'`) get
    // "external free" (handled by magic-link check below), bracketed links get
    // "external text", and links without content get "external autonumber".
    // Magic links (`stx: 'magiclink'`) set no class here (handled separately).
    let stx = data_parsoid_stx(node);
    let class_info_text = match stx.as_deref() {
        Some("url") => Some("external free"),
        Some("magiclink") => None,
        _ => {
            if node.children.is_empty() {
                Some("external autonumber")
            } else {
                Some("external text")
            }
        }
    };
    if let Some(class) = class_info_text {
        add_class(node, class);
    }

    let href = node.get_attr("href").unwrap_or("").to_string();
    for (key, values) in config.external_link_attribs(&href) {
        if key == "rel" {
            for v in &values {
                add_rel(node, v);
            }
        } else if key == "class" {
            for v in &values {
                add_class(node, v);
            }
        } else {
            node.set_attr(key, values.join(" "));
        }
    }
}

/// `AddLinkAttributes::handler` for an internal (`rel="mw:WikiLink"`) link.
fn add_wikilink_attrs(node: &mut Node) {
    if has_rel(node, "mw:WikiLink/Interwiki") {
        add_class(node, "extiw");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    fn extlink() -> Node {
        let mut n = Node::element(ElementKind::ExtLink);
        n.set_attr("rel", "mw:ExtLink");
        n.set_attr("href", "https://example.com");
        n.push_child(Node::text("example"));
        n
    }

    #[test]
    fn test_external_link_adds_nofollow_and_class() {
        let mut doc = Node::document();
        doc.push_child(extlink());
        let config = MockSiteConfig::new();
        run(&mut doc, &config);
        let a = &doc.children[0];
        assert_eq!(a.get_attr("rel"), Some("mw:ExtLink nofollow"));
        assert_eq!(a.get_attr("class"), Some("external text"));
    }

    #[test]
    fn test_external_link_autonumber_class() {
        let mut n = Node::element(ElementKind::ExtLink);
        n.set_attr("rel", "mw:ExtLink");
        n.set_attr("href", "https://example.com");
        let mut doc = Node::document();
        doc.push_child(n);
        let config = MockSiteConfig::new();
        run(&mut doc, &config);
        assert_eq!(
            doc.children[0].get_attr("class"),
            Some("external autonumber")
        );
    }

    #[test]
    fn test_interwiki_link_adds_extiw() {
        let mut n = Node::element(ElementKind::Wikilink);
        n.set_attr("rel", "mw:WikiLink/Interwiki");
        n.set_attr("href", "https://en.wikipedia.org/wiki/X");
        let mut doc = Node::document();
        doc.push_child(n);
        let config = MockSiteConfig::new();
        run(&mut doc, &config);
        assert_eq!(doc.children[0].get_attr("class"), Some("extiw"));
    }
}
