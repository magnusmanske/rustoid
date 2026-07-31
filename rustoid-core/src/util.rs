//! Internal utilities.
//!
//! Small helper functions used across the codebase.

/// Convert underscores to spaces in page titles.
#[allow(dead_code)]
pub fn normalize_title(title: &str) -> String {
    title.replace('_', " ").trim().to_string()
}

/// Convert spaces to underscores for URL-safe representation.
#[allow(dead_code)]
pub fn title_to_url(title: &str) -> String {
    title.replace(' ', "_")
}
