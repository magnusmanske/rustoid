//! Stage 1 — Wikitext tokenization and preprocessing.
//!
//! This stage tokenizes raw wikitext and then runs the preprocessor
//! to expand templates, parser functions, and magic words.

use crate::error::Result;
use crate::traits::{DataSource, SiteConfig};
use crate::wikitext::preprocessor::Preprocessor;
use crate::wikitext::tokenizer::{Tokenizer, TokenizerOptions};
use crate::wikitext::tokens::WikitextToken;

/// Run Stage 1: tokenize and preprocess wikitext.
pub fn run_stage1<S: DataSource, C: SiteConfig>(
    wikitext: &str,
    source: &S,
    config: &C,
) -> Result<Vec<WikitextToken>> {
    // Step 1: Tokenize
    let tokenizer_opts = TokenizerOptions::default();
    let mut tokenizer = Tokenizer::new(wikitext, tokenizer_opts);
    let tokens = tokenizer.tokenize()?;

    // Step 2: Preprocess (expand templates, parser functions, arguments)
    let preprocessor = Preprocessor::new(source, config);
    let expanded = preprocessor.expand(tokens)?;

    Ok(expanded)
}
