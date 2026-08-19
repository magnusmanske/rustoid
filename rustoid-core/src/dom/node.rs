//! AST node types.
//!
//! The AST is format-agnostic: elements carry semantic kind tags
//! rather than HTML tag names, and attributes are key-value pairs.
//! This allows the same tree to be lowered to HTML, JSON, Typst, etc.

// ---------------------------------------------------------------------------
// Element kinds
// ---------------------------------------------------------------------------

/// Semantic kind of an AST element node.
///
/// These correspond to wikitext constructs, not HTML elements.
/// Serialization backends map these to their output format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementKind {
    /// Root document node.
    Document,
    /// A paragraph of text/inline content.
    Paragraph,
    /// A heading section (level 1-6).
    Heading(u8),
    /// Bold text span.
    Bold,
    /// Italic text span.
    Italic,
    /// A wikilink (internal link).
    Wikilink,
    /// An external link (URL).
    ExtLink,
    /// An image or media file reference.
    Image,
    /// A gallery of images.
    Gallery,
    /// A table.
    Table,
    /// A table row.
    TableRow,
    /// A table cell (header or data).
    TableCell,
    /// A table header cell.
    TableHeader,
    /// A table caption.
    TableCaption,
    /// An unordered list.
    UnorderedList,
    /// An ordered list.
    OrderedList,
    /// A single list item.
    ListItem,
    /// A definition list (`;term :definition`).
    DefinitionList,
    /// A definition term.
    DefinitionTerm,
    /// A definition description.
    DefinitionDescription,
    /// Block of preformatted text.
    Preformatted,
    /// A horizontal rule.
    HorizontalRule,
    /// A template transclusion placeholder.
    Transclusion,
    /// An extension tag placeholder (e.g., `<ref>`, `<poem>`).
    ExtensionTag,
    /// An annotation placeholder.
    Annotation,
    /// A `<div>` wrapper (generic block container).
    Div,
    /// A `<span>` wrapper (generic inline container).
    Span,
    /// A line break.
    LineBreak,
    /// An HTML comment.
    Comment,
    /// Raw HTML passthrough.
    RawHtml,
    /// A `<section>` wrapper.
    Section,
    /// An interlanguage link marker.
    InterlanguageLink,
    /// A category link marker.
    CategoryLink,
    /// A redirect marker.
    Redirect,
    /// A table of contents placeholder.
    TableOfContents,
    /// A behavior indicator.
    Indicator,
    /// A figure wrapper (for media with captions).
    Figure,
    /// A figure caption.
    FigCaption,
    /// Generic catch-all for unknown/unclassified elements.
    Other(String),
}

// ---------------------------------------------------------------------------
// Node types
// ---------------------------------------------------------------------------

/// The type of an AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// The root document.
    Document,
    /// An element with a semantic kind and attributes.
    Element(ElementKind),
    /// A text run.
    Text(String),
    /// An HTML/XML comment.
    Comment(String),
}

/// A single node in the AST.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// The node type.
    pub kind: NodeKind,
    /// Child nodes (for Element and Document nodes).
    pub children: Vec<Node>,
    /// Attributes, if this node is an Element.
    pub attrs: Vec<Attribute>,
    /// The `data-parsoid` JSON blob for round-tripping.
    /// Stored as raw JSON for now; parsed on demand.
    pub data_parsoid: Option<String>,
    /// The `data-mw` JSON blob for transclusion/extension metadata.
    pub data_mw: Option<String>,
}

impl Node {
    /// Create a new document node.
    pub fn document() -> Self {
        Self {
            kind: NodeKind::Document,
            children: Vec::new(),
            attrs: Vec::new(),
            data_parsoid: None,
            data_mw: None,
        }
    }

    /// Create a new element node with the given kind.
    pub fn element(kind: ElementKind) -> Self {
        Self {
            kind: NodeKind::Element(kind),
            children: Vec::new(),
            attrs: Vec::new(),
            data_parsoid: None,
            data_mw: None,
        }
    }

    /// Create a new text node.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            kind: NodeKind::Text(content.into()),
            children: Vec::new(),
            attrs: Vec::new(),
            data_parsoid: None,
            data_mw: None,
        }
    }

    /// Create a new comment node.
    pub fn comment(content: impl Into<String>) -> Self {
        Self {
            kind: NodeKind::Comment(content.into()),
            children: Vec::new(),
            attrs: Vec::new(),
            data_parsoid: None,
            data_mw: None,
        }
    }

    /// Add a child node to this element/document.
    pub fn push_child(&mut self, child: Node) {
        self.children.push(child);
    }

    /// Set an attribute on this element.
    pub fn set_attr(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        // Replace if exists, otherwise append
        if let Some(attr) = self.attrs.iter_mut().find(|a| a.key == key) {
            attr.value = value.into();
        } else {
            self.attrs.push(Attribute {
                key,
                value: value.into(),
            });
        }
    }

    /// Get an attribute value by key.
    pub fn get_attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.key == key)
            .map(|a| a.value.as_str())
    }
}

/// A key-value attribute on an element node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub key: String,
    pub value: String,
}

/// Alias for readability.
pub type Element = Node;
pub type Document = Node;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_document() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("Hello, world!"));
        doc.push_child(p);

        assert_eq!(doc.children.len(), 1);
        assert_eq!(doc.children[0].children.len(), 1);
        assert!(matches!(
            doc.children[0].kind,
            NodeKind::Element(ElementKind::Paragraph)
        ));
    }

    #[test]
    fn test_attributes() {
        let mut img = Node::element(ElementKind::Image);
        img.set_attr("src", "Example.jpg");
        img.set_attr("width", "300");

        assert_eq!(img.get_attr("src"), Some("Example.jpg"));
        assert_eq!(img.get_attr("width"), Some("300"));
    }
}
