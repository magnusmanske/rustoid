//! WikitextSerializer — faithful port of PHP Parsoid's
//! `src/Html2Wt/WikitextSerializer.php`.
//!
//! The orchestrator of html2wt serialization: it walks the DOM (via
//! [`SerializerState`] and the [`DomTree`] arena), delegating each element to
//! the [`DomHandler`](./dom_handler) chosen by [`get_dom_handler`], and emits
//! the result into `SerializerState::out`.
//!
//! This module is being ported bottom-up; the `walk_children` spine and the
//! `serialize_html_tag`/`serialize_html_end_tag` helpers are provided first so
//! the simple concrete handlers (`BodyHandler`, `JustChildrenHandler`,
//! `QuoteHandler`) can be exercised. The deep methods (`escapeWikitext`,
//! `serializeAttributes`, `serializeDOM`) are stubbed and layered on with
//! `WikitextEscapeHandlers`.

use crate::dom::node::NodeKind;
use crate::html::dom_handler_factory::get_dom_handler;
use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::serializer_state::SerializerState;

/// Walk the children of `node`, serializing each via its handler. This is the
/// shared spine for `SerializerState::serializeChildren` and stands in for the
/// `WikitextSerializer::serializeNode` walk (which handles text/comment
/// merging, separators, and selser) until that full walk is ported.
pub fn walk_children(tree: &DomTree, node: NodeId, state: &mut SerializerState) {
    let mut child = tree.first_child(node);
    while let Some(c) = child {
        serialize_node(tree, c, state);
        child = tree.next_sibling(c);
    }
}

/// Serialize a single node, delegating to its handler (or emitting text). This
/// is the minimal `WikitextSerializer::serializeNode`: it handles the
/// text/comment/diff-marker branches and delegates elements to the factory.
pub fn serialize_node(tree: &DomTree, node: NodeId, state: &mut SerializerState) {
    let n = tree.node(node);
    match &n.kind {
        // Text: accumulate pure whitespace into the separator, else emit.
        NodeKind::Text(text) => {
            if !state.in_indent_pre && text.chars().all(|c| c.is_whitespace()) {
                state.append_sep(text);
            } else {
                state.emit_chunk(text.clone(), node);
            }
        }
        // Comment: merge into the separator source.
        NodeKind::Comment(_c) => {
            // `WTSUtils::commentWT` (decode → `<!--…-->`) is layered on later;
            // for the skeleton, comments are dropped from output.
        }
        // Element: delegate to the handler.
        NodeKind::Element(_) | NodeKind::Document => {
            let mut handler = get_dom_handler(tree, node);
            handler.handle(tree, node, state);
        }
    }
}

/// A minimal `WikitextSerializer` structure (the methods the handlers reference
/// are added in later modules; the walk is `walk_children`/`serialize_node`).
pub struct WikitextSerializer;

impl WikitextSerializer {
    /// Serialize the opening HTML tag for a literal-HTML node. Faithful to the
    /// non-selser, non-`autoInsertedStart` skeleton of `serializeHTMLTag`.
    pub fn serialize_html_tag(node: &crate::dom::node::Node) -> String {
        let name = crate::html::wts_utils::node_name(node);
        let attrs = serialize_attributes(node);
        let suffix = if attrs.is_empty() {
            String::new()
        } else {
            format!(" {attrs}")
        };
        format!("<{name}{suffix}>")
    }

    /// Serialize the closing HTML tag for a literal-HTML node. Faithful to the
    /// non-`autoInsertedEnd` skeleton of `serializeHTMLEndTag`.
    pub fn serialize_html_end_tag(node: &crate::dom::node::Node) -> String {
        let name = crate::html::wts_utils::node_name(node);
        format!("</{name}>")
    }
}

/// Serialize an element's attributes to an HTML attribute string. This is a
/// faithful-enough skeleton of `serializeAttributes` for the IGNORED list and
/// the plain (non-shadow) attribute case; `data-parsoid`/`data-mw` and RDFa
/// `about`/`typeof`/`rel` attributes are stripped.
fn serialize_attributes(node: &crate::dom::node::Node) -> String {
    let mut out = Vec::new();
    for attr in &node.attrs {
        let k = &attr.key;
        let v = &attr.value;
        if matches!(
            k.as_str(),
            "data-parsoid"
                | "data-mw"
                | "data-ve-changed"
                | "data-parsoid-changed"
                | "data-parsoid-diff"
                | "data-parsoid-serialize"
                | "data-object-id"
                | "about"
                | "typeof"
                | "rel"
                | "class"
        ) {
            continue;
        }
        out.push(format!("{}=\"{}\"", k, v.replace('"', "&quot;")));
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_serialize_html_tag() {
        let mut div = Node::element(ElementKind::Div);
        div.set_attr("class", "mw-empty-elt");
        div.set_attr("style", "color:red");
        // data-* and class are stripped.
        assert_eq!(
            WikitextSerializer::serialize_html_tag(&div),
            "<div style=\"color:red\">"
        );
    }

    #[test]
    fn test_walk_children_emits_text() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("hi"));
        doc.push_child(p);

        let tree = DomTree::new(doc);
        let p_id = tree.first_child(tree.root()).unwrap();
        // Walking the paragraph's children buffers the text on the current line.
        let mut state = SerializerState::new();
        walk_children(&tree, p_id, &mut state);
        // The faithful path buffers into `curr_line` and flushes at line end.
        assert_eq!(state.curr_line.text, "hi");
        state.flush_line();
        assert_eq!(state.out, "hi");
    }
}
