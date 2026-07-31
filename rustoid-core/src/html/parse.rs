//! HTML → AST parser for round-tripping.
//!
//! Uses `html5ever` to parse Parsoid-format HTML back into our AST,
//! extracting `data-parsoid` and `data-mw` attributes.

use html5ever::driver::ParseOpts;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::dom::node::{self, ElementKind, Node};
use crate::error::{Result, RustoidError};

#[cfg(test)]
use crate::dom::node::NodeKind;

/// Parse a Parsoid HTML string into our AST.
pub fn parse_html(html: &str) -> Result<Node> {
    let opts = ParseOpts::default();
    let rc_dom = html5ever::parse_document(RcDom::default(), opts)
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .map_err(|e| RustoidError::Parse(format!("HTML parse error: {e}")))?;

    convert_document(&rc_dom.document)
}

/// Convert an RcDom document handle to our AST Node.
fn convert_document(handle: &Handle) -> Result<Node> {
    let _doc = Node::document();
    // Find the <html> → <body> → {content} and extract children directly
    if let Some(body) = find_body(handle) {
        return convert_body_content(body);
    }
    // Fallback: process the document directly
    let mut doc = Node::document();
    if let NodeData::Document = &handle.data {
        for child in handle.children.borrow().iter() {
            if let Some(node) = convert_node(child)? {
                doc.push_child(node);
            }
        }
    }
    Ok(doc)
}

/// Find the <body> element within a document.
fn find_body(handle: &Handle) -> Option<Handle> {
    if let NodeData::Document = &handle.data {
        for child in handle.children.borrow().iter() {
            if let NodeData::Element { name, .. } = &child.data {
                let tag = name.local.as_ref().to_lowercase();
                if tag == "html" {
                    // Look for body within html
                    for gc in child.children.borrow().iter() {
                        if let NodeData::Element { name, .. } = &gc.data
                            && name.local.as_ref().to_lowercase() == "body"
                        {
                            return Some(gc.clone());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Extract content children from a body element.
fn convert_body_content(body_handle: Handle) -> Result<Node> {
    let mut doc = Node::document();
    if let NodeData::Element { .. } = &body_handle.data {
        for child in body_handle.children.borrow().iter() {
            if let Some(node) = convert_node(child)? {
                doc.push_child(node);
            }
        }
    }
    Ok(doc)
}

/// Convert a single RcDom node to our AST Node.
/// Returns None for nodes that should be skipped.
fn convert_node(handle: &Handle) -> Result<Option<Node>> {
    match &handle.data {
        NodeData::Document => {
            let mut doc = Node::document();
            for child in handle.children.borrow().iter() {
                if let Some(node) = convert_node(child)? {
                    doc.push_child(node);
                }
            }
            Ok(Some(doc))
        }
        NodeData::Text { contents } => {
            let text = contents.borrow().to_string();
            Ok(Some(Node::text(text)))
        }
        NodeData::Element { name, attrs, .. } => {
            let tag_name = name.local.as_ref().to_lowercase();

            // Skip document structure elements — we only care about body content
            if matches!(tag_name.as_str(), "html" | "head" | "body" | "!doctype") {
                // Process children of body/html directly
                let mut nodes = Vec::new();
                for child in handle.children.borrow().iter() {
                    if let Some(node) = convert_node(child)? {
                        nodes.push(node);
                    }
                }
                if nodes.len() == 1 {
                    return Ok(Some(nodes.into_iter().next().unwrap()));
                }
                // Multiple children — wrap in a synthetic container
                let mut container = Node::element(ElementKind::Div);
                for n in nodes {
                    container.push_child(n);
                }
                return Ok(Some(container));
            }

            let attrs = attrs.borrow();

            // Extract data-parsoid and data-mw
            let mut data_parsoid = None;
            let mut data_mw = None;
            let mut html_attrs = Vec::new();

            for attr in attrs.iter() {
                let key = attr.name.local.as_ref();
                let value = attr.value.as_ref();
                match key {
                    "data-parsoid" => data_parsoid = Some(value.to_string()),
                    "data-mw" => data_mw = Some(value.to_string()),
                    _ => {
                        html_attrs.push(node::Attribute {
                            key: key.to_string(),
                            value: value.to_string(),
                        });
                    }
                }
            }

            let kind = html_tag_to_element_kind(&tag_name, &html_attrs);
            let mut node = Node::element(kind);
            node.attrs = html_attrs;
            node.data_parsoid = data_parsoid;
            node.data_mw = data_mw;

            // Convert children
            for child in handle.children.borrow().iter() {
                if let Some(child_node) = convert_node(child)? {
                    node.push_child(child_node);
                }
            }

            Ok(Some(node))
        }
        NodeData::Comment { contents } => Ok(Some(Node::comment(contents.to_string()))),
        _ => Ok(None),
    }
}

/// Map an HTML tag name to our ElementKind.
fn html_tag_to_element_kind(tag: &str, attrs: &[node::Attribute]) -> ElementKind {
    // Check typeof attribute for transclusions, annotations, etc.
    if let Some(type_of) = attrs.iter().find(|a| a.key == "typeof") {
        let types: Vec<&str> = type_of.value.split_whitespace().collect();
        for t in &types {
            match *t {
                "mw:Transclusion" => return ElementKind::Transclusion,
                "mw:Extension/ref" | "mw:Extension/references" => return ElementKind::ExtensionTag,
                "mw:Annotation/ref" | "mw:Annotation/dummyanno" | "mw:Annotation/ann2" => {
                    return ElementKind::Annotation;
                }
                "mw:ExpandedAttrs" => {} // Continue to check other types
                _ => {}
            }
        }
        // If any type contains "mw:Extension/", it's an extension
        if let Some(_ext) = types.iter().find(|t| t.starts_with("mw:Extension/")) {
            return ElementKind::ExtensionTag;
        }
    }

    // Check rel attribute for links
    if let Some(rel) = attrs.iter().find(|a| a.key == "rel") {
        if rel.value == "mw:PageProp/Category" {
            return ElementKind::CategoryLink;
        }
        if rel.value == "mw:WikiLink/Interwiki" {
            return ElementKind::Wikilink;
        }
    }

    match tag {
        "html" | "head" | "body" | "meta" | "link" | "title" | "base" => {
            ElementKind::Other(tag.to_string())
        }
        "p" => ElementKind::Paragraph,
        "h1" => ElementKind::Heading(1),
        "h2" => ElementKind::Heading(2),
        "h3" => ElementKind::Heading(3),
        "h4" => ElementKind::Heading(4),
        "h5" => ElementKind::Heading(5),
        "h6" => ElementKind::Heading(6),
        "b" | "strong" => ElementKind::Bold,
        "i" | "em" => ElementKind::Italic,
        "a" => {
            if attrs
                .iter()
                .any(|a| a.key == "rel" && a.value == "mw:ExtLink")
            {
                ElementKind::ExtLink
            } else if attrs.iter().any(|a| a.key == "href") {
                ElementKind::Wikilink
            } else {
                ElementKind::Span
            }
        }
        "ul" => ElementKind::UnorderedList,
        "ol" => ElementKind::OrderedList,
        "li" => ElementKind::ListItem,
        "dl" => ElementKind::DefinitionList,
        "dt" => ElementKind::DefinitionTerm,
        "dd" => ElementKind::DefinitionDescription,
        "table" => ElementKind::Table,
        "tr" => ElementKind::TableRow,
        "td" | "th" => ElementKind::TableCell,
        "caption" => ElementKind::TableCaption,
        "pre" => ElementKind::Preformatted,
        "hr" => ElementKind::HorizontalRule,
        "br" => ElementKind::LineBreak,
        "div" => ElementKind::Div,
        "span" => ElementKind::Span,
        "section" => ElementKind::Section,
        "figure" | "figure-inline" => ElementKind::Figure,
        "figcaption" => ElementKind::FigCaption,
        "img" => {
            // Check if it's a figure child
            ElementKind::Image
        }
        _ => ElementKind::Other(tag.to_string()),
    }
}

/// Check if a tag name represents a block-level element.
#[allow(dead_code)]
fn is_block_ish(tag: &str) -> bool {
    matches!(
        tag,
        "div"
            | "p"
            | "table"
            | "tr"
            | "td"
            | "th"
            | "ul"
            | "ol"
            | "li"
            | "dl"
            | "dt"
            | "dd"
            | "section"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "pre"
            | "blockquote"
            | "hr"
            | "figure"
            | "figcaption"
            | "body"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_html() {
        let html = "<!DOCTYPE html><html><head></head><body><p>Hello world</p></body></html>";
        let doc = parse_html(html).unwrap();
        assert!(!doc.children.is_empty());
    }

    #[test]
    fn test_parse_paragraph() {
        let html = "<p>Hello <b>world</b></p>";
        let doc = parse_html(html).unwrap();
        assert_eq!(doc.children.len(), 1);
        assert!(matches!(
            doc.children[0].kind,
            NodeKind::Element(ElementKind::Paragraph)
        ));
    }

    #[test]
    fn test_parse_parsoid_attributes() {
        let html = r#"<p data-parsoid='{"dsr":[0,10,0,0]}' data-mw='{"parts":[]}'>text</p>"#;
        let doc = parse_html(html).unwrap();
        let p = &doc.children[0];
        assert!(p.data_parsoid.is_some());
        assert!(p.data_mw.is_some());
    }

    #[test]
    fn test_parse_heading() {
        let html = "<h2>Title</h2>";
        let doc = parse_html(html).unwrap();
        assert!(matches!(
            doc.children[0].kind,
            NodeKind::Element(ElementKind::Heading(2))
        ));
    }

    #[test]
    fn test_parse_transclusion() {
        let html = r#"<span typeof="mw:Transclusion" data-mw='{"parts":[{"template":{"target":{"wt":"Foo"}}}]}'>content</span>"#;
        let doc = parse_html(html).unwrap();
        assert!(matches!(
            doc.children[0].kind,
            NodeKind::Element(ElementKind::Transclusion)
        ));
    }
}
