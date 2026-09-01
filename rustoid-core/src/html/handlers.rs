//! Concrete DOM handlers — faithful ports of PHP Parsoid's
//! `src/Html2Wt/DOMHandlers/*.php`.
//!
//! Each struct implements the [`DomHandler`] trait. They are instantiated by
//! [`get_dom_handler`] and layered in one at a time. The simplest handlers are
//! ported first (`BodyHandler`, `JustChildrenHandler`, `QuoteHandler`), since
//! they only depend on `SerializerState::emit_chunk`/`serialize_children` and
//! the `DomTree` navigation arena.

use crate::dom::node::NodeKind;
use crate::html::dom_handler::DefaultDomHandler;
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
/// single-line-context semantics). Faithful to `DOMHandlers/BRHandler.php`.
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

/// `HeadingHandler` — serialize `<hN>` as repeated `=`.
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

/// `ListHandler` — serialize `<ul>`/`<ol>`/`<dl>`.
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
        // A list in a block node (<div>, <td>, etc.) doesn't need a leading
        // empty line if it is the first non-separator child.
        let parent = tree.parent(node);
        if parent.is_some_and(|p| dom_utils::is_wikitext_block_node(tree.node(p)))
            && crate::html::dom_tree::first_non_sep_child(tree, parent.unwrap()) == Some(node)
        {
            return Some(Constraints {
                min: Some(1),
                max: Some(2),
            });
        }
        if dom_utils::is_formatting_elt(tree.node(other)) {
            return Some(Constraints {
                min: Some(1),
                max: Some(1),
            });
        }
        let min = if dom_utils::is_new_elt(tree, node)
            && !crate::html::wts_utils::is_marker_annotation(tree.node(other))
        {
            2
        } else {
            1
        };
        Some(Constraints {
            min: Some(min),
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

/// `LIHandler` — serialize an `<li>`.
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
/// `DOMHandlers/PHandler.php`.
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
        // Faithful to DTHandler's `$liHandler` closure (same escapeds as LI).
        let escaper: WtEscapeHandler = Box::new(move |state, text, opts, tree| {
            crate::html::wikitext_escape_handlers::li_handler(node, state, text, opts, tree)
        });
        state.serialize_children_with_escaper(tree, node, escaper);
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
        // Faithful to DDHandler's `$liHandler` closure (same escapes as LI).
        let escaper: WtEscapeHandler = Box::new(move |state, text, opts, tree| {
            crate::html::wikitext_escape_handlers::li_handler(node, state, text, opts, tree)
        });
        state.serialize_children_with_escaper(tree, node, escaper);
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

/// `TDHandler` — serialize a `<td>`. Faithful to `DOMHandlers/TDHandler.php`.
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

/// `THHandler` — serialize a `<th>`. Faithful to `DOMHandlers/THHandler.php`.
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

/// `SpanHandler` — serialize a `<span>` wrapper bearing a recognized
/// `typeof` marker (`mw:Nowiki`, `mw:Entity`, `mw:DisplaySpace`,
/// `mw:Placeholder`, or inline `mw:File` media), falling back to literal HTML
/// for unrecognized/editor spans. Faithful to `DOMHandlers/SpanHandler.php`.
pub struct SpanHandler;

impl SpanHandler {
    /// `SpanHandler::isRecognizedSpanWrapper` — `typeof` matches one of the
    /// recognized span-wrapper markers.
    fn is_recognized_span_wrapper(node: &crate::dom::node::Node) -> bool {
        crate::html::dom_utils::match_type_of(
            node,
            "^mw:(Nowiki|Entity|DisplaySpace|Placeholder(/\\w+)?|File(/(Frameless|Frame|Thumb))?)$",
        )
        .is_some()
    }

    /// `DOMHandler::emitPlaceholderSrc` — emit a placeholder's source, moving
    /// newline-only source into the separator (mirrors PHP).
    fn emit_placeholder_src(tree: &DomTree, node: NodeId, state: &mut SerializerState) {
        let dp = tree.node(node).dp.clone();
        let src = dp.and_then(|d| d.src).unwrap_or_default();
        if src.contains("<nowiki") && src.contains("/>") {
            state.has_self_closing_nowikis = true;
        }
        if !src.is_empty() && src.chars().all(|c| c == '\n') {
            state.append_sep(&src);
        } else {
            state.emit_chunk(src, node, tree);
        }
    }
}

impl DomHandler for SpanHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let n = tree.node(node);
        let dp = n.dp.clone();

        if Self::is_recognized_span_wrapper(n) {
            if crate::html::dom_utils::has_type_of(n, "mw:Nowiki") {
                // `<nowiki>…</nowiki>`: the extension body is the span's
                // serialized children (mirrors the native `nowiki` ext tag's
                // `domToWikitext` for the plain case).
                state.single_line_context.disable();
                let inner = state.serialize_indent_pre_children_to_string(tree, node);
                state.emit_chunk(format!("<nowiki>{inner}</nowiki>"), node, tree);
                state.single_line_context.pop();
            } else if crate::html::wts_utils::is_inline_media(n) {
                if let Some(env) = state.env {
                    let ms = crate::html::media_structure::MediaStructure::parse(tree, node);
                    crate::html::link_handler_utils::figure_handler(state, tree, &env, node, ms);
                } else {
                    FallbackHTMLHandler.handle(tree, node, state);
                }
            } else if crate::html::dom_utils::has_type_of(n, "mw:Entity")
                && crate::html::dom_tree::has_n_children(tree, node, 1)
            {
                // Serialize an `mw:Entity` span: reuse `src` when it matches the
                // content, else re-encode its text child, else serialize children.
                let content_src = first_text_content(tree, node).unwrap_or_default();
                let src_content = dp
                    .as_ref()
                    .and_then(|d| d.src_content.as_deref())
                    .unwrap_or("");
                if dp.as_ref().and_then(|d| d.src.as_ref()).is_some() && content_src == src_content
                {
                    let src = dp.as_ref().and_then(|d| d.src.clone()).unwrap_or_default();
                    state.emit_chunk(src, node, tree);
                } else if let Some(fc) = tree.first_child(node)
                    && let NodeKind::Text(t) = &tree.node(fc).kind
                {
                    state.emit_chunk(entity_encode_all(t), fc, tree);
                } else {
                    state.serialize_children(tree, node);
                }
            } else if crate::html::dom_utils::has_type_of(n, "mw:DisplaySpace") {
                state.emit_chunk(" ", node, tree);
            } else if crate::html::dom_utils::match_type_of(n, "^mw:Placeholder(/|$)").is_some() {
                if dp.as_ref().and_then(|d| d.src.as_ref()).is_some() {
                    Self::emit_placeholder_src(tree, node, state);
                } else {
                    FallbackHTMLHandler.handle(tree, node, state);
                }
            }
        } else if n.get_attr("data-mw-selser-wrapper").is_some() {
            state.serialize_children(tree, node);
        } else {
            let misnested = dp.as_ref().and_then(|d| d.misnested).unwrap_or(false);
            let is_html_stx = dp.as_ref().and_then(|d| d.stx.as_deref()) == Some("html");
            if misnested && !is_html_stx && !has_non_ignorable_attributes(n) {
                // Discard span wrappers added to flag misnested content.
                state.serialize_children(tree, node);
            } else {
                FallbackHTMLHandler.handle(tree, node, state);
            }
        }
        tree.next_sibling(node)
    }
}

/// The text content of the first text child of `node` (or `None`). Mirrors the
/// `$node->textContent` fetch in the `mw:Entity` span branch (single-text-child
/// spans only).
fn first_text_content(tree: &DomTree, node: NodeId) -> Option<String> {
    tree.first_child(node)
        .and_then(|c| match &tree.node(c).kind {
            crate::dom::node::NodeKind::Text(t) => Some(t.clone()),
            _ => None,
        })
}

/// `Utils::entityEncodeAll` — encode every character as a numeric character
/// reference (`&#xNN;`), mirroring the PHP helper used to re-encode decoded
/// `mw:Entity` content.
fn entity_encode_all(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 6);
    for c in s.chars() {
        out.push_str(&format!("&#x{:X};", c as u32));
    }
    out
}

/// `EncapsulatedContentHandler` — serialize a transclusion/extension/language-
/// variant content wrapper (the first encapsulation wrapper node). Faithful to
/// `DOMHandlers/EncapsulatedContentHandler.php`.
///
/// Transclusions are reconstructed from `data-mw.parts` (via
/// [`template_serializer`]); extension tags from their `data-mw` name/attrs/body;
/// language variants are currently a no-op (the variant handler is not ported).
pub struct EncapsulatedContentHandler;

impl EncapsulatedContentHandler {
    /// `languageVariantHandler` placeholder: faithfully no-op until the
    /// language-variant serializer is ported (PHP routes to a dedicated handler).
    /// Returns `true` when `node` is a `mw:LanguageVariant` wrapper.
    fn is_language_variant(tree: &DomTree, node: NodeId) -> bool {
        crate::html::dom_utils::has_type_of(tree.node(node), "mw:LanguageVariant")
    }

    /// Serialize an extension tag from `data-mw` (name/attrs/body), mirroring
    /// `WikitextSerializer::defaultExtensionHandler` for the plain (non-`extApi`)
    /// case.
    fn default_extension_handler(tree: &DomTree, node: NodeId) -> String {
        let n = tree.node(node);
        let mut out = String::new();
        let ext_name = n
            .data_mw
            .as_deref()
            .and_then(|dm| serde_json::from_str::<serde_json::Value>(dm).ok())
            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .or_else(|| crate::html::wts_utils::get_ext_tag_name(n))
            .unwrap_or_default();

        out.push('<');
        out.push_str(&ext_name);

        if let Some(attrs) = n
            .data_mw
            .as_deref()
            .and_then(|dm| serde_json::from_str::<serde_json::Value>(dm).ok())
            .and_then(|v| v.get("attrs").and_then(|a| a.as_object()).cloned())
        {
            for (k, v) in attrs {
                out.push(' ');
                out.push_str(&k);
                out.push_str("=\"");
                out.push_str(v.as_str().unwrap_or(""));
                out.push('\"');
            }
        }

        if let Some(body) = n
            .data_mw
            .as_deref()
            .and_then(|dm| serde_json::from_str::<serde_json::Value>(dm).ok())
            .and_then(|v| {
                v.get("body")
                    .and_then(|b| b.get("extsrc"))
                    .and_then(|e| e.as_str())
                    .map(str::to_string)
            })
        {
            out.push('>');
            out.push_str(&body);
            out.push_str("</");
            out.push_str(&ext_name);
            out.push('>');
        } else {
            out.push_str(" />");
        }
        out
    }
}

impl DomHandler for EncapsulatedContentHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let n = tree.node(node);
        let dp = n.dp.clone();

        let transclusion_type =
            crate::html::dom_utils::match_type_of(n, "^mw:(Transclusion|Param)$");
        let ext_tag_name = crate::html::wts_utils::get_ext_tag_name(n);

        let src = if transclusion_type.is_some() {
            // Transclusion: serialize from `data-mw.parts`, or fall back to src.
            match n
                .data_mw
                .as_deref()
                .and_then(|dm| serde_json::from_str::<serde_json::Value>(dm).ok())
                .and_then(|v| v.get("parts").cloned())
                .and_then(|parts| crate::html::template_serializer::parse_parts(&parts))
            {
                Some(parts) => crate::html::template_serializer::serialize_from_parts(&parts),
                None => dp.as_ref().and_then(|d| d.src.clone()).unwrap_or_default(),
            }
        } else if ext_tag_name.is_some() {
            // Extension tag: reconstruct from data-mw.
            Self::default_extension_handler(tree, node)
        } else if Self::is_language_variant(tree, node) {
            // Language variant: not yet ported; skip (faithful no-op until the
            // variant serializer lands).
            return tree.next_sibling(node);
        } else {
            // Should never reach here (the dispatch only calls us for an
            // encapsulation wrapper). Fall back to literal HTML.
            FallbackHTMLHandler.handle(tree, node, state);
            return tree.next_sibling(node);
        };

        state.single_line_context.disable();
        let bullets = Self::handle_list_prefix(tree, node);
        state.emit_chunk(format!("{bullets}{src}"), node, tree);
        state.single_line_context.pop();

        // Skip over the rest of the encapsulated forest (siblings with the same
        // `about` id).
        skip_over_encapsulated_content(tree, node)
    }
}

/// `EncapsulatedContentHandler::handleListPrefix` — recover list bullets for a
/// first-encapsulation-wrapper list/list-item whose template source lacks the
/// shared bullet prefix (so serializing `data-mw.parts`/`src` alone would drop
/// the container's assigned bullet). Faithful to the three PHP helpers
/// (`handleListPrefix`, `parentBulletsHaveBeenEmitted`,
/// `isTplListWithoutSharedPrefix`).
impl EncapsulatedContentHandler {
    fn handle_list_prefix(tree: &DomTree, node: NodeId) -> String {
        let n = tree.node(node);
        if !dom_utils::is_list_or_list_item(n)
            || Self::parent_bullets_have_been_emitted(tree, node)
            || crate::html::dom_tree::previous_non_sep_sibling(tree, node).is_some()
            || !Self::is_tpl_list_without_shared_prefix(tree, node)
            // Definition-list rows are emitted for the parent node, so there's
            // nothing to prefix for a `dd` in row syntax.
            || (dom_utils::node_name(n) == "dd"
                && n.dp.as_ref().and_then(|d| d.stx.as_deref()) == Some("row"))
        {
            return String::new();
        }
        // `getListBullets` on the *parent* list node.
        match tree.parent(node) {
            Some(parent) => DefaultDomHandler.get_list_bullets(tree, parent),
            None => String::new(),
        }
    }

    /// `EncapsulatedContentHandler::parentBulletsHaveBeenEmitted` — whether the
    /// containing list's bullets are already emitted (so we must not re-emit).
    fn parent_bullets_have_been_emitted(tree: &DomTree, node: NodeId) -> bool {
        let n = tree.node(node);
        if dom_utils::is_literal_html_node(n) {
            return true;
        }
        if dom_utils::is_list(n) {
            // A list's bullets are emitted unless it is itself a list item's
            // child (nested list); in that case the parent item owns them.
            return !tree
                .parent(node)
                .is_some_and(|p| dom_utils::is_list_item(tree.node(p)));
        }
        // Otherwise the node must be a list item (`li`/`dt`/`dd`): its bullets
        // are already emitted unless its (unwrapped) parent is the expected
        // container (`ul`/`ol` for `li`, `dl` for `dt`/`dd`).
        let name = dom_utils::node_name(n);
        let expected: &[&str] = match name.as_str() {
            "li" => &["ul", "ol"],
            "dt" | "dd" => &["dl"],
            _ => return true,
        };
        let mut parent = match tree.parent(node) {
            Some(p) => p,
            None => return true,
        };
        // Skip builder-inserted wrappers.
        while DefaultDomHandler.is_builder_inserted_elt(tree, parent) {
            match tree.parent(parent) {
                Some(p) => parent = p,
                None => return true,
            }
        }
        !expected.contains(&dom_utils::node_name(tree.node(parent)).as_str())
    }

    /// `EncapsulatedContentHandler::isTplListWithoutSharedPrefix` — whether a
    /// transclusion/extension/param wrapper's list lacks the shared bullet
    /// prefix (so the container's bullet must be recovered).
    fn is_tpl_list_without_shared_prefix(tree: &DomTree, node: NodeId) -> bool {
        let n = tree.node(node);
        if !crate::html::wts_utils::is_first_encapsulation_wrapper_node(n) {
            return false;
        }
        if dom_utils::has_type_of(n, "mw:Transclusion") {
            // If the first part is a string, the template range was expanded to
            // include this list element; otherwise the container isn't part of
            // the template source and we must emit its bullets.
            let first_part_is_string = n
                .data_mw
                .as_deref()
                .and_then(|dm| serde_json::from_str::<serde_json::Value>(dm).ok())
                .and_then(|v| v.get("parts").and_then(|p| p.get(0)).cloned())
                .is_some_and(|p| p.is_string());
            if !first_part_is_string {
                return true;
            }
            // Less than two bullets => no shared prefix was assigned.
            let first = n
                .data_mw
                .as_deref()
                .and_then(|dm| serde_json::from_str::<serde_json::Value>(dm).ok())
                .and_then(|v| {
                    v.get("parts")
                        .and_then(|p| p.get(0))
                        .and_then(|p| p.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            !first.chars().all(|c| matches!(c, '*' | '#' | ':' | ';')) || first.chars().count() < 2
        } else if dom_utils::match_type_of(n, "^mw:(Extension|Param)").is_some() {
            // Containers aren't part of the source here, so emit their bullets.
            true
        } else {
            false
        }
    }
}

/// `WTUtils::skipOverEncapsulatedContent` — advance past siblings carrying the
/// same `about` id (the encapsulated forest), returning the next node.
fn skip_over_encapsulated_content(tree: &DomTree, node: NodeId) -> Option<NodeId> {
    let about = tree.node(node).get_attr("about").map(str::to_string);
    let Some(about) = about else {
        return tree.next_sibling(node);
    };
    let mut cur = tree.next_sibling(node);
    while let Some(c) = cur {
        let same_about = tree
            .node(c)
            .get_attr("about")
            .map(|a| a == about)
            .unwrap_or(false);
        if !same_about {
            break;
        }
        cur = tree.next_sibling(c);
    }
    cur
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

/// `MetaHandler` — serialize a `<meta>` (placeholder, page property, include
/// directive, or annotation marker). Faithful to `DOMHandlers/MetaHandler.php`
/// (the annotation-attribute serialization and `getMagicWordWT` use the
/// `SerializerEnv`; diff-marker metas are ignored).
pub struct MetaHandler;

impl DomHandler for MetaHandler {
    fn handle(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        state: &mut SerializerState,
    ) -> Option<NodeId> {
        let n = tree.node(node);
        let dp_src = n.dp.as_ref().and_then(|d| d.src.clone());
        let placeholder = crate::html::dom_utils::match_type_of(n, "^mw:Placeholder(/|$)");

        // `mw:Placeholder` → emit its source.
        if dp_src.is_some() && placeholder.is_some() {
            let src = dp_src.unwrap_or_default();
            state.emit_chunk(src, node, tree);
            return tree.next_sibling(node);
        }

        let property = n.get_attr("property").unwrap_or("");

        if !property.is_empty() {
            // `#^mw\:PageProp/(.*)$#`.
            if let Some(prop) = property.strip_prefix("mw:PageProp/") {
                let magic_src =
                    n.dp.as_ref()
                        .and_then(|d| d.magic_src.clone())
                        .unwrap_or_default();
                let out = if let Some(env) = state.env {
                    env.get_site_config().get_magic_word_wt(prop, &magic_src)
                } else {
                    magic_src
                };
                state.emit_chunk(out, node, tree);
            } else {
                FallbackHTMLHandler.handle(tree, node, state);
            }
            return tree.next_sibling(node);
        }

        if crate::html::wts_utils::is_annotation_start_marker_meta(n) {
            let mut is_start = false;
            if let Some(ann_type) =
                crate::html::wts_utils::extract_annotation_type(n, &mut is_start)
            {
                state.emit_chunk(format!("<{ann_type}>"), node, tree);
            }
            return tree.next_sibling(node);
        }

        if crate::html::wts_utils::is_annotation_end_marker_meta(n) {
            let mut is_start = false;
            if let Some(ann_type) =
                crate::html::wts_utils::extract_annotation_type(n, &mut is_start)
            {
                state.emit_chunk(format!("</{ann_type}>"), node, tree);
            }
            return tree.next_sibling(node);
        }

        // The `typeof` switch.
        let ty = n.get_attr("typeof").unwrap_or("");
        match ty {
            "mw:Includes/IncludeOnly" => {
                let src = crate::html::wts_utils::get_data_mw_src(n)
                    .or_else(|| n.dp.as_ref().and_then(|d| d.src.clone()))
                    .unwrap_or_default();
                state.emit_chunk(src, node, tree);
            }
            "mw:Includes/IncludeOnly/End" => {
                // Just ignore.
            }
            "mw:Includes/NoInclude" => {
                let src =
                    n.dp.as_ref()
                        .and_then(|d| d.src.clone())
                        .unwrap_or_else(|| "<noinclude>".to_string());
                state.emit_chunk(src, node, tree);
            }
            "mw:Includes/NoInclude/End" => {
                let src =
                    n.dp.as_ref()
                        .and_then(|d| d.src.clone())
                        .unwrap_or_else(|| "</noinclude>".to_string());
                state.emit_chunk(src, node, tree);
            }
            "mw:Includes/OnlyInclude" => {
                let src =
                    n.dp.as_ref()
                        .and_then(|d| d.src.clone())
                        .unwrap_or_else(|| "<onlyinclude>".to_string());
                state.emit_chunk(src, node, tree);
            }
            "mw:Includes/OnlyInclude/End" => {
                let src =
                    n.dp.as_ref()
                        .and_then(|d| d.src.clone())
                        .unwrap_or_else(|| "</onlyinclude>".to_string());
                state.emit_chunk(src, node, tree);
            }
            "mw:DiffMarker/inserted"
            | "mw:DiffMarker/deleted"
            | "mw:DiffMarker/moved"
            | "mw:Separator" => {
                // Just ignore.
            }
            _ => {
                FallbackHTMLHandler.handle(tree, node, state);
            }
        }

        tree.next_sibling(node)
    }

    fn before(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        let n = tree.node(node);
        let other_name = crate::html::wts_utils::node_name(tree.node(other));
        let other_is_element = !other_name.is_empty();

        // `needNewLineSepBeforeMeta`: other is not the parent, and (other is a
        // wikitext block element, or other is text and the next sibling is block).
        let need_nl = other != tree.parent(node).unwrap_or(usize::MAX)
            && (other_is_element && dom_utils::is_wikitext_block_node(tree.node(other)));

        if crate::html::wts_utils::is_annotation_start_marker_meta(n) {
            return if need_nl {
                Some(Constraints {
                    min: Some(2),
                    max: None,
                })
            } else {
                None
            };
        }
        if crate::html::wts_utils::is_annotation_end_marker_meta(n) {
            return if need_nl {
                Some(Constraints {
                    min: Some(1),
                    max: None,
                })
            } else {
                None
            };
        }

        // `mw:PageProp/categorydefaultsort` needs a leading newline (2 before a
        // plain `<p>`), otherwise a single newline.
        let typeof_attr = n.get_attr("typeof").unwrap_or("");
        let property_attr = n.get_attr("property").unwrap_or("");
        if typeof_attr.contains("mw:PageProp/categorydefaultsort")
            || property_attr.contains("mw:PageProp/categorydefaultsort")
        {
            let before_p = other_is_element
                && other_name == "p"
                && tree.node(other).dp.as_ref().and_then(|d| d.stx.as_deref()) != Some("html");
            return Some(Constraints {
                min: Some(if before_p { 2 } else { 1 }),
                max: None,
            });
        }

        // New elements (that are not placeholders/includes/annotations) need a
        // leading newline.
        if dom_utils::is_new_elt(tree, node)
            && crate::html::dom_utils::match_type_of(
                n,
                "^mw:(Placeholder|Includes|Annotation)(/|$)",
            )
            .is_none()
        {
            return Some(Constraints {
                min: Some(1),
                max: None,
            });
        }

        None
    }

    fn after(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        other: NodeId,
        _state: &mut SerializerState,
    ) -> Option<Constraints> {
        let n = tree.node(node);
        let other_is_block = dom_utils::is_wikitext_block_node(tree.node(other));
        let other_not_parent = other != tree.parent(node).unwrap_or(usize::MAX);

        if crate::html::wts_utils::is_annotation_end_marker_meta(n) {
            return if other_not_parent && other_is_block {
                Some(Constraints {
                    min: Some(2),
                    max: None,
                })
            } else {
                None
            };
        }
        if crate::html::wts_utils::is_annotation_start_marker_meta(n) {
            return if other_not_parent && other_is_block {
                Some(Constraints {
                    min: Some(1),
                    max: None,
                })
            } else {
                None
            };
        }

        if dom_utils::is_new_elt(tree, node)
            && crate::html::dom_utils::match_type_of(
                n,
                "^mw:(Placeholder|Includes|Annotation)(/|$)",
            )
            .is_none()
        {
            return Some(Constraints {
                min: Some(1),
                max: None,
            });
        }

        None
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
        // `<pre>foo\n</pre>`: `serialize_text` strips the trailing newline
        // (faithful to `WikitextSerializer::serializeText`'s unconditional
        // `SEPARATOR_SUFFIX_WITH_NLS_RE` split) into the indent-pre
        // sub-serialization's discarded separator, so the `<pre>` body is
        // `foo` and the trailing newline does not leak into the outer
        // separator. The PreHandler's own `append_sep(trailing_nl)` is
        // therefore a no-op (mirroring PHP's dead `str_ends_with` check).
        let mut doc = Node::document();
        let mut pre = Node::element(ElementKind::Preformatted);
        pre.push_child(Node::text("foo\n"));
        doc.push_child(pre);

        let tree = DomTree::new(doc);
        let pre_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = PreHandler;
        handler.handle(&tree, pre_id, &mut state);
        // The trailing newline is consumed by the indent-pre sub-serialization,
        // not moved into the outer separator.
        assert_eq!(state.separator.src.as_deref(), None);
        state.flush_line();
        assert_eq!(state.out, " foo");
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

    #[test]
    fn test_meta_handler_noinclude() {
        let mut doc = Node::document();
        let mut meta = Node::element(ElementKind::Annotation);
        meta.set_attr("typeof", "mw:Includes/NoInclude");
        doc.push_child(meta);

        let tree = DomTree::new(doc);
        let meta_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = MetaHandler;
        handler.handle(&tree, meta_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "<noinclude>");
    }

    #[test]
    fn test_meta_handler_annotation_start() {
        let mut doc = Node::document();
        let mut meta = Node::element(ElementKind::Annotation);
        meta.set_attr("typeof", "mw:Annotation/translate");
        doc.push_child(meta);

        let tree = DomTree::new(doc);
        let meta_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = MetaHandler;
        handler.handle(&tree, meta_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "<translate>");
    }

    #[test]
    fn test_span_handler_display_space() {
        let mut doc = Node::document();
        let mut span = Node::element(ElementKind::Span);
        span.set_attr("typeof", "mw:DisplaySpace");
        doc.push_child(span);

        let tree = DomTree::new(doc);
        let span_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = SpanHandler;
        handler.handle(&tree, span_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, " ");
    }

    #[test]
    fn test_span_handler_entity_reencodes_text() {
        // An `mw:Entity` span with a text child (and no matching `src`) re-encodes
        // the text as numeric character references.
        let mut doc = Node::document();
        let mut span = Node::element(ElementKind::Span);
        span.set_attr("typeof", "mw:Entity");
        span.push_child(Node::text("&").clone());
        doc.push_child(span);

        let tree = DomTree::new(doc);
        let span_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = SpanHandler;
        handler.handle(&tree, span_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "&#x26;");
    }

    #[test]
    fn test_encapsulated_transclusion_serializes_from_parts() {
        // A transclusion wrapper serializes `{{Foo|bar}}` from `data-mw.parts`.
        let mut doc = Node::document();
        let mut span = Node::element(ElementKind::Transclusion);
        span.set_attr("typeof", "mw:Transclusion");
        span.set_attr("about", "#mwt1");
        span.data_mw = Some(
            r#"{"parts":[{"template":{"target":{"wt":"Foo"},"params":{"1":{"wt":"bar"}}}}]}"#
                .to_string(),
        );
        doc.push_child(span);

        let tree = DomTree::new(doc);
        let span_id = tree.first_child(tree.root()).unwrap();

        let mut state = SerializerState::new();
        let mut handler = EncapsulatedContentHandler;
        handler.handle(&tree, span_id, &mut state);
        state.flush_line();
        assert_eq!(state.out, "{{Foo|bar}}");
    }

    #[test]
    fn test_parent_bullets_have_been_emitted() {
        // <ul><li>a</li></ul>: the `li`'s containing `ul` owns the bullet, so it
        // has NOT been emitted yet.
        let mut doc = Node::document();
        let mut ul = Node::element(ElementKind::UnorderedList);
        ul.push_child(Node::element(ElementKind::ListItem));
        doc.push_child(ul);
        let tree = DomTree::new(doc);
        let ul_id = tree.first_child(tree.root()).unwrap();
        let li_id = tree.first_child(ul_id).unwrap();
        assert!(!EncapsulatedContentHandler::parent_bullets_have_been_emitted(&tree, li_id));

        // A top-level `ul` (parent is not a list item) has its bullets already
        // emitted (there is no wrapping item).
        assert!(EncapsulatedContentHandler::parent_bullets_have_been_emitted(&tree, ul_id));

        // Literal-HTML list item has its bullets emitted as literal HTML.
        let mut doc2 = Node::document();
        let mut li = Node::element(ElementKind::ListItem);
        li.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            stx: Some("html".to_string()),
            ..Default::default()
        });
        doc2.push_child(li);
        let tree2 = DomTree::new(doc2);
        let li_id2 = tree2.first_child(tree2.root()).unwrap();
        assert!(EncapsulatedContentHandler::parent_bullets_have_been_emitted(&tree2, li_id2));
    }

    #[test]
    fn test_is_tpl_list_without_shared_prefix() {
        // A transclusion wrapper whose first part is a plain string with fewer
        // than two bullets => no shared prefix.
        let mut doc = Node::document();
        let mut li = Node::element(ElementKind::ListItem);
        li.set_attr("typeof", "mw:Transclusion");
        li.set_attr("about", "#mwt1");
        li.data_mw = Some(r#"{"parts":["a"]}"#.to_string());
        doc.push_child(li);
        let tree = DomTree::new(doc);
        let li_id = tree.first_child(tree.root()).unwrap();
        assert!(EncapsulatedContentHandler::is_tpl_list_without_shared_prefix(&tree, li_id));

        // A transclusion wrapper whose first part is `**` (two bullets) has a
        // shared prefix, so no recovery is needed.
        let mut doc2 = Node::document();
        let mut li2 = Node::element(ElementKind::ListItem);
        li2.set_attr("typeof", "mw:Transclusion");
        li2.set_attr("about", "#mwt2");
        li2.data_mw = Some(r#"{"parts":["**"]}"#.to_string());
        doc2.push_child(li2);
        let tree2 = DomTree::new(doc2);
        let li_id2 = tree2.first_child(tree2.root()).unwrap();
        assert!(!EncapsulatedContentHandler::is_tpl_list_without_shared_prefix(&tree2, li_id2));

        // An extension wrapper (no bullet string first part) needs recovery.
        let mut doc3 = Node::document();
        let mut li3 = Node::element(ElementKind::ListItem);
        li3.set_attr("typeof", "mw:Extension/foo");
        li3.set_attr("about", "#mwt3");
        doc3.push_child(li3);
        let tree3 = DomTree::new(doc3);
        let li_id3 = tree3.first_child(tree3.root()).unwrap();
        assert!(EncapsulatedContentHandler::is_tpl_list_without_shared_prefix(&tree3, li_id3));
    }
}
