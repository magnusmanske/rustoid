//! Expanded-attribute fragments for `data-mw`.
//!
//! `WikiLinkHandler::renderFile` renders a media option whose value is a token
//! array (`alt= …''x''…`) through `PipelineUtils::expandAttrValueToDOM` and
//! stores the result as `data-mw.attribs[i].value.html`. The `AttributeExpander`
//! does the same for a templated attribute of a page element
//! (`title="{{#tag:nowiki|…}}"` in `data-mw.attribs[[{"txt":…},{"html":…}]]`).
//! Either way the fragment's nodes are part of the document: the id pass numbers
//! them, and Parsoid serializes a node's `data-mw` — numbering its embedded
//! fragments — *before* numbering the node itself.
//!
//! `render_file` and `build_expanded_attrs` tunnel each such fragment as a
//! `mw:dom-fragment-token` placeholder marked with [`MW_ATTR_KEY_ATTR`] instead
//! of splicing it into view. This module moves those placeholders onto their
//! container's [`Node::attr_mw_fragments`] ([`collect`]), and serializes each
//! fragment back into the matching `data-mw.attribs` entry's `html` once the id
//! pass has run ([`serialize_into_data_mw`]).

use crate::dom::node::Node;
use crate::pipeline::wiki_link_render::MW_ATTR_KEY_ATTR;

/// Move every marked option placeholder onto its parent's `attr_mw_fragments`.
///
/// Must run after tree building and before `unpack_dom_fragments`, which would
/// otherwise splice a placeholder's fragment into the visible tree.
pub fn collect(root: &mut Node) {
    for mut child in std::mem::take(&mut root.children) {
        if let Some(ck) = child.get_attr(MW_ATTR_KEY_ATTR).map(str::to_string) {
            if let Some(fragment) = child.fragment.take() {
                root.attr_mw_fragments.push((ck, fragment));
            }
            // The placeholder never renders: its fragment lives in `data-mw`.
            continue;
        }
        collect(&mut child);
        root.children.push(child);
    }
}

/// Serialize each node's option fragments into its `data-mw.attribs`.
///
/// Runs after `assign_node_ids`, so the fragments carry their final ids (Parsoid
/// numbers a node's embedded `data-mw` fragments before the node itself).
pub fn serialize_into_data_mw(root: &mut Node, strip_data_parsoid: bool) {
    for child in &mut root.children {
        serialize_into_data_mw(child, strip_data_parsoid);
    }
    if root.attr_mw_fragments.is_empty() {
        return;
    }
    let Some(raw) = root.data_mw.clone() else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    if !json.is_object() {
        return;
    }
    for (ck, fragment) in std::mem::take(&mut root.attr_mw_fragments) {
        let html = serialize_fragment(&fragment, strip_data_parsoid);
        set_attrib_html(&mut json, &ck, &html);
    }
    root.data_mw = Some(json.to_string());
}

/// Serialize a fragment to its `data-mw.attribs[i].value.html` spelling.
fn serialize_fragment(fragment: &Node, strip_data_parsoid: bool) -> String {
    let serializer = crate::html::serialize::HtmlSerializer::new(crate::options::ParserOptions {
        body_only: true,
        strip_data_parsoid,
        ..crate::options::ParserOptions::for_page("")
    });
    serializer.serialize(fragment).unwrap_or_default()
}

/// Set `html` on the `data-mw.attribs` entry keyed by `ck`.
///
/// The entry's key is a plain string for a media option (`["alt", {…}]`) and a
/// `{"txt":…}` object for an expanded attribute (`[{"txt":"title"}, {…}]`).
fn set_attrib_html(json: &mut serde_json::Value, ck: &str, html: &str) {
    let Some(attribs) = json.get_mut("attribs").and_then(|a| a.as_array_mut()) else {
        return;
    };
    for pair in attribs.iter_mut() {
        let Some(arr) = pair.as_array_mut() else {
            continue;
        };
        let key_matches = match arr.first() {
            Some(serde_json::Value::String(s)) => s == ck,
            Some(serde_json::Value::Object(o)) => o.get("txt").and_then(|t| t.as_str()) == Some(ck),
            _ => false,
        };
        if !key_matches {
            continue;
        }
        if let Some(obj) = arr.get_mut(1).and_then(|v| v.as_object_mut()) {
            obj.insert(
                "html".to_string(),
                serde_json::Value::String(html.to_string()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, NodeKind};

    fn placeholder(ck: &str, text: &str) -> Node {
        let mut ph = Node::element(ElementKind::Other("span".to_string()));
        ph.set_attr(MW_ATTR_KEY_ATTR, ck);
        ph.fragment = Some(Box::new(Node::document_with_child(Node::text(text))));
        ph
    }

    #[test]
    fn collect_moves_a_marked_placeholder_onto_its_parent() {
        let mut figure = Node::element(ElementKind::Figure);
        figure.push_child(placeholder("alt", "A fossil skull"));
        figure.push_child(Node::text("visible"));
        let mut root = Node::document();
        root.push_child(figure);

        collect(&mut root);

        let figure = &root.children[0];
        assert!(matches!(figure.kind, NodeKind::Element(_)));
        assert_eq!(figure.attr_mw_fragments.len(), 1);
        assert_eq!(figure.attr_mw_fragments[0].0, "alt");
        assert_eq!(figure.children.len(), 1, "the placeholder is removed");
    }

    #[test]
    fn serialize_fills_the_matching_attrib_html() {
        let mut figure = Node::element(ElementKind::Figure);
        figure.data_mw = Some(r#"{"attribs":[["alt",{"txt":"x","html":""}]]}"#.to_string());
        figure.attr_mw_fragments = vec![(
            "alt".to_string(),
            Box::new(Node::document_with_child(Node::text("A fossil skull"))),
        )];
        let mut root = Node::document();
        root.push_child(figure);

        serialize_into_data_mw(&mut root, false);

        let dmw = root.children[0].data_mw.as_deref().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(dmw).unwrap();
        assert_eq!(parsed["attribs"][0][1]["html"], "A fossil skull");
    }

    #[test]
    fn serialize_fills_an_expanded_attr_html() {
        // An expanded attribute keys its `data-mw.attribs` entry by a
        // `{"txt":…}` object rather than a bare string.
        let mut span = Node::element(ElementKind::Span);
        span.data_mw = Some(r#"{"attribs":[[{"txt":"title"},{"html":""}]]}"#.to_string());
        span.attr_mw_fragments = vec![(
            "title".to_string(),
            Box::new(Node::document_with_child(Node::text("Zebras"))),
        )];
        let mut root = Node::document();
        root.push_child(span);

        serialize_into_data_mw(&mut root, false);

        let dmw = root.children[0].data_mw.as_deref().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(dmw).unwrap();
        assert_eq!(parsed["attribs"][0][1]["html"], "Zebras");
    }
}
