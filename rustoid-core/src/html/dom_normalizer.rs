//! DOM normalization (pre-serialization quote-tag minimization).
//!
//! Faithful port of the quote-tag parts of PHP Parsoid's
//! `Html2Wt\DOMNormalizer`. This runs *before* html2wt serialization and:
//!
//! - strips essentially-empty quote/heading tags (`stripIfEmpty`), and
//! - minimizes adjacent rewriteable `<i>`/`<b>` sibling pairs (`merge`/`swap`).
//!
//! The remaining normalizer responsibilities (link hoisting, `<td>` prefix
//! spaces, `<p></p>` → `<br/>`, bidi stripping, `<font>` unwrapping, and the
//! selser diff-marker bookkeeping) are layered on as needed; this module covers
//! the high-value, round-trip-correct quote minimization that most affects
//! wikitext fidelity.

use crate::dom::node::{Node, NodeKind};

/// DOM normalization over the owned `Node` tree.
pub fn normalize(root: &mut Node) {
    process_children(&mut root.children);
}

/// Assert that a node is essentially empty (no element children, no non-space
/// text, no comments), excluding diff markers. Faithful to
/// `DiffDOMUtils::nodeEssentiallyEmpty($node, false)`.
fn essentially_empty(node: &Node) -> bool {
    for child in node.children.iter() {
        match &child.kind {
            NodeKind::Element(_) => {
                if !crate::html::diff_utils::DiffUtils::is_diff_marker(child, None) {
                    return false;
                }
            }
            NodeKind::Text(t) => {
                if !t.trim_matches([' ', '\t']).is_empty() {
                    return false;
                }
            }
            NodeKind::Comment(_) => return false,
            NodeKind::Document => return false,
        }
    }
    true
}

/// Whether a node is a quote tag (`<i>`/`<b>`).
fn is_quote_tag(node: &Node) -> bool {
    crate::html::wts_utils::is_quote_elt(node)
}

/// Move `b`'s children into `a` (the sibling-pair merge in
/// `DOMNormalizer::merge`). Returns nothing; `b` is emptied and removed by the
/// caller.
fn merge(a: &mut Node, b: &mut Node) {
    let b_children = std::mem::take(&mut b.children);
    // Migrate any intermediate diff markers already handled by the caller; here
    // we simply append b's children to a's.
    a.children.extend(b_children);
}

/// Process the children of `parent` in order, applying sibling-pair
/// minimization and recursively normalizing each child's subtree.
fn process_children(children: &mut Vec<Node>) {
    let mut i = 0;
    while i < children.len() {
        // Recurse into this child's subtree first (post-order).
        process_children(&mut children[i].children);

        // If this child is an essentially-empty quote tag, strip it.
        if is_quote_tag(&children[i]) && essentially_empty(&children[i]) {
            children.remove(i);
            continue;
        }

        // Try to minimize this child with its next sibling.
        if i + 1 < children.len() {
            // Recurse into the next sibling's subtree first.
            process_children(&mut children[i + 1].children);

            let a = &children[i];
            let b = &children[i + 1];
            if rewriteable_pair(a, b) {
                if mergable(a, b) {
                    // Merge b into a.
                    let mut b = children.remove(i + 1);
                    merge(&mut children[i], &mut b);
                    // Recurse into the merged node's children.
                    process_children(&mut children[i].children);
                } else if swappable(a, b) {
                    // swap(a, a.firstChild) then merge with b.
                    let mut b = children.remove(i + 1);
                    swap_first_child(&mut children[i]);
                    merge(&mut children[i], &mut b);
                    process_children(&mut children[i].children);
                } else if swappable(b, a) {
                    // swap(b, b.firstChild) then merge a with it.
                    let mut b = children.remove(i + 1);
                    swap_first_child(&mut b);
                    merge(&mut children[i], &mut b);
                    process_children(&mut children[i].children);
                }
            }
        }

        i += 1;
    }
}

/// `DOMNormalizer::rewriteablePair` for quote tags: both must be quote tags.
fn rewriteable_pair(a: &Node, b: &Node) -> bool {
    is_quote_tag(a) && is_quote_tag(b)
}

/// `DOMNormalizer::mergable` — same node name and "similar" (for quote tags,
/// both non-literal-HTML, or both literal-HTML with equal attributes).
fn mergable(a: &Node, b: &Node) -> bool {
    crate::html::wts_utils::node_name(a) == crate::html::wts_utils::node_name(b) && similar(a, b)
}

/// `DOMNormalizer::similar` for quote tags: both non-HTML (plain wiki quote) or
/// both HTML with equal attributes.
fn similar(a: &Node, b: &Node) -> bool {
    let a_html = crate::html::wts_utils::is_literal_html_node(a);
    let b_html = crate::html::wts_utils::is_literal_html_node(b);
    if !a_html && !b_html {
        return true;
    }
    a_html && b_html && a.attrs == b.attrs
}

/// `DOMNormalizer::swappable` — `a` has exactly one non-diff-marker child, and
/// that child is similar to `a` and mergable with `b`.
fn swappable(a: &Node, b: &Node) -> bool {
    let first = a
        .children
        .iter()
        .find(|c| !crate::html::diff_utils::DiffUtils::is_diff_marker(c, None));
    match first {
        Some(fc) => similar(a, fc) && mergable(fc, b),
        None => false,
    }
}

/// Swap a node with its single first child (the `DOMNormalizer::swap` core):
/// make the child the parent and the parent the child's child.
fn swap_first_child(a: &mut Node) {
    if a.children.is_empty() {
        return;
    }
    // a becomes the content wrapped by its first child: we take the first child
    // out, move a's remaining children into it, and replace a's identity.
    let first = a.children.remove(0);
    let remaining = std::mem::take(&mut a.children);
    // The original a (now with the first child's content) is appended to first.
    let mut a_content = std::mem::replace(a, first);
    a_content.children = remaining;
    a.push_child(a_content);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::ElementKind;

    fn i(content: Node) -> Node {
        let mut n = Node::element(ElementKind::Italic);
        n.push_child(content);
        n
    }
    fn b(content: Node) -> Node {
        let mut n = Node::element(ElementKind::Bold);
        n.push_child(content);
        n
    }

    #[test]
    fn test_strips_empty_quote_tag() {
        // <i>foo</i><b></b> → the empty <b></b> is stripped.
        let mut doc = Node::document();
        doc.push_child(i(Node::text("foo")));
        doc.push_child(b(Node::text("")));
        normalize(&mut doc);
        assert_eq!(doc.children.len(), 1);
    }

    #[test]
    fn test_merges_adjacent_i() {
        // <i>x</i><i>y</i> → <i>xy</i>.
        let mut doc = Node::document();
        doc.push_child(i(Node::text("x")));
        doc.push_child(i(Node::text("y")));
        normalize(&mut doc);
        assert_eq!(doc.children.len(), 1);
        let text: String = doc.children[0]
            .children
            .iter()
            .filter_map(|c| match &c.kind {
                NodeKind::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "xy");
    }

    #[test]
    fn test_essentially_empty() {
        assert!(essentially_empty(&Node::element(ElementKind::Bold)));
        assert!(!essentially_empty(&i(Node::text("x"))));
        let mut with_comment = Node::element(ElementKind::Bold);
        with_comment.push_child(Node::comment("c"));
        assert!(!essentially_empty(&with_comment));
    }
}
