//! Wikitext tokenizer and preprocessor.
//!
//! This module handles the first phase of parsing: converting raw
//! wikitext into a stream of tokens that the tree builder can consume.

pub mod preprocessor;
pub mod tokenizer;
pub mod tokens;

// V2 token types and tokenizer (faithful PHP Parsoid port).
pub mod tokenizer_v2;
pub mod tokens_v2;
