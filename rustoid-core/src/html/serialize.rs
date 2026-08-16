//! HTML serializer — AST → HTML string.
//!
//! This backend produces Parsoid-compatible HTML5 output, including
//! `data-parsoid` and `data-mw` attributes for round-tripping.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::error::Result;
use crate::options::ParserOptions;

/// Serialize an AST to an HTML string.
pub struct HtmlSerializer {
    options: ParserOptions,
}

impl HtmlSerializer {
    /// Create a new HTML serializer with the given options.
    pub fn new(options: ParserOptions) -> Self {
        Self { options }
    }

    /// Serialize a document node to HTML.
    pub fn serialize(&self, doc: &Node) -> Result<String> {
        let mut buf = String::new();

        if !self.options.body_only {
            buf.push_str("<!DOCTYPE html>\n");
            buf.push_str("<html");
            if !self.options.language.is_empty() {
                buf.push_str(&format!(" lang=\"{}\"", self.options.language));
            }
            buf.push_str(">\n<head>\n");
            buf.push_str("<meta charset=\"utf-8\"/>\n");
            buf.push_str("</head>\n<body>\n");
        }

        self.serialize_node(doc, &mut buf, 0)?;

        if !self.options.body_only {
            buf.push_str("</body>\n</html>\n");
        }

        Ok(buf)
    }

    fn serialize_node(&self, node: &Node, buf: &mut String, depth: usize) -> Result<()> {
        self.serialize_node_esc(node, buf, depth, true)
    }

    fn serialize_node_esc(
        &self,
        node: &Node,
        buf: &mut String,
        depth: usize,
        escape: bool,
    ) -> Result<()> {
        let indent = "  ".repeat(depth);

        match &node.kind {
            NodeKind::Document => {
                for child in node.children.iter() {
                    self.serialize_node(child, buf, depth)?;
                }
            }
            NodeKind::Element(kind) => {
                let _tag = self.element_tag(kind);
                match kind {
                    ElementKind::Paragraph => {
                        buf.push_str(&format!("{indent}<p"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth + 1)?;
                        buf.push_str("</p>");
                    }
                    ElementKind::Heading(level) => {
                        let h = format!("h{level}");
                        buf.push_str(&format!("{indent}<{h}"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str(&format!("</{h}>"));
                    }
                    ElementKind::Bold => {
                        buf.push_str("<b>");
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</b>");
                    }
                    ElementKind::Italic => {
                        buf.push_str("<i>");
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</i>");
                    }
                    ElementKind::Preformatted => {
                        buf.push_str(&format!("{indent}<pre"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children_esc(node, buf, depth, false)?;
                        buf.push_str("</pre>");
                    }
                    ElementKind::Table => {
                        buf.push_str(&format!("{indent}<table"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth + 1)?;
                        buf.push_str(&format!("{indent}</table>"));
                    }
                    ElementKind::TableRow => {
                        buf.push_str(&format!("{indent}<tr"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        if !node.children.is_empty() {
                            buf.push('\n');
                            self.serialize_children(node, buf, depth + 1)?;
                            buf.push_str(&format!("{indent}</tr>"));
                        } else {
                            buf.push_str("</tr>");
                        }
                    }
                    ElementKind::TableCell => {
                        buf.push_str(&format!("{indent}<td"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</td>");
                    }
                    ElementKind::UnorderedList => {
                        buf.push_str(&format!("{indent}<ul"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        let count = node.children.len();
                        for (i, child) in node.children.iter().enumerate() {
                            self.serialize_node(child, buf, 0)?;
                            // Add newline between items but not after the last
                            if i + 1 < count
                                && matches!(child.kind, NodeKind::Element(ElementKind::ListItem))
                            {
                                buf.push('\n');
                            }
                        }
                        buf.push_str("</ul>");
                    }
                    ElementKind::OrderedList => {
                        buf.push_str(&format!("{indent}<ol"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        let count = node.children.len();
                        for (i, child) in node.children.iter().enumerate() {
                            self.serialize_node(child, buf, 0)?;
                            if i + 1 < count
                                && matches!(child.kind, NodeKind::Element(ElementKind::ListItem))
                            {
                                buf.push('\n');
                            }
                        }
                        buf.push_str("</ol>");
                    }
                    ElementKind::ListItem => {
                        buf.push_str("<li");
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, 0)?;
                        buf.push_str("</li>");
                    }
                    ElementKind::Div => {
                        buf.push_str(&format!("{indent}<div"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        if !node.children.is_empty() {
                            self.serialize_children(node, buf, depth + 1)?;
                            buf.push_str(&format!("{indent}</div>"));
                        } else {
                            buf.push_str("</div>");
                        }
                    }
                    ElementKind::Span => {
                        buf.push_str("<span");
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</span>");
                    }
                    ElementKind::LineBreak => {
                        buf.push_str("<br/>");
                    }
                    ElementKind::HorizontalRule => {
                        buf.push_str(&format!("{indent}<hr/>\n"));
                    }
                    ElementKind::Wikilink => {
                        let href = node.get_attr("href").unwrap_or("");
                        buf.push_str(&format!("<a rel=\"mw:WikiLink\" href=\"{href}\""));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</a>");
                    }
                    ElementKind::ExtLink => {
                        let href = node.get_attr("href").unwrap_or("");
                        buf.push_str(&format!("<a rel=\"mw:ExtLink\" href=\"{href}\""));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</a>");
                    }
                    ElementKind::Image => {
                        buf.push_str("<figure-inline>");
                        buf.push_str(&format!(
                            "<img src=\"{}\"/>",
                            node.get_attr("src").unwrap_or("")
                        ));
                        buf.push_str("</figure-inline>");
                    }
                    _ => {
                        // Generic element serialization.
                        let tag = self.element_tag(kind);
                        // Void/self-closing elements (meta, link, img, etc.)
                        // serialize without a closing tag.
                        if is_void_element(tag) {
                            buf.push_str(&format!("{indent}<{tag}"));
                            self.serialize_attrs(node, buf);
                            buf.push_str("/>\n");
                        } else {
                            buf.push_str(&format!("{indent}<{tag}"));
                            self.serialize_attrs(node, buf);
                            buf.push('>');
                            self.serialize_children(node, buf, depth)?;
                            buf.push_str(&format!("</{tag}>\n"));
                        }
                    }
                }
            }
            NodeKind::Text(text) => {
                if escape {
                    buf.push_str(&html_escape(text));
                } else {
                    buf.push_str(text);
                }
            }
            NodeKind::Comment(content) => {
                // Escape per Parsoid: & -> &#x26;, then - -> &#x2D;, then > -> &#x3E;
                let escaped = content
                    .replace("&", "&#x26;")
                    .replace("-", "&#x2D;")
                    .replace(">", "&#x3E;");
                buf.push_str(&format!("<!--{escaped}-->"));
            }
        }

        Ok(())
    }

    fn serialize_children(&self, node: &Node, buf: &mut String, depth: usize) -> Result<()> {
        self.serialize_children_esc(node, buf, depth, true)
    }

    fn serialize_children_esc(
        &self,
        node: &Node,
        buf: &mut String,
        depth: usize,
        escape: bool,
    ) -> Result<()> {
        for child in &node.children {
            self.serialize_node_esc(child, buf, depth, escape)?;
        }
        Ok(())
    }

    fn serialize_attrs(&self, node: &Node, buf: &mut String) {
        // Sort attributes for deterministic output
        let mut sorted: Vec<_> = node
            .attrs
            .iter()
            .filter(|a| a.key != "href" && a.key != "src")
            .collect();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        for attr in &sorted {
            buf.push_str(&format!(" {}=\"{}\"", attr.key, attr_escape(&attr.value)));
        }
        // Add data-parsoid and data-mw if present
        if let Some(ref dp) = node.data_parsoid {
            // Single-quoted attribute: only escape & and '
            let escaped = dp.replace('&', "&amp;").replace('\'', "&#39;");
            buf.push_str(&format!(" data-parsoid='{escaped}'"));
        }
        if let Some(ref dm) = node.data_mw {
            let escaped = dm.replace('&', "&amp;").replace('\'', "&#39;");
            buf.push_str(&format!(" data-mw='{escaped}'"));
        }
    }

    /// Map an ElementKind to its HTML tag name.
    fn element_tag<'a>(&self, kind: &'a ElementKind) -> &'a str {
        match kind {
            ElementKind::Document => "html",
            ElementKind::Paragraph => "p",
            ElementKind::Heading(1) => "h1",
            ElementKind::Heading(2) => "h2",
            ElementKind::Heading(3) => "h3",
            ElementKind::Heading(4) => "h4",
            ElementKind::Heading(5) => "h5",
            ElementKind::Heading(6) => "h6",
            ElementKind::Bold => "b",
            ElementKind::Italic => "i",
            ElementKind::Wikilink | ElementKind::ExtLink => "a",
            ElementKind::Image => "figure-inline",
            ElementKind::Gallery => "ul",
            ElementKind::Table => "table",
            ElementKind::TableRow => "tr",
            ElementKind::TableCell => "td",
            ElementKind::TableCaption => "caption",
            ElementKind::UnorderedList => "ul",
            ElementKind::OrderedList => "ol",
            ElementKind::ListItem => "li",
            ElementKind::DefinitionList => "dl",
            ElementKind::DefinitionTerm => "dt",
            ElementKind::DefinitionDescription => "dd",
            ElementKind::Preformatted => "pre",
            ElementKind::HorizontalRule => "hr",
            ElementKind::Transclusion => "span",
            ElementKind::ExtensionTag => "span",
            ElementKind::Annotation => "meta",
            ElementKind::Div => "div",
            ElementKind::Span => "span",
            ElementKind::LineBreak => "br",
            ElementKind::Comment => "",
            ElementKind::RawHtml => "",
            ElementKind::Section => "section",
            ElementKind::InterlanguageLink => "link",
            ElementKind::CategoryLink => "link",
            ElementKind::Redirect => "link",
            ElementKind::TableOfContents => "div",
            ElementKind::Indicator => "div",
            ElementKind::Figure => "figure",
            ElementKind::FigCaption => "figcaption",
            ElementKind::Heading(_) => "h2",
            ElementKind::Other(name) => name.as_str(),
        }
    }
}

/// Whether a tag is void (self-closing) in HTML serialization. Mirrors the
/// HTML void-element list that Parsoid emits without an explicit close tag.
fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Basic HTML entity escaping for text content.
/// Escapes &, <, and > per HTML5 recommendation.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

/// HTML entity escaping for attribute values (also escapes quotes).
fn attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::ElementKind;

    #[test]
    fn test_empty_document() {
        let doc = Node::document();
        let serializer = HtmlSerializer::new(ParserOptions::default());
        let html = serializer.serialize(&doc).unwrap();
        assert!(html.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn test_simple_paragraph() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("Hello, world!"));
        doc.push_child(p);

        let serializer = HtmlSerializer::new(ParserOptions::default());
        let html = serializer.serialize(&doc).unwrap();
        assert!(html.contains("<p>"));
        assert!(html.contains("Hello, world!"));
        assert!(html.contains("</p>"));
    }

    #[test]
    fn test_bold_italic() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        let mut b = Node::element(ElementKind::Bold);
        b.push_child(Node::text("bold"));
        p.push_child(b);
        p.push_child(Node::text(" and "));
        let mut i = Node::element(ElementKind::Italic);
        i.push_child(Node::text("italic"));
        p.push_child(i);
        doc.push_child(p);

        let serializer = HtmlSerializer::new(ParserOptions::default());
        let html = serializer.serialize(&doc).unwrap();
        assert!(html.contains("<b>bold</b>"));
        assert!(html.contains("<i>italic</i>"));
    }

    #[test]
    fn test_body_only() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("text"));
        doc.push_child(p);

        let mut opts = ParserOptions::default();
        opts.body_only = true;
        let serializer = HtmlSerializer::new(opts);
        let html = serializer.serialize(&doc).unwrap();
        assert!(!html.contains("<!DOCTYPE"));
        assert!(html.contains("<p>"));
    }

    #[test]
    fn test_html_escape() {
        // html_escape: escapes &, <, and >
        assert_eq!(html_escape("<>&\"'"), "&lt;>&amp;\"'");
        // attr_escape: also escapes " and '
        assert_eq!(attr_escape("<>&\"'"), "&lt;>&amp;&quot;&#39;");
    }
}
