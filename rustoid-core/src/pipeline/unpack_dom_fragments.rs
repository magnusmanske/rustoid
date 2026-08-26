//! UnpackDOMFragments — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Handlers/UnpackDOMFragments.php`.
//!
//! Placeholder elements carrying `typeof="mw:DOMFragment"` (and a stashed
//! sub-fragment in `Node.fragment`, mirroring PHP's `data-parsoid.html`) are
//! replaced by their content children during finalize. Typeof/about/data-mw
//! metadata on the placeholder is transferred to the fragment's first
//! (span-wrapped) child.

use crate::dom::node::Node;

/// Whether a node's `typeof` contains the `mw:DOMFragment` token.
fn has_dom_fragment_type(node: &Node) -> bool {
    node.get_attr("typeof")
        .is_some_and(|t| t.split_whitespace().any(|ty| ty == "mw:DOMFragment"))
}

/// Whether a node's `typeof` contains `mw:Transclusion`.
fn has_transclusion_type(node: &Node) -> bool {
    node.get_attr("typeof")
        .is_some_and(|t| t.split_whitespace().any(|ty| ty == "mw:Transclusion"))
}

/// Add whitespace-separated token(s) to a node's `typeof` attribute.
fn add_typeof(node: &mut Node, token: &str) {
    let mut tokens: Vec<String> = node
        .get_attr("typeof")
        .map(|t| t.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    for tok in token.split_whitespace() {
        if !tokens.iter().any(|t| t == tok) {
            tokens.push(tok.to_string());
        }
    }
    node.set_attr("typeof", tokens.join(" "));
}

/// Unpack every `mw:DOMFragment` placeholder in the subtree, replacing it in
/// place with its stashed fragment children (mirrors
/// `UnpackDOMFragments::handler` for the well-formed, non-fostered, non-badly-
/// nested cases).
pub fn run(node: &mut Node) {
    // Recurse into children first (the placeholder's own children are its
    // shallow wrapper plus stashed fragment, so recurse before unpacking).
    let mut new_children: Vec<Node> = Vec::with_capacity(node.children.len());
    for mut child in std::mem::take(&mut node.children) {
        run(&mut child);
        if has_dom_fragment_type(&child) {
            unpack_placeholder(node, &mut new_children, child);
        } else {
            new_children.push(child);
        }
    }
    node.children = new_children;
}

/// Replace a single `mw:DOMFragment` placeholder `placeholder` with its
/// fragment children, transferring metadata. `parent` is used only for span
/// wrapping of bare text children; `out` receives the replacement nodes.
fn unpack_placeholder(_parent: &Node, out: &mut Vec<Node>, mut placeholder: Node) {
    let Some(fragment) = placeholder.fragment.take() else {
        // No stashed fragment (should not happen for a real placeholder).
        return;
    };
    let mut fragment = *fragment;

    // First child receives the placeholder's `typeof`, `data-mw`, and (for a
    // transclusion) `data-parsoid` pi info. Mirrors the transclusion transfer
    // in PHP, which span-wraps bare text children first.
    let is_transclusion = has_transclusion_type(&placeholder);
    let about = placeholder.get_attr("about").map(str::to_string);
    let dmw = placeholder.data_mw.take();

    // Children of a fragment are already span-wrapped (or are lone elements).
    let mut kids = std::mem::take(&mut fragment.children);
    for (i, child) in kids.iter_mut().enumerate() {
        if i == 0 {
            // Transfer metadata from the placeholder onto the first child.
            if is_transclusion {
                add_typeof(child, "mw:Transclusion");
            }
            if let Some(d) = &dmw {
                child.data_mw = Some(d.clone());
            }
        }
        if let Some(ab) = &about {
            child.set_attr("about", ab.clone());
        }
    }

    if kids.is_empty() {
        out.push(fragment);
    } else {
        out.extend(kids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, NodeKind};

    fn fragment_placeholder(kind: ElementKind) -> Node {
        let mut n = Node::element(kind);
        n.set_attr("typeof", "mw:DOMFragment");
        n
    }

    #[test]
    fn test_unpacks_simple_fragment() {
        let mut frag = Node::document();
        frag.push_child(Node::text("hello"));
        // A placeholder carrying a text child via its fragment field.
        let mut ph = fragment_placeholder(ElementKind::Other("span".into()));
        ph.fragment = Some(Box::new(frag));

        let mut doc = Node::document();
        doc.push_child(ph);
        run(&mut doc);

        assert_eq!(doc.children.len(), 1);
        assert_eq!(doc.children[0].kind, NodeKind::Text("hello".into()));
    }

    #[test]
    fn test_transfers_about_and_typeof() {
        let mut frag = Node::document();
        let mut span = Node::element(ElementKind::Span);
        span.push_child(Node::text("x"));
        frag.push_child(span);

        let mut ph = fragment_placeholder(ElementKind::Other("span".into()));
        ph.set_attr("about", "#mwt1");
        add_typeof(&mut ph, "mw:Transclusion");
        ph.data_mw = Some("{\"name\":\"pre\"}".into());
        ph.fragment = Some(Box::new(frag));

        let mut doc = Node::document();
        doc.push_child(ph);
        run(&mut doc);

        let child = &doc.children[0];
        assert_eq!(child.get_attr("about"), Some("#mwt1"));
        assert!(
            child
                .get_attr("typeof")
                .unwrap()
                .contains("mw:Transclusion")
        );
        assert_eq!(child.data_mw.as_deref(), Some("{\"name\":\"pre\"}"));
    }

    #[test]
    fn test_non_fragment_passes_through() {
        let mut doc = Node::document();
        doc.push_child(Node::text("plain"));
        run(&mut doc);
        assert_eq!(doc.children.len(), 1);
        assert_eq!(doc.children[0].kind, NodeKind::Text("plain".into()));
    }
}
