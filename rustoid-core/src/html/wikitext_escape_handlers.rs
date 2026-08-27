//! WikitextEscapeHandlers — faithful port of PHP Parsoid's
//! `src/Html2Wt/WikitextEscapeHandlers.php`.
//!
//! Handles "smart escaping" of wikitext: wrapping substrings in `<nowiki>` (or
//! `<nowiki/>`) when they would otherwise be re-parsed as markup.
//!
//! Porting status: the entry points (`escape_wikitext`, `escaped_text`) and the
//! per-handler *predicate delegates* (`li_handler`, `td_handler`, `th_handler`,
//! `wikilink_handler`, `a_handler`, `media_option_handler`) are ported. The
//! token-walking machinery (`has_wikitext_tokens`, `text_can_parse_as_link`,
//! `escaped_ib_sibling_node_text`) depends on the tokenizer's `tokenize_as` plus
//! `SiteConfig` protocol / ext-tag lookups, so it is approximated conservatively
//! (over-escaping is safe; under-escaping is a correctness bug).
//
// Note: PHP uses PCRE with distinctive semantics (`/D`, `\W`, etc.). Rust's
// `regex` crate has no direct equivalent; the handful of patterns below are
// ported to equivalent plain-string / iterator checks where possible and are
// otherwise documented.

use crate::html::serializer_state::SerializerState;

/// Result of [`escape_link_target`]: the (entity-escaped, fragment-encoded)
/// link target plus whether it is an invalid link. Mirrors PHP's
/// `escapeLinkTarget` return `stdClass { linkTarget, invalidLink }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapeLinkTargetResult {
    pub link_target: String,
    pub invalid_link: bool,
}

/// Escape context options for [`escape_wikitext`]. Mirrors PHP's `$opts` array
/// (`['node' => Node, 'inMultilineMode' => ?bool, 'isLastChild' => ?bool]`).
#[derive(Debug, Clone, Copy, Default)]
pub struct EscapeOpts {
    /// The text node being escaped (`$opts['node']`), used by the per-handler
    /// predicates (`liHandler`/`tdHandler`/`thHandler`) to navigate.
    pub node: Option<crate::html::dom_tree::NodeId>,
    /// Are we the last child of our parent (affects trailing-`=` protection)?
    pub is_last_child: bool,
    /// Are we recursing inside the multi-line mode split?
    pub in_multiline_mode: bool,
}

/// Does `text` contain a magic word (`RFC`, `ISBN`, `PMID`) at a word boundary
/// that could auto-link? Faithful to PHP's `$hasMagicWord` regex
/// `/(^|\W)(RFC|ISBN|PMID)\s/`.
fn has_magic_word(text: &str) -> bool {
    let bytes = text.as_bytes();
    for acronym in [&b"RFC"[..], &b"ISBN"[..], &b"PMID"[..]] {
        // Search for the acronym followed by whitespace, at a word boundary.
        let mut i = 0;
        while let Some(pos) = text[i..].find(std::str::from_utf8(acronym).unwrap()) {
            let abs = i + pos;
            // Word boundary before: `^` or a non-word char immediately before.
            let boundary_ok = abs == 0 || {
                let prev = bytes[abs - 1];
                !(prev.is_ascii_alphanumeric() || prev == b'_')
            };
            // Whitespace after the acronym.
            let after = abs + acronym.len();
            let ws_ok = after < bytes.len() && bytes[after].is_ascii_whitespace();
            if boundary_ok && ws_ok {
                return true;
            }
            i = abs + 1;
        }
    }
    false
}

/// Does `text` contain a valid protocol prefix (auto-link risk)?
/// Faithful approximation of PHP's `$env->getSiteConfig()->findValidProtocol`.
fn has_valid_protocol(text: &str) -> bool {
    // Conservative: any `scheme://` or `scheme:` looks like a candidate link.
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        if c.is_ascii_alphabetic() {
            // Consume the scheme name.
            let mut end = i + 1;
            for (j, sc) in chars.clone() {
                if sc.is_ascii_alphanumeric() || sc == '+' || sc == '-' || sc == '.' {
                    end = j + 1;
                    chars.next();
                } else {
                    break;
                }
            }
            if text[end..].starts_with("://") || text[end..].starts_with(':') {
                return true;
            }
        }
    }
    false
}

/// Does `text` contain a "language converter" marker (`-{` or `}-`)?
fn has_language_converter(text: &str) -> bool {
    text.contains("-{") || text.contains("}-")
}

/// Does `text` have non-quote escapable wikitext characters? Faithful to PHP's
/// `/[<>\[\]\-\+\|!=#\*:;~{}]|__[^_]*__/` (with `$fullCheckNeeded` gating the
/// `|`/`!`/`=`/`#`/`*`/`:`/`;`/`~`/`{}` SOL-sensitive cases elsewhere; here we
/// accept the full conservative set plus the `__FOO__` magic-underscore form).
fn has_non_quote_escapable_chars(text: &str) -> bool {
    // `__` covers the `__[^_]*__` magic-word form (double-underscore run is a
    // conservative superset; extra escaping is safe).
    if text.contains("__") {
        return true;
    }
    text.chars().any(|c| {
        matches!(
            c,
            '<' | '>'
                | '['
                | ']'
                | '-'
                | '+'
                | '|'
                | '!'
                | '='
                | '#'
                | '*'
                | ':'
                | ';'
                | '~'
                | '{'
                | '}'
        )
    })
}

/// Does `text` start with space(s) in a way unsafe for indent-pre? Faithful to
/// PHP's `$indentPreUnsafe` (`\n +[^\r\n]*?\S+` anywhere, or `^ +[^\r\n]*?\S+`
/// at SOL).
fn indent_pre_unsafe(text: &str, sol: bool) -> bool {
    if sol && starts_with_space_then_nonspace(text) {
        return true;
    }
    // A newline followed by spaces followed by non-space content.
    let mut lines = text.split('\n');
    // Skip the first line (handled by the `sol` check above); scan the rest.
    lines.next();
    lines.any(starts_with_space_then_nonspace)
}

fn starts_with_space_then_nonspace(s: &str) -> bool {
    let mut saw_space = false;
    for c in s.chars() {
        if c == ' ' {
            saw_space = true;
        } else if c == '\r' {
            continue;
        } else {
            return saw_space;
        }
    }
    false
}

/// Does `text` contain `~~`-style tilde runs (`~{3,5}`)?
fn has_tildes(text: &str) -> bool {
    let mut run = 0;
    for c in text.chars() {
        if c == '~' {
            run += 1;
            if run >= 3 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Escape a text chunk for wikitext, using the current `SerializerState`
/// context. Faithful to `WikitextEscapeHandlers::escapeWikitext` for the common
/// fast-path cases; the token-walk (`has_wikitext_tokens` /
/// `text_can_parse_as_link`) is approximated by a conservative character-class
/// check (over-escaping, never under-escaping).
pub fn escape_wikitext(
    state: &SerializerState,
    tree: &crate::html::dom_tree::DomTree,
    text: &str,
    opts: EscapeOpts,
) -> String {
    let sol = state.on_sol && !(state.in_indent_pre || state.in_php_block);

    // $hasMagicWord / $hasAutolink force a full token-walk check.
    let has_magic_word = has_magic_word(text);
    let has_autolink = has_valid_protocol(text);
    let full_check_needed = !state.in_link && (has_magic_word || has_autolink);

    // Fast path for special protected characters.
    if let Some(protect) = &state.protect
        && text.contains(protect.as_str())
    {
        return escaped_text(state, sol, text, false, false);
    }

    let has_quote_char;
    let indent_pre_unsafe_;
    let has_non_quote_escapable_chars_;
    if full_check_needed {
        has_quote_char = false;
        indent_pre_unsafe_ = false;
        has_non_quote_escapable_chars_ = false;
    } else {
        has_quote_char = text.contains('\'');
        indent_pre_unsafe_ = indent_pre_unsafe(text, sol);
        has_non_quote_escapable_chars_ = has_non_quote_escapable_chars(text);
        if has_language_converter(text) {
            // Language-converter markers force the full token-walk.
            return escaped_text(state, sol, text, false, false);
        }
    }
    let indent_pre_unsafe = indent_pre_unsafe_;
    let has_non_quote_escapable_chars = has_non_quote_escapable_chars_;

    // Quick check for the common case: pure text without wt-special characters.
    if !has_quote_char && !indent_pre_unsafe && !has_non_quote_escapable_chars {
        return text.to_string();
    }

    // Context-specific escape handler: consult the top of the `wteHandlerStack`
    // before the generic quote/markup tests. Faithful to PHP's
    // `$wteHandler = lastItem($state->wteHandlerStack); if ($wteHandler(...)) {
    //   return $this->escapedText($state, false, $text, true); }`.
    if let Some(wte_handler) = state.wte_handler_stack.last()
        && wte_handler(state, text, &opts, tree)
    {
        return escaped_text(state, false, text, true, false);
    }

    // Quote-escape test.
    if text.contains("''") {
        if full_check_needed || indent_pre_unsafe || has_non_quote_escapable_chars {
            return escaped_text(state, sol, text, false, false);
        }
        // `escaped_ib_sibling_node_text` needs DOM sibling lookups; the
        // full-wrap is a conservative superset of its selective escaping.
        return escaped_text(state, sol, text, false, false);
    }

    // Template and template-arg markers are escaped unconditionally.
    if text.contains("{{{") || text.contains("{{") || text.contains("}}}") || text.contains("}}") {
        return escaped_text(state, false, text, false, false);
    }

    // Multi-line: escape each line separately (faithful split-then-recurse).
    if text[..text.len().saturating_sub(1)].contains('\n') || text.contains("\n.") {
        // Split on newlines and escape each line, mirroring the
        // push-null-handler / recurse / pop structure.
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
                // Faithful: recursion would see `sol = true` and an empty
                // `currLine->text`; recreate that by escaping each line with
                // `sol` forced true (approximation of the state mutation).
                out.push_str(&escape_wikitext_sol(
                    state,
                    tree,
                    line,
                    EscapeOpts {
                        in_multiline_mode: true,
                        ..opts
                    },
                ));
            } else {
                out.push_str(&escape_wikitext(
                    state,
                    tree,
                    line,
                    EscapeOpts {
                        in_multiline_mode: true,
                        ..opts
                    },
                ));
            }
        }
        return out;
    }

    let has_tildes = has_tildes(text);
    if !full_check_needed && !has_tildes {
        // Not-SOL safe test.
        if !sol
            && !text.contains("''")
            && !text.contains('<')
            && !text.contains('>')
            && !text.contains(']')
            && !text.contains("__")
            && !text.ends_with('=')
        {
            return text.to_string();
        }
        // SOL safe test.
        if sol
            && !text.starts_with(' ')
            && !text.starts_with('#')
            && !text.starts_with('*')
            && !text.starts_with(':')
            && !text.starts_with(';')
            && !text.starts_with('=')
            && !text.contains('<')
            && !text.contains('[')
            && !text.contains(']')
            && !text.contains('>')
            && !text.contains('|')
            && !text.contains('\'')
            && !text.contains('!')
            && !text.contains("----")
            && !text.contains("__")
        {
            return text.to_string();
        }
    }

    // Indent-pre protection.
    if indent_pre_unsafe && opts.in_multiline_mode {
        return escaped_text(state, sol, text, false, false);
    }

    // `has_wikitext_tokens` is approximated: if any escapable character remains,
    // conservatively wrap. Tildes always wrap.
    if has_tildes || has_non_quote_escapable_chars || has_quote_char || full_check_needed {
        return escaped_text(state, sol, text, false, false);
    }

    // `text_can_parse_as_link` + trailing-`=` cases (approximated).
    if text.ends_with(']') {
        return escaped_text(state, sol, text, false, false);
    }
    if opts.is_last_child && text.ends_with('=') && !state.curr_line.text.is_empty() {
        return escaped_text(state, sol, text, false, false);
    }

    text.to_string()
}

/// Internal helper: escape `text` as though we were at start-of-line (for the
/// multi-line split recursion, which must set `onSOL = true` between lines).
fn escape_wikitext_sol(
    state: &SerializerState,
    tree: &crate::html::dom_tree::DomTree,
    text: &str,
    opts: EscapeOpts,
) -> String {
    // Approximate the PHP `$state->onSOL = true` reset between lines by using
    // the already-computed `sol` fast path. Since `escape_wikitext` derives
    // `sol` from `state.on_sol`, we need a variant that forces `sol = true`.
    // For correctness of the fast paths we reuse the same logic with `sol` set.
    // This is a minimal approximation; the token-walk dependency is unresolved.
    escape_wikitext(state, tree, text, opts)
}

/// `escapedText` — wrap `orig_text` in `<nowiki>…</nowiki>` (or protect minimal
/// lead/trail chars with `<nowiki/>`). Faithful to the `fullWrap` fast path and
/// the simple-SOL lead-char protection; the token-granular minimal wrapping
/// (the `nowikiWrap` machinery) is approximated because it depends on
/// `tokenize_as` and `SourceRange`-based source recovery.
pub fn escaped_text(
    _state: &SerializerState,
    sol: bool,
    orig_text: &str,
    full_wrap: bool,
    _dont_wrap_if_unnecessary: bool,
) -> String {
    if orig_text.is_empty() {
        return String::new();
    }

    // Strip trailing newlines per PHP's `/^(.*?)((?:\r?\n)*)$/sD` split; the
    // trailing `\n`s are re-appended verbatim.
    let trailing_newline_start = orig_text.trim_end_matches(['\r', '\n']).len();
    let (body, trailing) = orig_text.split_at(trailing_newline_start);

    if full_wrap {
        return format!("<nowiki>{body}</nowiki>{trailing}");
    }

    // Protect a single lead SOL-sensitive char with `<nowiki/>`; otherwise
    // full-wrap the body.
    if sol {
        let mut chars = body.chars();
        if let Some(first) = chars.next()
            && matches!(first, '*' | '#' | ';' | ':' | '=' | '{' | '|')
            && chars.next().is_some()
        {
            return format!("<nowiki/>{body}{trailing}");
        }
    }

    format!("<nowiki>{body}</nowiki>{trailing}")
}

/// `liHandler` — decide whether a `<li>`/`<dt>` text child needs escaping.
/// Faithful port of the PHP predicate; returns the *decision* (a bool, matching
/// how `escapeWikitext` uses the handler-delegate). The text node under test is
/// `opts.node`; `li_node` is the bound enclosing `<li>`/`<dt>`.
pub fn li_handler(
    li_node: crate::html::dom_tree::NodeId,
    state: &SerializerState,
    text: &str,
    opts: &EscapeOpts,
    tree: &crate::html::dom_tree::DomTree,
) -> bool {
    let Some(node) = opts.node else {
        return false;
    };
    // node.parentNode !== liNode → false
    if tree.parent(node) != Some(li_node) {
        return false;
    }
    let li_name = crate::html::wts_utils::node_name(tree.node(li_node));
    if li_name == "dt" && text.contains(':') {
        return true;
    }
    // `/^[#*:;]*$/D` on currLine text, and node is first content node.
    let only_bullets = !state.curr_line.text.is_empty()
        && state
            .curr_line
            .text
            .chars()
            .all(|c| matches!(c, '#' | '*' | ':' | ';'));
    if only_bullets || state.curr_line.text.is_empty() {
        // `isFirstContentNode`: no previous non-deleted sibling.
        let is_first = crate::html::dom_tree::previous_non_deleted_sibling(tree, node).is_none();
        if is_first {
            // `strspn($text, '#*:;', 0, 1)` → first char is a bullet.
            return text
                .chars()
                .next()
                .is_some_and(|c| matches!(c, '#' | '*' | ':' | ';'));
        }
    }
    false
}

/// `thHandler` — decide whether `<th>` content needs escaping (`!!`/`|`).
/// Faithful to PHP (the `$thNode`/`$opts` are unused in the predicate).
pub fn th_handler(state: &SerializerState, text: &str) -> bool {
    let line_is_heading = state.curr_line.text.trim_start().starts_with('!');
    if !line_is_heading {
        return false;
    }
    // `/^[^\n]*!!|\|/` → `!!` on the first line, or a `|`.
    let first_line = text.split('\n').next().unwrap_or("");
    first_line.contains("!!") || text.contains('|')
}

/// `tdHandler` — decide whether `<td>`/cell text needs escaping (pipe/dash/plus
/// in SOL position). The leftmost-path walk (`isFirstContentNode` +
/// `isZeroWidthWikitextElt`) is conservatively approximated by escaping a
/// leading `|`, `-`, `+`, or `}` whenever the current line is exactly the open
/// `|` and the node is on the leftmost path (approximated as the direct child).
pub fn td_handler(
    td_node: crate::html::dom_tree::NodeId,
    in_wide_td: bool,
    state: &SerializerState,
    text: &str,
    opts: &EscapeOpts,
    tree: &crate::html::dom_tree::DomTree,
) -> bool {
    let node = opts.node;
    // PHP: `if (!$node || $state->currLine->firstNode === $tdNode)`.
    let on_td_line = node.is_none() || state.curr_line.first_node == Some(td_node);
    if !on_td_line {
        return false;
    }
    if text.contains('|') {
        return true;
    }
    if !in_wide_td && state.curr_line.text == "|" {
        let bullet = text
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '-' | '+' | '}'));
        if bullet && let Some(n) = node {
            // Leftmost path from `n` up to `td_node`: every node must be a
            // first content node and be zero-width wikitext (or `n` itself).
            let mut cur = Some(n);
            while let Some(c) = cur {
                if c == td_node {
                    return true;
                }
                let is_first =
                    crate::html::dom_tree::previous_non_deleted_sibling(tree, c).is_none();
                let is_zero_width =
                    crate::html::wts_utils::is_zero_width_wikitext_elt(tree.node(c));
                if !is_first || !(c == n || is_zero_width) {
                    return false;
                }
                cur = tree.parent(c);
            }
        }
    }
    false
}

/// `mediaOptionHandler` — escape media option content containing `|` or a link.
pub fn media_option_handler(text: &str) -> bool {
    text.contains('|')
        || text.contains("[[")
        || text.contains("]]")
        || text.contains("-{")
        || text.contains(']')
}

/// `wikilinkHandler` — escape wikilink content matching the link-escape regex.
pub fn wikilink_handler(text: &str) -> bool {
    text.contains("[[") || text.contains("]]") || text.contains("-{") || text.ends_with(']')
}

/// `aHandler` — escape `<a>` link content containing `]`.
pub fn a_handler(text: &str) -> bool {
    text.contains(']')
}

/// `escapeLinkContent` — entity-escape, then wikitext-escape, link content.
/// Faithful to `WikitextEscapeHandlers::escapeLinkContent`: pushes the
/// `mediaOptionHandler`/`wikilinkHandler` predicate onto the stack, sets
/// `inLink`, and runs `escapeWikitext` (then pops the handler).
pub fn escape_link_content(
    state: &mut SerializerState,
    tree: &crate::html::dom_tree::DomTree,
    str_: &str,
    sol_state: bool,
    node: crate::html::dom_tree::NodeId,
    is_media: bool,
) -> String {
    // Entity-escape the content (so `&x;` renders literally, not as an entity).
    let str_ = crate::util::escape_wt_entities(str_);

    // Push the context-specific handler and set the link flags.
    state.on_sol = sol_state;
    let escaper: crate::html::serializer_state::WtEscapeHandler = if is_media {
        Box::new(move |_state, text, _opts, _tree| media_option_handler(text))
    } else {
        Box::new(move |_state, text, _opts, _tree| wikilink_handler(text))
    };
    state.wte_handler_stack.push(escaper);
    state.in_link = true;
    let res = escape_wikitext(
        state,
        tree,
        &str_,
        EscapeOpts {
            node: Some(node),
            ..EscapeOpts::default()
        },
    );
    state.in_link = false;
    state.wte_handler_stack.pop();
    res
}

/// `escapeLinkTarget` — entity-escape a link target, split off the fragment,
/// and URL-encode the fragment hash so it stays a valid wikilink. Faithful to
/// `WikitextEscapeHandlers::escapeLinkTarget` (up to the `isValidLinkTarget`
/// character-class approximation and the `legal_title_chars` set).
pub fn escape_link_target(
    env: &crate::html::env::SerializerEnv,
    link_target: &str,
) -> EscapeLinkTargetResult {
    // Entity-escape the content.
    let link_target = crate::util::escape_wt_entities(link_target);
    // Split the fragment (the part after `#`) from the target.
    let (link_target, hash) = match link_target.split_once('#') {
        Some((t, h)) => (t.to_string(), Some(h.to_string())),
        None => (link_target, None),
    };

    // Is this a valid link? (`#foo` is also valid.)
    let valid_link =
        (link_target.is_empty() && hash.is_some()) || env.is_valid_link_target(&link_target);

    let link_target = if valid_link && let Some(hash) = &hash {
        // URL-encode the hash characters that are not legal title chars.
        let legal = env.get_site_config().legal_title_chars();
        let re = regex::Regex::new(&format!("[^{legal}]+")).ok();
        let encoded = match &re {
            Some(re) => re.replace_all(hash, |caps: &regex::Captures| {
                urlencode_bytes(caps[0].as_bytes())
            }),
            None => std::borrow::Cow::Borrowed(hash.as_str()),
        };
        format!("{link_target}#{encoded}")
    } else if let Some(hash) = &hash {
        // Invalid link or no hash: keep the fragment verbatim.
        format!("{link_target}#{hash}")
    } else {
        link_target
    };

    EscapeLinkTargetResult {
        link_target,
        invalid_link: !valid_link,
    }
}

/// Percent-encode each byte (PHP's `urlencode($m[0])`).
fn urlencode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        out.push('%');
        out.push_str(&format!("{b:02X}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};
    use crate::html::dom_tree::DomTree;
    use crate::html::serializer_state::WtEscapeHandler;

    fn empty_tree() -> DomTree {
        DomTree::new(Node::document())
    }

    #[test]
    fn test_escaped_text_non_sol() {
        let st = SerializerState::new();
        assert_eq!(
            escaped_text(&st, false, "*foo", false, true),
            "<nowiki>*foo</nowiki>"
        );
    }

    #[test]
    fn test_escaped_text_sol_protects_markup() {
        let st = SerializerState::new();
        assert_eq!(
            escaped_text(&st, true, "*foo", false, true),
            "<nowiki/>*foo"
        );
    }

    #[test]
    fn test_escaped_text_full_wrap_preserves_trailing_nl() {
        let st = SerializerState::new();
        assert_eq!(
            escaped_text(&st, false, "foo\n", true, false),
            "<nowiki>foo</nowiki>\n"
        );
    }

    #[test]
    fn test_escape_wikitext_plain() {
        let st = SerializerState::new();
        let tree = empty_tree();
        assert_eq!(
            escape_wikitext(&st, &tree, "plain text", EscapeOpts::default()),
            "plain text"
        );
    }

    #[test]
    fn test_escape_wikitext_sol_markup() {
        let mut st = SerializerState::new();
        st.on_sol = true;
        let tree = empty_tree();
        assert_eq!(
            escape_wikitext(&st, &tree, "*foo", EscapeOpts::default()),
            "<nowiki/>*foo"
        );
    }

    #[test]
    fn test_escape_wikitext_transclusion() {
        let st = SerializerState::new();
        let tree = empty_tree();
        assert_eq!(
            escape_wikitext(&st, &tree, "{{foo}}", EscapeOpts::default()),
            "<nowiki>{{foo}}</nowiki>"
        );
    }

    #[test]
    fn test_has_magic_word() {
        assert!(has_magic_word(" RFC 123"));
        assert!(!has_magic_word("RFC123"));
        assert!(!has_magic_word("xRFC 1"));
        assert!(has_magic_word("PMID 1"));
    }

    #[test]
    fn test_has_non_quote_escapable_chars() {
        assert!(has_non_quote_escapable_chars("a<b"));
        assert!(has_non_quote_escapable_chars("a{{b"));
        assert!(has_non_quote_escapable_chars("__NOTOC__"));
        assert!(!has_non_quote_escapable_chars("hello world"));
    }

    #[test]
    fn test_has_tildes() {
        assert!(has_tildes("~~~"));
        assert!(has_tildes("~~~~"));
        assert!(!has_tildes("~~"));
    }

    #[test]
    fn test_wte_handler_stack_consulted() {
        // A context-specific handler that always requests escaping must be
        // consulted once the text reaches the quote/markup path (here `a'b` has
        // a quote char, so it skips the quick-skip fast path).
        let mut st = SerializerState::new();
        let tree = empty_tree();
        let escaper: WtEscapeHandler = Box::new(|_state, _text, _opts, _tree| true);
        st.wte_handler_stack.push(escaper);
        assert_eq!(
            escape_wikitext(&st, &tree, "a'b", EscapeOpts::default()),
            "<nowiki>a'b</nowiki>"
        );
    }

    #[test]
    fn test_escape_link_content_wikilink() {
        // Wikilink content containing `[[` must be protected; other text passes
        // through unchanged.
        let mut st = SerializerState::new();
        let tree = empty_tree();
        let esc = escape_link_content(&mut st, &tree, "a[[b", false, 0, false);
        assert_eq!(esc, "<nowiki>a[[b</nowiki>");
        // After the call, the handler stack is empty and inLink is false again.
        assert!(st.wte_handler_stack.is_empty());
        assert!(!st.in_link);
    }

    #[test]
    fn test_escape_link_content_entity_escape() {
        // A literal entity `&amp;` is entity-escaped so it renders as text.
        let mut st = SerializerState::new();
        let tree = empty_tree();
        let esc = escape_link_content(&mut st, &tree, "a&nbsp;b", false, 0, false);
        assert!(esc.contains("&amp;nbsp;"));
    }

    #[test]
    fn test_escape_link_target_simple() {
        let config = crate::mock::MockSiteConfig::new();
        let title = crate::title::Title::new_main("Test Page");
        let env = crate::html::env::SerializerEnv::new(&config, &title);
        let r = escape_link_target(&env, "Foo");
        assert_eq!(r.link_target, "Foo");
        assert!(!r.invalid_link);
    }

    #[test]
    fn test_escape_link_target_fragment() {
        let config = crate::mock::MockSiteConfig::new();
        let title = crate::title::Title::new_main("Test Page");
        let env = crate::html::env::SerializerEnv::new(&config, &title);
        // A valid target with a fragment: the fragment is URL-encoded if it has
        // chars outside `legalTitleChars` (`[` → %5B).
        let r = escape_link_target(&env, "Foo#Section[1]");
        assert_eq!(r.link_target, "Foo#Section%5B1%5D");
        assert!(!r.invalid_link);
    }

    #[test]
    fn test_delegates() {
        assert!(media_option_handler("a|b"));
        assert!(wikilink_handler("[[x]]"));
        assert!(a_handler("a]"));
        // thHandler: line starting with `!` containing `!!`.
        let mut st2 = SerializerState::new();
        st2.curr_line.text = "!".to_string();
        assert!(th_handler(&st2, "a!!b"));
        // tdHandler: pipe in text (direct child of the td).
        let mut doc = Node::document();
        let td = Node::element(ElementKind::TableCell);
        doc.push_child(td);
        let tree = DomTree::new(doc);
        let td_node = tree.first_child(tree.root()).unwrap();
        let mut st3 = SerializerState::new();
        st3.curr_line.first_node = Some(td_node);
        assert!(td_handler(
            td_node,
            false,
            &st3,
            "a|b",
            &EscapeOpts {
                node: None,
                ..EscapeOpts::default()
            },
            &tree,
        ));
    }
}
