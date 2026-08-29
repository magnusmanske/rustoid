//! DOM diff / annotation pass for selective serialization.
//!
//! Faithful port of PHP Parsoid's `Html2Wt\DOMDiff`. Compares an original DOM
//! (`node_a`) against an edited DOM (`node_b`) and annotates `node_b` in place
//! with `data-parsoid-diff` markers (and, for inserted/deleted non-element
//! content, `mw:DiffMarker/*` meta elements) so that the selser serializer can
//! reuse unmodified source and re-serialize the rest.
//!
//! This operates directly on the *owned* `Node` tree (with `children: Vec<Node>`),
//! not the read-only `DomTree` navigation arena, because it must *mutate* the
//! DOM (insert metas, set rich attributes). Parent/child navigation is done by
//! index within the sibling `Vec<Node>`.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::html::diff_utils::{DiffMarkers, DiffUtils};

/// Attributes ignored for equality purposes (faithful to
/// `DOMDiff::IGNORE_ATTRIBUTES`).
const IGNORE_ATTRIBUTES: [&str; 4] = [
    "data-parsoid-diff",
    "about",
    "data-object-id",
    "data-parsoid-serialize",
];

/// A DOM diff helper. `skip_encapsulated_content` mirrors
/// `DOMDiff::$skipEncapsulatedContent` (default `true`).
pub struct DomDiff {
    pub skip_encapsulated_content: bool,
}

impl Default for DomDiff {
    fn default() -> Self {
        Self {
            skip_encapsulated_content: true,
        }
    }
}

/// Whether two (owned) element nodes have equal attributes, using the diff's
/// ignore-list. Faithful to `DiffUtils::attribsEquals` with
/// `DOMDiff::IGNORE_ATTRIBUTES`.
fn attribs_equals(node_a: &Node, node_b: &Node) -> bool {
    let a = DiffUtils::get_attributes(node_a, &IGNORE_ATTRIBUTES);
    let b = DiffUtils::get_attributes(node_b, &IGNORE_ATTRIBUTES);
    a == b
}

/// `DiffDOMUtils::isContentNode` — not a comment, not IEW, not a diff marker.
fn is_content_node(node: &Node) -> bool {
    !matches!(node.kind, NodeKind::Comment(_))
        && !crate::html::dom_utils::is_iew(node)
        && !DiffUtils::is_diff_marker(node, None)
}

/// `WTUtils::isEncapsulationWrapper` — a first-encapsulation wrapper node. For
/// the diff's single-node use, this reduces to a `typeof` check (the backward
/// sibling walk is handled by skipping the whole encapsulated forest via
/// `skip_over_encapsulated_content_index`).
fn is_encapsulation_wrapper(node: &Node) -> bool {
    crate::html::wts_utils::is_first_encapsulation_wrapper_node(node)
}

/// Build a `<meta typeof="...">` diff-marker node (faithful to
/// `prependTypedMeta`'s meta creation).
fn diff_marker_meta(ty: &str) -> Node {
    let mut meta = Node::element(ElementKind::Other("meta".to_string()));
    meta.set_attr("typeof", ty);
    meta
}

/// Skip over the encapsulated forest (siblings sharing the leading node's
/// `about`), returning the index just past it. Faithful to
/// `WTUtils::skipOverEncapsulatedContent` + `getAboutSiblings`.
fn skip_over_encapsulated_content_index(children: &[Node], from: usize) -> Option<usize> {
    let Some(about) = children.get(from).and_then(|n| n.get_attr("about")) else {
        return (from + 1 < children.len()).then_some(from + 1);
    };
    let about = about.to_string();
    let mut i = from + 1;
    while i < children.len() {
        let same = children[i]
            .get_attr("about")
            .map(|a| a == about)
            .unwrap_or(false);
        if !same {
            break;
        }
        i += 1;
    }
    // Step past any trailing IEW.
    while i < children.len() && crate::html::dom_utils::is_iew(&children[i]) {
        i += 1;
    }
    Some(i)
}

impl DomDiff {
    /// `DOMDiff::nextAnalyzableSibling` — skip over encapsulated content when
    /// requested; returns the index of the next analyzable sibling.
    fn next_analyzable_sibling(&self, children: &[Node], from: usize) -> Option<usize> {
        let cur = children.get(from)?;
        if self.skip_encapsulated_content && is_encapsulation_wrapper(cur) {
            skip_over_encapsulated_content_index(children, from)
        } else {
            (from + 1 < children.len()).then_some(from + 1)
        }
    }

    /// `DOMDiff::diff` — diff two (rooted) DOMs, annotating `node_b` in place.
    /// Returns `true` when any change was found (the inverse of PHP's
    /// `isEmpty`).
    pub fn diff(&mut self, node_a: &Node, node_b: &mut Node) -> bool {
        self.do_dom_diff(node_a, node_b)
    }

    /// `DOMDiff::treeEquals` — shallow (`deep=false`) or deep equality of two
    /// nodes.
    pub fn tree_equals(&self, node_a: &Node, node_b: &Node, deep: bool) -> bool {
        if std::mem::discriminant(&node_a.kind) != std::mem::discriminant(&node_b.kind) {
            return false;
        }
        match (&node_a.kind, &node_b.kind) {
            (NodeKind::Text(a), NodeKind::Text(b)) => a == b,
            (NodeKind::Comment(a), NodeKind::Comment(b)) => {
                crate::html::wts_utils::decode_comment(a)
                    == crate::html::wts_utils::decode_comment(b)
            }
            (NodeKind::Element(_), NodeKind::Element(_))
            | (NodeKind::Document, NodeKind::Document) => {
                if !matches!(node_a.kind, NodeKind::Document)
                    && (crate::html::wts_utils::node_name(node_a)
                        != crate::html::wts_utils::node_name(node_b)
                        || !attribs_equals(node_a, node_b))
                {
                    return false;
                }
                if deep {
                    if node_a.children.len() != node_b.children.len() {
                        return false;
                    }
                    for (ca, cb) in node_a.children.iter().zip(&node_b.children) {
                        if !self.tree_equals(ca, cb, true) {
                            return false;
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// `DOMDiff::doDOMDiff` — diff the children of two parents, annotating
    /// `new_parent` (whose children change) in place. Returns `true` when the
    /// subtree differs.
    fn do_dom_diff(&mut self, base_parent: &Node, new_parent: &mut Node) -> bool {
        let base_children = base_parent.children.clone();
        let mut bi: usize = 0;
        let mut ni: usize = 0;
        let mut found_overall = false;

        while bi < base_children.len() && ni < new_parent.children.len() {
            let mut dont_advance_new = false;
            let base_node = &base_children[bi];
            let new_node = new_parent.children[ni].clone();

            if !self.tree_equals(base_node, &new_node, false) {
                let mut found_diff = false;
                let saved_new_node = new_node.clone();
                let saved_new_index = ni;

                // Look-ahead in the *new* DOM to detect insertions.
                if is_content_node(base_node) {
                    let mut lookahead = self.next_analyzable_sibling(&new_parent.children, ni);
                    while let Some(la) = lookahead {
                        let la_node = new_parent.children[la].clone();
                        if is_content_node(&la_node) && self.tree_equals(base_node, &la_node, true)
                        {
                            // Mark skipped-over nodes (ni .. la) as inserted, going
                            // right-to-left so preceding meta insertions don't shift
                            // indices still to be processed.
                            let mut mark = la;
                            while mark > ni {
                                mark -= 1;
                                let _ = self.mark_node(new_parent, mark, DiffMarkers::Inserted);
                            }
                            ni = la;
                            found_diff = true;
                            break;
                        }
                        lookahead = self.next_analyzable_sibling(&new_parent.children, la);
                    }
                }

                // Look-ahead in the *base* DOM to detect deletions.
                if !found_diff && is_content_node(&new_node) {
                    let mut lookahead = self.next_analyzable_sibling(&base_children, bi);
                    while let Some(la) = lookahead {
                        let la_node = &base_children[la];
                        if is_content_node(la_node) && self.tree_equals(la_node, &new_node, true) {
                            self.mark_node(new_parent, ni, DiffMarkers::Deleted);
                            bi = la;
                            found_diff = true;
                            break;
                        }
                        lookahead = self.next_analyzable_sibling(&base_children, la);
                    }
                }

                if !found_diff {
                    let saved_is_element = matches!(saved_new_node.kind, NodeKind::Element(_));
                    if !saved_is_element {
                        // Modified text/comment → mark deleted.
                        self.mark_node(new_parent, saved_new_index, DiffMarkers::Deleted);
                    } else if matches!(base_node.kind, NodeKind::Element(_))
                        && crate::html::wts_utils::node_name(&saved_new_node)
                            == crate::html::wts_utils::node_name(base_node)
                        && base_node.dp.as_ref().and_then(|d| d.stx.clone())
                            == saved_new_node.dp.as_ref().and_then(|d| d.stx.clone())
                    {
                        // Identical wrapper type but modified → MODIFIED_WRAPPER +
                        // recurse.
                        self.mark_node(new_parent, saved_new_index, DiffMarkers::ModifiedWrapper);
                        self.subtree_differs_at(base_node, new_parent, saved_new_index);
                    } else {
                        dont_advance_new = true;
                        self.mark_node(new_parent, saved_new_index, DiffMarkers::Deleted);
                    }
                }

                // Direct children changed in the parent → CHILDREN_CHANGED.
                DiffUtils::set_diff_mark(new_parent, DiffMarkers::ChildrenChanged);
                found_overall = true;
            } else if self.subtree_differs_at(base_node, new_parent, ni) {
                found_overall = true;
            }

            // Advance to the next pair (skipping over encapsulated content).
            if bi < base_children.len() && ni < new_parent.children.len() {
                bi = self
                    .next_analyzable_sibling(&base_children, bi)
                    .unwrap_or(base_children.len());
                if !dont_advance_new {
                    ni = self
                        .next_analyzable_sibling(&new_parent.children, ni)
                        .unwrap_or(new_parent.children.len());
                }
            }
        }

        // Mark trailing new nodes as inserted.
        while ni < new_parent.children.len() {
            ni = self.mark_node(new_parent, ni, DiffMarkers::Inserted);
            found_overall = true;
            ni = self
                .next_analyzable_sibling(&new_parent.children, ni.saturating_sub(1))
                .unwrap_or(new_parent.children.len());
        }

        // If there are extra base nodes, something was deleted.
        if bi < base_children.len() {
            DiffUtils::set_diff_mark(new_parent, DiffMarkers::ChildrenChanged);
            if !new_parent.children.is_empty() {
                let is_block =
                    crate::html::wts_utils::is_block_node_with_visible_wt(&base_children[bi]);
                let mut meta = diff_marker_meta("mw:DiffMarker/deleted");
                if is_block {
                    meta.set_attr("data-is-block", "true");
                }
                new_parent.children.push(meta);
            }
            found_overall = true;
        }

        found_overall
    }

    /// `DOMDiff::subtreeDiffers` — recursively diff `base_node` against the new
    /// child at `new_index` within `new_parent`, marking SUBTREE_CHANGED on the
    /// new node when the subtree differs.
    fn subtree_differs_at(
        &mut self,
        base_node: &Node,
        new_parent: &mut Node,
        new_index: usize,
    ) -> bool {
        let base_encapsulated = is_encapsulation_wrapper(base_node);
        let new_encapsulated = is_encapsulation_wrapper(&new_parent.children[new_index]);

        let subtree_differs = if !base_encapsulated && !new_encapsulated {
            // Recurse into the (owned) new child.
            let mut new_child = new_parent.children[new_index].clone();
            let changed = self.do_dom_diff(base_node, &mut new_child);
            new_parent.children[new_index] = new_child;
            changed
        } else if base_encapsulated && new_encapsulated {
            // Encapsulated content: we don't know about the subtree when
            // skipping encapsulated content.
            !self.skip_encapsulated_content
        } else {
            true
        };

        if subtree_differs {
            DiffUtils::set_diff_mark(
                &mut new_parent.children[new_index],
                DiffMarkers::SubtreeChanged,
            );
        }
        subtree_differs
    }

    /// Mark the node at `index` within `new_parent.children` with `mark`,
    /// prepending a `mw:DiffMarker/*` meta when the mark requires it (faithful
    /// to `DiffUtils::addDiffMark` + `markNode`). Returns the index of the
    /// *next sibling after* the marked node (accounting for any inserted meta).
    fn mark_node(&mut self, new_parent: &mut Node, index: usize, mark: DiffMarkers) -> usize {
        let node_is_element = matches!(new_parent.children[index].kind, NodeKind::Element(_));

        let insert_meta = match mark {
            DiffMarkers::Deleted | DiffMarkers::Moved => true,
            DiffMarkers::Inserted => !node_is_element,
            _ => false,
        };

        if insert_meta {
            let ty = format!("mw:DiffMarker/{}", mark.value());
            let meta = diff_marker_meta(&ty);
            new_parent.children.insert(index, meta);
            // The marked node is now at index+1; its next sibling is at index+2.
            index + 2
        } else {
            DiffUtils::set_diff_mark(&mut new_parent.children[index], mark);
            index + 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Node {
        let mut node = Node::element(ElementKind::Paragraph);
        node.push_child(Node::text(text));
        node
    }

    #[test]
    fn test_tree_equals_identical() {
        let d = DomDiff::default();
        assert!(d.tree_equals(&p("a"), &p("a"), true));
        assert!(!d.tree_equals(&p("a"), &p("b"), true));
    }

    #[test]
    fn test_diff_no_change() {
        let mut a = Node::document();
        a.push_child(p("x"));
        let mut b = Node::document();
        b.push_child(p("x"));

        let mut d = DomDiff::default();
        assert!(!d.diff(&a, &mut b));
        assert!(b.children[0].get_attr("data-parsoid-diff").is_none());
    }

    #[test]
    fn test_diff_insertion() {
        let mut a = Node::document();
        a.push_child(p("x"));
        let mut b = Node::document();
        b.push_child(p("x"));
        b.push_child(p("y"));

        let mut d = DomDiff::default();
        assert!(d.diff(&a, &mut b));
        let inserted = b.children.iter().any(|n| {
            n.get_attr("data-parsoid-diff")
                .map(|s| s.contains("inserted"))
                .unwrap_or(false)
        });
        assert!(inserted);
    }

    #[test]
    fn test_diff_deletion() {
        let mut a = Node::document();
        a.push_child(p("x"));
        a.push_child(p("y"));
        let mut b = Node::document();
        b.push_child(p("x"));

        let mut d = DomDiff::default();
        assert!(d.diff(&a, &mut b));
        let changed = b
            .get_attr("data-parsoid-diff")
            .map(|s| s.contains("children-changed"))
            .unwrap_or(false)
            || b.children.iter().any(|c| {
                c.get_attr("typeof")
                    .map(|t| t.contains("DiffMarker/deleted"))
                    .unwrap_or(false)
            });
        assert!(changed);
    }
}
