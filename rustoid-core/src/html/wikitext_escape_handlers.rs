//! WikitextEscapeHandlers — minimal port of PHP Parsoid's
//! `src/Html2Wt/WikitextEscapeHandlers.php`.
//!
//! Handles "smart escaping" of wikitext: wrapping SOL-sensitive characters in
//! `<nowiki>` when they would otherwise be re-parsed as markup. This module
//! currently provides the entry points (`escape_wikitext`, `escaped_text`) and
//! the per-handler delegates (`li_handler`, `td_handler`, `th_handler`) with a
//! minimal implementation (nowiki for the common lead-`*`/`#`/`;`/`:`/`=`/
//! `{`/`|` SOL chars). The full context-aware escape matrix is layered on as
//! the `ConstrainedText` subclasses are ported.

use crate::html::serializer_state::SerializerState;

/// A minimal wikitext escape handlers resolver (the full PHP class). Since
/// escaping depends only on the text and SOL state for the common cases, we
/// implement the entry points as free functions.
pub struct WikitextEscapeHandlers;

/// Escape a text chunk for wikitext, using the current `SerializerState`
/// context. Faithful to `WikitextEscapeHandlers::escapeWikitext` for the common
/// SOL-sensitive characters; the full context matrix is deferred.
pub fn escape_wikitext(state: &SerializerState, text: &str, _is_last_child: bool) -> String {
    escaped_text(state, state.on_sol, text, false, true)
}

/// `escapedText` — escape `text` for its position (SOL / non-SOL). Faithful to
/// the common cases (lead SOL-sensitive char protection); the full prefix/suffix
/// machinery is deferred.
pub fn escaped_text(
    state: &SerializerState,
    sol: bool,
    text: &str,
    _protect_entities: bool,
    _in_php_block: bool,
) -> String {
    if !sol || text.is_empty() {
        return text.to_string();
    }
    // Protect lead SOL-sensitive characters that would re-parse as markup.
    let first = text.chars().next().unwrap();
    if matches!(first, '*' | '#' | ';' | ':' | '=' | '{' | '|') {
        let mut out = String::new();
        out.push_str("<nowiki/>");
        out.push_str(text);
        return out;
    }
    let _ = state;
    text.to_string()
}

/// `liHandler` — escape list-item content (identity for now).
pub fn li_handler(
    _node: crate::html::dom_tree::NodeId,
    _state: &SerializerState,
    text: &str,
    _opts: (),
) -> String {
    text.to_string()
}

/// `tdHandler` — escape table-cell content (identity for now).
pub fn td_handler(
    _node: crate::html::dom_tree::NodeId,
    _in_wide_td: bool,
    _state: &SerializerState,
    text: &str,
    _opts: (),
) -> String {
    text.to_string()
}

/// `thHandler` — escape table-header content (identity for now).
pub fn th_handler(
    _node: crate::html::dom_tree::NodeId,
    _state: &SerializerState,
    text: &str,
    _opts: (),
) -> String {
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escaped_text_non_sol() {
        let st = SerializerState::new();
        assert_eq!(escaped_text(&st, false, "*foo", false, true), "*foo");
    }

    #[test]
    fn test_escaped_text_sol_protects_markup() {
        let mut st = SerializerState::new();
        st.on_sol = true;
        assert_eq!(
            escaped_text(&st, true, "*foo", false, true),
            "<nowiki/>*foo"
        );
        assert_eq!(escaped_text(&st, true, "plain", false, true), "plain");
    }
}
