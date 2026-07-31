//! Magic word handling.
//!
//! MediaWiki magic words include behavior switches (`__TOC__`, `__NOTOC__`),
//! variables (`{{PAGENAME}}`, `{{CURRENTYEAR}}`), and parser function aliases.

use std::collections::HashMap;

/// A registry of magic words with their canonical names and localized aliases.
#[derive(Debug, Clone, Default)]
pub struct MagicWordRegistry {
    /// Map from localized alias to canonical magic word.
    aliases: HashMap<String, String>,
}

impl MagicWordRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a magic word with its aliases.
    pub fn register(&mut self, canonical: &str, aliases: &[&str]) {
        for alias in aliases {
            self.aliases
                .insert(alias.to_lowercase(), canonical.to_string());
        }
    }

    /// Look up a magic word alias and return its canonical name.
    pub fn resolve(&self, word: &str) -> Option<&str> {
        self.aliases.get(&word.to_lowercase()).map(|s| s.as_str())
    }
}
