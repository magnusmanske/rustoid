//! HTML serialization and parsing backends.
//!
//! - `serialize` — AST → HTML string output.
//! - `parse` — HTML → AST (for round-tripping, via `html5ever`).
//! - `selser` — selective serialization for VE editing.

pub mod dom_tree;
pub mod parse;
pub mod selser;
pub mod serialize;
pub mod serialize_wt;
