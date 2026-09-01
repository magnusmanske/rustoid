//! A `TreeHandler` that builds the crate's `Node` AST, mirroring how Parsoid's
//! `DOMBuilder` consumes RemexHtml tree events to produce DOM nodes.
//!
//! Nodes are kept in an arena of `Rc<RefCell<Node>>` (so merge/reparent can
//! mutate a node after it has been attached), with parent/child links tracked
//! separately by arena index. The owned `Node` tree is materialized at
//! `finish()`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::dom::node::{ElementKind, Node, NodeKind};

use super::element::{Attributes, Element};
use super::tree_handler::{Preposition, TreeHandler};

/// Normalize an attribute value per the HTML5 tree-construction algorithm: any
/// U+000A LF, U+000C FF, U+000D CR, or U+0009 TAB is replaced with U+0020 SPACE.
fn normalize_attr_value(value: &str) -> String {
    if !value
        .chars()
        .any(|c| matches!(c, '\n' | '\u{000C}' | '\r' | '\t'))
    {
        return value.to_string();
    }
    value
        .chars()
        .map(|c| match c {
            '\n' | '\u{000C}' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect()
}

/// A node reference within the arena.
type DomNode = Rc<RefCell<Node>>;

/// Builds a `Node` document from tree-builder events.
pub struct NodeTreeHandler {
    /// The arena of nodes.
    arena: Vec<DomNode>,
    /// Children of each arena index (in document order).
    children: Vec<Vec<usize>>,
    /// Parent arena index of each node (`None` for the root).
    parents: Vec<Option<usize>>,
    /// Maps element `uid` → arena index.
    uids: HashMap<usize, usize>,
    /// The root arena index.
    root: usize,
}

impl NodeTreeHandler {
    pub fn new() -> Self {
        let root_node = Rc::new(RefCell::new(Node::document()));
        Self {
            arena: vec![Rc::clone(&root_node)],
            children: vec![Vec::new()],
            parents: vec![None],
            uids: HashMap::new(),
            root: 0,
        }
    }

    /// Freeze the arena into an owned `Node` tree rooted at `root`.
    pub fn finish(self) -> Node {
        self.materialize(self.root)
    }

    /// The `data-object-id` attribute value of the node for `uid`, if any.
    /// Used by the tree-builder stage to correlate a popped element (including an
    /// AFE-reconstructed clone, whose attributes were copied from its
    /// formatting-element source) with its stashed node data.
    pub fn data_object_id(&self, uid: usize) -> Option<String> {
        let &idx = self.uids.get(&uid)?;
        let node = self.arena[idx].borrow();
        node.get_attr("data-object-id").map(str::to_string)
    }

    fn materialize(&self, idx: usize) -> Node {
        let node = self.arena[idx].borrow().clone();
        let mut node = node;
        let mut children = Vec::new();
        for &child_idx in &self.children[idx] {
            children.push(self.materialize(child_idx));
        }
        node.children = children;
        node
    }

    /// The arena index that a `reference` uid points to, or the root.
    fn reference_idx(&self, reference: Option<usize>) -> usize {
        reference
            .and_then(|uid| self.uids.get(&uid).copied())
            .unwrap_or(self.root)
    }

    /// Attach a child (arena index or text) under a parent arena index.
    fn attach_under(&mut self, parent_idx: usize, child: Child) {
        match child {
            Child::Node(idx) => {
                self.children[parent_idx].push(idx);
                self.parents[idx] = Some(parent_idx);
            }
            Child::Text(text) => {
                // Merge with the last text child if it is text.
                if let Some(&last) = self.children[parent_idx].last() {
                    let mut last_node = self.arena[last].borrow_mut();
                    if let NodeKind::Text(existing) = &mut last_node.kind {
                        existing.push_str(&text);
                        return;
                    }
                }
                let text_node = Rc::new(RefCell::new(Node::text(text)));
                let idx = self.arena.len();
                self.arena.push(text_node);
                self.children.push(Vec::new());
                self.parents.push(Some(parent_idx));
                self.children[parent_idx].push(idx);
            }
        }
    }

    /// Insert a child (arena index or text) immediately before the reference
    /// arena index (i.e. as its previous sibling). Falls back to attaching under
    /// the reference's parent if the reference is not yet linked into a children
    /// list (it should always be by the time foster-parenting fires).
    fn insert_before(&mut self, ref_idx: usize, child: Child) {
        let Some(parent_idx) = self.parents[ref_idx] else {
            self.attach_under(self.root, child);
            return;
        };
        let Some(pos) = self.children[parent_idx].iter().position(|&c| c == ref_idx) else {
            self.attach_under(parent_idx, child);
            return;
        };
        match child {
            Child::Node(idx) => {
                self.children[parent_idx].insert(pos, idx);
                self.parents[idx] = Some(parent_idx);
            }
            Child::Text(text) => {
                // Merge into an immediately preceding text sibling when possible.
                if pos > 0 {
                    let &prev = &self.children[parent_idx][pos - 1];
                    let mut prev_node = self.arena[prev].borrow_mut();
                    if let NodeKind::Text(existing) = &mut prev_node.kind {
                        existing.push_str(&text);
                        return;
                    }
                }
                let text_node = Rc::new(RefCell::new(Node::text(text)));
                let idx = self.arena.len();
                self.arena.push(text_node);
                self.children.push(Vec::new());
                self.parents.push(Some(parent_idx));
                self.children[parent_idx].insert(pos, idx);
            }
        }
    }

    fn place(&mut self, preposition: Preposition, reference: Option<usize>, child: Child) {
        match preposition {
            Preposition::Root => {
                self.attach_under(self.root, child);
            }
            Preposition::Before => {
                // Foster parenting: insert as a sibling immediately before the
                // reference element (rather than under it).
                let ref_idx = self.reference_idx(reference);
                self.insert_before(ref_idx, child);
            }
            Preposition::Under => {
                let parent = self.reference_idx(reference);
                self.attach_under(parent, child);
            }
        }
    }

    fn kind_for(name: &str) -> ElementKind {
        match name {
            "div" => ElementKind::Div,
            "span" => ElementKind::Span,
            "p" => ElementKind::Paragraph,
            "b" => ElementKind::Bold,
            "i" => ElementKind::Italic,
            "table" => ElementKind::Table,
            "tr" => ElementKind::TableRow,
            "td" => ElementKind::TableCell,
            "th" => ElementKind::TableHeader,
            "caption" => ElementKind::TableCaption,
            "ul" => ElementKind::UnorderedList,
            "ol" => ElementKind::OrderedList,
            "li" => ElementKind::ListItem,
            "dl" => ElementKind::DefinitionList,
            "dt" => ElementKind::DefinitionTerm,
            "dd" => ElementKind::DefinitionDescription,
            "pre" => ElementKind::Preformatted,
            "hr" => ElementKind::HorizontalRule,
            "br" => ElementKind::LineBreak,
            // Internal self-closing tag names emitted by the V2 tokenizer before
            // the wiki-link renderer expands them to `<a rel="mw:WikiLink">`.
            "wikilink" => ElementKind::Wikilink,
            "extlink" => ElementKind::ExtLink,
            "h1" => ElementKind::Heading(1),
            "h2" => ElementKind::Heading(2),
            "h3" => ElementKind::Heading(3),
            "h4" => ElementKind::Heading(4),
            "h5" => ElementKind::Heading(5),
            "h6" => ElementKind::Heading(6),
            "figure" => ElementKind::Figure,
            "figcaption" => ElementKind::FigCaption,
            "section" => ElementKind::Section,
            other => ElementKind::Other(other.to_string()),
        }
    }

    /// Classify an `<a>` element by its `rel` attribute so the serializer can
    /// emit `href`/`rel` correctly.
    fn a_kind(element: &Element) -> ElementKind {
        if let Some(rel) = element.attrs.get("rel") {
            if rel == "mw:WikiLink" {
                return ElementKind::Wikilink;
            }
            if rel == "mw:ExtLink" {
                return ElementKind::ExtLink;
            }
            if rel == "mw:ExtLink/interwiki" || rel == "mw:WikiLink/Interwiki" {
                return ElementKind::Wikilink;
            }
        }
        ElementKind::Other("a".to_string())
    }
}

impl Default for NodeTreeHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// A child to attach: an existing arena element or a text run.
enum Child {
    Node(usize),
    Text(String),
}

impl TreeHandler for NodeTreeHandler {
    fn start_document(&mut self, _fragment_ns: Option<&str>, _fragment_name: Option<&str>) {
        let root_node = Rc::new(RefCell::new(Node::document()));
        self.arena.clear();
        self.arena.push(root_node);
        self.children.clear();
        self.children.push(Vec::new());
        self.parents.clear();
        self.parents.push(None);
        self.uids.clear();
        self.root = 0;
    }

    fn end_document(&mut self, _pos: usize) {}

    fn characters(
        &mut self,
        preposition: Preposition,
        reference: Option<usize>,
        text: &str,
        start: usize,
        length: usize,
        _source_start: usize,
        _source_length: usize,
    ) {
        let text = &text[start..start + length];
        self.place(preposition, reference, Child::Text(text.to_string()));
    }

    fn insert_element(
        &mut self,
        preposition: Preposition,
        reference: Option<usize>,
        element: &mut Element,
        _void: bool,
        _source_start: usize,
        _source_length: usize,
    ) {
        let kind = Self::kind_for(&element.name);
        let kind = if element.name == "a" {
            Self::a_kind(element)
        } else {
            kind
        };
        let mut node = Node::element(kind);
        for (k, v) in element.attrs.get_values() {
            // HTML5 tree-construction attribute-value normalization
            // ("create an element for a token"): replace LF/FF/CR/TAB with
            // U+0020 SPACE, so `<pre class="one\ntwo">` yields `one two`.
            node.set_attr(k.clone(), normalize_attr_value(v));
        }
        let dom = Rc::new(RefCell::new(node));
        let idx = self.arena.len();
        self.arena.push(dom);
        self.children.push(Vec::new());
        self.parents.push(None);
        element.user_data = idx;
        self.uids.insert(element.uid, idx);
        self.place(preposition, reference, Child::Node(idx));
    }

    fn end_tag(&mut self, _element: &Element, _source_start: usize, _source_length: usize) {}

    fn doctype(
        &mut self,
        _name: &str,
        _public: &str,
        _system: &str,
        _quirks: u8,
        _source_start: usize,
        _source_length: usize,
    ) {
    }

    fn comment(
        &mut self,
        preposition: Preposition,
        reference: Option<usize>,
        text: &str,
        _source_start: usize,
        _source_length: usize,
    ) {
        let dom = Rc::new(RefCell::new(Node::comment(text)));
        let idx = self.arena.len();
        self.arena.push(dom);
        self.children.push(Vec::new());
        self.parents.push(None);
        self.place(preposition, reference, Child::Node(idx));
    }

    fn error(&mut self, _text: &str, _pos: usize) {}

    fn merge_attributes(&mut self, element: &Element, attrs: &Attributes, _source_start: usize) {
        if let Some(&idx) = self.uids.get(&element.uid) {
            let mut node = self.arena[idx].borrow_mut();
            for (k, v) in attrs.get_values() {
                if node.get_attr(k).is_none() {
                    node.set_attr(k.clone(), v.clone());
                }
            }
        }
    }

    fn remove_node(&mut self, _element: &Element, _source_start: usize) {}

    fn reparent_children(&mut self, element: &Element, new_parent: &Element, _source_start: usize) {
        if let (Some(&from), Some(&to)) =
            (self.uids.get(&element.uid), self.uids.get(&new_parent.uid))
        {
            let children = std::mem::take(&mut self.children[from]);
            for &c in &children {
                self.parents[c] = Some(to);
            }
            self.children[to].extend(children);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_attr_value() {
        // Newlines, tabs, form feeds, and carriage returns become spaces
        // (HTML5 tree-construction attribute-value normalization).
        assert_eq!(normalize_attr_value("one\ntwo"), "one two");
        assert_eq!(normalize_attr_value("one\t two"), "one  two");
        assert_eq!(normalize_attr_value("plain"), "plain");
        assert_eq!(normalize_attr_value("a\rb\u{000C}c"), "a b c");
    }

    #[test]
    fn test_data_object_id() {
        use super::super::element::{Attributes, Element};
        use super::super::tree_handler::{Preposition, TreeHandler};

        let mut handler = NodeTreeHandler::new();
        let attrs = Attributes::from_pairs(vec![("data-object-id".to_string(), "42".to_string())]);
        let mut elt = Element::new(super::super::html_data::NS_HTML, "code", attrs, 7);
        // The handler records `uid -> arena idx` in `insert_element`; an
        // AFE-reconstructed clone calls it with the original's copied attrs.
        TreeHandler::insert_element(&mut handler, Preposition::Root, None, &mut elt, false, 0, 0);
        assert_eq!(handler.data_object_id(7).as_deref(), Some("42"));
        assert_eq!(handler.data_object_id(999), None);
    }
}
