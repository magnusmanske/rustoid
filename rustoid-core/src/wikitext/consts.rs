//! Wikitext constants — faithful port of PHP Parsoid's `src/Wikitext/Consts.php`.
//!
//! These constants govern block/inline element classification and are used by
//! the ParagraphWrapper, ListHandler, TreeBuilder, and other pipeline stages.

use std::collections::HashSet;

/// Create a set of string literals.
macro_rules! set {
    ($($s:expr),* $(,)?) => {
        {
            let mut set = HashSet::new();
            $( set.insert($s.to_string()); )*
            set
        }
    };
}

/// Block elements (open block scope when entering, close when exiting).
pub fn block_elems() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        set![
            "table", "h1", "h2", "h3", "h4", "h5", "h6", "pre", "p", "ul", "ol", "dl"
        ]
    });
    &SET
}

/// Anti-block elements (close block scope when entering, open when exiting).
pub fn anti_block_elems() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> =
        once_cell::sync::Lazy::new(|| set!["td", "th"]);
    &SET
}

/// Always-block elements (open block scope when entering, open when exiting).
pub fn always_block_elems() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> =
        once_cell::sync::Lazy::new(|| set!["tr", "caption", "dt", "dd", "li"]);
    &SET
}

/// Never-block elements (close block scope when entering, close when exiting).
pub fn never_block_elems() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> =
        once_cell::sync::Lazy::new(|| set!["center", "blockquote", "div", "hr", "figure", "aside"]);
    &SET
}

/// All wikitext block elements (union of the four categories above).
pub fn wikitext_block_elems() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        let mut s = HashSet::new();
        s.extend(block_elems().iter().cloned());
        s.extend(anti_block_elems().iter().cloned());
        s.extend(always_block_elems().iter().cloned());
        s.extend(never_block_elems().iter().cloned());
        s
    });
    &SET
}

/// HTML void elements (self-closing in HTML5).
pub fn void_tags() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        set![
            "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
            "source", "track", "wbr"
        ]
    });
    &SET
}

/// Metadata content tags (mirrors `Consts::$HTML['MetaDataTags']`).
pub fn meta_data_tags() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        set![
            "base", "link", "meta", "noscript", "script", "style", "template", "title"
        ]
    });
    &SET
}

/// Formatting tags (mirrors `Consts::$HTML['FormattingTags']`).
pub fn formatting_tags() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        set![
            "a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small", "strike", "strong",
            "tt", "u",
        ]
    });
    &SET
}

/// Only-inline elements (mirrors `Consts::$HTML['OnlyInlineElements']`).
pub fn only_inline_elements() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        set![
            "a", "abbr", "acronym", "applet", "audio", "b", "basefont", "bdi", "bdo", "big", "br",
            "button", "cite", "code", "data", "del", "dfn", "em", "font", "i", "iframe", "img",
            "input", "ins", "kbd", "label", "legend", "map", "mark", "object", "param", "q", "rb",
            "rbc", "rp", "rt", "rtc", "ruby", "s", "samp", "select", "small", "source", "span",
            "strike", "strong", "sub", "sup", "textarea", "time", "track", "tt", "u", "var",
            "video", "wbr",
        ]
    });
    &SET
}

/// HTML tags under which a text node/placeholder would be fostered out
/// (mirrors `Consts::$HTML['FosterablePosition']`).
pub fn fosterable_position() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> =
        once_cell::sync::Lazy::new(|| set!["table", "thead", "tbody", "tfoot", "tr"]);
    &SET
}

/// Tags whose native wikitext markup should have whitespace trimmed from its
/// content (unless they are literal HTML tags). Mirrors
/// `Consts::$WikitextTagsWithTrimmableWS`.
pub fn wikitext_tags_with_trimmable_ws() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        set![
            "h1", "h2", "h3", "h4", "h5", "h6", "ol", "li", "ul", "dd", "dl", "dt", "td", "th",
            "caption"
        ]
    });
    &SET
}

/// HTML tags that are generated only when the corresponding wikitext occurs in
/// a start-of-line (SOL) context. Mirrors
/// `Consts::$HTMLTagsRequiringSOLContext`.
pub fn html_tags_requiring_sol_context() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        set![
            "pre", "h1", "h2", "h3", "h4", "h5", "h6", "ol", "li", "ul", "dd", "dl", "dt"
        ]
    });
    &SET
}

/// Wikitext tags that are composed of quote characters. Mirrors
/// `Consts::$WTQuoteTags`.
pub fn wt_quote_tags() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> =
        once_cell::sync::Lazy::new(|| set!["i", "b"]);
    &SET
}

/// Table tags that are valid children of another table tag. Mirrors
/// `Consts::$HTML['ChildTableTags']`.
pub fn child_table_tags() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> =
        once_cell::sync::Lazy::new(|| set!["tbody", "thead", "tfoot", "tr", "caption", "th", "td"]);
    &SET
}

/// Elements that should be output as empty (`<li/>` etc.) when they have no
/// content. Mirrors `Consts::$Output['FlaggedEmptyElts']`.
pub fn flagged_empty_elts() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> =
        once_cell::sync::Lazy::new(|| set!["li", "tbody", "tr", "p"]);
    &SET
}

/// Statically-known wikitext tag widths as `[open_width, close_width]`, where
/// `None` represents a width that must be computed from the source. Mirrors
/// `Consts::$WtTagWidths`.
pub fn wt_tag_widths(tag: &str) -> Option<(Option<usize>, Option<usize>)> {
    // `null` widths become `None`; everything else becomes `Some(n)`.
    let open = |w: Option<usize>| w;
    let entry: Option<(Option<usize>, Option<usize>)> = match tag {
        "body" => Some((open(Some(0)), open(Some(0)))),
        "html" => Some((open(Some(0)), open(Some(0)))),
        "head" => Some((open(Some(0)), open(Some(0)))),
        "p" => Some((open(Some(0)), open(Some(0)))),
        "meta" => Some((open(Some(0)), open(Some(0)))),
        // PreHandler::newIndentPreWS explains why the opening width is 0, not 1.
        "pre" => Some((open(Some(0)), open(Some(0)))),
        "ol" => Some((open(Some(0)), open(Some(0)))),
        "ul" => Some((open(Some(0)), open(Some(0)))),
        "dl" => Some((open(Some(0)), open(Some(0)))),
        "li" => Some((open(Some(1)), open(Some(0)))),
        "dt" => Some((open(Some(1)), open(Some(0)))),
        "dd" => Some((open(Some(1)), open(Some(0)))),
        "h1" => Some((open(Some(1)), open(Some(1)))),
        "h2" => Some((open(Some(2)), open(Some(2)))),
        "h3" => Some((open(Some(3)), open(Some(3)))),
        "h4" => Some((open(Some(4)), open(Some(4)))),
        "h5" => Some((open(Some(5)), open(Some(5)))),
        "h6" => Some((open(Some(6)), open(Some(6)))),
        "hr" => Some((open(Some(4)), open(Some(0)))),
        "table" => Some((open(Some(2)), open(Some(2)))),
        "tbody" => Some((open(Some(0)), open(Some(0)))),
        "thead" => Some((open(Some(0)), open(Some(0)))),
        "tfoot" => Some((open(Some(0)), open(Some(0)))),
        "tr" => Some((open(None), open(Some(0)))),
        "td" => Some((open(None), open(Some(0)))),
        "th" => Some((open(None), open(Some(0)))),
        "caption" => Some((open(None), open(Some(0)))),
        "b" => Some((open(Some(3)), open(Some(3)))),
        "i" => Some((open(Some(2)), open(Some(2)))),
        "br" => Some((open(Some(0)), open(Some(0)))),
        "figure" => Some((open(Some(2)), open(Some(2)))),
        "figcaption" => Some((open(Some(0)), open(Some(0)))),
        _ => None,
    };
    entry
}

/// HTML tags whose wikitext equivalents are zero-width. This is *derived* from
/// `wt_tag_widths` (any tag with `[0, 0]` widths, excluding `html`/`head`/`body`
/// and the special-cased `pre`), exactly as PHP derives
/// `Consts::$ZeroWidthWikitextTags`.
pub fn zero_width_wikitext_tags() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        // The derived result, per the `consts` init loop:
        //   p, meta, ol, ul, dl, tbody, thead, tfoot, br, figcaption
        set![
            "p",
            "meta",
            "ol",
            "ul",
            "dl",
            "tbody",
            "thead",
            "tfoot",
            "br",
            "figcaption"
        ]
    });
    &SET
}

/// HTML block-level tags (used for wikitext-to-HTML block detection).
pub fn block_tags() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> = once_cell::sync::Lazy::new(|| {
        set![
            "div",
            "blockquote",
            "pre",
            "table",
            "tr",
            "td",
            "th",
            "center",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "ul",
            "ol",
            "li",
            "dl",
            "dt",
            "dd",
            "section",
            "article",
            "aside",
            "nav",
            "header",
            "footer",
            "hr",
            "p",
            "figure",
            "figcaption",
            "main"
        ]
    });
    &SET
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_elems_contains_table() {
        assert!(block_elems().contains("table"));
        assert!(block_elems().contains("h1"));
    }

    #[test]
    fn test_wikitext_block_elems_union() {
        assert!(wikitext_block_elems().contains("table"));
        assert!(wikitext_block_elems().contains("td"));
        assert!(wikitext_block_elems().contains("li"));
        assert!(wikitext_block_elems().contains("div"));
    }

    #[test]
    fn test_void_tags() {
        assert!(void_tags().contains("br"));
        assert!(void_tags().contains("img"));
        assert!(!void_tags().contains("div"));
    }

    #[test]
    fn test_trimmable_ws() {
        assert!(wikitext_tags_with_trimmable_ws().contains("h1"));
        assert!(wikitext_tags_with_trimmable_ws().contains("caption"));
        assert!(!wikitext_tags_with_trimmable_ws().contains("p"));
    }

    #[test]
    fn test_sol_context() {
        assert!(html_tags_requiring_sol_context().contains("pre"));
        assert!(html_tags_requiring_sol_context().contains("li"));
        assert!(!html_tags_requiring_sol_context().contains("p"));
    }

    #[test]
    fn test_wt_quote_tags() {
        assert!(wt_quote_tags().contains("i"));
        assert!(wt_quote_tags().contains("b"));
        assert!(!wt_quote_tags().contains("u"));
    }

    #[test]
    fn test_child_table_tags() {
        assert!(child_table_tags().contains("tr"));
        assert!(child_table_tags().contains("td"));
        assert!(!child_table_tags().contains("table"));
    }

    #[test]
    fn test_flagged_empty_elts() {
        assert!(flagged_empty_elts().contains("li"));
        assert!(flagged_empty_elts().contains("p"));
        assert!(!flagged_empty_elts().contains("ul"));
    }

    #[test]
    fn test_wt_tag_widths() {
        assert_eq!(wt_tag_widths("h3"), Some((Some(3), Some(3))));
        assert_eq!(wt_tag_widths("tr"), Some((None, Some(0))));
        assert_eq!(wt_tag_widths("pre"), Some((Some(0), Some(0))));
        assert_eq!(wt_tag_widths("bogus"), None);
    }

    #[test]
    fn test_zero_width_wikitext_tags() {
        // Derived from WtTagWidths: [0,0] excluding html/head/body/pre.
        assert!(zero_width_wikitext_tags().contains("p"));
        assert!(zero_width_wikitext_tags().contains("ol"));
        assert!(zero_width_wikitext_tags().contains("br"));
        assert!(zero_width_wikitext_tags().contains("figcaption"));
        assert!(!zero_width_wikitext_tags().contains("pre"));
        assert!(!zero_width_wikitext_tags().contains("li"));
        assert!(!zero_width_wikitext_tags().contains("h1"));
    }
}
