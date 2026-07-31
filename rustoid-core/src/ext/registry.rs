//! Registry of known extension tag handlers.
//!
//! Extension tags like `<ref>`, `<gallery>`, `<poem>` are handled by registered
//! `ExtensionHandler` implementations. The registry maps tag names to handlers.

use std::collections::HashMap;
use std::sync::Arc;

use crate::traits::ExtensionHandler;

/// A registry mapping extension tag names to their handlers.
pub struct ExtensionRegistry {
    handlers: HashMap<String, Arc<dyn ExtensionHandler>>,
}

impl ExtensionRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register an extension handler for a tag name.
    pub fn register(&mut self, tag_name: impl Into<String>, handler: Arc<dyn ExtensionHandler>) {
        self.handlers.insert(tag_name.into(), handler);
    }

    /// Get the handler for a tag name, if registered.
    pub fn get(&self, tag_name: &str) -> Option<&Arc<dyn ExtensionHandler>> {
        self.handlers.get(tag_name)
    }

    /// Check if a tag name is a known extension.
    pub fn contains(&self, tag_name: &str) -> bool {
        self.handlers.contains_key(tag_name)
    }
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}
