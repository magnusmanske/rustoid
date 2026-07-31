//! Stage 3 — AST post-processing and serialization.
//!
//! Applies final DOM cleanup (fixup fostered content, annotation ranges,
//! section wrapping) and serializes the AST to the chosen output format.

use crate::dom::node::Node;
use crate::error::Result;
use crate::html::serialize::HtmlSerializer;
use crate::options::ParserOptions;

/// Run Stage 3: serialize the AST to HTML.
pub fn run_stage3_html(ast: &Node, options: &ParserOptions) -> Result<String> {
    // Apply DOM post-processing
    let processed = post_process(ast)?;

    // Serialize to HTML
    let serializer = HtmlSerializer::new(options.clone());
    serializer.serialize(&processed)
}

/// Apply post-processing to the AST (fostering fixups, annotation cleanup, etc.).
fn post_process(ast: &Node) -> Result<Node> {
    // Currently a pass-through; Phase 6 will implement full DOM post-processing.
    Ok(ast.clone())
}
