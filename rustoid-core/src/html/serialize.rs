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
        // The tree builder produces `<html><head>…</head><body>…</body></html>`;
        // flatten that structure so we don't double-wrap it here.
        let (head_children, body_children) = split_structure(doc);

        let mut buf = String::new();

        if !self.options.body_only {
            buf.push_str("<!DOCTYPE html>\n");
            buf.push_str("<html");
            if !self.options.language.is_empty() {
                buf.push_str(&format!(" lang=\"{}\"", self.options.language));
            }
            buf.push_str(">\n<head>\n");
            buf.push_str("<meta charset=\"utf-8\"/>\n");
            for child in &head_children {
                self.serialize_node(child, &mut buf, 0)?;
            }
            buf.push_str("</head>\n<body>\n");
        }

        for child in &body_children {
            self.serialize_node(child, &mut buf, 0)?;
        }

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
                        // `<pre>` is a raw-text escaping element in HTML
                        // serialization; its text nodes are escaped like any other
                        // element (`&` → `&amp;`, `<` → `&lt;`).
                        //
                        // `<pre>`/`<textarea>`/`<listing>` are newline-stripping
                        // elements (HTML fragment serialization): if the first
                        // child is a text node whose data starts with `\n`, append
                        // an extra `\n` so a re-parse of the output preserves the
                        // leading newline (mirrors `XHtmlSerializer::NEWLINE_
                        // STRIPPING_ELEMENTS` in PHP).
                        if let Some(NodeKind::Text(first)) = node.children.first().map(|c| &c.kind)
                            && first.starts_with('\n')
                        {
                            buf.push('\n');
                        }
                        self.serialize_children(node, buf, depth)?;
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
                        self.serialize_children(node, buf, depth + 1)?;
                        buf.push_str(&format!("{indent}</tr>"));
                    }
                    ElementKind::TableCell => {
                        buf.push_str(&format!("{indent}<td"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</td>");
                    }
                    ElementKind::TableHeader => {
                        buf.push_str(&format!("{indent}<th"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</th>");
                    }
                    ElementKind::UnorderedList => {
                        buf.push_str(&format!("{indent}<ul"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth + 1)?;
                        buf.push_str("</ul>");
                    }
                    ElementKind::OrderedList => {
                        buf.push_str(&format!("{indent}<ol"));
                        self.serialize_attrs(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth + 1)?;
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
                        buf.push_str(&format!("{indent}<hr/>"));
                    }
                    ElementKind::Wikilink => {
                        let href = attr_escape(node.get_attr("href").unwrap_or(""));
                        let rel = attr_escape(node.get_attr("rel").unwrap_or("mw:WikiLink"));
                        buf.push_str(&format!("<a rel=\"{rel}\" href=\"{href}\""));
                        self.serialize_attrs_skip_rel(node, buf);
                        buf.push('>');
                        self.serialize_children(node, buf, depth)?;
                        buf.push_str("</a>");
                    }
                    ElementKind::ExtLink => {
                        let href = attr_escape(node.get_attr("href").unwrap_or(""));
                        let rel = attr_escape(node.get_attr("rel").unwrap_or("mw:ExtLink"));
                        buf.push_str(&format!("<a rel=\"{rel}\" href=\"{href}\""));
                        self.serialize_attrs_skip_rel(node, buf);
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
                            self.serialize_attrs_full(node, buf);
                            buf.push_str("/>");
                        } else {
                            buf.push_str(&format!("{indent}<{tag}"));
                            self.serialize_attrs_full(node, buf);
                            buf.push('>');
                            self.serialize_children(node, buf, depth)?;
                            buf.push_str(&format!("</{tag}>"));
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
                // The comment content is already DOM-escaped (the tokenizer
                // applies `WTUtils::encodeComment`). Serialize it verbatim.
                buf.push_str(&format!("<!--{content}-->"));
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
        self.serialize_attrs_impl(node, buf, false);
    }

    /// Serialize a `<a>` element's remaining attributes, skipping `rel`/`href`/`src`
    /// which are emitted inline by the `Wikilink`/`ExtLink` arms (so they are not
    /// duplicated). Mirrors PHP, where `buildLinkAttrs` emits `rel` once and
    /// `addNormalizedAttribute` handles `href`.
    fn serialize_attrs_skip_rel(&self, node: &Node, buf: &mut String) {
        let attrs = node
            .attrs
            .iter()
            .filter(|a| a.key != "href" && a.key != "src" && a.key != "rel");
        for attr in attrs {
            serialize_attr(attr, buf);
        }
        if let Some(ref dp) = node.data_parsoid {
            let escaped = dp.replace('&', "&amp;").replace('\'', "&#39;");
            buf.push_str(&format!(" data-parsoid='{escaped}'"));
        }
        if let Some(ref dm) = node.data_mw {
            let escaped = dm.replace('&', "&amp;").replace('\'', "&#39;");
            buf.push_str(&format!(" data-mw='{escaped}'"));
        }
    }

    /// Like [`serialize_attrs`], but keeps `href`/`src` attributes. Used for
    /// generic elements (e.g. `<link rel="mw:PageProp/redirect">`) where the
    /// attributes are not emitted inline by a special-cased arm.
    fn serialize_attrs_full(&self, node: &Node, buf: &mut String) {
        self.serialize_attrs_impl(node, buf, true);
    }

    fn serialize_attrs_impl(&self, node: &Node, buf: &mut String, include_href_src: bool) {
        // Preserve attribute insertion order (PHP Parsoid emits attributes in
        // the order they were set, e.g. `rel` before `href` on redirect links).
        let attrs = node
            .attrs
            .iter()
            .filter(|a| include_href_src || (a.key != "href" && a.key != "src"));
        for attr in attrs {
            serialize_attr(attr, buf);
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
            ElementKind::TableHeader => "th",
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

/// Split a document node into `(head_children, body_children)`, flattening the
/// `<html><head>…</head><body>…</body></html>` structure produced by the
/// tree-construction fragment builder. If the document is a plain fragment
/// (no structural `<html>`), `head_children` is empty and `body_children` is
/// the document's own children.
fn split_structure(doc: &Node) -> (Vec<Node>, Vec<Node>) {
    // Look for an `<html>` element among the top-level children.
    for child in &doc.children {
        if let NodeKind::Element(ElementKind::Other(tag)) = &child.kind
            && tag == "html"
        {
            let mut head = Vec::new();
            let mut body = Vec::new();
            let mut has_structural = false;
            for section in &child.children {
                if let NodeKind::Element(ElementKind::Other(tag2)) = &section.kind {
                    match tag2.as_str() {
                        "head" => {
                            head = section.children.clone();
                            has_structural = true;
                        }
                        "body" => {
                            body = section.children.clone();
                            has_structural = true;
                        }
                        _ => {}
                    }
                }
            }
            if has_structural {
                return (head, body);
            }
            // Fragment mode: no <head>/<body> wrappers — all children are body.
            return (Vec::new(), child.children.clone());
        }
    }

    (Vec::new(), doc.children.clone())
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

/// Serialize a single attribute. JSON data attributes set via `DOMDataUtils`
/// (`data-mw-i18n`, and the `data-parsoid`/`data-mw` fields handled separately)
/// are emitted single-quoted with raw inner quotes, matching PHP's
/// `XHtmlSerializer`. All other attributes are double-quoted with full escaping.
fn serialize_attr(attr: &crate::dom::node::Attribute, buf: &mut String) {
    if attr.key.starts_with("data-mw-i18n") {
        let escaped = attr.value.replace('&', "&amp;").replace('\'', "&#39;");
        buf.push_str(&format!(" {}='{escaped}'", attr.key));
    } else {
        buf.push_str(&format!(" {}=\"{}\"", attr.key, attr_escape(&attr.value)));
    }
}

/// Basic HTML entity escaping for text content.
/// Mirrors Parsoid's `XHtmlSerializer::ENTITY_ENCODINGS_XML`: escapes `&` and
/// `<` (plus U+0338 COMBINING LONG SOLIDUS OVERLAY as `&#x338;`). Unlike the
/// legacy PHP parser, `>` is *not* escaped in text nodes.
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

        let opts = ParserOptions {
            body_only: true,
            ..ParserOptions::default()
        };
        let serializer = HtmlSerializer::new(opts);
        let html = serializer.serialize(&doc).unwrap();
        assert!(!html.contains("<!DOCTYPE"));
        assert!(html.contains("<p>"));
    }

    #[test]
    fn test_html_escape() {
        // html_escape: escapes & and < (matches Parsoid XHtmlSerializer).
        assert_eq!(html_escape("<>&\"'"), "&lt;>&amp;\"'");
        // attr_escape: also escapes " and '
        assert_eq!(attr_escape("<>&\"'"), "&lt;>&amp;&quot;&#39;");
    }
}
