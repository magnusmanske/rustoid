//! HTML serialization and parsing backends.
//!
//! - `serialize` — AST → HTML string output.
//! - `parse` — HTML → AST (for round-tripping, via `html5ever`).
//! - `selser` — selective serialization for VE editing.

pub mod constrained_text;
pub mod dom_tree;
pub mod dom_utils;
pub mod dsr;
pub mod env;
pub mod parse;
pub mod selser;
pub mod serialize;
pub mod serialize_wt;
pub mod single_line_context;
pub mod wts_utils;
