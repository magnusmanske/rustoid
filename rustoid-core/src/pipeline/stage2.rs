//! Stage 2 — Tree building (tokens → AST).
//!
//! Converts the flat token stream from Stage 1 into a nested
//! AST with proper block/inline structure.

use crate::dom::builder::TreeBuilder;
use crate::dom::node::Node;
use crate::error::Result;
use crate::pipeline::quote_transformer::QuoteTransformer;
use crate::wikitext::tokens::WikitextToken;

/// Run Stage 2: build the AST from the token stream.
pub fn run_stage2(tokens: Vec<WikitextToken>) -> Result<Node> {
    // Apply quote transformer (converts Quote tokens to Bold/Italic open/close)
    let tokens = QuoteTransformer::transform(tokens)?;
    let mut builder = TreeBuilder::new();
    builder.build(&tokens)
}
