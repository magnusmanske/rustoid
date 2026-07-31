//! Wikitext preprocessor — template expansion and parser function evaluation.
//!
//! This stage takes the raw token stream from the tokenizer and resolves
//! templates, parser functions, magic words, and template arguments into
//! a flat token stream with all transclusions expanded.

use crate::error::Result;
use crate::traits::{DataSource, SiteConfig};
use crate::wikitext::tokens::WikitextToken;

/// The preprocessor evaluates templates, parser functions, and magic words.
#[allow(dead_code)]
pub struct Preprocessor<'a, S: DataSource, C: SiteConfig> {
    source: &'a S,
    config: &'a C,
    /// Maximum template expansion depth before we error out.
    max_depth: u32,
}

impl<'a, S: DataSource, C: SiteConfig> Preprocessor<'a, S, C> {
    /// Create a new preprocessor.
    pub fn new(source: &'a S, config: &'a C) -> Self {
        Self {
            source,
            config,
            max_depth: 40,
        }
    }

    /// Expand all templates, parser functions, and arguments in the token stream.
    ///
    /// This is called repeatedly until no more expansions remain (or max depth
    /// is reached).
    pub fn expand(&self, tokens: Vec<WikitextToken>) -> Result<Vec<WikitextToken>> {
        // Placeholder: return tokens as-is for now.
        // Phase 3 will implement recursive expansion.
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    // Tests will be added in Phase 3.
}
