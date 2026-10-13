//! # Rustoid AST — shared node and token types.
//!
//! This crate holds the data model shared between the parser (`rustoid-core`)
//! and the parser extensions (`rustoid-extensions`): the AST/DOM node types and
//! the Parsoid token types. It has no dependency on either, so extensions can
//! build and mutate trees without depending on the parser.
//!
//! - [`dom`] — the format-agnostic AST node types.
//! - [`tokens_v2`] — Parsoid-compatible token types and `DataParsoid`.
//! - [`data_mw`] — textual `data-mw` JSON surgery.

pub mod data_mw;
pub mod dom;
pub mod tokens_v2;
