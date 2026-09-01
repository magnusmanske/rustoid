//! DOM navigation arena for html2wt serialization.
//!
//! Faithful port of the DOM-navigation primitives the PHP `WikitextSerializer`
//! relies on (`DOMNode` parent/sibling links and `DiffDOMUtils`). The
//! serializer walks the tree imperatively via `nextSibling`, `previousSibling`,
//! `parentNode`, `previousNonSepSibling`, `nextNonSepSibling`, etc., none of
//! which our owned `Node` tree exposes.
//!
//! Rather than thread parent pointers through the entire wt2html pipeline (a
//! high-risk change to the hot parse path), we build a **read-only navigation
//! index** at the serializer boundary: a single document-order flattening that
//! assigns each node a stable `NodeId` and records its parent, first/last
//! child, and previous/next sibling. Serialization then navigates by stable id,
//! exactly mirroring the DOM relationship semantics the PHP code assumes.

use crate::dom::node::{Node, NodeKind};

/// Stable index of a node within a [`DomTree`]. Indices are assigned in
/// document order; `0` is always the root.
pub type NodeId = usize;

/// Per-node navigation links within a [`DomTree`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NodeInfo {
    pub parent: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
}

/// A read-only, document-order navigation index over an owned `Node` tree.
///
/// Nodes are stored flat (`nodes`), keyed by `NodeId`; structural relationships
/// are captured in `info`. This mirrors the parent/sibling links a DOM exposes
/// and lets serializer code traverse the tree without borrow-checker gymnastics.
pub struct DomTree {
    /// Document-order nodes; `nodes[0]` is the root.
    pub nodes: Vec<Node>,
    /// Navigation links, parallel to `nodes`.
    pub info: Vec<NodeInfo>,
}

impl DomTree {
    /// Build a navigation index over `root` (which becomes `nodes[0]`).
    pub fn new(root: Node) -> Self {
        let mut nodes = Vec::new();
        let mut info = Vec::new();
        Self::build(root, None, &mut nodes, &mut info);
        Self { nodes, info }
    }

    /// Recursively flatten `node` into `nodes`/`info`, returning its assigned id.
    /// `parent` is the id of the enclosing node (the previous sibling's next
    /// link is patched up here so sibling lists stay linked).
    fn build(
        node: Node,
        parent: Option<NodeId>,
        nodes: &mut Vec<Node>,
        info: &mut Vec<NodeInfo>,
    ) -> NodeId {
        let id = nodes.len();
        // Take the children out cheaply so we can recurse without cloning the
        // whole (remaining) subtree. Navigation uses the `info` links, so the
        // stored `nodes[id].children` being empty is irrelevant.
        let mut node = node;
        let children = std::mem::take(&mut node.children);
        nodes.push(node);
        info.push(NodeInfo {
            parent,
            ..NodeInfo::default()
        });

        if let Some(pid) = parent {
            // Link this node into its parent's sibling chain.
            if let Some(prev_last) = info[pid].last_child {
                info[prev_last].next_sibling = Some(id);
                info[id].prev_sibling = Some(prev_last);
            } else {
                info[pid].first_child = Some(id);
            }
            info[pid].last_child = Some(id);
        }

        for child in children {
            Self::build(child, Some(id), nodes, info);
        }

        id
    }

    // -----------------------------------------------------------------------
    // Basic DOM navigation (mirrors DOMNode accessors).
    // -----------------------------------------------------------------------

    /// The root node's id (always `0`).
    pub fn root(&self) -> NodeId {
        0
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.info[id].parent
    }

    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.info[id].first_child
    }

    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.info[id].last_child
    }

    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.info[id].prev_sibling
    }

    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.info[id].next_sibling
    }

    /// The HTML tag/construct name of a node, used by predicate helpers.
    /// Mirrors the coarse `DOMUtils::nodeName` need of `DiffDOMUtils`.
    pub fn node_name(&self, id: NodeId) -> NodeName<'_> {
        match &self.nodes[id].kind {
            NodeKind::Document => NodeName::Document,
            NodeKind::Text(_) => NodeName::Text,
            NodeKind::Comment(_) => NodeName::Comment,
            NodeKind::Element(kind) => NodeName::Element(kind),
        }
    }
}

/// A cheap, borrow-free view of a node's kind for navigation predicates.
#[derive(Debug, Clone, Copy)]
pub enum NodeName<'a> {
    Document,
    Element(&'a crate::dom::node::ElementKind),
    Text,
    Comment,
}

/// Whether a node is "inter-element whitespace" (a text node whose content is
/// only whitespace), mirroring `DOMUtils::isIEW`. Such nodes are separators and
/// are skipped by the `*NonSep*` navigation helpers.
pub fn is_iew(tree: &DomTree, id: NodeId) -> bool {
    matches!(&tree.nodes[id].kind, NodeKind::Text(t) if t.trim().is_empty())
}

/// Whether a node is a diff marker (a `meta`/`mw:DiffMarker/…` element).
/// Diff markers are only introduced in selser mode via `DiffUtils`; for pure
/// html2wt they never appear.
pub fn is_diff_marker(tree: &DomTree, id: NodeId) -> bool {
    crate::html::diff_utils::DiffUtils::is_diff_marker(tree.node(id), None)
}

/// Is a node a "content" node (not a comment, not IEW, not a diff marker)?
/// Mirrors `DiffDOMUtils::isContentNode`.
pub fn is_content_node(tree: &DomTree, id: NodeId) -> bool {
    !matches!(tree.node_name(id), NodeName::Comment)
        && !is_iew(tree, id)
        && !is_diff_marker(tree, id)
}

/// First child element or non-IEW text node, ignoring whitespace-only text
/// nodes, comments, and (in selser) deleted nodes. `DiffDOMUtils::firstNonSepChild`.
pub fn first_non_sep_child(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    let mut child = tree.first_child(id);
    while let Some(c) = child {
        if is_content_node(tree, c) {
            return Some(c);
        }
        child = tree.next_sibling(c);
    }
    None
}

/// Last child element or non-IEW text node. `DiffDOMUtils::lastNonSepChild`.
pub fn last_non_sep_child(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    let mut child = tree.last_child(id);
    while let Some(c) = child {
        if is_content_node(tree, c) {
            return Some(c);
        }
        child = tree.prev_sibling(c);
    }
    None
}

/// Previous non-separator sibling. `DiffDOMUtils::previousNonSepSibling`.
pub fn previous_non_sep_sibling(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    let mut prev = tree.prev_sibling(id);
    while let Some(p) = prev {
        if is_content_node(tree, p) {
            return Some(p);
        }
        prev = tree.prev_sibling(p);
    }
    None
}

/// Next non-separator sibling. `DiffDOMUtils::nextNonSepSibling`.
pub fn next_non_sep_sibling(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    let mut next = tree.next_sibling(id);
    while let Some(n) = next {
        if is_content_node(tree, n) {
            return Some(n);
        }
        next = tree.next_sibling(n);
    }
    None
}

/// Number of non-(diff-marker) children. `DiffDOMUtils::numNonDeletedChildNodes`.
pub fn num_non_deleted_child_nodes(tree: &DomTree, id: NodeId) -> usize {
    let mut n = 0;
    let mut child = tree.first_child(id);
    while let Some(c) = child {
        if !is_diff_marker(tree, c) {
            n += 1;
        }
        child = tree.next_sibling(c);
    }
    n
}

/// Whether `id` has exactly `n` non-diff-marker children.
/// `DiffDOMUtils::hasNChildren`.
pub fn has_n_children(tree: &DomTree, id: NodeId, n: usize) -> bool {
    let mut remaining = n;
    let mut child = tree.first_child(id);
    while let Some(c) = child {
        if !is_diff_marker(tree, c) {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
        }
        child = tree.next_sibling(c);
    }
    remaining == 0
}

/// First non-deleted child. `DiffDOMUtils::firstNonDeletedChild` — skips only
/// diff markers (not separators).
pub fn first_non_deleted_child(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    let mut child = tree.first_child(id);
    while let Some(c) = child {
        if !is_diff_marker(tree, c) {
            return Some(c);
        }
        child = tree.next_sibling(c);
    }
    None
}

/// Last non-deleted child. `DiffDOMUtils::lastNonDeletedChild`.
pub fn last_non_deleted_child(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    let mut child = tree.last_child(id);
    while let Some(c) = child {
        if !is_diff_marker(tree, c) {
            return Some(c);
        }
        child = tree.prev_sibling(c);
    }
    None
}

/// Next non-deleted sibling. `DiffDOMUtils::nextNonDeletedSibling`.
pub fn next_non_deleted_sibling(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    let mut sib = tree.next_sibling(id);
    while let Some(s) = sib {
        if !is_diff_marker(tree, s) {
            return Some(s);
        }
        sib = tree.next_sibling(s);
    }
    None
}

/// Previous non-deleted sibling. `DiffDOMUtils::previousNonDeletedSibling`.
pub fn previous_non_deleted_sibling(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    let mut sib = tree.prev_sibling(id);
    while let Some(s) = sib {
        if !is_diff_marker(tree, s) {
            return Some(s);
        }
        sib = tree.prev_sibling(s);
    }
    None
}

/// Concatenated text of all descendant text nodes (document order). Mirrors
/// the `textContent` accessor PHP's `currWikitextLineHasBlockNode` relies on.
pub fn text_content(tree: &DomTree, id: NodeId) -> String {
    let mut out = String::new();
    fn collect(tree: &DomTree, id: NodeId, out: &mut String) {
        if let NodeKind::Text(t) = &tree.node(id).kind {
            out.push_str(t);
        }
        let mut child = tree.first_child(id);
        while let Some(c) = child {
            collect(tree, c, out);
            child = tree.next_sibling(c);
        }
    }
    collect(tree, id, &mut out);
    out
}

/// Is `ancestor` an ancestor of (or equal to) `id`? Mirrors `DOMUtils::isAncestorOf`.
pub fn is_ancestor_of(tree: &DomTree, ancestor: NodeId, id: NodeId) -> bool {
    let mut cur = tree.parent(id);
    while let Some(p) = cur {
        if p == ancestor {
            return true;
        }
        cur = tree.parent(p);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    fn build_doc() -> DomTree {
        // <p>a</p> <p>b</p>
        //
        // Root (0)
        //  ├─ P (1) "a"
        //  │   └─ Text "a" (2)
        //  ├─ IEW "\n" (3)
        //  └─ P (4) "b"
        //      └─ Text "b" (5)
        let mut doc = Node::document();
        let mut p1 = Node::element(ElementKind::Paragraph);
        p1.push_child(Node::text("a"));
        doc.push_child(p1);
        doc.push_child(Node::text("\n"));
        let mut p2 = Node::element(ElementKind::Paragraph);
        p2.push_child(Node::text("b"));
        doc.push_child(p2);
        DomTree::new(doc)
    }

    #[test]
    fn test_parent_and_sibling_links() {
        let tree = build_doc();
        // root has three children: P(1), IEW(3), P(4).
        assert_eq!(tree.first_child(tree.root()), Some(1));
        assert_eq!(tree.last_child(tree.root()), Some(4));
        let p1 = 1;
        let iew = 3;
        let p2 = 4;
        assert_eq!(tree.parent(p1), Some(0));
        assert_eq!(tree.parent(p2), Some(0));
        assert_eq!(tree.next_sibling(p1), Some(iew));
        assert_eq!(tree.next_sibling(iew), Some(p2));
        assert_eq!(tree.prev_sibling(p2), Some(iew));
        // And the depths: Text "a" (2) is p1's child; Text "b" (5) is p2's.
        assert_eq!(tree.parent(2), Some(p1));
        assert_eq!(tree.parent(5), Some(p2));
    }

    #[test]
    fn test_non_sep_navigation() {
        let tree = build_doc();
        let p1 = 1;
        let p2 = 4;
        // The IEW node between the two <p>s is skipped.
        assert_eq!(next_non_sep_sibling(&tree, p1), Some(p2));
        assert_eq!(previous_non_sep_sibling(&tree, p2), Some(p1));
        // Root has three children (one is IEW).
        assert_eq!(num_non_deleted_child_nodes(&tree, tree.root()), 3);
        assert!(has_n_children(&tree, tree.root(), 3));
        assert_eq!(first_non_sep_child(&tree, tree.root()), Some(p1));
        assert_eq!(last_non_sep_child(&tree, tree.root()), Some(p2));
    }

    #[test]
    fn test_is_iew() {
        let tree = build_doc();
        assert!(is_iew(&tree, 3)); // "\n"
        assert!(!is_iew(&tree, 1)); // <p>
    }

    #[test]
    fn test_text_content_and_is_ancestor_of() {
        let tree = build_doc();
        // <p>a</p> concatenates its text descendant "a".
        assert_eq!(text_content(&tree, 1), "a");
        // Root text content is "a\nb".
        assert_eq!(text_content(&tree, 0), "a\nb");
        // is_ancestor_of: root is an ancestor of p1 and text "a"; p1 of text "a".
        assert!(is_ancestor_of(&tree, 0, 1));
        assert!(is_ancestor_of(&tree, 1, 2));
        assert!(!is_ancestor_of(&tree, 1, 4)); // p1 is not an ancestor of p2
        assert!(!is_ancestor_of(&tree, 2, 1)); // text "a" is not an ancestor of p1
    }
}
