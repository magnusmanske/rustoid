//! Lua/Scribunto engine integration.
//!
//! Uses the `mlua` crate to provide a sandboxed Lua runtime for
//! evaluating Scribunto modules. Implements the `mw` global table
//! with MediaWiki API stubs.

mod language_names;

pub mod engine;
pub mod invoke;
pub mod language;
pub mod ustring;
pub mod wikibase;

pub(crate) use language_names::LANGUAGE_NAMES;
