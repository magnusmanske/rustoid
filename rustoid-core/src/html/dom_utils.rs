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

// ---------------------------------------------------------------------------
// Multivalued attributes (`typeof` / `rel`): `DOMUtils::matchTypeOf`/`hasTypeOf`
// and `matchRel`/`hasRel`. The PHP regexes are PCRE with `#…#D` delimiters and a
// `$`-end anchor; callers pass the Rust-`regex`-compatible body (delimiters and
// flags already stripped, `$` kept as end-of-string).
// ---------------------------------------------------------------------------

/// Split a `typeof`/`rel` attribute value into its space-separated tokens,
/// skipping empty tokens. Faithful to `DOMUtils::matchMultivalAttr`'s
/// `explode(' ', $attrValue)`.
fn multival_attr_tokens(value: &str) -> impl Iterator<Item = &str> {
    value.split(' ').filter(|s| !s.is_empty())
}

/// `DOMUtils::matchTypeOf` — return the first space-separated `typeof` token
/// matching `ty_re` (a Rust-regex body), or `None`. Faithful to
/// `DOMUtils::matchMultivalAttr($n, 'typeof', $typeRe)`.
pub fn match_type_of(node: &Node, ty_re: &str) -> Option<String> {
    match_multival_attr(node, "typeof", ty_re)
}

/// `DOMUtils::hasTypeOf` — literal-token membership in the `typeof` attribute.
pub fn has_type_of(node: &Node, ty: &str) -> bool {
    has_value_in_multival_attr(node, "typeof", ty)
}

/// `DOMUtils::matchRel` — first space-separated `rel` token matching `rel_re`.
pub fn match_rel(node: &Node, rel_re: &str) -> Option<String> {
    match_multival_attr(node, "rel", rel_re)
}

/// `DOMUtils::hasRel` — literal-token membership in the `rel` attribute.
pub fn has_rel(node: &Node, rel: &str) -> bool {
    has_value_in_multival_attr(node, "rel", rel)
}

/// `DOMUtils::matchMultivalAttr` — first token of `attr_name` matching `value_re`.
fn match_multival_attr(node: &Node, attr_name: &str, value_re: &str) -> Option<String> {
    let value = node.get_attr(attr_name)?;
    if value.is_empty() {
        return None;
    }
    let re = regex::Regex::new(value_re).ok()?;
    for token in multival_attr_tokens(value) {
        if re.is_match(token) {
            return Some(token.to_string());
        }
    }
    None
}

/// `DOMUtils::hasValueInMultivalAttr` — membership test for a multivalued attr.
fn has_value_in_multival_attr(node: &Node, attr_name: &str, value: &str) -> bool {
    match node.get_attr(attr_name) {
        None => false,
        Some(attr_value) => {
            attr_value == value || multival_attr_tokens(attr_value).any(|t| t == value)
        }
    }
}

/// `DOMUtils::selectMediaElt` — the first descendant element matching
/// `img`, `video`, or `audio` (depth-first, document order). Faithful to
/// `DOMCompat::querySelector($node, 'img, video, audio')`.
pub fn select_media_elt(tree: &DomTree, id: NodeId) -> Option<NodeId> {
    select_first_descendant(tree, id, &["img", "video", "audio"])
}

/// Depth-first search for the first descendant whose tag name is in `tags`.
fn select_first_descendant(tree: &DomTree, id: NodeId, tags: &[&str]) -> Option<NodeId> {
    fn walk(tree: &DomTree, id: NodeId, tags: &[&str]) -> Option<NodeId> {
        let mut child = tree.first_child(id);
        while let Some(c) = child {
            let name = node_name(tree.node(c));
            if tags.contains(&name.as_str()) {
                return Some(c);
            }
            if let Some(found) = walk(tree, c, tags) {
                return Some(found);
            }
            child = tree.next_sibling(c);
        }
        None
    }
    // The PHP `selectMediaElt` searches descendants *of* `node` (not incl. node).
    walk(tree, id, tags)
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
    fn test_match_type_of_and_rel() {
        let mut span = Node::element(ElementKind::Span);
        span.set_attr("typeof", "mw:File/Thumb mw:Transclusion");
        span.set_attr("rel", "mw:WikiLink/Interwiki");
        // match_type_of: first token matching the regex.
        assert_eq!(
            match_type_of(&span, "^mw:File($|/)").as_deref(),
            Some("mw:File/Thumb")
        );
        // has_type_of: literal token membership.
        assert!(has_type_of(&span, "mw:Transclusion"));
        assert!(!has_type_of(&span, "mw:Param"));
        // match_rel / has_rel.
        assert_eq!(
            match_rel(&span, "^mw:WikiLink").as_deref(),
            Some("mw:WikiLink/Interwiki")
        );
        assert!(has_rel(&span, "mw:WikiLink/Interwiki"));
        assert!(!has_rel(&span, "mw:ExtLink"));
    }

    #[test]
    fn test_select_media_elt() {
        let mut doc = Node::document();
        let mut span = Node::element(ElementKind::Span);
        let mut img = Node::element(ElementKind::Other("img".to_string()));
        img.set_attr("resource", "Foo.jpg");
        span.push_child(img);
        doc.push_child(span);
        let tree = DomTree::new(doc);
        let span_id = tree.first_child(tree.root()).unwrap();
        let media = select_media_elt(&tree, span_id).unwrap();
        assert_eq!(node_name(tree.node(media)), "img");
        // No media descendant → None.
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("x"));
        let tree2 = DomTree::new(p);
        assert!(select_media_elt(&tree2, tree2.root()).is_none());
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
