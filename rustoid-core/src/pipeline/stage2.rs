//! Stage 2 — Tree building (tokens → AST).
//!
//! Converts the flat token stream from Stage 1 into a nested
//! AST with proper block/inline structure.

use crate::dom::builder::TreeBuilder;
use crate::dom::node::Node;
use crate::error::Result;
use crate::wikitext::tokens::WikitextToken;

/// Run Stage 2: build the AST from the token stream.
pub fn run_stage2(tokens: Vec<WikitextToken>, wrap_sections: bool) -> Result<Node> {
    let builder = TreeBuilder::new(wrap_sections);
    builder.build(tokens)
}
