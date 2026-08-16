//! Public `Parser` facade — ties the V2 token pipeline together into a single
//! entry point for wikitext → HTML.
//!
//! This is the Rust port of PHP Parsoid's top-level `Parser`/`Wikitext` entry
//! points, using the V2 token pipeline (`PegTokenizer` → TT2 handlers →
//! `TreeBuilderStage`).
//!
//! Template/parser-function expansion is layered in as a TT2 handler that
//! consumes the `SiteConfig`/`DataSource`; see `TemplateHandler` and
//! `TokenHandlerPipeline`.

use crate::dom::node::Node;
use crate::error::Result;
use crate::options::ParserOptions;
use crate::pipeline::tree_builder_stage::TreeBuilderStage;
use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};
use crate::wikitext::tokens_v2::{Either, Item};

/// The wikitext parser.
#[derive(Default)]
pub struct Parser;

impl Parser {
    pub fn new() -> Self {
        Self
    }

    /// Tokenize raw wikitext into the V2 `Item` stream.
    fn tokenize(&self, wikitext: &str) -> Result<Vec<Item>> {
        let options = TokenizerOptions::default();
        let mut tokenizer = PegTokenizer::new(wikitext, &options);
        let chunks = tokenizer.tokenize()?;
        Ok(chunks
            .into_iter()
            .map(|e| match e {
                Either::Left(s) => Item::Str(s),
                Either::Right(t) => Item::Tok(t),
            })
            .collect())
    }

    /// Convert wikitext to the format-agnostic AST.
    pub fn wikitext_to_ast(&self, wikitext: &str) -> Result<Node> {
        let tokens = self.tokenize(wikitext)?;
        let stage = TreeBuilderStage::new(false);
        Ok(stage.to_ast(tokens))
    }

    /// Convert wikitext to an HTML string (Parsoid-style output).
    pub fn wikitext_to_html(&self, wikitext: &str, options: &ParserOptions) -> Result<String> {
        let ast = self.wikitext_to_ast(wikitext)?;
        let serializer = crate::html::serialize::HtmlSerializer::new(options.clone());
        serializer.serialize(&ast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::ParserOptions;

    #[test]
    fn test_wikitext_to_html_heading() {
        let parser = Parser::new();
        let html = parser
            .wikitext_to_html("== Heading ==\n", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<h2>"), "got: {html}");
        assert!(html.contains("Heading"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_bold() {
        let parser = Parser::new();
        let html = parser
            .wikitext_to_html("'''bold'''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<b>"), "got: {html}");
        assert!(html.contains("bold"), "got: {html}");
    }
}
