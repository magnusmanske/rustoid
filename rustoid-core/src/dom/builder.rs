//! Tree builder — converts token stream to AST.
//!
//! This implements Phase 2 of the pipeline: takes the flat token stream
//! from the preprocessor and constructs a nested AST with proper block/inline
//! structure, paragraph wrapping, list nesting, table parsing, etc.

use crate::dom::node::{ElementKind, Node};
use crate::error::Result;
use crate::wikitext::tokens::WikitextToken;

/// Builds an AST from a stream of wikitext tokens.
///
/// The tree builder applies wikitext-specific rules:
/// - Paragraph wrapping (text runs become `<p>` elements).
/// - List nesting (`*`, `#`, `;`, `:`).
/// - Table structure (`{|...|}`).
/// - Heading hierarchy.
/// - Inline formatting (bold, italic, links).
/// - Fostering (content moved out of tables).
/// - Section wrapping.
#[allow(dead_code)]
pub struct TreeBuilder {
    /// Whether to wrap sections.
    wrap_sections: bool,
}

impl TreeBuilder {
    /// Create a new tree builder.
    pub fn new(wrap_sections: bool) -> Self {
        Self { wrap_sections }
    }

    /// Build an AST from a token stream.
    ///
    /// Returns the root document node.
    pub fn build(&self, tokens: Vec<WikitextToken>) -> Result<Node> {
        let mut doc = Node::document();

        // Placeholder implementation: for now, put everything into a single paragraph.
        // Phase 5 will implement full tree construction.
        let mut para = Node::element(ElementKind::Paragraph);
        for token in &tokens {
            match token {
                WikitextToken::Text(s) => {
                    para.push_child(Node::text(s.clone()));
                }
                WikitextToken::EOF => {}
                _ => {
                    // For now, just capture token debug representation
                    para.push_child(Node::text(format!("[{token}]")));
                }
            }
        }
        doc.push_child(para);

        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens::WikitextToken;

    #[test]
    fn test_simple_text_to_ast() {
        let builder = TreeBuilder::new(false);
        let tokens = vec![
            WikitextToken::Text("Hello, world!".to_string()),
            WikitextToken::EOF,
        ];
        let doc = builder.build(tokens).unwrap();
        assert!(!doc.children.is_empty());
    }
}
