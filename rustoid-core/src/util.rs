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

/// Decode a percent-encoded URI component. Mirrors PHP's `rawurldecode` /
/// `Uri\decodeURIComponent` semantics: decodes `%XX` sequences to bytes.
pub fn decode_uri_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Convert a hex nibble to its value.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Normalize a namespace name (trim + collapse internal spaces + lowercase
/// first letter). Mirrors the essential part of PHP's
/// `Utils::normalizeNamespaceName`.
pub fn normalize_namespace_name(name: &str) -> String {
    let trimmed = name.trim();
    // Replace runs of whitespace with a single space.
    let mut result = String::with_capacity(trimmed.len());
    let mut last_was_space = false;
    for c in trimmed.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                result.push(' ');
                last_was_space = true;
            }
        } else {
            result.push(c);
            last_was_space = false;
        }
    }

    // Lowercase the first character (MediaWiki namespace names are
    // case-insensitive in the first letter).
    let mut chars: Vec<char> = result.chars().collect();
    if let Some(first) = chars.first_mut() {
        *first = first.to_lowercase().next().unwrap_or(*first);
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_uri_component() {
        assert_eq!(decode_uri_component("Foo%20bar"), "Foo bar");
        assert_eq!(decode_uri_component("%C3%A9"), "é");
        assert_eq!(decode_uri_component("plain"), "plain");
    }

    #[test]
    fn test_normalize_namespace_name() {
        assert_eq!(normalize_namespace_name("Template"), "template");
        assert_eq!(normalize_namespace_name("  User talk  "), "user talk");
    }
}
