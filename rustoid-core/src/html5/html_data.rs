//! Faithful port of the RemexHtml tree-construction data (`HTMLData`), the
//! same data source that Wikimedia Parsoid's tree builder (`TreeBuilderStage`)
//! relies on.
//!
//! Ports `Wikimedia\RemexHtml\HTMLData` — the `SPECIAL` element set (used for
//! scope checks and the adoption agency algorithm) and the tag property sets
//! (`void`, `prefixLF`, `rawText`).

/// HTML namespace URI.
pub const NS_HTML: &str = "http://www.w3.org/1999/xhtml";
/// MathML namespace URI.
pub const NS_MATHML: &str = "http://www.w3.org/1998/Math/MathML";
/// SVG namespace URI.
pub const NS_SVG: &str = "http://www.w3.org/2000/svg";

/// The elements in the "special" category, keyed by (namespace, name).
/// This mirrors `HTMLData::SPECIAL`.
pub fn is_special(ns: &str, name: &str) -> bool {
    match ns {
        NS_HTML => matches!(
            name,
            "address"
                | "applet"
                | "area"
                | "article"
                | "aside"
                | "base"
                | "basefont"
                | "bgsound"
                | "blockquote"
                | "body"
                | "br"
                | "button"
                | "caption"
                | "center"
                | "col"
                | "colgroup"
                | "dd"
                | "details"
                | "dir"
                | "div"
                | "dl"
                | "dt"
                | "embed"
                | "fieldset"
                | "figcaption"
                | "figure"
                | "footer"
                | "form"
                | "frame"
                | "frameset"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "head"
                | "header"
                | "hr"
                | "html"
                | "iframe"
                | "img"
                | "input"
                | "li"
                | "link"
                | "listing"
                | "main"
                | "marquee"
                | "menu"
                | "menuitem"
                | "meta"
                | "nav"
                | "noembed"
                | "noframes"
                | "noscript"
                | "object"
                | "ol"
                | "p"
                | "param"
                | "plaintext"
                | "pre"
                | "script"
                | "section"
                | "select"
                | "source"
                | "style"
                | "summary"
                | "table"
                | "tbody"
                | "td"
                | "template"
                | "textarea"
                | "tfoot"
                | "th"
                | "thead"
                | "title"
                | "tr"
                | "track"
                | "ul"
                | "wbr"
                | "xmp"
        ),
        NS_MATHML => matches!(name, "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml"),
        NS_SVG => matches!(name, "foreignObject" | "desc" | "title"),
        _ => false,
    }
}

/// Whether a tag is a void element (mirrors `HTMLData::TAGS['void']`).
pub fn is_void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "menuitem"
            | "meta"
            | "param"
            | "source"
            | "spacer"
            | "track"
            | "wbr"
    )
}

/// Whether a tag is a "prefix LF" element (mirrors `HTMLData::TAGS['prefixLF']`).
pub fn is_prefix_lf(name: &str) -> bool {
    matches!(name, "pre" | "textarea" | "listing")
}

/// Whether a tag is a raw-text element (mirrors `HTMLData::TAGS['rawText']`).
pub fn is_raw_text(name: &str) -> bool {
    matches!(
        name,
        "style" | "script" | "xmp" | "iframe" | "noembed" | "noframes" | "plaintext"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_special_html() {
        assert!(is_special(NS_HTML, "table"));
        assert!(is_special(NS_HTML, "div"));
        assert!(is_special(NS_HTML, "p"));
        assert!(!is_special(NS_HTML, "span"));
        assert!(!is_special(NS_HTML, "em"));
    }

    #[test]
    fn test_special_foreign() {
        assert!(is_special(NS_MATHML, "mi"));
        assert!(is_special(NS_SVG, "foreignObject"));
        assert!(!is_special(NS_SVG, "circle"));
    }

    #[test]
    fn test_void() {
        assert!(is_void_tag("br"));
        assert!(is_void_tag("meta"));
        assert!(!is_void_tag("div"));
    }

    #[test]
    fn test_raw_text() {
        assert!(is_raw_text("script"));
        assert!(is_raw_text("style"));
        assert!(!is_raw_text("div"));
    }
}
