//! Link handling — wikilinks, interwiki links, external links.
//!
//! Handles parsing and resolution of `[[...]]` wikilinks,
//! interwiki prefixes, and `[http://...]` external links.

use crate::title::Title;
use crate::traits::SiteConfig;

/// Link target information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// An internal wikilink.
    Wikilink {
        title: Title,
        fragment: Option<String>,
    },
    /// An interwiki link (prefix + title on another wiki).
    Interwiki { prefix: String, title: String },
    /// An external URL link.
    ExtLink { url: String },
}

/// Parse a wikilink target (the content inside `[[...]]`).
///
/// Returns the link target and optional display text.
pub fn parse_wikilink(_raw: &str, _config: &dyn SiteConfig) -> (LinkTarget, Option<String>) {
    // Placeholder — will be implemented in Phase 2/3
    (
        LinkTarget::Wikilink {
            title: Title::new_main(""),
            fragment: None,
        },
        None,
    )
}

/// Parse an external link (`[http://example.com text]`).
pub fn parse_extlink(url: &str, text: &str) -> (String, Option<String>) {
    let url = url.trim().to_string();
    let display = if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    };
    (url, display)
}
