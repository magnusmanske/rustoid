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

/// A node reference within the arena.
type DomNode = Rc<RefCell<Node>>;

/// Builds a `Node` document from tree-builder events.
pub struct NodeTreeHandler {
    /// The arena of nodes.
    arena: Vec<DomNode>,
    /// Children of each arena index (in document order).
    children: Vec<Vec<usize>>,
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
            uids: HashMap::new(),
            root: 0,
        }
    }

    /// Freeze the arena into an owned `Node` tree rooted at `root`.
    pub fn finish(self) -> Node {
        self.materialize(self.root)
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
            Child::Node(idx) => self.children[parent_idx].push(idx),
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
                self.children[parent_idx].push(idx);
            }
        }
    }

    fn place(&mut self, preposition: Preposition, reference: Option<usize>, child: Child) {
        match preposition {
            Preposition::Root | Preposition::Before => {
                // BEFORE (foster) and ROOT both attach under the root for now.
                self.attach_under(self.root, child);
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
            "td" | "th" => ElementKind::TableCell,
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
        let mut node = Node::element(kind);
        for (k, v) in element.attrs.get_values() {
            node.set_attr(k.clone(), v.clone());
        }
        let dom = Rc::new(RefCell::new(node));
        let idx = self.arena.len();
        self.arena.push(dom);
        self.children.push(Vec::new());
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
            self.children[to].extend(children);
        }
    }
}
