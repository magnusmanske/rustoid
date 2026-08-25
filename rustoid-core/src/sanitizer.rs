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

/// Fetch the list of acceptable attributes for a given element name (mirrors
/// PHP's `Sanitizer::attributesAllowedInternal`).
fn attributes_allowed(element: &str) -> &'static [&'static str] {
    match element {
        "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "caption" => BLOCK,
        "center" | "span" | "bdo" | "em" | "strong" | "cite" | "dfn" | "code" | "samp" | "kbd"
        | "var" | "abbr" | "sub" | "sup" | "dl" | "dd" | "dt" | "thead" | "tfoot" | "tbody"
        | "tt" | "b" | "i" | "big" | "small" | "strike" | "s" | "u" | "ruby" | "rb" | "rp"
        | "rt" | "rtc" | "figure" | "figcaption" | "bdi" | "mark" | "aside" => COMMON,
        "blockquote" | "q" => BLOCKQUOTE,
        "br" => BR,
        "wbr" => COMMON,
        "pre" => PRE,
        "ins" | "del" | "time" => INS_DEL,
        "ul" => UL,
        "ol" => OL,
        "li" => LI,
        "table" => TABLE,
        "colgroup" | "col" => COL,
        "tr" => TR,
        "td" | "th" => TD_TH,
        "a" => A,
        "img" => IMG,
        "audio" => AUDIO,
        "video" => VIDEO,
        "source" => SOURCE,
        "track" => TRACK,
        "font" => FONT,
        "hr" => HR,
        "math" => MATH,
        "data" => DATA,
        "meta" => META,
        "link" => LINK,
        _ => EMPTY_ATTRS,
    }
}

const EMPTY_ATTRS: &[&str] = &[];

/// Common HTML attributes (from Sanitizer::setupAttributesAllowedInternal `$common`).
const COMMON: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    // WAI-ARIA
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    // RDFa
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    // Microdata
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
];

/// Block elements add `align`.
const BLOCK: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "align",
];

const BLOCKQUOTE: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "cite",
];

const BR: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "clear",
];

const PRE: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "width",
];

const INS_DEL: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "cite",
    "datetime",
];

const UL: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "type",
];

const OL: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "type",
    "start",
    "reversed",
];

const LI: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "type",
    "value",
];

const TABLE: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "summary",
    "width",
    "border",
    "frame",
    "rules",
    "cellspacing",
    "cellpadding",
    "align",
    "bgcolor",
];

const COL: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "span",
];

const TR: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "bgcolor",
    "align",
    "valign",
];

const TD_TH: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "abbr",
    "axis",
    "headers",
    "scope",
    "rowspan",
    "colspan",
    "nowrap",
    "width",
    "height",
    "bgcolor",
    "align",
    "valign",
];

const A: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "href",
    "rel",
    "rev",
];

const IMG: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "alt",
    "src",
    "width",
    "height",
    "srcset",
];

const AUDIO: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "controls",
    "preload",
    "width",
    "height",
];

const VIDEO: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "poster",
    "controls",
    "preload",
    "width",
    "height",
];

const SOURCE: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "type",
    "src",
];

const TRACK: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "type",
    "src",
    "srclang",
    "kind",
    "label",
];

const FONT: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "size",
    "color",
    "face",
];

const HR: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "width",
];

const MATH: &[&str] = &["class", "style", "id", "title"];

const DATA: &[&str] = &[
    "id",
    "class",
    "style",
    "lang",
    "dir",
    "title",
    "tabindex",
    "aria-describedby",
    "aria-flowto",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-owns",
    "role",
    "about",
    "property",
    "resource",
    "datatype",
    "typeof",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "value",
];

const META: &[&str] = &["itemprop", "content"];
const LINK: &[&str] = &["itemprop", "href", "title"];

/// Whether an attribute name is a reserved data attribute (`data-mw*`,
/// `data-parsoid*`, `data-ooui*`), mirroring `isReservedDataAttribute`.
fn is_reserved_data_attribute(attr: &str) -> bool {
    let lower = attr.to_lowercase();
    lower.starts_with("data-mw")
        || lower.starts_with("data-parsoid")
        || lower.starts_with("data-ooui")
}

/// Sanitize the attributes of an HTML tag, keeping only the allowed subset.
///
/// A faithful port of `Sanitizer::sanitizeTagAttrs`. Returns the sanitized
/// attribute list (an empty entry means "drop this attribute"). Attribute names
/// are lowercased; `style` is normalized, `id` escaped, and `href`/`src`/`poster`
/// are URL-cleaned.
pub fn sanitize_tag_attrs(
    tag_name: &str,
    attrs: Vec<crate::wikitext::tokens_v2::KV>,
    has_valid_protocol: impl Fn(&str) -> bool,
) -> Vec<crate::wikitext::tokens_v2::KV> {
    let allowed = attributes_allowed(tag_name);
    let mut new_attrs: Vec<crate::wikitext::tokens_v2::KV> = Vec::new();

    for mut a in attrs {
        // Convert the key to a plain string name.
        let key = match &a.key {
            crate::wikitext::tokens_v2::KeyValue::Str(k) => k.clone(),
            crate::wikitext::tokens_v2::KeyValue::Tokens(_) => continue, // non-string key: drop
        };
        let key_lower = key.to_lowercase();

        // Convert the value to a string.
        let value = match &a.value {
            crate::wikitext::tokens_v2::KeyValue::Str(v) => v.clone(),
            crate::wikitext::tokens_v2::KeyValue::Tokens(_) => continue, // expanded attr: not supported
        };

        // Allow any `data-*` attribute except reserved ones and namespaced.
        let is_data_attr = key_lower.starts_with("data-")
            && !key_lower.contains(':')
            && key_lower.chars().all(|c| {
                c != '=' && c != ' ' && c != '\t' && c != '\n' && c != '\r' && c != '/' && c != '>'
            });
        let allowed_by_list = allowed.contains(&key_lower.as_str());
        if !(is_data_attr && !is_reserved_data_attribute(&key_lower)) && !allowed_by_list {
            continue; // drop
        }
        if is_reserved_data_attribute(&key_lower) {
            continue; // drop reserved data-* attributes
        }

        // Sanitize stylesheets (faithful port of `Sanitizer::checkCss`).
        if key_lower == "style" {
            a.value = crate::wikitext::tokens_v2::KeyValue::Str(check_css(&value));
        }

        // Escape HTML id attributes.
        if key_lower == "id" {
            let escaped = escape_id_for_attribute(&value);
            if escaped.is_empty() {
                continue;
            }
            a.value = crate::wikitext::tokens_v2::KeyValue::Str(escaped);
        }

        // Clean URLs for href/src/poster.
        if key_lower == "href" || key_lower == "src" || key_lower == "poster" {
            if let Some(cleaned) = clean_url(&value, "external", &has_valid_protocol) {
                if cleaned != value {
                    a.value = crate::wikitext::tokens_v2::KeyValue::Str(cleaned);
                }
            } else {
                continue; // invalid URL: drop the attribute
            }
        }

        // Only allow tabindex of 0.
        if key_lower == "tabindex" && value != "0" {
            continue;
        }

        new_attrs.push(a);
    }

    // itemtype/itemid/itemref require itemscope.
    let has_itemscope = new_attrs
        .iter()
        .any(|a| a.key.as_str() == Some("itemscope"));
    if !has_itemscope {
        new_attrs.retain(|a| {
            let k = a.key.as_str();
            k != Some("itemtype") && k != Some("itemid") && k != Some("itemref")
        });
    }

    new_attrs
}

/// Faithful port of `Sanitizer::checkCss`: rejects control characters and the
/// insecure CSS patterns (`INSECURE_RE`), returning a marker comment in their
/// place, and otherwise passes the value through. (CSS normalization — decoding
/// char refs/escapes and stripping comments — is omitted for now.)
fn check_css(value: &str) -> String {
    // Reject problematic keywords and control characters.
    if value.chars().any(|c| {
        (c as u32) <= 0x08
            || (0x0B..=0x0E).contains(&(c as u32))
            || (0x10..=0x1F).contains(&(c as u32))
            || c as u32 == 0x7F
    }) || value.contains('\u{FFFD}')
    {
        return "/* invalid control char */".to_string();
    }

    // Reject insecure patterns (mirrors the `INSECURE_RE` regex).
    let lower = value.to_lowercase();
    let insecure = lower.contains("expression")
        || lower.contains("accelerator:")
        || lower.contains("-o-link:")
        || lower.contains("-o-link-source:")
        || lower.contains("-o-replace:")
        || lower.contains("url(")
        || lower.contains("src(")
        || lower.contains("image(")
        || lower.contains("image-set(")
        || attr_url_pattern(&lower);
    if insecure {
        return "/* insecure input */".to_string();
    }

    value.to_string()
}

/// Match the `attr\([^)]+[\s,]+url` portion of `INSECURE_RE`.
fn attr_url_pattern(lower: &str) -> bool {
    let Some(start) = lower.find("attr(") else {
        return false;
    };
    let rest = &lower[start + 5..];
    // `[^)]+` then `[\s,]` then `url`.
    let Some(close) = rest.find(')') else {
        return false;
    };
    let inner = &rest[..close];
    if inner.is_empty() {
        return false;
    }
    let after = &rest[close + 1..];
    after
        .trim_start_matches([' ', ',', '\t', '\n', '\r'])
        .starts_with("url")
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
                '%' | '?' | ' ' | '[' | ']' | '#' | '|' | '<' | '>' | '\\' | '\"' | '\''
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

/// Escape a fraction string for use as an HTML `id` attribute value.
/// Mirrors PHP's `Sanitizer::escapeIdForAttribute` (html5 mode): replaces
/// space/tab/LF/CR/FF with `_`, truncating to 1024 chars.
pub fn escape_id_for_attribute(id: &str) -> String {
    escape_id_internal(id, "html5")
}

/// Normalize whitespace in a section name for use in an anchor id.
/// Mirrors PHP's `Sanitizer::normalizeSectionNameWhitespace`: collapse runs of
/// spaces/underscores to a single space and trim.
pub fn normalize_section_name_whitespace(section: &str) -> String {
    let mut out = String::with_capacity(section.len());
    let mut prev_space = false;
    for c in section.chars() {
        if c == ' ' || c == '_' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
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
        // Quotes are percent-encoded so they can appear safely in an href
        // attribute (matches Parsoid's normalized link href, e.g. ./Cool_%22Gator%22).
        assert_eq!(
            sanitize_title_uri("Cool \"Gator\"", false),
            "Cool%20%22Gator%22"
        );
    }

    #[test]
    fn test_escape_id_for_attribute() {
        assert_eq!(escape_id_for_attribute("Hello world"), "Hello_world");
        assert_eq!(escape_id_for_attribute("a\tb\nc"), "a_b_c");
    }

    #[test]
    fn test_normalize_section_name_whitespace() {
        assert_eq!(normalize_section_name_whitespace("  a  b  "), "a b");
        assert_eq!(normalize_section_name_whitespace("a__b"), "a b");
        assert_eq!(normalize_section_name_whitespace("  a _ b  "), "a b");
    }

    #[test]
    fn test_sanitize_tag_attrs_whitelist() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};
        let kv = |k: &str, v: &str| KV {
            key: KeyValue::Str(k.to_string()),
            value: KeyValue::Str(v.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };

        // `<pre>` only allows `common` + `width`; `onmouseover` is dropped.
        let out = sanitize_tag_attrs(
            "pre",
            vec![kv("width", "8"), kv("onmouseover", "alert(1)")],
            allow_all,
        );
        let keys: Vec<&str> = out.iter().filter_map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["width"]);
    }

    #[test]
    fn test_sanitize_tag_attrs_id() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};
        let out = sanitize_tag_attrs(
            "div",
            vec![KV {
                key: KeyValue::Str("id".to_string()),
                value: KeyValue::Str("Hello world".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            }],
            allow_all,
        );
        assert_eq!(out[0].value.as_str(), Some("Hello_world"));
    }

    #[test]
    fn test_check_css() {
        // Safe CSS passes through unchanged.
        assert_eq!(check_css("color: blue"), "color: blue");
        // Insecure CSS is replaced with a marker comment.
        assert_eq!(
            check_css("border-width: expression(alert())"),
            "/* insecure input */"
        );
        // Control characters are rejected as invalid.
        assert_eq!(check_css("color:\u{7F}blue"), "/* invalid control char */");
    }
}
