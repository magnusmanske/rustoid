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

/// Entity-escape anything that would decode to a valid wikitext entity: escape
/// the `&` of any `&…;` sequence that decodes to a different string (a valid
/// entity), leaving non-entities untouched. Mirrors PHP's
/// `Utils::escapeWtEntities`.
///
/// Consumed by the html2wt `WikitextEscapeHandlers` (pending), hence the
/// dead-code allow until that port lands.
#[allow(dead_code)]
pub fn escape_wt_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            // Match `&[#0-9a-zA-Z\x80-\xff]+;` (the PHP regexp).
            let mut j = i + 1;
            while j < bytes.len() && !(bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() {
                // Semicolon found at `j`; check the entity body is valid.
                let body = &text[i + 1..j];
                let body_ok = !body.is_empty()
                    && body
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'#' || b >= 0x80);
                if body_ok {
                    let entity = &text[i..j + 1]; // includes '&' and ';'
                    let decoded = crate::wikitext::tokenizer_v2::decode_wt_entities(entity);
                    if decoded != entity {
                        // It's a valid entity: escape the ampersand.
                        out.push_str("&amp;");
                        out.push_str(&text[i + 1..j + 1]);
                        i = j + 1;
                        continue;
                    }
                }
            }
        }
        // Copy a single `char` (preserve UTF-8 boundaries).
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Length in bytes of the UTF-8 sequence starting at `b` (1-4).
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if (b >> 5) == 0b110 {
        2
    } else if (b >> 4) == 0b1110 {
        3
    } else {
        4
    }
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

    #[test]
    fn test_escape_wt_entities() {
        // A valid named entity's ampersand is escaped.
        assert_eq!(escape_wt_entities("a &amp; b"), "a &amp;amp; b");
        // A valid numeric entity is likewise escaped.
        assert_eq!(escape_wt_entities("x &#65; y"), "x &amp;#65; y");
        // A lone ampersand (no terminating entity) is untouched.
        assert_eq!(escape_wt_entities("a & b"), "a & b");
        // A non-entity `&...;` is left as-is.
        assert_eq!(escape_wt_entities("&zzz;"), "&zzz;");
    }
}
