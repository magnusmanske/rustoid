//! Abstract Syntax Tree (AST) / Document Object Model types.
//!
//! This module defines the internal representation of parsed wikitext.
//! It is intentionally NOT HTML-specific — elements use semantic kinds
//! (e.g., `Paragraph`, `Heading`, `Wikilink`) rather than HTML tag names.
//!
//! Serialization backends (HTML, JSON, Typst, etc.) implement the
//! [`AstVisitor`] trait to traverse and output the AST.

pub mod builder;
pub mod node;
pub mod visitor;

pub use self::node::{Attribute, Document, Element, ElementKind, Node, NodeKind};
pub use self::visitor::AstVisitor;
