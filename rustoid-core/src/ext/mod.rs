//! Extension tag handling.
//!
//! Manages registered extension tags (e.g., `<ref>`, `<gallery>`, `<poem>`)
//! and delegates processing to registered handlers.

pub mod registry;

pub use self::registry::ExtensionRegistry;
