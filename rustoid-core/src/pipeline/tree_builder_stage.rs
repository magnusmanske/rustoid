//! TreeBuilderStage — drives the TokenTransform3 (line-based) handlers and
//! converts the resulting token stream into the format-agnostic AST.
//!
//! Mirrors the structure of PHP Parsoid's `TreeBuilderStage` (which itself is
//! a thin driver over the `TreeBuilder`), minus the full HTML5 tree
//! construction (which is layered in below).
//!
//! The TT3 handlers run in the following order (mirroring Parsoid's
//! `PipelineFactory`):
//!   PreHandler → QuoteTransformer → ListHandler → ParagraphWrapper

use crate::wikitext::tokens_v2::Item;

use super::list_handler::ListHandler;
use super::paragraph_wrapper_v2::ParagraphWrapper;
use super::pre_handler::PreHandler;
use super::quote_transformer_v2::QuoteTransformer;

/// Run the TokenTransform3 (line-based) handlers over a token stream.
///
/// This is the token-level half of tree building; the resulting `Vec<Item>` is
/// then handed to a token→AST converter (not yet wired) to produce the DOM.
pub struct TreeBuilderStage {
    inline_context: bool,
}

impl TreeBuilderStage {
    pub fn new(inline_context: bool) -> Self {
        Self { inline_context }
    }

    /// Run the TT3 handlers in order and return the transformed token stream.
    pub fn process(&self, tokens: Vec<Item>) -> Vec<Item> {
        let mut out = tokens;

        // 1. PreHandler (indent-pre detection).
        let mut pre_handler = PreHandler::with_options(self.inline_context);
        out = pre_handler.run(out);

        // 2. QuoteTransformer (mw-quote → b/i).
        out = QuoteTransformer::transform(out);

        // 3. ListHandler (listItem → ul/ol/li).
        let mut list_handler = ListHandler::new();
        out = list_handler.run(out);

        // 4. ParagraphWrapper (wrap content in <p>).
        let mut pw = ParagraphWrapper::with_options(self.inline_context);
        out = pw.wrap(out);

        out
    }
}

impl Default for TreeBuilderStage {
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};

    fn tokenize(wikitext: &str) -> Vec<Item> {
        let options = TokenizerOptions::default();
        let mut tokenizer = PegTokenizer::new(wikitext, &options);
        tokenizer
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|e| match e {
                crate::wikitext::tokens_v2::Either::Left(s) => Item::Str(s),
                crate::wikitext::tokens_v2::Either::Right(t) => Item::Tok(t),
            })
            .collect()
    }

    #[test]
    fn test_process_plain_text() {
        let stage = TreeBuilderStage::new(false);
        let out = stage.process(tokenize("hello world"));
        assert!(!out.is_empty());
    }

    #[test]
    fn test_process_heading() {
        let stage = TreeBuilderStage::new(false);
        let out = stage.process(tokenize("== Heading ==\n"));
        // Should contain an h2 tag after TT3.
        assert!(out.iter().any(|it| {
            matches!(it, Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::Tag(t)) if t.name == "h2")
        }));
    }

    #[test]
    fn test_process_bold() {
        let stage = TreeBuilderStage::new(false);
        let out = stage.process(tokenize("'''bold'''"));
        // Should contain a <b> tag (from quote transformer).
        assert!(out.iter().any(|it| {
            matches!(it, Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::Tag(t)) if t.name == "b")
        }));
    }
}
