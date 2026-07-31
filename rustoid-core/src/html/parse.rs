//! HTML → AST parser for round-tripping.
//!
//! Uses `html5ever` to parse Parsoid-format HTML back into our AST,
//! extracting `data-parsoid` and `data-mw` attributes.

use crate::dom::node::Node;
use crate::error::Result;

/// Parse a Parsoid HTML string into an AST.
///
/// Placeholder — Phase 7 will implement full HTML→AST conversion.
pub fn parse_html(_html: &str) -> Result<Node> {
    Ok(Node::document())
}

#[cfg(test)]
mod tests {
    // Tests will be added in Phase 7
}
