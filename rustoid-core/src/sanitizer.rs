//! HTML sanitization.
//!
//! Cleans up HTML output: removes dangerous attributes, sanitizes URLs,
//! strips XSS vectors, and applies MediaWiki allowlists.

/// Sanitize an HTML attribute value based on the element and attribute name.
pub fn sanitize_attr(_element: &str, _attr_name: &str, value: &str) -> String {
    // Placeholder: basic sanitization only
    value.replace("javascript:", "").replace("data:", "")
}

/// Sanitize a URL for use in `href` or `src` attributes.
pub fn sanitize_url(url: &str) -> String {
    let url = url.trim();
    if url.to_lowercase().starts_with("javascript:") {
        return String::new();
    }
    url.to_string()
}
