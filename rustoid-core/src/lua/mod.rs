//! Lua/Scribunto engine integration.
//!
//! Uses the `mlua` crate to provide a sandboxed Lua runtime for
//! evaluating Scribunto modules. Implements the `mw` global table
//! with MediaWiki API stubs.

pub mod engine;
