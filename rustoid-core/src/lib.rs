//! # Rustoid — A Rust implementation of the Parsoid MediaWiki parser.
//!
//! Rustoid is a bidirectional wikitext ↔ HTML5 parser. It aims for byte-perfect
//! output compatibility with the PHP Parsoid implementation.
//!
//! ## Architecture
//!
//! The parser operates in three stages:
//!
//! 1. **Preprocessing** — tokenize wikitext, resolve templates, parser functions,
//!    and magic words into a flat token stream.
//! 2. **Tree building** — convert the token stream into a format-agnostic AST
//!    (block/inline structure, paragraphs, headings, lists, tables, etc.).
//! 3. **Serialization** — lower the AST to a chosen output format (HTML, JSON, etc.).
//!
//! ## Feature flags
//!
//! - `mwapi` — enables the MediaWiki API data source (requires `reqwest` and `tokio`).
//! - `ruwex` — enables the indexed dump data source via the `ruwex` crate.
//!
//! ## Quick example
//!
//! ```rust,ignore
//! use rustoid_core::{Parser, MockDataSource, MockSiteConfig, ParserOptions};
//!
//! let source = MockDataSource::new();
//! let config = MockSiteConfig::new();
//! let parser = Parser::new(&config);
//! let html = parser
//!     .wikitext_to_html("'''Hello''' world!", &ParserOptions::for_page("Main Page"))
//!     .unwrap();
//! ```

pub mod error;
pub mod options;
pub mod title;
pub mod traits;

pub mod dom;
pub mod expand;
pub mod ext;
pub mod lua;
pub mod pipeline;
pub mod wikitext;

pub mod html;
pub mod html5;
pub mod links;
pub mod magic;
pub mod sanitizer;

pub mod mock;

pub mod test_harness;

#[cfg(feature = "mwapi")]
pub mod mw_api;

#[cfg(feature = "ruwex")]
pub mod ruwex_source;

mod util;

// Re-export key types
pub use self::error::{Result, RustoidError};
pub use self::options::ParserOptions;
pub use self::pipeline::parser::Parser;
pub use self::title::Title;
pub use self::traits::{DataSource, ExtensionHandler, SiteConfig};
