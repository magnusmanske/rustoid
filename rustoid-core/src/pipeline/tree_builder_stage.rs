//! TreeBuilderStage — drives the TokenTransform3 (line-based) handlers and
//! converts the resulting token stream into the format-agnostic AST.
//!
//! Mirrors the structure of PHP Parsoid's `TreeBuilderStage`, which is a thin
//! driver over the HTML5 tree builder (`tree_builder_html`).
//!
//! The TT3 handlers run in the following order (mirroring Parsoid's
//! `PipelineFactory`):
//!   PreHandler → QuoteTransformer → ListHandler → ParagraphWrapper

use crate::dom::node::Node;
use crate::wikitext::tokens_v2::Item;

use super::list_handler::ListHandler;
use super::paragraph_wrapper_v2::ParagraphWrapper;
use super::pre_handler::PreHandler;
use super::quote_transformer_v2::QuoteTransformer;
use super::tree_builder_html::token_stream_to_ast_html_with_fragments;

/// Run the TokenTransform3 (line-based) handlers over a token stream.
///
/// This is the token-level half of tree building; the resulting `Vec<Item>` is
/// then handed to the HTML5 token→AST converter.
pub struct TreeBuilderStage {
    inline_context: bool,
}

impl TreeBuilderStage {
    pub fn new(inline_context: bool) -> Self {
        Self { inline_context }
    }

    /// Run the TT3 handlers in order and return the transformed token stream.
    pub fn process(
        &self,
        tokens: Vec<Item>,
        config: &dyn crate::traits::SiteConfig,
        fragments: &mut std::collections::HashMap<usize, Node>,
        next_id: &mut usize,
    ) -> Vec<Item> {
        let mut out = tokens;

        // 1. PreHandler (indent-pre detection).
        let mut pre_handler = PreHandler::with_options(self.inline_context);
        out = pre_handler.run(out);

        // 2. QuoteTransformer (mw-quote → b/i).
        out = QuoteTransformer::transform(out);

        // 2b. ExtensionHandler (expand built-in `<nowiki>` extension tokens).
        out = crate::pipeline::extension_handler::run(out, config, fragments, next_id);

        // 3. ListHandler (listItem → ul/ol/li).
        let mut list_handler = ListHandler::new();
        out = list_handler.run(out);

        // 4. ParagraphWrapper (wrap content in <p>).
        let mut pw = ParagraphWrapper::with_options(self.inline_context);
        out = pw.wrap(out);

        // 5. SanitizerHandler (drop disallowed tags/attributes; runs last).
        let mut sanitizer = crate::pipeline::sanitizer_handler::SanitizerHandler::new(false);
        out = sanitizer.run(out);

        out
    }

    /// Run the TT3 handlers and convert the result to an AST.
    pub fn to_ast(&self, tokens: Vec<Item>, config: &dyn crate::traits::SiteConfig) -> Node {
        self.to_ast_with_fragments(tokens, None, config, std::collections::HashMap::new())
    }

    /// Run the TT3 handlers and convert to an AST, with the page source
    /// available for `tsr`-based source recovery.
    pub fn to_ast_with_source(
        &self,
        tokens: Vec<Item>,
        source: Option<&str>,
        config: &dyn crate::traits::SiteConfig,
    ) -> Node {
        self.to_ast_with_fragments(tokens, source, config, std::collections::HashMap::new())
    }

    /// Like [`to_ast_with_source`], but accepts pre-built sub-fragments for
    /// `mw:dom-fragment-token` placeholders.
    pub fn to_ast_with_fragments(
        &self,
        tokens: Vec<Item>,
        source: Option<&str>,
        config: &dyn crate::traits::SiteConfig,
        mut fragments: std::collections::HashMap<usize, crate::dom::node::Node>,
    ) -> Node {
        // Continue fragment-id allocation after any pre-built fragments (from
        // `format="wikitext"` pre, etc.).
        let mut next_id = fragments.len();
        let tokens = self.process(tokens, config, &mut fragments, &mut next_id);
        token_stream_to_ast_html_with_fragments(&tokens, source, fragments)
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

    fn config() -> crate::mock::MockSiteConfig {
        crate::mock::MockSiteConfig::new()
    }

    /// Run `process` with fresh fragment bookkeeping, discarding the fragments
    /// (these tests only inspect the token stream).
    fn process(tokens: Vec<Item>) -> Vec<Item> {
        let stage = TreeBuilderStage::new(false);
        let mut fragments = std::collections::HashMap::new();
        let mut next_id = 0usize;
        stage.process(tokens, &config(), &mut fragments, &mut next_id)
    }

    #[test]
    fn test_process_plain_text() {
        let out = process(tokenize("hello world"));
        assert!(!out.is_empty());
    }

    #[test]
    fn test_process_heading() {
        let out = process(tokenize("== Heading ==\n"));
        // Should contain an h2 tag after TT3.
        assert!(out.iter().any(|it| {
            matches!(it, Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::Tag(t)) if t.name == "h2")
        }));
    }

    #[test]
    fn test_process_bold() {
        let out = process(tokenize("'''bold'''"));
        // Should contain a <b> tag (from quote transformer).
        assert!(out.iter().any(|it| {
            matches!(it, Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::Tag(t)) if t.name == "b")
        }));
    }

    #[test]
    fn test_dl_table_two_tables() {
        let stage = TreeBuilderStage::new(false);
        let ast = stage.to_ast(
            tokenize(":{|\n|foo\nbar\n|}\n\n:::{|\n|foo\nbar\n|}"),
            &config(),
        );
        // Two `<dl>` blocks each containing a `<table>` must both survive, and
        // must stay under the single synthetic `<html>` wrapper (not be forced
        // out as siblings of `<html>` by a spurious mid-stream EOF).
        fn count_tables(n: &Node) -> usize {
            let mut c = if matches!(
                &n.kind,
                crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::Table)
            ) {
                1
            } else {
                0
            };
            for ch in &n.children {
                c += count_tables(ch);
            }
            c
        }
        assert_eq!(count_tables(&ast), 2, "expected 2 tables, got {ast:?}");

        // The whole document must wrap in a single `<html>`; no top-level
        // fragment siblings may leak outside it.
        assert_eq!(ast.children.len(), 1, "single <html> child: {ast:?}");
        assert!(matches!(
            &ast.children[0].kind,
            crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::Other(t))
                if t == "html"
        ));
        let html = &ast.children[0];
        let n_dl = html
            .children
            .iter()
            .filter(|c| {
                matches!(
                    &c.kind,
                    crate::dom::node::NodeKind::Element(
                        crate::dom::node::ElementKind::DefinitionList
                    )
                )
            })
            .count();
        assert_eq!(n_dl, 2, "two <dl> blocks under <html>: {ast:?}");
    }

    #[test]
    fn test_to_ast_heading() {
        let stage = TreeBuilderStage::new(false);
        let doc = stage.to_ast(tokenize("== Heading ==\n"), &config());

        // The document should contain an h2 element somewhere in the tree
        // (Parsoid nests content under `<html><body>`).
        assert!(contains_heading_2(&doc));
    }

    fn contains_heading_2(node: &Node) -> bool {
        use crate::dom::node::{ElementKind, NodeKind};
        if let NodeKind::Element(ElementKind::Heading(2)) = &node.kind {
            return true;
        }
        node.children.iter().any(contains_heading_2)
    }

    #[test]
    fn test_to_ast_bold() {
        let stage = TreeBuilderStage::new(false);
        let doc = stage.to_ast(tokenize("'''bold'''"), &config());

        // The document should contain a bold element (possibly nested in <p>).
        assert!(contains_bold(&doc), "expected a bold element: {doc:?}");
    }

    fn contains_bold(node: &Node) -> bool {
        use crate::dom::node::{ElementKind, NodeKind};
        if let NodeKind::Element(ElementKind::Bold) = &node.kind {
            return true;
        }
        node.children.iter().any(contains_bold)
    }

    #[test]
    fn test_to_ast_wikilink() {
        let stage = TreeBuilderStage::new(false);
        let doc = stage.to_ast(tokenize("[[Main Page]]"), &config());

        assert!(contains_wikilink(&doc));
    }

    fn contains_wikilink(node: &Node) -> bool {
        use crate::dom::node::{ElementKind, NodeKind};
        if let NodeKind::Element(ElementKind::Wikilink) = &node.kind {
            return true;
        }
        node.children.iter().any(contains_wikilink)
    }

    #[test]
    fn test_process_div() {
        let out = process(tokenize("<div>foo</div>"));
        assert!(!out.is_empty(), "empty output");
    }

    #[test]
    fn test_tokenize_div_only() {
        // Isolate: tokenizer alone (no TT3) must not hang.
        let toks = tokenize("<div>foo</div>");
        assert!(!toks.is_empty());
    }
}
