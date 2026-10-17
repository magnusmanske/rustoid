//! Extension tag handling.
//!
//! Manages registered extension tags (e.g., `<ref>`, `<gallery>`, `<poem>`)
//! and delegates processing to registered handlers.

pub mod registry;

// Cite lives in `rustoid-extensions`; re-exported so `crate::ext::cite` keeps
// resolving, matching the other extension entry points.
pub use rustoid_extensions::cite;
pub use rustoid_extensions::secure_link_fixer;

pub use self::registry::ExtensionRegistry;
