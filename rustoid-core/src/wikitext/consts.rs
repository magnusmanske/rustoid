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

/// HTML tags under which a text node/placeholder would be fostered out
/// (mirrors `Consts::$HTML['FosterablePosition']`).
pub fn fosterable_position() -> &'static HashSet<String> {
    static SET: once_cell::sync::Lazy<HashSet<String>> =
        once_cell::sync::Lazy::new(|| set!["table", "thead", "tbody", "tfoot", "tr"]);
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
}
