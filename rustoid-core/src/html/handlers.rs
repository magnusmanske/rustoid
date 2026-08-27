//! Concrete DOM handlers — faithful ports of PHP Parsoid's
//! `src/Html2Wt/DOMHandlers/*.php`.
//!
//! Each struct implements the [`DomHandler`] trait. They are instantiated by
//! [`get_dom_handler`] and layered in one at a time. The simplest handlers are
//! ported first (`BodyHandler`, `JustChildrenHandler`, `QuoteHandler`), since
//! they only depend on `SerializerState::emit_chunk`/`serialize_children` and
//! the `DomTree` navigation arena.

use crate::html::dom_handler::DomHandler;
use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::dom_utils;
use crate::html::separators::Constraints;
use crate::html::serializer_state::SerializerState;

/// `BodyHandler` — serializes children, ignoring the `<body>` wrapper.
/// Faithful to `DOMHandlers/BodyHandler.php`.
pub struct BodyHandler;

impl DomHandler for BodyHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        walk_children(tree, node, state);
        tree.next_sibling(node)
    }

    fn first_child(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        Some(Constraints {
            min: Some(0),
            max: Some(1),
        })
    }

    fn last_child(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        Some(Constraints {
            min: Some(0),
            max: Some(1),
        })
    }
}

/// `JustChildrenHandler` — serialize children, ignore the implicit tag.
/// Faithful to `DOMHandlers/JustChildrenHandler.php` (used for `<thead>`/
/// `<tbody>`/`<tfoot>`).
pub struct JustChildrenHandler;

impl DomHandler for JustChildrenHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        walk_children(tree, node, state);
        tree.next_sibling(node)
    }
}

/// `QuoteHandler` — serialize `<b>`/`<i>` as `'''`/`''`.
/// Faithful to `DOMHandlers/QuoteHandler.php`.
pub struct QuoteHandler {
    pub quotes: String,
}

impl QuoteHandler {
    pub fn new(quotes: &str) -> Self {
        Self {
            quotes: quotes.to_string(),
        }
    }

    /// `QuoteHandler::precedingQuoteEltRequiresEscape`.
    fn preceding_quote_elt_requires_escape(&self, tree: &DomTree, node: NodeId) -> bool {
        let Some(prev) = crate::html::dom_tree::previous_non_deleted_sibling(tree, node) else {
            return false;
        };
        if !dom_utils::is_quote_elt(tree.node(prev)) {
            return false;
        }
        let last_prev_child = crate::html::dom_tree::last_non_deleted_child(tree, prev);
        let first_node_child = crate::html::dom_tree::first_non_deleted_child(tree, node);
        last_prev_child.is_some_and(|c| dom_utils::is_quote_elt(tree.node(c)))
            || first_node_child.is_some_and(|c| dom_utils::is_quote_elt(tree.node(c)))
    }
}

impl DomHandler for QuoteHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        if self.preceding_quote_elt_requires_escape(tree, node) {
            state.emit_chunk("<nowiki/>", node);
        }
        state.emit_chunk(self.quotes.clone(), node);

        if tree.first_child(node).is_some() {
            walk_children(tree, node, state);
        } else {
            // Empty nodes like <i></i> need a <nowiki/> placeholder.
            state.emit_chunk("<nowiki/>", node);
        }

        state.emit_chunk(self.quotes.clone(), node);
        tree.next_sibling(node)
    }
}

/// `HRHandler` — serialize `<hr>` as `----` (plus extra dashes).
/// Faithful to `DOMHandlers/HRHandler.php`.
pub struct HRHandler;

impl DomHandler for HRHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let extra = tree
            .node(node)
            .dp
            .as_ref()
            .and_then(|d| d.extra_dashes)
            .unwrap_or(0);
        state.emit_chunk("-".repeat(4 + extra), node);
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        _t: &DomTree,
        _n: NodeId,
        _o: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        Some(Constraints {
            min: Some(1),
            max: Some(2),
        })
    }

    fn after(
        &mut self,
        _t: &DomTree,
        _n: NodeId,
        _o: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        Some(Constraints {
            min: Some(0),
            max: Some(2),
        })
    }
}

/// `BRHandler` — serialize `<br>` (only when it has literal-HTML or
/// single-line-context semantics). Faithful to `DOMHandlers/BRHandler.php`,
/// except `PHandler::isPPTransition` is stubbed to `false`.
pub struct BRHandler;

impl BRHandler {
    fn is_pbr(&self, tree: &DomTree, node: NodeId) -> bool {
        if tree
            .node(node)
            .dp
            .as_ref()
            .is_some_and(|d| d.stx.as_deref() == Some("html"))
        {
            return false;
        }
        let Some(parent) = tree.parent(node) else {
            return false;
        };
        dom_utils::node_name(tree.node(parent)) == "p"
            && crate::html::dom_tree::first_non_sep_child(tree, parent) == Some(node)
    }

    fn is_pbr_p(&self, tree: &DomTree, node: NodeId) -> bool {
        self.is_pbr(tree, node) && crate::html::dom_tree::next_non_sep_sibling(tree, node).is_none()
    }
}

impl DomHandler for BRHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let html_stx = tree
            .node(node)
            .dp
            .as_ref()
            .is_some_and(|d| d.stx.as_deref() == Some("html"));
        let parent_is_p = tree
            .parent(node)
            .is_some_and(|p| dom_utils::node_name(tree.node(p)) == "p");
        if state.single_line_context.enforced() || html_stx || !parent_is_p {
            state.emit_chunk("<br />", node);
        }
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        _o: NodeId,
        state: &mut SerializerState,
    ) -> Option<Constraints> {
        if state.single_line_context.enforced() || !self.is_pbr(tree, node) {
            return None;
        }
        Some(Constraints {
            min: Some(3),
            max: None,
        })
    }

    fn after(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        _o: NodeId,
        state: &mut SerializerState,
    ) -> Option<Constraints> {
        if state.single_line_context.enforced() {
            return None;
        }
        if self.is_pbr_p(tree, node) {
            Some(Constraints {
                min: Some(4),
                max: None,
            })
        } else if self.is_pbr(tree, node) {
            Some(Constraints {
                min: Some(2),
                max: None,
            })
        } else {
            None
        }
    }
}

/// `HeadingHandler` — serialize `<hN>` as repeated `=`. Annotation-marker helper
/// is stubbed to `false` until `WTUtils::isAnnotationStartMarkerMeta` lands.
pub struct HeadingHandler {
    pub heading_wt: String,
}

impl HeadingHandler {
    pub fn new(heading_wt: &str) -> Self {
        Self {
            heading_wt: heading_wt.to_string(),
        }
    }
}

impl DomHandler for HeadingHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let space = self.get_leading_space(tree, node, " ");
        state.emit_chunk(format!("{}{}", self.heading_wt, space), node);
        state.single_line_context.enforce();
        if tree.first_child(node).is_some() {
            crate::html::serializer::walk_children(tree, node, state);
        } else {
            state.emit_chunk("<nowiki/>", node);
        }
        let space = self.get_trailing_space(tree, node, " ");
        state.emit_chunk(format!("{}{}", space, self.heading_wt), node);
        state.single_line_context.pop();
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        _o: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        if dom_utils::is_new_elt(tree, node)
            && crate::html::dom_tree::previous_non_sep_sibling(tree, node).is_some()
        {
            Some(Constraints {
                min: Some(2),
                max: Some(2),
            })
        } else {
            Some(Constraints {
                min: Some(1),
                max: Some(2),
            })
        }
    }

    fn after(
        &mut self,
        _t: &DomTree,
        _n: NodeId,
        _o: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        Some(Constraints {
            min: Some(1),
            max: Some(2),
        })
    }

    fn force_sol(&self) -> bool {
        true
    }
}

/// `ListHandler` — serialize `<ul>`/`<ol>`/`<dl>`. `liHandler` escaping is stubbed.
pub struct ListHandler {
    pub first_child_names: Vec<String>,
}

impl ListHandler {
    pub fn new(first_child_names: &[&str]) -> Self {
        Self {
            first_child_names: first_child_names.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl DomHandler for ListHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        state.single_line_context.disable();
        let mut first_child_elt = crate::html::dom_tree::first_non_sep_child(tree, node);
        while let Some(fc) = first_child_elt {
            if self.is_builder_inserted_elt(tree, fc) {
                first_child_elt = crate::html::dom_tree::first_non_sep_child(tree, fc);
            } else {
                break;
            }
        }
        let should_emit = match first_child_elt {
            Some(fc) => {
                !self
                    .first_child_names
                    .contains(&dom_utils::node_name(tree.node(fc)))
                    || dom_utils::is_literal_html_node(tree.node(fc))
            }
            None => true,
        };
        if should_emit {
            let bullets = self.get_list_bullets(tree, node);
            state.emit_chunk(bullets, node);
        }
        crate::html::serializer::walk_children(tree, node, state);
        state.single_line_context.pop();
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        if dom_utils::at_the_top(tree, other) {
            return Some(Constraints {
                min: Some(0),
                max: Some(0),
            });
        }
        if tree
            .parent(node)
            .is_some_and(|p| dom_utils::is_list_item(tree.node(p)))
            && tree.parent(other) == tree.parent(node)
        {
            return Some(Constraints {
                min: Some(1),
                max: Some(1),
            });
        }
        Some(Constraints {
            min: Some(1),
            max: Some(2),
        })
    }

    fn after(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        self.wt_list_eol(tree, node, other)
    }

    fn force_sol(&self) -> bool {
        true
    }
}

/// `LIHandler` — serialize an `<li>`. Trailing-whitespace recovery is stubbed.
pub struct LIHandler;

impl DomHandler for LIHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let first_child_element = crate::html::dom_tree::first_non_sep_child(tree, node);
        let first_is_list = first_child_element.is_some_and(|c| dom_utils::is_list(tree.node(c)));
        let first_is_literal =
            first_child_element.is_some_and(|c| dom_utils::is_literal_html_node(tree.node(c)));
        if !first_is_list || first_is_literal {
            let bullets = self.get_list_bullets(tree, node);
            state.emit_chunk(bullets, node);
        }
        state.single_line_context.enforce();
        crate::html::serializer::walk_children(tree, node, state);
        state.single_line_context.pop();
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        let other_is_parent = tree.parent(node) == Some(other);
        let parent_is_list = tree
            .parent(node)
            .is_some_and(|p| matches!(dom_utils::node_name(tree.node(p)).as_str(), "ul" | "ol"));
        let other_is_html = tree
            .node(other)
            .dp
            .as_ref()
            .is_some_and(|d| d.stx.as_deref() == Some("html"));
        if (other_is_parent && parent_is_list) || other_is_html {
            None
        } else {
            Some(Constraints {
                min: Some(1),
                max: Some(2),
            })
        }
    }

    fn after(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        self.wt_list_eol(tree, node, other)
    }

    fn force_sol(&self) -> bool {
        true
    }
}

/// `FallbackHTMLHandler` — serialize a node as its literal HTML form.
/// Faithful to `DOMHandlers/FallbackHTMLHandler.php`.
pub struct FallbackHTMLHandler;

impl DomHandler for FallbackHTMLHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let tag = crate::html::serializer::WikitextSerializer::serialize_html_tag(tree.node(node));
        state.emit_chunk(tag, node);

        if tree.first_child(node).is_some() {
            let in_php_block = state.in_php_block;
            let name = dom_utils::node_name(tree.node(node));
            if crate::wikitext::token_utils::tag_opens_block_scope(&name) || name == "blockquote" {
                state.in_php_block = true;
            }
            walk_children(tree, node, state);
            state.in_php_block = in_php_block;
        }

        let end_tag =
            crate::html::serializer::WikitextSerializer::serialize_html_end_tag(tree.node(node));
        state.emit_chunk(end_tag, node);
        tree.next_sibling(node)
    }
}

// Re-export the shared walk for convenience (handlers serialize children via
// `SerializerState::serialize_children`, which delegates to the serializer).
fn walk_children(tree: &DomTree, node: NodeId, state: &mut SerializerState) {
    crate::html::serializer::walk_children(tree, node, state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_body_handler_serializes_children() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("hi"));
        doc.push_child(p);

        let tree = DomTree::new(doc);
        let p_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = BodyHandler;
        handler.handle(&tree, p_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "hi");
    }

    #[test]
    fn test_quote_handler_italic() {
        let mut doc = Node::document();
        let mut i = Node::element(ElementKind::Italic);
        i.push_child(Node::text("foo"));
        doc.push_child(i);

        let tree = DomTree::new(doc);
        let i_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = QuoteHandler::new("''");
        handler.handle(&tree, i_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "''foo''");
    }

    #[test]
    fn test_hr_handler() {
        let mut doc = Node::document();
        doc.push_child(Node::element(ElementKind::HorizontalRule));
        let tree = DomTree::new(doc);
        let hr_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = HRHandler;
        handler.handle(&tree, hr_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "----");
    }

    #[test]
    fn test_heading_handler() {
        let mut doc = Node::document();
        let mut h2 = Node::element(ElementKind::Heading(2));
        h2.push_child(Node::text("foo"));
        // An unmodified heading has DSR, so no prettifier space is added.
        h2.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(0),
                end: Some(7),
                open_width: Some(2),
                close_width: Some(2),
            }),
            ..Default::default()
        });
        doc.push_child(h2);
        let tree = DomTree::new(doc);
        let h2_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = HeadingHandler::new("==");
        handler.handle(&tree, h2_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "==foo==");
    }
}
