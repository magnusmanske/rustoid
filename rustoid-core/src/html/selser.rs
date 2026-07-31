//! Selective serialization (selser).
//!
//! Given original wikitext, original HTML, and modified HTML,
//! produce modified wikitext with minimal changes.
//!
//! Phase 8 will implement basic selser.
//! v1.1 will add full DOM-diff based selser for the VE editing pipeline.

use crate::error::Result;

/// Run selective serialization.
///
/// Placeholder — Phase 8 will implement this.
pub fn selser(
    _original_wikitext: &str,
    _original_html: &str,
    _modified_html: &str,
) -> Result<String> {
    Err(crate::error::RustoidError::Unsupported(
        "Selser not yet implemented".to_string(),
    ))
}
