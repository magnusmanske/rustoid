//! DOMHandler — faithful port of PHP Parsoid's
//! `src/Html2Wt/DOMHandlers/DOMHandler.php`.
//!
//! The interface (trait) that every element serializer implements. The core
//! `handle` method converts a node to wikitext (via `state.emit_chunk`), while
//! the four newline-constraint methods (`before`, `after`, `first_child`,
//! `last_child`) specify how many newlines must separate a node from its
//! neighbor. The shared helpers (`wt_list_eol`, `get_list_bullets`, …) are
//! provided with default implementations, mirroring PHP's `protected` methods.
//!
//! PHP's implementations navigate the DOM through the node's parent/sibling
//! links; here they navigate by `NodeId` through the [`DomTree`] arena.

use crate::html::dom_tree::{DomTree, NodeId, next_non_sep_sibling};
use crate::html::dom_utils;
use crate::html::separators::Constraints;
use crate::html::serializer_state::SerializerState;
use crate::html::wts_utils;

/// A newline constraint pair. Faithful to the `['min'=>…,'max'=>…]` arrays
/// returned by the constraint methods (an empty array → `None`).
pub type NlConstraint = Option<Constraints>;

/// The DOM handler interface (port of `DOMHandler`).
///
/// All methods default to PHP's base-class behavior (`handle` raises; the
/// constraint methods return `[]`; `force_sol` returns the stored flag).
pub trait DomHandler {
    /// Serialize `node` to wikitext (via `state::emit_chunk`), returning the
    /// node to continue with (or `None` to stop). Mirrors `handle`.
    fn handle(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _state: &mut SerializerState,
    ) -> Option<NodeId> {
        // Mirrors the `LogicException("Not implemented.")` in the PHP base class.
        None
    }

    /// Newlines to emit *before* this node. Mirrors `before`.
    fn before(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> NlConstraint {
        None
    }

    /// Newlines to emit *after* this node. Mirrors `after`.
    fn after(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> NlConstraint {
        None
    }

    /// Newlines to emit before the first child. Mirrors `firstChild`.
    fn first_child(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> NlConstraint {
        None
    }

    /// Newlines to emit after the last child. Mirrors `lastChild`.
    fn last_child(
        &mut self,
        _tree: &DomTree,
        _node: NodeId,
        _other: NodeId,
        _state: &mut SerializerState,
    ) -> NlConstraint {
        None
    }

    /// Put the serializer in SOL mode before this node is handled. Mirrors
    /// `forceSOL`.
    fn force_sol(&self) -> bool {
        false
    }

    // -----------------------------------------------------------------------
    // Shared (protected) helpers.
    // -----------------------------------------------------------------------

    /// List helper: the "*after*" newline constraint for list items. Faithful
    /// port of `DOMHandler::wtListEOL`.
    fn wt_list_eol(&self, tree: &DomTree, node: NodeId, other: NodeId) -> NlConstraint {
        let other_name = dom_utils::node_name(tree.node(other));
        let other_is_element = !other_name.is_empty();

        if !other_is_element || dom_utils::at_the_top(tree, other) {
            return Some(Constraints {
                min: Some(0),
                max: Some(2),
            });
        }

        if wts_utils::is_first_encapsulation_wrapper_node(tree.node(other)) {
            return Some(Constraints {
                min: Some(if dom_utils::is_list(tree.node(node)) {
                    1
                } else {
                    0
                }),
                max: Some(2),
            });
        }

        let next_sibling = next_non_sep_sibling(tree, node);
        let dp = tree.node(other).dp.clone();

        if (next_sibling == Some(other)
            && dp
                .as_ref()
                .is_some_and(|d| d.stx.as_deref() == Some("html")))
            || dp.as_ref().is_some_and(|d| d.src.is_some())
        {
            Some(Constraints {
                min: Some(0),
                max: Some(2),
            })
        } else if next_sibling == Some(other) && dom_utils::is_list_or_list_item(tree.node(other)) {
            if dom_utils::is_list(tree.node(node))
                && dom_utils::node_name(tree.node(other)) == dom_utils::node_name(tree.node(node))
            {
                // Adjacent lists of the same type need an extra newline.
                Some(Constraints {
                    min: Some(2),
                    max: Some(2),
                })
            } else if dom_utils::is_list_item(tree.node(node))
                || tree.parent(node).is_some_and(|p| {
                    matches!(dom_utils::node_name(tree.node(p)).as_str(), "li" | "dd")
                })
            {
                // Top-level list.
                Some(Constraints {
                    min: Some(1),
                    max: Some(1),
                })
            } else {
                Some(Constraints {
                    min: Some(1),
                    max: Some(2),
                })
            }
        } else if dom_utils::is_list(tree.node(other))
            || dp
                .as_ref()
                .is_some_and(|d| d.stx.as_deref() == Some("html"))
        {
            // Last child in ul/ol (the list element is our parent): defer
            // separator constraints to the list.
            None
        } else if tree
            .parent(node)
            .is_some_and(|p| dom_utils::is_wikitext_block_node(tree.node(p)))
            && crate::html::dom_tree::last_non_sep_child(tree, tree.parent(node).unwrap())
                == Some(node)
        {
            // A list in a block node doesn't need a trailing empty line if it is
            // the last non-separator child.
            Some(Constraints {
                min: Some(1),
                max: Some(2),
            })
        } else if dom_utils::is_formatting_elt(tree.node(other)) {
            Some(Constraints {
                min: Some(1),
                max: Some(1),
            })
        } else {
            Some(Constraints {
                min: Some(if dom_utils::is_new_elt(tree, node) {
                    2
                } else {
                    1
                }),
                max: Some(2),
            })
        }
    }

    /// List helper: DOM-based list bullet construction. Faithful to
    /// `DOMHandler::getListBullets` for the `ul`/`ol`/`li`/`dl`/`dt`/`dd` cases.
    fn get_list_bullets(&self, tree: &DomTree, node: NodeId) -> String {
        let parent_types: &[(&str, char)] = &[("ul", '*'), ("ol", '#')];
        let list_types: &[(&str, char)] = &[
            ("ul", '\0'),
            ("ol", '\0'),
            ("dl", '\0'),
            ("li", '\0'),
            ("dt", ';'),
            ("dd", ':'),
        ];

        let mut res = String::new();
        let mut cur = Some(node);
        while let Some(c) = cur {
            if dom_utils::at_the_top(tree, c) {
                break;
            }
            let name = dom_utils::node_name(tree.node(c));
            if let Some((_, ch)) = list_types.iter().find(|(n, _)| *n == name.as_str()) {
                if name == "li" {
                    // Walk up to the nearest ul/ol.
                    let mut parent = tree.parent(c);
                    while let Some(p) = parent {
                        let pname = dom_utils::node_name(tree.node(p));
                        if parent_types.iter().any(|(n, _)| *n == pname.as_str()) {
                            break;
                        }
                        parent = tree.parent(p);
                    }
                    if let Some(p) = parent {
                        let pname = dom_utils::node_name(tree.node(p));
                        if !wts_utils::is_literal_html_node(tree.node(p))
                            && let Some((_, bch)) =
                                parent_types.iter().find(|(n, _)| *n == pname.as_str())
                        {
                            res.insert(0, *bch);
                        }
                    }
                } else if *ch != '\0' && !wts_utils::is_literal_html_node(tree.node(c)) {
                    res.insert(0, *ch);
                }
            } else if !wts_utils::is_literal_html_node(tree.node(c))
                || tree
                    .node(c)
                    .dp
                    .as_ref()
                    .is_some_and(|d| !(d.auto_inserted_start && d.auto_inserted_end))
            {
                break;
            }
            cur = tree.parent(c);
        }

        res
    }

    /// Newline-constraint helper for table nodes. Faithful to `maxNLsInTable`.
    fn max_nls_in_table(&self, tree: &DomTree, node: NodeId, orig: NodeId) -> usize {
        if dom_utils::is_new_elt(tree, node) || dom_utils::is_new_elt(tree, orig) {
            1
        } else {
            2
        }
    }

    /// Whitespace to emit between a node's markup and its content, for *new*
    /// elements (for prettier serialization). Faithful to
    /// `DOMHandler::getLeadingSpace`.
    fn get_leading_space(&self, tree: &DomTree, node: NodeId, new_elt_default: &str) -> String {
        if !dom_utils::is_new_elt(tree, node) {
            return String::new();
        }
        let Some(fc) = crate::html::dom_tree::first_non_deleted_child(tree, node) else {
            return String::new();
        };
        // If the first child is a text node not beginning with whitespace, emit
        // the default space.
        match &tree.node(fc).kind {
            crate::dom::node::NodeKind::Text(t) if t.starts_with(char::is_whitespace) => {
                String::new()
            }
            crate::dom::node::NodeKind::Text(_) => new_elt_default.to_string(),
            _ => new_elt_default.to_string(),
        }
    }

    /// Whitespace to emit between a node's content and its markup, for *new*
    /// elements. Faithful to `DOMHandler::getTrailingSpace`.
    fn get_trailing_space(&self, tree: &DomTree, node: NodeId, new_elt_default: &str) -> String {
        if !dom_utils::is_new_elt(tree, node) {
            return String::new();
        }
        let Some(lc) = crate::html::dom_tree::last_non_deleted_child(tree, node) else {
            return String::new();
        };
        match &tree.node(lc).kind {
            crate::dom::node::NodeKind::Text(t) if t.ends_with(char::is_whitespace) => {
                String::new()
            }
            crate::dom::node::NodeKind::Text(_) => new_elt_default.to_string(),
            _ => new_elt_default.to_string(),
        }
    }

    /// Is this node an element auto-inserted by the HTML5 tree builder
    /// (`autoInsertedStart` && `autoInsertedEnd`). Faithful to
    /// `DOMHandler::isBuilderInsertedElt`.
    fn is_builder_inserted_elt(&self, tree: &DomTree, node: NodeId) -> bool {
        let Some(dp) = tree.node(node).dp.as_ref() else {
            return false;
        };
        dp.auto_inserted_start && dp.auto_inserted_end
    }

    /// Serialize a table tag (`|`, `|+`, `|-`, `|`) with the given symbol and
    /// optional end symbol + attributes. Faithful to
    /// `DOMHandler::serializeTableElement`.
    fn serialize_table_element(
        &self,
        symbol: &str,
        end_symbol: Option<&str>,
        tree: &DomTree,
        node: NodeId,
    ) -> String {
        let s_attribs = crate::html::serializer::serialize_attributes_partial(tree.node(node));
        if !s_attribs.is_empty() {
            format!("{symbol} {s_attribs} {}", end_symbol.unwrap_or("|"))
        } else {
            format!("{symbol}{}", end_symbol.unwrap_or(""))
        }
    }

    /// Serialize a table tag, using original source when `wrapper_unmodified`.
    /// Faithful to `DOMHandler::serializeTableTag` (non-selser branch).
    fn serialize_table_tag(
        &self,
        symbol: &str,
        end_symbol: Option<&str>,
        tree: &DomTree,
        node: NodeId,
    ) -> String {
        self.serialize_table_element(symbol, end_symbol, tree, node)
    }

    /// Whether `stx === 'row'` table-cell syntax is still valid after table edits
    /// (i.e. there is an identical previous sibling). Faithful to
    /// `DOMHandler::stxInfoValidForTableCell`.
    fn stx_info_valid_for_table_cell(&self, tree: &DomTree, node: NodeId) -> bool {
        let Some(dp) = tree.node(node).dp.clone() else {
            return false;
        };
        if dp.stx.as_deref() != Some("row") {
            return true;
        }
        let prev = crate::html::dom_tree::previous_non_deleted_sibling(tree, node);
        prev.is_some_and(|p| {
            dom_utils::node_name(tree.node(p)) == dom_utils::node_name(tree.node(node))
        })
    }
}

/// The default handler used for unhandled nodes: `handle` raises (returns
/// `None`), as in PHP's base `DOMHandler`.
pub struct DefaultDomHandler;

impl DomHandler for DefaultDomHandler {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_force_sol_defaults_false() {
        assert!(!DefaultDomHandler.force_sol());
        assert!(
            DefaultDomHandler
                .handle(
                    &DomTree::new(Node::document()),
                    0,
                    &mut SerializerState::new()
                )
                .is_none()
        );
    }

    #[test]
    fn test_get_list_bullets_nested() {
        // <ul><li>x</li></ul> → bullets "*".
        let mut doc = Node::document();
        let mut ul = Node::element(ElementKind::UnorderedList);
        let li = Node::element(ElementKind::ListItem);
        ul.push_child(li);
        doc.push_child(ul);

        let tree = DomTree::new(doc);
        let ul_id = tree.first_child(tree.root()).unwrap();
        let li_id = tree.first_child(ul_id).unwrap();
        assert_eq!(DefaultDomHandler.get_list_bullets(&tree, li_id), "*");
    }
}
