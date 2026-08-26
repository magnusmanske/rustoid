//! `DOMUtils`/`WTUtils` leaf predicates for the html2wt serializer.
//!
//! Faithful ports of the pure, state-free DOM query helpers the serializer's
//! `DOMHandler`/`Separators`/`SerializerState` rely on, from PHP Parsoid's
//! `src/Utils/DOMUtils.php` and the `isNewElt`/`isLiteralHTMLNode`/etc. helpers
//! in `src/Utils/WTUtils.php`.
//!
//! Name-based predicates operate on `&Node`; parent-aware ones (which need
//! ancestor navigation) operate on the `DomTree`/`NodeId` navigation arena.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::html::dom_tree::{DomTree, NodeId, NodeName};
use crate::wikitext::consts;

/// `DOMUtils::nodeName` (via the shared serializer `node_name`).
pub fn node_name(node: &Node) -> String {
    crate::html::wts_utils::node_name(node)
}

/// `DOMUtils::isBody` — is this the `<body>` element?
pub fn is_body(node: &Node) -> bool {
    node_name(node) == "body"
}

/// `DOMUtils::isList` — `ul`/`ol`/`dl`.
pub fn is_list(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::Element(ElementKind::UnorderedList)
            | NodeKind::Element(ElementKind::OrderedList)
            | NodeKind::Element(ElementKind::DefinitionList)
    )
}

/// `DOMUtils::isListItem` — `li`/`dd`/`dt`.
pub fn is_list_item(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::Element(ElementKind::ListItem)
            | NodeKind::Element(ElementKind::DefinitionTerm)
            | NodeKind::Element(ElementKind::DefinitionDescription)
    )
}

/// `DOMUtils::isListOrListItem`.
pub fn is_list_or_list_item(node: &Node) -> bool {
    is_list(node) || is_list_item(node)
}

/// `DOMUtils::isListItem` by *name* (rather than `ElementKind`), for cases that
/// operate on the serialized tag name (`li`/`dd`/`dt`). Faithful to
/// `Consts::$HTML['ListItemTags']`.
pub fn is_list_item_name(name: &str) -> bool {
    matches!(name, "li" | "dd" | "dt")
}

/// `DOMUtils::isHeading` — `/^h[1-6]$/`.
pub fn is_heading(node: &Node) -> bool {
    matches!(node.kind, NodeKind::Element(ElementKind::Heading(_)))
}

/// `DOMUtils::isFormattingElt` — a tag in `Consts::$HTML['FormattingTags']`.
pub fn is_formatting_elt(node: &Node) -> bool {
    consts::formatting_tags().contains(&node_name(node))
}

/// `DOMUtils::isWikitextBlockNode` — a wikitext block tag (see
/// `TokenUtils::isWikitextBlockTag`, i.e. `wikiTextBlockElems`).
pub fn is_wikitext_block_node(node: &Node) -> bool {
    consts::wikitext_block_elems().contains(&node_name(node))
}

/// `DOMUtils::isQuoteElt` (`Consts::$WTQuoteTags` = `b`/`i`).
pub fn is_quote_elt(node: &Node) -> bool {
    crate::html::wts_utils::is_quote_elt(node)
}

/// `DOMUtils::isIEW` — a text node that is entirely whitespace.
pub fn is_iew(node: &Node) -> bool {
    matches!(&node.kind, NodeKind::Text(t) if !t.is_empty() && t.chars().all(|c| c.is_whitespace()))
}

/// `DOMUtils::isTableTag` — a tag in `Consts::$HTML['TableTags']`.
pub fn is_table_tag(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::Element(ElementKind::Table)
            | NodeKind::Element(ElementKind::TableRow)
            | NodeKind::Element(ElementKind::TableCell)
            | NodeKind::Element(ElementKind::TableHeader)
            | NodeKind::Element(ElementKind::TableCaption)
    )
}

/// `WTUtils::isLiteralHTMLNode` — `data-parsoid.stx === 'html'`.
pub fn is_literal_html_node(node: &Node) -> bool {
    crate::html::wts_utils::is_literal_html_node(node)
}

// ---------------------------------------------------------------------------
// Parent-aware predicates (operate on `DomTree`/`NodeId`).
// ---------------------------------------------------------------------------

/// `DOMUtils::isBody` for a tree node (id-based).
pub fn is_body_id(tree: &DomTree, id: NodeId) -> bool {
    is_body(tree.node(id))
}

/// `DOMUtils::atTheTop` — `isBody(node) || isDocumentFragment(node)`. Our root is
/// a `Document` node, so "at the top" means the node is a direct child of the
/// root (or is the root itself).
pub fn at_the_top(tree: &DomTree, id: NodeId) -> bool {
    match tree.node_name(id) {
        NodeName::Document => true,
        NodeName::Element(ElementKind::Document) => true,
        _ => tree.parent(id).is_none(),
    }
}

/// `DOMUtils::isNestedInListItem` — the node has a list-item ancestor.
pub fn is_nested_in_list_item(tree: &DomTree, id: NodeId) -> bool {
    let mut cur = tree.parent(id);
    while let Some(pid) = cur {
        if is_list_item(tree.node(pid)) {
            return true;
        }
        cur = tree.parent(pid);
    }
    false
}

/// `DOMUtils::findAncestorOfName` — the nearest ancestor with the given name.
pub fn find_ancestor_of_name(tree: &DomTree, id: NodeId, name: &str) -> Option<NodeId> {
    let mut cur = tree.parent(id);
    while let Some(pid) = cur {
        if node_name(tree.node(pid)) == name {
            return Some(pid);
        }
        cur = tree.parent(pid);
    }
    None
}

/// `DOMUtils::hasNameOrHasAncestorOfName`.
pub fn has_name_or_has_ancestor_of_name(tree: &DomTree, id: NodeId, name: &str) -> bool {
    node_name(tree.node(id)) == name || find_ancestor_of_name(tree, id, name).is_some()
}

/// `WTUtils::isNewElt` — the node has no `dsr` (i.e. was newly inserted in an
/// edit, so no original source range). Faithful to PHP's `isNewElt` which checks
/// `!isset($dp->dsr)`.
pub fn is_new_elt(tree: &DomTree, id: NodeId) -> bool {
    tree.node(id).dp.as_ref().is_none_or(|dp| dp.dsr.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_is_list_and_item() {
        assert!(is_list(&Node::element(ElementKind::UnorderedList)));
        assert!(is_list(&Node::element(ElementKind::DefinitionList)));
        assert!(!is_list(&Node::element(ElementKind::Paragraph)));
        assert!(is_list_item(&Node::element(ElementKind::ListItem)));
        assert!(is_list_item(&Node::element(ElementKind::DefinitionTerm)));
        assert!(!is_list_item(&Node::element(ElementKind::UnorderedList)));
    }

    #[test]
    fn test_is_formatting_and_block() {
        assert!(is_formatting_elt(&Node::element(ElementKind::Bold)));
        assert!(is_formatting_elt(&Node::element(ElementKind::Italic)));
        assert!(!is_formatting_elt(&Node::element(ElementKind::Paragraph)));
        assert!(is_wikitext_block_node(&Node::element(ElementKind::Table)));
        assert!(!is_wikitext_block_node(&Node::element(ElementKind::Bold)));
    }

    #[test]
    fn test_is_iew() {
        assert!(is_iew(&Node::text("  \n\t")));
        assert!(!is_iew(&Node::text(" x ")));
        assert!(!is_iew(&Node::element(ElementKind::Paragraph)));
    }

    #[test]
    fn test_at_top_and_ancestors() {
        let mut doc = Node::document();
        let mut ul = Node::element(ElementKind::UnorderedList);
        let mut li = Node::element(ElementKind::ListItem);
        let mut nested_ul = Node::element(ElementKind::UnorderedList);
        let mut nested_li = Node::element(ElementKind::ListItem);
        nested_li.push_child(Node::text("nested"));
        nested_ul.push_child(nested_li);
        li.push_child(Node::text("x"));
        li.push_child(nested_ul);
        ul.push_child(li);
        doc.push_child(ul);

        let tree = DomTree::new(doc);
        let ul_id = tree.first_child(tree.root()).unwrap();
        let li_id = tree.first_child(ul_id).unwrap();
        let nested_ul_id = tree.last_child(li_id).unwrap();
        let nested_li_id = tree.first_child(nested_ul_id).unwrap();

        assert!(!at_the_top(&tree, ul_id));
        // `nested_li` has a list-item ancestor (`li`).
        assert!(is_nested_in_list_item(&tree, nested_li_id));
        assert!(!is_nested_in_list_item(&tree, li_id));
        assert!(find_ancestor_of_name(&tree, nested_li_id, "ul").is_some());
        assert!(has_name_or_has_ancestor_of_name(&tree, nested_li_id, "ul"));
    }
}
