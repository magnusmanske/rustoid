//! The `InsertionMode` trait — abstract base for the tree-construction
//! insertion modes. Ports `Wikimedia\RemexHtml\TreeBuilder\InsertionMode`.

use super::element::Attributes;
use super::tree_builder::TreeBuilder;
use super::tree_handler::TreeHandler;

/// A tree-construction insertion mode. Each mode handles token events
/// (`characters`, `startTag`, `endTag`, `endDocument`) plus optional
/// `doctype`/`comment`.
pub trait InsertionMode<H: TreeHandler> {
    /// A valid doctype token.
    fn doctype(
        &mut self,
        name: &str,
        public: &str,
        system: &str,
        quirks: bool,
        source_start: usize,
        source_length: usize,
    );

    /// A comment token.
    fn comment(&mut self, text: &str, source_start: usize, source_length: usize);

    /// A parse error.
    fn error(&mut self, text: &str, pos: usize);

    /// Character data.
    fn characters(
        &mut self,
        text: &str,
        start: usize,
        length: usize,
        source_start: usize,
        source_length: usize,
    );

    /// A start tag.
    fn start_tag(
        &mut self,
        name: &str,
        attrs: Attributes,
        self_close: bool,
        source_start: usize,
        source_length: usize,
    );

    /// An end tag.
    fn end_tag(&mut self, name: &str, source_start: usize, source_length: usize);

    /// Called when parsing stops.
    fn end_document(&mut self, pos: usize);

    /// Access to the underlying tree builder.
    fn builder(&mut self) -> &mut TreeBuilder<H>;
}

/// Helper: split the leading run of characters in `mask` from `text`.
/// Mirrors `InsertionMode::splitInitialMatch` (the simple case, without CDATA
/// special-casing which is not needed for wikitext input).
pub fn split_initial_match(
    mask: &[char],
    text: &str,
    start: usize,
    length: usize,
) -> (usize, usize) {
    let slice = &text[start..start + length];
    let n = slice.chars().take_while(|c| mask.contains(c)).count();
    (start, n)
}
