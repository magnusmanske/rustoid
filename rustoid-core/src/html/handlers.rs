//! Concrete DOM handlers — faithful ports of PHP Parsoid's
//! `src/Html2Wt/DOMHandlers/*.php`.
//!
//! Each struct implements the [`DomHandler`] trait. They are instantiated by
//! [`get_dom_handler`] and layered in one at a time. The simplest handlers are
//! ported first (`BodyHandler`, `JustChildrenHandler`, `QuoteHandler`), since
//! they only depend on `SerializerState::emit_chunk`/`serialize_children` and
//! the `DomTree` navigation arena.

use crate::dom::node::NodeKind;
use crate::html::dom_handler::DomHandler;
use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::dom_utils;
use crate::html::separators::Constraints;
use crate::html::serializer_state::{SerializerState, WtEscapeHandler};

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
            state.emit_chunk("<nowiki/>", node, tree);
        }
        state.emit_chunk(self.quotes.clone(), node, tree);

        if tree.first_child(node).is_some() {
            walk_children(tree, node, state);
        } else {
            // Empty nodes like <i></i> need a <nowiki/> placeholder.
            state.emit_chunk("<nowiki/>", node, tree);
        }

        state.emit_chunk(self.quotes.clone(), node, tree);
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
        state.emit_chunk("-".repeat(4 + extra), node, tree);
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
            state.emit_chunk("<br />", node, tree);
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
        state.emit_chunk(format!("{}{}", self.heading_wt, space), node, tree);
        state.single_line_context.enforce();
        if tree.first_child(node).is_some() {
            crate::html::serializer::walk_children(tree, node, state);
        } else {
            state.emit_chunk("<nowiki/>", node, tree);
        }
        let space = self.get_trailing_space(tree, node, " ");
        state.emit_chunk(format!("{}{}", space, self.heading_wt), node, tree);
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
            state.emit_chunk(bullets, node, tree);
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
            state.emit_chunk(bullets, node, tree);
        }
        state.single_line_context.enforce();
        // Push a context-specific escaping handler for `<li>`/`<dt>` children,
        // faithful to LIHandler's `$liHandler` closure.
        let escaper: WtEscapeHandler = Box::new(move |state, text, opts, tree| {
            crate::html::wikitext_escape_handlers::li_handler(node, state, text, opts, tree)
        });
        state.serialize_children_with_escaper(tree, node, escaper);
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

/// `PHandler` — serialize a `<p>`, with the paragraph newline constraints
/// (`isPPTransition`) that `BRHandler` and others depend on. Faithful to
/// `DOMHandlers/PHandler.php`; the block-node/sol-transparent line-walk helpers
/// are stubbed.
pub struct PHandler;

impl PHandler {
    /// `PHandler::treatAsPPTransition`: should `node` be treated as a P-wrapped
    /// node for newline-constraint purposes? Faithful to the private PHP helper.
    fn treat_as_pp_transition(tree: &DomTree, node: NodeId) -> bool {
        if crate::html::dom_tree::is_iew(tree, node)
            || matches!(tree.node(node).kind, NodeKind::Text(_))
        {
            // Text nodes are treated as P/P transitions.
            return matches!(tree.node(node).kind, NodeKind::Text(_));
        }
        let name = dom_utils::node_name(tree.node(node));
        !dom_utils::at_the_top(tree, node)
            && !dom_utils::is_wikitext_block_node(tree.node(node))
            && !dom_utils::is_literal_html_node(tree.node(node))
            && name != "meta"
    }

    /// `PHandler::isPPTransition`: is `node` a P-wrapped node or one to treat as
    /// such? Faithful to the static PHP method.
    pub fn is_pp_transition(tree: &DomTree, node: Option<NodeId>) -> bool {
        let Some(node) = node else {
            return false;
        };
        if matches!(tree.node(node).kind, NodeKind::Element(_))
            && dom_utils::node_name(tree.node(node)) == "p"
            && tree
                .node(node)
                .dp
                .as_ref()
                .is_none_or(|d| d.stx.as_deref() != Some("html"))
        {
            return true;
        }
        Self::treat_as_pp_transition(tree, node)
    }
}

impl DomHandler for PHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        crate::html::serializer::walk_children(tree, node, state);
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        let other_name = dom_utils::node_name(tree.node(other));
        // parent is a list-item / td / th / body.
        let parent_is_table_cell_or_body = tree.parent(node) == Some(other)
            && (dom_utils::is_list_item(tree.node(other))
                || matches!(other_name.as_str(), "td" | "th" | "body"));
        if parent_is_table_cell_or_body {
            let max = if matches!(other_name.as_str(), "td" | "th" | "body") {
                1
            } else {
                0
            };
            return Some(Constraints {
                min: Some(0),
                max: Some(max),
            });
        }

        // P-P transition: previous sibling is a wikitext `<p>`.
        let prev = crate::html::dom_tree::previous_non_deleted_sibling(tree, node);
        let is_p_p = prev == Some(other)
            && matches!(tree.node(other).kind, NodeKind::Element(_))
            && other_name == "p"
            && tree
                .node(other)
                .dp
                .as_ref()
                .is_none_or(|d| d.stx.as_deref() != Some("html"));
        if is_p_p || Self::treat_as_pp_transition(tree, other) {
            return Some(Constraints {
                min: Some(2),
                max: Some(2),
            });
        }

        Some(Constraints {
            min: Some(0),
            max: Some(2),
        })
    }

    fn after(
        &mut self,
        tree: &DomTree,
        _node: NodeId,
        other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        if Self::is_pp_transition(tree, Some(other)) {
            return Some(Constraints {
                min: Some(2),
                max: Some(2),
            });
        }
        if dom_utils::at_the_top(tree, other) {
            return Some(Constraints {
                min: Some(0),
                max: Some(2),
            });
        }
        Some(Constraints {
            min: Some(0),
            max: Some(2),
        })
    }

    fn force_sol(&self) -> bool {
        true
    }
}

/// `DTHandler` — serialize a `<dt>`. Faithful to `DOMHandlers/DTHandler.php`.
pub struct DTHandler;

impl DomHandler for DTHandler {
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
            state.emit_chunk(bullets, node, tree);
        }
        state.single_line_context.enforce();
        crate::html::serializer::walk_children(tree, node, state);
        state.single_line_context.pop();
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
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        let other_is_row_dd = dom_utils::node_name(tree.node(other)) == "dd"
            && tree
                .node(other)
                .dp
                .as_ref()
                .is_some_and(|d| d.stx.as_deref() == Some("row"));
        if other_is_row_dd {
            Some(Constraints {
                min: Some(0),
                max: Some(0),
            })
        } else {
            self.wt_list_eol(tree, node, other)
        }
    }

    fn first_child(
        &mut self,
        tree: &DomTree,
        _n: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        if !dom_utils::is_list(tree.node(other)) {
            Some(Constraints {
                min: Some(0),
                max: Some(0),
            })
        } else {
            None
        }
    }

    fn force_sol(&self) -> bool {
        true
    }
}

/// `DDHandler` — serialize a `<dd>` (single-line `row` or multi-line).
/// Faithful to `DOMHandlers/DDHandler.php`.
pub struct DDHandler {
    pub stx: Option<String>,
}

impl DDHandler {
    pub fn new(stx: Option<&str>) -> Self {
        Self {
            stx: stx.map(|s| s.to_string()),
        }
    }
}

impl DomHandler for DDHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let first_child_element = crate::html::dom_tree::first_non_sep_child(tree, node);
        let chunk = if self.stx.as_deref() == Some("row") {
            ":".to_string()
        } else {
            self.get_list_bullets(tree, node)
        };
        let first_is_list = first_child_element.is_some_and(|c| dom_utils::is_list(tree.node(c)));
        let first_is_literal =
            first_child_element.is_some_and(|c| dom_utils::is_literal_html_node(tree.node(c)));
        if !first_is_list || first_is_literal {
            state.emit_chunk(chunk, node, tree);
        }
        state.single_line_context.enforce();
        crate::html::serializer::walk_children(tree, node, state);
        state.single_line_context.pop();
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        _t: &DomTree,
        _n: NodeId,
        _o: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        if self.stx.as_deref() == Some("row") {
            Some(Constraints {
                min: Some(0),
                max: Some(0),
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
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        self.wt_list_eol(tree, node, other)
    }

    fn first_child(
        &mut self,
        tree: &DomTree,
        _n: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        if !dom_utils::is_list(tree.node(other)) {
            Some(Constraints {
                min: Some(0),
                max: Some(0),
            })
        } else {
            None
        }
    }

    fn force_sol(&self) -> bool {
        self.stx.as_deref() != Some("row")
    }
}

/// `CaptionHandler` — serialize a `<caption>`. Faithful to
/// `DOMHandlers/CaptionHandler.php`.
pub struct CaptionHandler;

impl DomHandler for CaptionHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let symbol = tree
            .node(node)
            .dp
            .as_ref()
            .and_then(|d| d.start_tag_src.clone())
            .unwrap_or_else(|| "|+".to_string());
        let table_tag = self.serialize_table_tag(&symbol, None, tree, node);
        state.emit_chunk(table_tag, node, tree);
        crate::html::serializer::walk_children(tree, node, state);
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        let max = self.max_nls_in_table(tree, node, other);
        if dom_utils::node_name(tree.node(other)) != "table" {
            Some(Constraints {
                min: Some(1),
                max: Some(max),
            })
        } else {
            Some(Constraints {
                min: Some(0),
                max: Some(max),
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
        let max = self.max_nls_in_table(tree, node, other);
        Some(Constraints {
            min: Some(1),
            max: Some(max),
        })
    }
}

/// `TableHandler` — serialize a `<table>`. Faithful to `DOMHandlers/TableHandler.php`.
pub struct TableHandler;

impl DomHandler for TableHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let wt = tree
            .node(node)
            .dp
            .as_ref()
            .and_then(|d| d.start_tag_src.clone())
            .unwrap_or_else(|| "{|".to_string());
        let indent_table = tree
            .parent(node)
            .is_some_and(|p| dom_utils::node_name(tree.node(p)) == "dd")
            && crate::html::dom_tree::previous_non_sep_sibling(tree, node).is_none();
        if indent_table {
            state.single_line_context.disable();
        }
        let tag = self.serialize_table_tag(&wt, Some(""), tree, node);
        state.emit_chunk(tag, node, tree);
        if !dom_utils::is_literal_html_node(tree.node(node)) {
            state.wiki_table_nesting += 1;
        }
        crate::html::serializer::walk_children(tree, node, state);
        if !dom_utils::is_literal_html_node(tree.node(node)) {
            state.wiki_table_nesting -= 1;
        }
        let end_tag = tree
            .node(node)
            .dp
            .as_ref()
            .and_then(|d| d.end_tag_src.clone())
            .unwrap_or_else(|| "|}".to_string());
        state.emit_chunk(end_tag, node, tree);
        if indent_table {
            state.single_line_context.pop();
        }
        tree.next_sibling(node)
    }

    fn force_sol(&self) -> bool {
        false
    }
}

/// `TRHandler` — serialize a `<tr>`. Faithful to `DOMHandlers/TRHandler.php`
/// (with `hasNonIgnorableAttributes` approximated).
pub struct TRHandler;

impl TRHandler {
    fn tr_wikitext_needed(&self, tree: &DomTree, node: NodeId) -> bool {
        let has_start_tag_src = tree
            .node(node)
            .dp
            .as_ref()
            .and_then(|d| d.start_tag_src.clone())
            .is_some();
        if has_start_tag_src
            || crate::html::dom_tree::previous_non_sep_sibling(tree, node).is_some()
        {
            return true;
        }
        let parent = tree.parent(node);
        let parent_sibling =
            parent.and_then(|p| crate::html::dom_tree::previous_non_sep_sibling(tree, p));
        if let Some(ps) = parent_sibling
            && dom_utils::node_name(tree.node(ps)) != "caption"
        {
            return true;
        }
        has_non_ignorable_attributes(tree.node(node))
    }
}

impl DomHandler for TRHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        if self.tr_wikitext_needed(tree, node) {
            let wt = tree
                .node(node)
                .dp
                .as_ref()
                .and_then(|d| d.start_tag_src.clone())
                .unwrap_or_else(|| "|-".to_string());
            let tag = self.serialize_table_tag(&wt, Some(""), tree, node);
            state.emit_chunk(tag, node, tree);
        }
        crate::html::serializer::walk_children(tree, node, state);
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        let max = self.max_nls_in_table(tree, node, other);
        if self.tr_wikitext_needed(tree, node) {
            Some(Constraints {
                min: Some(1),
                max: Some(max),
            })
        } else {
            Some(Constraints {
                min: Some(0),
                max: Some(max),
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
        let max = self.max_nls_in_table(tree, node, other);
        Some(Constraints {
            min: Some(0),
            max: Some(max),
        })
    }

    fn force_sol(&self) -> bool {
        true
    }
}

/// `TDHandler` — serialize a `<td>`. Faithful to `DOMHandlers/TDHandler.php`,
/// with `tdHandler` escaping and trimmed-whitespace recovery stubbed.
pub struct TDHandler;

impl DomHandler for TDHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let dp = tree.node(node).dp.clone().unwrap_or_default();
        let usable = self.stx_info_valid_for_table_cell(tree, node);
        let attr_sep_src = if usable {
            dp.attr_sep_src.clone()
        } else {
            None
        };
        let start_tag_src = if usable {
            dp.start_tag_src.clone()
        } else {
            None
        };
        let start_tag_src = start_tag_src.unwrap_or_else(|| {
            if usable && dp.stx.as_deref() == Some("row") {
                "||".to_string()
            } else {
                "|".to_string()
            }
        });

        let td_tag = self.serialize_table_tag(&start_tag_src, attr_sep_src.as_deref(), tree, node);
        // `$inWideTD = (bool)preg_match('/\|\||^{{!}}({{!}}|\|)|^(\||{{!}}){{!}}/', $tdTag)`.
        let in_wide_td = td_tag.contains("||")
            || td_tag.starts_with("{{!}}{{!}}")
            || td_tag.starts_with("{{!}}|")
            || td_tag.starts_with("|{{!}}");
        let leading_space = self.get_leading_space(tree, node, "");
        state.emit_chunk(format!("{td_tag}{leading_space}"), node, tree);

        let next_td = crate::html::dom_tree::next_non_sep_sibling(tree, node);
        let next_uses_row_syntax = next_td.is_some_and(|n| {
            matches!(tree.node(n).kind, NodeKind::Element(_))
                && tree
                    .node(n)
                    .dp
                    .as_ref()
                    .is_some_and(|d| d.stx.as_deref() == Some("row"))
        });

        if next_uses_row_syntax
            && crate::html::dom_tree::first_non_deleted_child(tree, node).is_none()
        {
            state.emit_chunk(" ", node, tree);
            return tree.next_sibling(node);
        }

        // Push the `<td>` escaping handler (faithful to `$tdHandler` closure).
        let escaper: WtEscapeHandler = Box::new(move |state, text, opts, tree| {
            crate::html::wikitext_escape_handlers::td_handler(
                node, in_wide_td, state, text, opts, tree,
            )
        });
        state.serialize_children_with_escaper(tree, node, escaper);
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        let force_single_line = dom_utils::node_name(tree.node(other)) == "td"
            && tree
                .node(node)
                .dp
                .as_ref()
                .is_some_and(|d| d.stx.as_deref() == Some("row"));
        let max = self.max_nls_in_table(tree, node, other);
        Some(Constraints {
            min: Some(if force_single_line { 0 } else { 1 }),
            max: Some(max),
        })
    }

    fn after(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        let max = self.max_nls_in_table(tree, node, other);
        Some(Constraints {
            min: Some(0),
            max: Some(max),
        })
    }
}

/// `THHandler` — serialize a `<th>`. Faithful to `DOMHandlers/THHandler.php`,
/// with `thHandler` escaping and trimmed-whitespace recovery stubbed.
pub struct THHandler;

impl DomHandler for THHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let dp = tree.node(node).dp.clone().unwrap_or_default();
        let usable = self.stx_info_valid_for_table_cell(tree, node);
        let attr_sep_src = if usable {
            dp.attr_sep_src.clone()
        } else {
            None
        };
        let start_tag_src = if usable {
            dp.start_tag_src.clone()
        } else {
            None
        };
        let start_tag_src = start_tag_src.unwrap_or_else(|| {
            if usable && dp.stx.as_deref() == Some("row") {
                "!!".to_string()
            } else {
                "!".to_string()
            }
        });

        let th_tag = self.serialize_table_tag(&start_tag_src, attr_sep_src.as_deref(), tree, node);
        let leading_space = self.get_leading_space(tree, node, "");
        state.emit_chunk(format!("{th_tag}{leading_space}"), node, tree);

        let next_th = crate::html::dom_tree::next_non_sep_sibling(tree, node);
        let next_uses_row_syntax = next_th.is_some_and(|n| {
            matches!(tree.node(n).kind, NodeKind::Element(_))
                && tree
                    .node(n)
                    .dp
                    .as_ref()
                    .is_some_and(|d| d.stx.as_deref() == Some("row"))
        });
        if next_uses_row_syntax
            && crate::html::dom_tree::first_non_deleted_child(tree, node).is_none()
        {
            state.emit_chunk(" ", node, tree);
            return tree.next_sibling(node);
        }

        // Push the `<th>` escaping handler (faithful to `$thHandler` closure).
        let escaper: WtEscapeHandler = Box::new(move |state, text, _opts, _tree| {
            crate::html::wikitext_escape_handlers::th_handler(state, text)
        });
        state.serialize_children_with_escaper(tree, node, escaper);
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        let force_single_line = dom_utils::node_name(tree.node(other)) == "th"
            && tree
                .node(node)
                .dp
                .as_ref()
                .is_some_and(|d| d.stx.as_deref() == Some("row"));
        let max = self.max_nls_in_table(tree, node, other);
        Some(Constraints {
            min: Some(if force_single_line { 0 } else { 1 }),
            max: Some(max),
        })
    }

    fn after(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _s: &mut SerializerState,
    ) -> Option<Constraints> {
        let max = self.max_nls_in_table(tree, node, other);
        if dom_utils::node_name(tree.node(other)) == "td" {
            Some(Constraints {
                min: Some(1),
                max: Some(max),
            })
        } else {
            Some(Constraints {
                min: Some(0),
                max: Some(max),
            })
        }
    }
}

/// `SpanHandler` — serialize a `<span>` wrapper. Faithful to
/// `DOMHandlers/SpanHandler.php`, with nowiki/media/entity/placeholder branches
/// stubbed and plain-span falling back to HTML.
pub struct SpanHandler;

impl DomHandler for SpanHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        // Fall back to plain HTML serialization for spans (the recognized
        // nowiki/entity/media/placeholder branches are deferred).
        let tag = crate::html::serializer::WikitextSerializer::serialize_html_tag(tree.node(node));
        state.emit_chunk(tag, node, tree);
        crate::html::serializer::walk_children(tree, node, state);
        let end_tag =
            crate::html::serializer::WikitextSerializer::serialize_html_end_tag(tree.node(node));
        state.emit_chunk(end_tag, node, tree);
        tree.next_sibling(node)
    }
}

/// `PreHandler` — serialize an indent-`<pre>` block. Faithful to
/// `DOMHandlers/PreHandler.php`.
pub struct PreHandler;

/// Match a wikitext comment at `text[i..]`: `<!--` … `-->` (first closing).
/// Returns the end index (past `-->`) when `text[i..]` begins with a comment.
/// Mirrors PHP's `COMMENT_REGEXP = /<!--(?>[\s\S]*?-->)/`.
fn match_comment_at(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    if !rest.starts_with("<!--") {
        return None;
    }
    // Non-greedy: the first `-->` terminates the comment.
    let end = rest[4..].find("-->")?;
    Some(i + 4 + end + 3)
}

/// Insert a leading space, then a space after each newline (and after any
/// comments that immediately follow it), as required to re-indent a `<pre>`:
/// PHP `' ' . preg_replace( $solRE, '$1 ', $content )` where
/// `$solRE = '/(\n(' . COMMENT_REGEXP . ')*)/'`.
fn indent_pre_insert(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len() + 1);
    // Leading space (always, mirroring the `' ' .` prefix).
    out.push(' ');
    let mut i = 0;
    while i < bytes.len() {
        if content[i..].starts_with('\n') {
            out.push('\n');
            i += 1;
            // Consume zero-or-more comments immediately following the newline.
            let mut j = i;
            while let Some(end) = match_comment_at(content, j) {
                out.push_str(&content[j..end]);
                j = end;
            }
            // Emit the rest-of-line up to the next newline, then the inserted
            // space (the `$1` group is `\n(comment)*`, so the space is appended
            // after the comments).
            let line_end = content[j..]
                .find('\n')
                .map(|r| j + r)
                .unwrap_or(content.len());
            out.push_str(&content[j..line_end]);
            out.push(' ');
            i = line_end;
        } else {
            // Copy up to the next newline verbatim (first line has no match).
            let line_end = content[i..]
                .find('\n')
                .map(|r| i + r)
                .unwrap_or(content.len());
            out.push_str(&content[i..line_end]);
            i = line_end;
        }
    }
    out
}

/// Remove the inserted indentation on comment-only "empty" lines, faithful to
/// PHP's `$emptyLinesRE = '/(^|\n) ((?:[ \t]*comment[ \t]*)+)(?=\n|$)/D'`
/// replaced once (`preg_replace(..., '$1$2', $content, 1)`).
fn indent_pre_strip_empty(content: &str) -> String {
    // Scan for `(^|\n) ` (the inserted space), then one-or-more runs of
    // `[ \t]*comment[ \t]*`, up to `\n` or end-of-string.
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    let mut replaced = false;
    while i < bytes.len() {
        // Anchor: start-of-string or a `\n`.
        let at_start = i == 0;
        let at_nl = content[i..].starts_with('\n');
        let space_pos = if at_start {
            0
        } else if at_nl {
            i + 1
        } else {
            // No anchor here: copy one char and advance.
            let c = content[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
            continue;
        };

        // Need the inserted `' '` right after the anchor.
        if !replaced && space_pos < content.len() && content[space_pos..].starts_with(' ') {
            // Match `(?:[ \t]*comment[ \t]*)+` greedily and ensure it ends at
            // `\n` or end-of-string.
            let mut j = space_pos + 1;
            let mut matched_any = false;
            loop {
                let mut k = j;
                while k < content.len()
                    && matches!(content[k..].chars().next().unwrap(), ' ' | '\t')
                {
                    let c = content[k..].chars().next().unwrap();
                    k += c.len_utf8();
                }
                match match_comment_at(content, k) {
                    Some(end) => {
                        k = end;
                        while k < content.len()
                            && matches!(content[k..].chars().next().unwrap(), ' ' | '\t')
                        {
                            let c = content[k..].chars().next().unwrap();
                            k += c.len_utf8();
                        }
                        matched_any = true;
                        j = k;
                    }
                    None => break,
                }
            }
            let end_ok = j == content.len() || content[j..].starts_with('\n');
            if matched_any && end_ok {
                // Drop the inserted space: emit anchor + the comment run.
                if at_nl {
                    out.push('\n');
                }
                out.push_str(&content[space_pos + 1..j]);
                replaced = true;
                i = j;
                continue;
            }
        }

        // No match: copy the anchor/space and continue.
        if at_nl {
            out.push('\n');
            i += 1;
        } else {
            let c = content[space_pos..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

impl DomHandler for PreHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        // Serialize the children in indent-pre context, then re-indent.
        let content = state.serialize_indent_pre_children_to_string(tree, node);

        // Strip (only the) trailing newline.
        let (body, trailing_nl) = match content.strip_suffix('\n') {
            Some(body) => (body.to_string(), "\n"),
            None => (content, ""),
        };

        // Insert indentation, then strip it on empty/comment-only lines.
        let content = indent_pre_strip_empty(&indent_pre_insert(&body));

        state.emit_chunk(content, node, tree);

        // Preserve the stripped trailing newline as separator source.
        if !trailing_nl.is_empty() {
            state.append_sep(trailing_nl);
        }
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        _node: NodeId,
        other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        let other_name = dom_utils::node_name(tree.node(other));
        let other_stx_is_non_html = tree
            .node(other)
            .dp
            .as_ref()
            .is_none_or(|dp| dp.stx.as_deref() != Some("html"));
        if other_name == "pre" && other_stx_is_non_html {
            Some(Constraints {
                min: Some(2),
                max: None,
            })
        } else {
            Some(Constraints {
                min: Some(1),
                max: None,
            })
        }
    }

    fn after(
        &mut self,
        tree: &DomTree,
        _node: NodeId,
        other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        let other_name = dom_utils::node_name(tree.node(other));
        let other_stx_is_non_html = tree
            .node(other)
            .dp
            .as_ref()
            .is_none_or(|dp| dp.stx.as_deref() != Some("html"));
        if other_name == "pre" && other_stx_is_non_html {
            Some(Constraints {
                min: Some(2),
                max: None,
            })
        } else {
            Some(Constraints {
                min: Some(1),
                max: None,
            })
        }
    }

    fn first_child(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        None
    }

    fn last_child(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        None
    }
}

/// `HTMLPreHandler` — serialize an HTML-syntax `<pre>` literally. Faithful to
/// `DOMHandlers/HTMLPreHandler.php` (delegates to `FallbackHTMLHandler`).
pub struct HTMLPreHandler;

impl DomHandler for HTMLPreHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        // Delegate to the literal-HTML fallback.
        FallbackHTMLHandler.handle(tree, node, state);
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
            min: None,
            max: Some(usize::MAX),
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
            min: None,
            max: Some(usize::MAX),
        })
    }
}

/// `AHandler` — serialize an `<a>` link via `link_handler`. Faithful to
/// `DOMHandlers/AHandler.php` (delegates to `WikitextSerializer::linkHandler`).
pub struct AHandler;

impl DomHandler for AHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        // `SerializerEnv` is `Copy`; extract it before mutably borrowing `state`.
        if let Some(env) = state.env {
            crate::html::link_handler_utils::link_handler(state, tree, &env, node);
        } else {
            FallbackHTMLHandler.handle(tree, node, state);
        }
        tree.next_sibling(node)
    }
}

/// `LinkHandler` — serialize a `<link>` (redirect/category/… link) via
/// `link_handler`. Faithful to `DOMHandlers/LinkHandler.php`.
pub struct LinkHandler;

impl DomHandler for LinkHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        if let Some(env) = state.env {
            crate::html::link_handler_utils::link_handler(state, tree, &env, node);
        } else {
            FallbackHTMLHandler.handle(tree, node, state);
        }
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        None
    }

    fn after(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        None
    }
}

/// `FigureHandler` — serialize a `<figure>` via `figure_handler`. Faithful to
/// `DOMHandlers/FigureHandler.php`.
pub struct FigureHandler;

impl DomHandler for FigureHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        if let Some(env) = state.env {
            let ms = crate::html::media_structure::MediaStructure::parse(tree, node);
            crate::html::link_handler_utils::figure_handler(state, tree, &env, node, ms);
        } else {
            FallbackHTMLHandler.handle(tree, node, state);
        }
        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        _tree: &DomTree,
        node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        let _ = node;
        None
    }

    fn after(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        None
    }
}

/// `ImgHandler` — serialize an `<img>`. Faithful to `DOMHandlers/ImgHandler.php`
/// (external image → `src`; otherwise `figure_handler`).
pub struct ImgHandler;

impl DomHandler for ImgHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        if let Some(env) = state.env {
            if crate::html::dom_utils::has_rel(tree.node(node), "mw:externalImage") {
                let src = tree.node(node).get_attr("src").unwrap_or("");
                state.emit_chunk(src, node, tree);
            } else {
                let ms = crate::html::media_structure::MediaStructure::parse(tree, node);
                crate::html::link_handler_utils::figure_handler(state, tree, &env, node, ms);
            }
        } else {
            FallbackHTMLHandler.handle(tree, node, state);
        }
        tree.next_sibling(node)
    }
}

/// `MediaHandler` — serialize an `<audio>`/`<video>` via `figure_handler`. Faithful
/// to `DOMHandlers/MediaHandler.php` (the element is its own media element).
pub struct MediaHandler;

impl DomHandler for MediaHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        if let Some(env) = state.env {
            // `MediaStructure::parse` would reject a bare `<audio>`/`<video>` (not
            // inline `<span>`/`figure`); construct the structure directly.
            let ms = crate::html::media_structure::MediaStructure {
                container_elt: node,
                link_elt: None,
                media_elt: node,
                caption_elt: None,
            };
            crate::html::link_handler_utils::figure_handler(state, tree, &env, node, Some(ms));
        } else {
            FallbackHTMLHandler.handle(tree, node, state);
        }
        tree.next_sibling(node)
    }
}

/// `WTSUtils::hasNonIgnorableAttributes` — whether a node has any attribute that
/// is not a Parsoid bookkeeping attribute. Approximate stub.
fn has_non_ignorable_attributes(node: &crate::dom::node::Node) -> bool {
    node.attrs.iter().any(|a| {
        !matches!(
            a.key.as_str(),
            "data-parsoid" | "data-mw" | "data-object-id" | "typeof" | "about" | "rel" | "class"
        )
    })
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
        state.emit_chunk(tag, node, tree);

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
        state.emit_chunk(end_tag, node, tree);
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

    #[test]
    fn test_indent_pre_insert() {
        assert_eq!(indent_pre_insert("foo\nbar"), " foo\nbar ");
        assert_eq!(indent_pre_insert("foo"), " foo");
    }

    #[test]
    fn test_pre_handler() {
        let mut doc = Node::document();
        let mut pre = Node::element(ElementKind::Preformatted);
        pre.push_child(Node::text("foo\nbar"));
        doc.push_child(pre);

        let tree = DomTree::new(doc);
        let pre_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = PreHandler;
        handler.handle(&tree, pre_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, " foo\nbar ");
    }

    #[test]
    fn test_pre_handler_trailing_newline_preserved() {
        let mut doc = Node::document();
        let mut pre = Node::element(ElementKind::Preformatted);
        pre.push_child(Node::text("foo\n"));
        doc.push_child(pre);

        let tree = DomTree::new(doc);
        let pre_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = PreHandler;
        handler.handle(&tree, pre_id, &mut state);
        // Trailing newline is stripped from the content and moved to the
        // separator source.
        assert_eq!(state.out, "");
        assert_eq!(state.separator.src.as_deref(), Some("\n"));
    }

    #[test]
    fn test_html_pre_handler_serializes_literally() {
        let mut doc = Node::document();
        let mut pre = Node::element(ElementKind::Preformatted);
        // Mark it as HTML-syntax (`stx: "html"`) so it takes the HTML-pre path.
        pre.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            stx: Some("html".to_string()),
            ..Default::default()
        });
        pre.push_child(Node::text("foo"));
        doc.push_child(pre);

        let tree = DomTree::new(doc);
        let pre_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = HTMLPreHandler;
        handler.handle(&tree, pre_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "<pre>foo</pre>");
    }
}
