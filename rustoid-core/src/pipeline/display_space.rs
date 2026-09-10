//! `DisplaySpace` — faithful port of `src/Wt2Html/DOM/Handlers/DisplaySpace.php`.
//!
//! Applies "French space armoring": a regular space before a punctuation mark
//! (`?`, `:`, `;`, `!`, `%`, `»`, `›`) — or after an opening guillemet (`«`,
//! `‹`) — is replaced by a non-breaking `mw:DisplaySpace` span so the browser
//! keeps it on the same line.
//!
//! This runs as a global DOM pass (`displayspace` in
//! `ParserPipelineFactory::FULL_PARSE_GLOBAL_DOM_TRANSFORMS`), after extension
//! post-processing and before `cleanup`.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::wikitext::tokens_v2::{DataParsoid, DomSourceRange};

/// `Sanitizer::FIXTAGS[0]` — the "space before punctuation" lookahead set.
const LEFT_LOOKAHEAD: &[char] = &['?', ':', ';', '!', '%', '»', '›'];
/// `Sanitizer::FIXTAGS[1]` — the "space after opening guillemet" set.
const LEFT_GUILLEMETS: &[char] = &['«', '‹'];

/// Run the `displayspace` pass over the tree. Mirrors the `DisplaySpace` text
/// handler registered with `nodeName => null` (every node).
pub fn run(root: &mut Node) {
    visit(root);
}

/// Post-order over each node's child list, applying the handler to text
/// children and splicing in any `mw:DisplaySpace` splits.
fn visit(node: &mut Node) {
    // `textHandler` returns the next sibling for `pre`, `svg` and raw-text
    // elements, which skips their whole subtree.
    if skips_subtree(node) {
        return;
    }

    let mut out: Vec<Node> = Vec::with_capacity(node.children.len());
    for mut child in std::mem::take(&mut node.children) {
        if matches!(child.kind, NodeKind::Text(_)) {
            out.extend(split_text_child(&mut child));
        } else {
            visit(&mut child);
            out.push(child);
        }
    }
    node.children = out;
}

/// Whether `textHandler` bails out for this node (`pre`, `svg`, or a raw-text
/// element), skipping its subtree entirely.
fn skips_subtree(node: &Node) -> bool {
    if !matches!(node.kind, NodeKind::Element(_)) {
        return false;
    }
    let name = crate::html::wts_utils::node_name(node);
    name == "pre" || name == "svg" || crate::html5::html_data::is_raw_text(&name)
}

/// Apply both French-space handlers to a text node, returning the replacement
/// node sequence (`[prefix, span, suffix]` when a space was armored, else the
/// node unchanged).
///
/// PHP runs `leftHandler` then `rightHandler` on the *same* node; each mutates
/// the node in place and appends the suffix as a new sibling. Chaining them on
/// one node is equivalent only for the first match, which is what PHP's
/// non-global `preg_match` does too — so one replacement per handler invocation
/// is correct.
fn split_text_child(child: &mut Node) -> Vec<Node> {
    let NodeKind::Text(text) = &child.kind else {
        return vec![std::mem::replace(child, Node::text(""))];
    };
    let text = text.clone();

    // `leftHandler` then `rightHandler`, both on the current node value; PHP
    // replaces the node's value with the prefix, so a second match in the
    // *suffix* is found lazily on a later traversal in PHP. We mirror that by
    // only performing the first hit of each pattern on this node.
    if let Some(offset) = find_left_match(&text) {
        return build_split(&text, offset, &text);
    }
    if let Some(offset) = find_right_match(&text) {
        return build_split(&text, offset, &text);
    }
    vec![std::mem::replace(child, Node::text(""))]
}

/// Split `text` at `offset` (dropping the single space byte there) into
/// `[prefix, mw:DisplaySpace span, suffix]`.
fn build_split(text: &str, offset: usize, _orig: &str) -> Vec<Node> {
    let prefix = text[..offset].to_string();
    // `substr( $str, $offset + 1 )` — the space is one byte.
    let suffix = text[offset + 1..].to_string();

    let mut span = Node::element(ElementKind::Other("span".to_string()));
    span.push_child(Node::text("\u{00A0}"));
    span.set_attr("typeof", "mw:DisplaySpace");
    span.dp = Some(DataParsoid {
        dsr: Some(DomSourceRange {
            start: None,
            end: None,
            open_width: None,
            close_width: None,
            leading_ws: 0,
            trailing_ws: 0,
        }),
        ..Default::default()
    });

    let mut out = Vec::with_capacity(3);
    if !prefix.is_empty() {
        out.push(Node::text(prefix));
    }
    out.push(span);
    if !suffix.is_empty() {
        out.push(Node::text(suffix));
    }
    out
}

/// Byte offset of the first `' '` matching `'/ (?=[?:;!%»›](?!\w))/u'`.
fn find_left_match(text: &str) -> Option<usize> {
    for (i, _) in text.char_indices().filter(|&(_, c)| c == ' ') {
        let rest = &text[i + 1..];
        let Some(next) = rest.chars().next() else {
            continue;
        };
        if !LEFT_LOOKAHEAD.contains(&next) {
            continue;
        }
        // `(?!\w)`: the character after the punctuation must not be a word char.
        let after = &rest[next.len_utf8()..];
        if after.chars().next().is_some_and(is_word_char) {
            continue;
        }
        return Some(i);
    }
    None
}

/// Byte offset of the space matching `'/(?<!\w)([«‹]) /u'`.
///
/// PHP computes `$matches[1][1] + strlen( $matches[1][0] )` — the offset just
/// past the captured guillemet, which is where the space sits.
fn find_right_match(text: &str) -> Option<usize> {
    let mut prev_char: Option<char> = None;
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if LEFT_GUILLEMETS.contains(&c) {
            // `(?<!\w)`: the preceding character must not be a word character.
            let prev_is_word = prev_char.is_some_and(is_word_char);
            if !prev_is_word
                && let Some(&(_, next)) = iter.peek()
                && next == ' '
            {
                return Some(i + c.len_utf8());
            }
        }
        prev_char = Some(c);
    }
    None
}

/// Regex `\w` under the `u` flag: a word character (letter, digit, underscore).
fn is_word_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_match_basic() {
        // `two : tag` → the space before `:` (not followed by a word char).
        assert_eq!(find_left_match("two : tag"), Some(3));
    }

    #[test]
    fn left_match_requires_no_word_after() {
        assert_eq!(find_left_match("a ?b"), None);
        assert_eq!(find_left_match("a ?"), Some(1));
    }

    #[test]
    fn left_match_guillemet_right() {
        assert_eq!(find_left_match("a »"), Some(1));
    }

    #[test]
    fn right_match_basic() {
        assert_eq!(find_right_match("« x"), Some("«".len()));
    }

    #[test]
    fn right_match_requires_no_word_before() {
        assert_eq!(find_right_match("a« x"), None);
        assert_eq!(find_right_match(" « x"), Some(1 + "«".len()));
    }

    #[test]
    fn right_match_needs_space() {
        assert_eq!(find_right_match("«x"), None);
    }

    #[test]
    fn word_char_unicode() {
        assert!(is_word_char('é'));
        assert!(!is_word_char(' '));
        assert!(!is_word_char(':'));
    }

    #[test]
    fn armors_colon_in_dt() {
        let mut dt = Node::element(ElementKind::DefinitionTerm);
        dt.push_child(Node::text("one "));
        dt.push_child(Node::text("two : tag"));
        run(&mut dt);
        let names: Vec<String> = dt
            .children
            .iter()
            .map(crate::html::wts_utils::node_name)
            .collect();
        // "one " (text) + "two " (text) + span + " tag" (text)
        assert_eq!(names, vec!["", "", "span", ""]);
        assert!(crate::html::dom_utils::has_type_of(
            &dt.children[2],
            "mw:DisplaySpace"
        ));
    }
}
