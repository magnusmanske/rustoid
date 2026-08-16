//! The `TreeHandler` interface — the receiver of tree mutation events from the
//! tree builder — plus a concrete handler that builds the crate's `Node` AST.
//!
//! Ports `Wikimedia\RemexHtml\TreeBuilder\TreeHandler`.

use super::element::{Attributes, Element};

/// Placement of a new node relative to a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preposition {
    /// Insert as a sibling before the reference element.
    Before,
    /// Append as the last child of the reference element.
    Under,
    /// Append as the last child of the document node.
    Root,
}

/// Receiver of tree mutation events.
pub trait TreeHandler {
    /// Called when parsing starts.
    fn start_document(&mut self, fragment_ns: Option<&str>, fragment_name: Option<&str>);

    /// Called when parsing stops.
    fn end_document(&mut self, pos: usize);

    /// Insert characters.
    #[allow(clippy::too_many_arguments)]
    fn characters(
        &mut self,
        preposition: Preposition,
        reference: Option<usize>,
        text: &str,
        start: usize,
        length: usize,
        source_start: usize,
        source_length: usize,
    );

    /// Insert an element. Returns a stable id identifying the inserted DOM
    /// element (the handler typically stores this in `element.user_data`).
    fn insert_element(
        &mut self,
        preposition: Preposition,
        reference: Option<usize>,
        element: &mut Element,
        void: bool,
        source_start: usize,
        source_length: usize,
    );

    /// A hint that an element was removed from the stack.
    fn end_tag(&mut self, element: &Element, source_start: usize, source_length: usize);

    /// A valid doctype token.
    fn doctype(
        &mut self,
        name: &str,
        public: &str,
        system: &str,
        quirks: u8,
        source_start: usize,
        source_length: usize,
    );

    /// Insert a comment.
    fn comment(
        &mut self,
        preposition: Preposition,
        reference: Option<usize>,
        text: &str,
        source_start: usize,
        source_length: usize,
    );

    /// A parse error.
    fn error(&mut self, text: &str, pos: usize);

    /// Merge attributes into an existing element.
    fn merge_attributes(&mut self, element: &Element, attrs: &Attributes, source_start: usize);

    /// Remove a node and all its children.
    fn remove_node(&mut self, element: &Element, source_start: usize);

    /// Reparent an element's children.
    fn reparent_children(&mut self, element: &Element, new_parent: &Element, source_start: usize);
}

/// Quirks modes.
pub const NO_QUIRKS: u8 = 0;
pub const LIMITED_QUIRKS: u8 = 1;
pub const QUIRKS: u8 = 2;
