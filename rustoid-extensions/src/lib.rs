//! # Rustoid extensions
//!
//! MediaWiki parser extensions, ported from Parsoid's *native* PHP
//! implementations (not the legacy tag hooks) so the output matches what a live
//! wiki's Parsoid emits.
//!
//! Each extension is self-contained and depends only on [`rustoid_ast`], the
//! shared node/token types, so it can build and mutate trees without depending
//! on the parser. The parser (`rustoid-core`) depends on this crate and invokes
//! these entry points from its finalization passes.

pub mod cite;
pub mod secure_link_fixer;
