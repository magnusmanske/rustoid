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
}
