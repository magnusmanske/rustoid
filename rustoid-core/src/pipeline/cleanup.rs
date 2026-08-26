//! `CleanUp` — DOM-level cleanup pass, a faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Handlers/CleanUp.php`.
//!
//! Currently this implements the empty-element handling (`handleEmptyElements`):
//! "flagged" empty elements (those listed in `Consts::$Output['FlaggedEmptyElts']`
//! — `p`, `li`, `tbody`, `tr`) that contain only whitespace, comments, and
//! rendering-transparent nodes are marked with the `mw-empty-elt` class so that
//! wikis can style/hide them, and empty `mw-empty-elt` wrapper spans are removed.
//!
//! This mirrors the empty-`<p>` handling in Parsoid's `CleanUp` pass, which runs
//! after tree building and p-wrapping.

use crate::dom::node::{Node, NodeKind};
use crate::html::wts_utils::element_tag;

/// The "flagged empty elements" the cleanup pass inspects (`Consts::$Output['FlaggedEmptyElts']`).
fn flagged_empty_elts() -> &'static [&'static str] {
    &["li", "tbody", "tr", "p"]
}

fn has_class(node: &Node, class: &str) -> bool {
    node.get_attr("class")
        .is_some_and(|c| c.split_whitespace().any(|t| t == class))
}

/// Whether a node is "rendering transparent" (a comment, or a meta/link that
/// produces no visible output). Mirrors `WTUtils::isRenderingTransparentNode`
/// for the cases reachable here (`meta`, `link`, comments; annotations and
/// `mw:DOMFragment` wrappers are excluded).
fn is_rendering_transparent(node: &Node) -> bool {
    if matches!(node.kind, NodeKind::Comment(_)) {
        return true;
    }
    if let NodeKind::Element(kind) = &node.kind {
        let tag = element_tag(kind);
        let typeof_attr = node.get_attr("typeof").unwrap_or("");
        let has_excluded_typeof = typeof_attr
            .split_whitespace()
            .any(|t| t.starts_with("mw:Annotation/") || t == "mw:DOMFragment");
        if has_excluded_typeof {
            return false;
        }
        if tag == "meta" {
            return node
                .data_parsoid
                .as_deref()
                .is_none_or(|dp| !dp.contains("\"stx\":\"html\""));
        }
        // Rendering-transparent links (e.g. category/redirect/PageProp links) are
        // not exercised here; kept minimal to match the p/li/tr cases.
    }
    false
}

/// Whether a node is a nowiki or DOMFragment wrapper (whose contents should be
/// unwrapped when checking for emptiness).
fn is_nowiki_or_dom_fragment(node: &Node) -> bool {
    node.get_attr("typeof").is_some_and(|ty| {
        ty.split_whitespace()
            .any(|t| t == "mw:Nowiki" || t == "mw:DOMFragment")
    })
}

/// Whether a node carries the parsoid-added `wrapper` temp flag.
fn is_wrapper(node: &Node) -> bool {
    node.data_parsoid
        .as_deref()
        .is_some_and(|dp| dp.contains("\"wrapper\":true"))
}

/// Mirrors `CleanUp::isEmptyNode`: return true if `node` has only comments,
/// whitespace text, rendering-transparent nodes, nowiki/DOM-fragment wrappers
/// wrapping empty content, and flagged empty elements wrapping empty content.
fn is_empty_node(node: &Node, has_rt_nodes: &mut bool) -> bool {
    for child in &node.children {
        match &child.kind {
            NodeKind::Comment(_) => continue,
            NodeKind::Text(s) => {
                if !s.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
                    return false;
                }
            }
            NodeKind::Element(kind) => {
                let tag = element_tag(kind);
                if flagged_empty_elts().contains(&tag.as_str()) {
                    if is_empty_node(child, has_rt_nodes) {
                        continue;
                    }
                    return false;
                }
                if is_rendering_transparent(child) || has_class(child, "mw-empty-elt") {
                    *has_rt_nodes = true;
                    continue;
                }
                if (is_nowiki_or_dom_fragment(child) || is_wrapper(child))
                    && is_empty_node(child, has_rt_nodes)
                {
                    continue;
                }
                return false;
            }
            NodeKind::Document => return false,
        }
    }
    true
}

/// Whether a node is a "first encapsulation wrapper" (carries `about` + a
/// transclusion/extension `typeof`). Used to avoid deleting a wrapper that
/// anchors an about-chain.
fn is_first_encapsulation_wrapper(node: &Node) -> bool {
    node.get_attr("about").is_some()
        && node.get_attr("typeof").is_some_and(|ty| {
            ty.split_whitespace().any(|t| {
                t == "mw:Transclusion"
                    || t == "mw:Param"
                    || t.starts_with("mw:Transclusion/")
                    || t.starts_with("mw:Extension/")
            })
        })
}

/// Handle an empty element, adding the `mw-empty-elt` class or removing
/// deletable `mw-empty-elt` spans. Mirrors `CleanUp::handleEmptyElements`.
fn handle_empty_element(node: &mut Node) {
    let tag = match &node.kind {
        NodeKind::Element(kind) => element_tag(kind),
        _ => return,
    };

    // Remove deletable `mw-empty-elt` wrapper spans (those which are empty, or
    // carry only a single IEW child), unless they anchor an about-chain.
    if tag == "span" && has_class(node, "mw-empty-elt") {
        if is_first_encapsulation_wrapper(node) {
            return;
        }
        let deletable = node.children.is_empty()
            || (node.children.len() == 1
                && matches!(&node.children[0].kind, NodeKind::Text(t) if t.trim().is_empty()));
        if deletable {
            node.children.clear();
        }
        return;
    }

    if !flagged_empty_elts().contains(&tag.as_str()) {
        return;
    }

    let mut has_rt_nodes = false;
    if !is_empty_node(node, &mut has_rt_nodes) {
        return;
    }

    // After removing the empty-element class, a flagged element is only
    // "empty" (and hence marked) if it carries no meaningful attributes.
    // For `<p>` this mirrors the legacy parser: an empty `<p>` with only
    // `data-parsoid`/`stx` (parsoid-added) attributes is still markable.
    for attr in &node.attrs {
        if attr.key != "data-parsoid" && attr.key != "stx" {
            return;
        }
    }

    // Add the `mw-empty-elt` class (merging with any existing `class`).
    let existing = node.get_attr("class").map(str::to_string);
    let merged = match existing {
        Some(c) if !c.split_whitespace().any(|t| t == "mw-empty-elt") => {
            format!("{c} mw-empty-elt")
        }
        Some(c) => c,
        None => "mw-empty-elt".to_string(),
    };
    node.set_attr("class", merged);
}

/// Run the `CleanUp` empty-element pass over the document.
pub fn run(root: &mut Node) {
    handle_empty_element(root);
    for child in &mut root.children {
        run(child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::ElementKind;

    #[test]
    fn test_empty_p_gets_mw_empty_elt() {
        let mut p = Node::element(ElementKind::Paragraph);
        // Whitespace-only child text (IEW) does not prevent emptiness.
        p.push_child(Node::text("  \n "));

        let mut root = Node::document();
        root.push_child(p);
        run(&mut root);

        assert_eq!(
            root.children[0].get_attr("class"),
            Some("mw-empty-elt"),
            "got: {:?}",
            root.children[0]
        );
    }

    #[test]
    fn test_nonempty_p_untouched() {
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("hello"));

        let mut root = Node::document();
        root.push_child(p);
        run(&mut root);

        assert_eq!(root.children[0].get_attr("class"), None);
    }

    #[test]
    fn test_empty_p_with_comment_and_render_transparent() {
        // A comment and a rendering-transparent meta do not make the p non-empty.
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::comment("c"));

        let mut root = Node::document();
        root.push_child(p);
        run(&mut root);

        assert_eq!(root.children[0].get_attr("class"), Some("mw-empty-elt"));
    }
}
