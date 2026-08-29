//! HTML serialization and parsing backends.
//!
//! - `serialize` — AST → HTML string output.
//! - `parse` — HTML → AST (for round-tripping, via `html5ever`).
//! - `selser` — selective serialization for VE editing.

pub mod constrained_text;
pub mod diff_utils;
pub mod dom_diff;
pub mod dom_handler;
pub mod dom_handler_factory;
pub mod dom_normalizer;
pub mod dom_tree;
pub mod dom_utils;
pub mod dsr;
pub mod env;
pub mod handlers;
pub mod link_handler_utils;
pub mod media_structure;
pub mod parse;
pub mod selser;
pub mod separators;
pub mod serialize;
pub mod serialize_wt;
pub mod serializer;
pub mod serializer_state;
pub mod single_line_context;
pub mod template_serializer;
pub mod wikitext_escape_handlers;
pub mod wts_utils;
