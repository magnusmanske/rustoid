//! HTML/URL sanitization — faithful port of the relevant subset of PHP
//! Parsoid's `src/Core/Sanitizer.php`, focused on the URL helpers used by the
//! link handlers (`cleanUrl`, `encodeUrlForExtLink`, `escapeLiteralHTMLTag`).
//!
//! Keeps parity with the PHP logic for these specific methods.

use crate::wikitext::tokens_v2::ParsoidToken;

/// Characters that will be ignored in IDNs (RFC 8264 / Unicode DerivedCoreProperties).
/// Stripped before further processing so deny-lists and such work.
const IDN_RE: &[char] = &[
    // General whitespace
    ' ',
    '\t',
    '\n',
    '\r',
    '\u{0B}',
    '\u{0C}',
    // U+00AD SOFT HYPHEN
    '\u{00AD}',
    // U+034F COMBINING GRAPHEME JOINER
    '\u{034F}',
    // U+061C ARABIC LETTER MARK
    '\u{061C}',
    // U+115F..U+1160 HANGUL fillers
    '\u{115F}',
    '\u{1160}',
    // U+17B4..U+17B5 KHMER vowels
    '\u{17B4}',
    '\u{17B5}',
    // U+180B..U+180D MONGOLIAN variation selectors
    '\u{180B}',
    '\u{180C}',
    '\u{180D}',
    // U+180E MONGOLIAN VOWEL SEPARATOR
    '\u{180E}',
    // U+200B..U+200F zero-width space..RTL mark
    '\u{200B}',
    '\u{200C}',
    '\u{200D}',
    '\u{200E}',
    '\u{200F}',
    // U+202A..U+202E LTR/RTL embedding..override
    '\u{202A}',
    '\u{202B}',
    '\u{202C}',
    '\u{202D}',
    '\u{202E}',
    // U+2060..U+2064 word joiner..invisible plus
    '\u{2060}',
    '\u{2061}',
    '\u{2062}',
    '\u{2063}',
    '\u{2064}',
    // U+2065 <reserved>
    '\u{2065}',
    // U+2066..U+206F isolates..nominal digit shapes
    '\u{2066}',
    '\u{2067}',
    '\u{2068}',
    '\u{2069}',
    '\u{206A}',
    '\u{206B}',
    '\u{206C}',
    '\u{206D}',
    '\u{206E}',
    '\u{206F}',
    // U+3164 HANGUL FILLER
    '\u{3164}',
    // U+FE00..U+FE0F variation selectors
    '\u{FE00}',
    '\u{FE01}',
    '\u{FE02}',
    '\u{FE03}',
    '\u{FE04}',
    '\u{FE05}',
    '\u{FE06}',
    '\u{FE07}',
    '\u{FE08}',
    '\u{FE09}',
    '\u{FE0A}',
    '\u{FE0B}',
    '\u{FE0C}',
    '\u{FE0D}',
    '\u{FE0E}',
    '\u{FE0F}',
    // U+FEFF ZERO WIDTH NO-BREAK SPACE
    '\u{FEFF}',
    // U+FFA0 HALFWIDTH HANGUL FILLER
    '\u{FFA0}',
    // U+FFF0..U+FFF8 reserved
    '\u{FFF0}',
    '\u{FFF1}',
    '\u{FFF2}',
    '\u{FFF3}',
    '\u{FFF4}',
    '\u{FFF5}',
    '\u{FFF6}',
    '\u{FFF7}',
    '\u{FFF8}',
    // U+1BCA0..U+1BCA3 shorthand format letters
    '\u{1BCA0}',
    '\u{1BCA1}',
    '\u{1BCA2}',
    '\u{1BCA3}',
    // U+1D173..U+1D17A musical symbols
    '\u{1D173}',
    '\u{1D174}',
    '\u{1D175}',
    '\u{1D176}',
    '\u{1D177}',
    '\u{1D178}',
    '\u{1D179}',
    '\u{1D17A}',
    // U+E0000, U+E0001, U+E0002..U+E001F (tags), and E0020..E0FFF (variation selectors/reserved)
    '\u{E0000}',
    '\u{E0001}',
];

/// Strip IDN-ignored characters from a host string (part of `cleanUrl`).
fn strip_idns(host: &str) -> String {
    host.chars().filter(|c| !is_idn_ignored(*c)).collect()
}

/// Whether a char is in the IDN-ignored set.
fn is_idn_ignored(c: char) -> bool {
    if c.is_whitespace() {
        return true;
    }
    let cp = c as u32;
    IDN_RE.contains(&c) || (0xE0020..=0xE0FFF).contains(&cp) || (0xE0002..=0xE001F).contains(&cp)
}

/// URL-encode characters not in the legacy parser's `EXT_LINK_URL_CLASS`
/// (matching PHP's `encodeUrlForExtLink`). The pipe char is a special
/// exception introduced in core commit 2519512.
pub fn encode_url_for_ext_link(href: &str) -> String {
    href.chars()
        .map(|c| {
            if matches!(c, ']' | '[' | '<' | '>' | '"' | '|')
                || (c as u32) <= 0x20
                || (c as u32) == 0x7F
            {
                // urlencode the character.
                percent_encode_char(c)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Percent-encode a single character (UTF-8 bytes).
fn percent_encode_char(c: char) -> String {
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    s.bytes()
        .map(|b| format!("%{b:02X}"))
        .collect::<Vec<_>>()
        .join("")
}

/// Clean a URL, stripping IDN chars and validating protocol. Mirrors PHP's
/// `Sanitizer::cleanUrl`. `mode` is "wikilink" or "external".
///
/// `has_valid_protocol` is a site-config callback; a default (permissive)
/// implementation is provided.
pub fn clean_url(
    href: &str,
    mode: &str,
    has_valid_protocol: impl Fn(&str) -> bool,
) -> Option<String> {
    let href = if mode != "wikilink" {
        encode_url_for_ext_link(href)
    } else {
        href.to_string()
    };

    // Match: ^((?:[a-zA-Z][^:/]*:)?(?://)?)([^/]+)(/?.*)
    // We split into proto, host, path.
    let (proto, host, path) = split_url(&href);

    if !proto.is_empty() && !has_valid_protocol(proto) {
        // invalid proto, disallow URL
        return None;
    }

    let host = strip_idns(host);

    // Handle IPv6 hosts: %5B ... %5D(:port)?
    let host = if let Some(rest) = host.strip_prefix("%5B") {
        if let Some(end) = rest.find("%5D") {
            let ipv6 = &rest[..end];
            let after = &rest[end + 3..];
            let mut new_host = format!("[{ipv6}]");
            new_host.push_str(after);
            new_host
        } else {
            host
        }
    } else {
        host
    };

    Some(format!("{proto}{host}{path}"))
}

/// Split a URL into (proto, host, path) per the PHP regex
/// `#^((?:[a-zA-Z][^:/]*:)?(?://)?)([^/]+)(/?.*)#`.
///
/// Returns (proto, host, path), with empty strings for missing parts.
fn split_url(href: &str) -> (&str, &str, &str) {
    // proto: optional `scheme:` optionally followed by `//`.
    let mut proto_end = 0;

    // Look for `scheme:` where scheme starts with ASCII letter and contains no `/` or `:`.
    if let Some(colon) = href.find(':') {
        let scheme = &href[..colon];
        if !scheme.is_empty()
            && scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && !scheme.contains('/')
        {
            proto_end = colon + 1;
            // Check for `//`.
            if href[proto_end..].starts_with("//") {
                proto_end += 2;
            }
        }
    }

    let proto = &href[..proto_end];
    let rest = &href[proto_end..];

    // host: up to first `/`.
    let host_end = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..host_end];
    let path = &rest[host_end..];

    (proto, host, path)
}

/// Validate / sanitize a literal HTML tag token. Mirrors PHP's
/// `escapeLiteralHTMLTag`: `<meta>` and `<link>` must have `itemprop`, and
/// `<meta>` additionally needs `content`; `<link>` needs `href`.
pub fn escape_literal_html_tag(token: &ParsoidToken) -> bool {
    let name = token.get_name();
    if name != "meta" && name != "link" {
        return false;
    }

    if token.get_attribute_v("itemprop").is_none() {
        return true;
    }

    if name == "meta" && token.get_attribute_v("content").is_none() {
        return true;
    }

    if name == "link" && token.get_attribute_v("href").is_none() {
        return true;
    }

    false
}

/// Sanitize a title for use in a URI. Mirrors PHP's `Sanitizer::sanitizeTitleURI`.
///
/// Percent-encodes characters in the set `[%? \[\]#|<>\\]`, and (if a fragment
/// is present) escapes it as an HTML5 id fragment.
pub fn sanitize_title_uri(title: &str, _is_interwiki: bool) -> String {
    let idx = title.find('#');
    let (main_part, anchor) = match idx {
        Some(pos) => (&title[..pos], Some(&title[pos + 1..])),
        None => (title, None),
    };

    // Replace the unsafe set with percent-encoding.
    let encoded: String = main_part
        .chars()
        .map(|c| {
            if matches!(
                c,
                '%' | '?' | ' ' | '[' | ']' | '#' | '|' | '<' | '>' | '\\'
            ) {
                percent_encode_char(c)
            } else {
                c.to_string()
            }
        })
        .collect();

    if let Some(anchor) = anchor {
        format!("{encoded}#{}", escape_id_for_link(anchor))
    } else {
        encoded
    }
}

/// Escape a fragment string as an HTML5 id (used by `sanitize_title_uri`).
/// Mirrors PHP's `escapeIdForLink` with html5 mode.
fn escape_id_for_link(id: &str) -> String {
    let id: String = escape_id_internal(id, "html5");
    // Do percent encoding of percent signs for href (but not id) attrs.
    id.replace('%', "%25")
}

/// Escape a string into an HTML5/legacy id. Mirrors `escapeIdInternal`.
fn escape_id_internal(id: &str, mode: &str) -> String {
    // Truncate overly-long ids (griefer protection, T251506).
    let mut id = id.chars().take(1024).collect::<String>();
    match mode {
        "html5" => {
            id = id
                .chars()
                .map(|c| match c {
                    '\t' | '\n' | '\u{0C}' | '\r' | ' ' => '_',
                    other => other,
                })
                .collect();
            id
        }
        "legacy" => {
            id = id.replace(' ', "_");
            let encoded = percent_encode_all(&id);
            encoded.replace("%3A", ":").replace('%', ".")
        }
        _ => id,
    }
}

/// Percent-encode a full string (used by legacy id escaping).
fn percent_encode_all(s: &str) -> String {
    s.bytes()
        .map(|b| {
            // urlencode semantics: encode all except unreserved.
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::{DataParsoid, TagTk};

    fn allow_all(_proto: &str) -> bool {
        true
    }

    #[test]
    fn test_clean_url_basic() {
        let out = clean_url("http://example.com/foo", "wikilink", allow_all);
        assert_eq!(out.as_deref(), Some("http://example.com/foo"));
    }

    #[test]
    fn test_encode_url_for_ext_link() {
        let out = encode_url_for_ext_link("a b|c");
        assert_eq!(out, "a%20b%7Cc");
    }

    #[test]
    fn test_strip_idns() {
        // SOFT HYPHEN should be stripped.
        assert_eq!(strip_idns("exa\u{00AD}mple"), "example");
    }

    #[test]
    fn test_escape_literal_html_tag_meta_no_itemprop() {
        let tk = TagTk::new("meta", vec![], DataParsoid::default());
        let token = ParsoidToken::Tag(tk);
        assert!(escape_literal_html_tag(&token));
    }

    #[test]
    fn test_sanitize_title_uri() {
        assert_eq!(sanitize_title_uri("Foo", false), "Foo");
        assert_eq!(sanitize_title_uri("Foo bar", false), "Foo%20bar");
        assert_eq!(sanitize_title_uri("Foo#Section", false), "Foo#Section");
    }
}
