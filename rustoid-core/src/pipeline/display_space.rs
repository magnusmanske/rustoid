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

    // Walk by index because a split inserts nodes: PHP's traverser visits the
    // *suffix* text node next, so a second match in the same text is armored by a
    // later traversal. Resuming at the suffix (and re-descending into the new
    // span, whose only child holds a non-breaking space) reproduces that.
    let mut i = 0;
    while i < node.children.len() {
        if !matches!(
            node.children[i].kind,
            NodeKind::Text(_) | NodeKind::Comment(_)
        ) {
            visit(&mut node.children[i]);
        }
        if let NodeKind::Text(_) = node.children[i].kind {
            let text = match &node.children[i].kind {
                NodeKind::Text(t) => t.clone(),
                _ => unreachable!(),
            };
            // PHP's `textHandler` runs the left handler and then the right
            // handler on the *same* node. The left one rewrites the node to the
            // prefix before its space, so the right handler only sees that
            // prefix; both splits land in one child run, in insertion order
            // (`prefix2`, right span, `suffix2`, left span, `suffix1`).
            let left = find_left_match(&text);
            let left_prefix = match left {
                Some(o) => &text[..o],
                None => text.as_str(),
            };
            let right = find_right_match(left_prefix);
            if left.is_some() || right.is_some() {
                let dsr_start = dsr_start_of_child(node, i);
                let mut new_nodes: Vec<Node> = Vec::new();
                if let Some(o) = right {
                    push_text(&mut new_nodes, &left_prefix[..o]);
                    new_nodes.push(display_space_span(dsr_start, o));
                    push_text(&mut new_nodes, &left_prefix[o + 1..]);
                } else if left.is_some() {
                    push_text(&mut new_nodes, left_prefix);
                }
                if let Some(o) = left {
                    new_nodes.push(display_space_span(dsr_start, o));
                    push_text(&mut new_nodes, &text[o + 1..]);
                }
                node.children.splice(i..=i, new_nodes);
                // The node left at `i` already had both handlers run; resume
                // at the node after it.
                i += 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Append `text` unless it is empty (PHP never creates empty text nodes).
fn push_text(nodes: &mut Vec<Node>, text: &str) {
    if !text.is_empty() {
        nodes.push(Node::text(text.to_string()));
    }
}

/// The `mw:DisplaySpace` span that replaces the space at `offset`.
///
/// Its `data-parsoid` is the one-character source range of the space
/// (`new DomSourceRange( $start, $start + 1, 0, 0 )`), or all-null when the
/// text node's own start could not be recovered. Either way the metadata is
/// present, which is what makes the node take a generated id.
fn display_space_span(dsr_start: Option<usize>, offset: usize) -> Node {
    let mut span = Node::element(ElementKind::Other("span".to_string()));
    span.push_child(Node::text("\u{00A0}"));
    span.set_attr("typeof", "mw:DisplaySpace");
    let dsr = match dsr_start {
        Some(s) => {
            let start = s + offset;
            DomSourceRange {
                start: Some(start),
                end: Some(start + 1),
                open_width: Some(0),
                close_width: Some(0),
                leading_ws: 0,
                trailing_ws: 0,
            }
        }
        None => DomSourceRange {
            start: None,
            end: None,
            open_width: None,
            close_width: None,
            leading_ws: 0,
            trailing_ws: 0,
        },
    };
    let dp = DataParsoid {
        dsr: Some(dsr),
        ..Default::default()
    };
    span.data_parsoid = dp.to_data_parsoid_json();
    span.dp = Some(dp);
    span
}

/// The byte offset where `parent.children[index]` starts in the source, mirroring
/// `DisplaySpace::getTextNodeDSRStart`. `None` when the parent's DSR is unusable,
/// or a preceding sibling's is.
fn dsr_start_of_child(parent: &Node, index: usize) -> Option<usize> {
    let dsr = node_dsr(parent)?;
    if !valid_dsr(&dsr, true) {
        return None;
    }
    let mut start = dsr.inner_start();
    for child in &parent.children[..index] {
        match &child.kind {
            NodeKind::Comment(c) => {
                start += crate::html::wts_utils::decoded_comment_length(c, false);
            }
            NodeKind::Text(t) => start += t.len(),
            _ => {
                // An element stands for as many characters as its DSR spans; the
                // scan restarts from its end, as PHP does.
                let cdsr = node_dsr(child)?;
                if !valid_dsr(&cdsr, false) {
                    return None;
                }
                start = cdsr.end?;
            }
        }
    }
    Some(start)
}

/// A node's source range: the structured `dp` when the tree still carries it,
/// else the serialized `data-parsoid`.
fn node_dsr(node: &Node) -> Option<DomSourceRange> {
    if let Some(dp) = &node.dp
        && let Some(dsr) = &dp.dsr
    {
        return Some(dsr.clone());
    }
    node.data_parsoid
        .as_deref()
        .and_then(DataParsoid::from_data_parsoid_json)
        .and_then(|dp| dp.dsr)
}

/// `sanitizer::isValidDSR` for the tokenizer-side range: `start`/`end` must be
/// set, and in `all` mode so must the container widths.
fn valid_dsr(dsr: &DomSourceRange, all: bool) -> bool {
    dsr.start.is_some()
        && dsr.end.is_some()
        && (!all || (dsr.open_width.is_some() && dsr.close_width.is_some()))
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

    /// The span's `data-parsoid` is what makes it draw a node id — the live
    /// output is `<span typeof="mw:DisplaySpace" id="mwX">`. Without a parent
    /// `dsr` the range is all-null, but still present, which is Parsoid's
    /// `getTextNodeDSRStart` returning `null`.
    #[test]
    fn the_span_carries_a_source_range() {
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("a b : c"));
        run(&mut p);
        let span = &p.children[1];
        assert!(crate::html::dom_utils::has_type_of(span, "mw:DisplaySpace"));
        let json = span.data_parsoid.as_deref().expect("data-parsoid");
        assert!(json.contains("\"dsr\""), "got {json}");
    }

    /// A right-hand match survives a left-hand one in the same text node.
    ///
    /// PHP runs `leftHandler` and then `rightHandler` on the *same* text node;
    /// the left split rewrites the node to the text before its space, and the
    /// right handler then arms the opening guillemet in that prefix. Taking
    /// only the first match of either kind (and re-visiting the suffix) lost
    /// the guillemet in `Les « scandales » de 1878`.
    #[test]
    fn armors_both_guillemets_of_one_phrase() {
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("Les « scandales » de 1878"));
        run(&mut p);
        let html: Vec<String> = p
            .children
            .iter()
            .map(|c| match &c.kind {
                NodeKind::Text(t) => t.clone(),
                _ if crate::html::dom_utils::has_type_of(c, "mw:DisplaySpace") => {
                    "<DS>".to_string()
                }
                _ => "?".to_string(),
            })
            .collect();
        assert_eq!(
            html,
            vec!["Les «", "<DS>", "scandales", "<DS>", "» de 1878"]
        );
    }

    /// A second French space in one text node is armored too: the walk resumes at
    /// the suffix, which is what PHP's traverser does on its next step.
    #[test]
    fn armors_every_match_in_a_text_node() {
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("a : b ; c"));
        run(&mut p);
        let spans = p
            .children
            .iter()
            .filter(|c| crate::html::dom_utils::has_type_of(c, "mw:DisplaySpace"))
            .count();
        assert_eq!(spans, 2);
    }
}
