//! Faithful Rust port of the RemexHtml HTML5 tree-construction algorithm,
//! the same component Wikimedia Parsoid's `TreeBuilderStage` drives.
//!
//! This exists because Parsoid needs the tree builder to expose operations
//! (unfostered insertion, fosterable-position queries, stripped-tag reporting)
//! that html5ever's sealed `TreeBuilder` does not. We port RemexHtml directly
//! rather than approximating with html5ever.

pub mod active_formatting_elements;
pub mod element;
pub mod html_data;
pub mod stack;
