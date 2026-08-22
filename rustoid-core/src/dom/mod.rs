//! Abstract Syntax Tree (AST) / Document Object Model types.
//!
//! This module defines the internal representation of parsed wikitext.
//! It is intentionally NOT HTML-specific — elements use semantic kinds
//! (e.g., `Paragraph`, `Heading`, `Wikilink`) rather than HTML tag names.
//!
//! Serialization backends (HTML, JSON, Typst, etc.) traverse the AST
//! directly.

pub mod node;

pub use self::node::{Attribute, Document, Element, ElementKind, Node, NodeKind};
