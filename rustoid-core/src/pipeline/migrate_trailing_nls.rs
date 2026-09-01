//! `MigrateTrailingNLs` — a faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Processors/MigrateTrailingNLs.php`.
//!
//! When an element is closed implicitly at a block boundary (or by EOF), the
//! newline that triggered the close can "leak" into the element. This pass
//! hoists trailing newline/comment runs out of elements that may end a line in
//! wikitext (or that were auto-closed), so the newline lands *after* the
//! element and paragraph wrapping (which runs later) sees the correct
//! boundaries.
//!
//! The pass runs as part of the DOM transforms *after* p-wrapping is not yet
//! applicable: it is wired into [`super::tree_builder_html::post_pwrap_transforms`]
//! in PHP's `NESTED_PIPELINE_DOM_TRANSFORMS` order (`pwrap … migrate-nls …`).
//!
//! The algorithm is deliberately bottom-up: children are processed (in reverse)
//! before their parent, so a child's trailing newlines first migrate up into the
//! parent, then the parent (and the accumulated newlines) migrate up again.
//!
//! It relies on `autoInsertedEnd` (correctly marked in `finalize`, including for
//! AFE-reconstructed clones). Wire-up also required fixing the test harness's
//! legacy normalization path (`normalize_html` with `parsoid_only = false`) to
//! faithfully reproduce PHP's `TestUtils::normalizeIEWVisitor` `addAfter`
//! behavior: a hoisted newline that becomes the next sibling of a block element
//! must have its leading whitespace forced back to a newline, not a space.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::wikitext::tokens_v2::DataParsoid;

/// The HTML tag names that "end a line" in wikitext (mirrors PHP's
/// `MigrateTrailingNLs::nodeEndsLineInWT` set).
const NODES_TO_MIGRATE_FROM: &[&str] = &[
    "pre", "th", "td", "tr", "li", "dd", "ol", "ul", "dl", "caption", "p",
];

/// The lowercase HTML tag name of a node.
fn node_name(node: &Node) -> String {
    crate::html::wts_utils::node_name(node)
}

/// Whether a node's `data-parsoid` carries the literal-HTML marker
/// (`stx === "html"`). Mirrors `WTUtils::hasLiteralHTMLMarker`.
fn has_literal_html_marker(dp: &DataParsoid) -> bool {
    dp.stx.as_deref() == Some("html")
}

/// Whether a node is a `<span typeof="mw:Nowiki">` (the lean nowiki wrapper).
/// Mirrors the PHP reality that `<nowiki>` content is a DOM fragment unpacked
/// by `UnpackDOMFragments` *after* `MigrateTrailingNLs` runs, so its trailing
/// newline is never hoisted out.
fn is_nowiki_span(node: &Node) -> bool {
    matches!(&node.kind, NodeKind::Element(ElementKind::Span))
        && node
            .get_attr("typeof")
            .is_some_and(|ty| ty.split_whitespace().any(|t| t == "mw:Nowiki"))
}

/// Whether a node ends a line in wikitext: its tag name is in the migration set
/// and it is not a literal-HTML element. Mirrors
/// `MigrateTrailingNLs::nodeEndsLineInWT`.
fn node_ends_line_in_wt(node: &Node, dp: &DataParsoid) -> bool {
    NODES_TO_MIGRATE_FROM.contains(&node_name(node).as_str()) && !has_literal_html_marker(dp)
}

/// Whether the element immediately before index `i` (exclusive) in `children` is
/// an element with `dp.fostered == true`. Mirrors the `previousSibling instanceof
/// Element` + `getDataParsoid(...)->fostered` test in `getTableParent`'s
/// predecessor check.
fn prev_is_fostered_element(children: &[Node], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    matches!(children[i - 1].kind, NodeKind::Element(_))
        && children[i - 1].dp.as_ref().is_some_and(|dp| dp.fostered)
}

/// Whether a node is a "zero-width wikitext element": it has a valid TSR with
/// equal start/end, and (recursively) all its children are zero-width elements.
/// Mirrors `MigrateTrailingNLs::hasZeroWidthWT`.
fn has_zero_width_wt(node: &Node) -> bool {
    let Some(dp) = node.dp.as_ref() else {
        return false;
    };
    let Some(tsr) = dp.tsr.as_ref() else {
        return false;
    };
    // A `null` (unknown) start is not zero-width (mirrors PHP's
    // `$tsr->start === null` early-return).
    let Some(start) = tsr.start else {
        return false;
    };
    if start != tsr.end {
        return false;
    }
    node.children
        .iter()
        .all(|c| matches!(c.kind, NodeKind::Element(_)) && has_zero_width_wt(c))
}

/// The wikitext byte length needed to encode an HTML DOM comment, including the
/// `<!--` / `-->` delimiters (`7`), or `<!--` alone (`4`) for an unclosed
/// comment. Mirrors `WTUtils::decodedCommentLength`.
fn decoded_comment_length(value: &str, unclosed: bool) -> usize {
    let syntax_len = if unclosed { 4 } else { 7 };
    crate::html::wts_utils::decode_comment(value).len() + syntax_len
}

/// Whether the comment immediately preceding (in document order) is an unclosed
/// comment, indicated by a `mw:Placeholder/UnclosedComment` meta as its previous
/// sibling. Mirrors `decodedCommentLength`'s `previousSibling` check.
fn comment_is_unclosed(children: &[Node], preceding: usize) -> bool {
    if preceding == 0 {
        return false;
    }
    let prev = &children[preceding - 1];
    matches!(&prev.kind, NodeKind::Element(ElementKind::Other(name)) if name == "meta")
        && prev.get_attr("typeof").is_some_and(|t| {
            t.split_whitespace()
                .any(|x| x == "mw:Placeholder/UnclosedComment")
        })
}

/// Whether `s` consists solely of whitespace (` `, `\t`, `\r`, `\n`) and contains
/// at least one newline. Mirrors the `/^[ \t\r\n]*\n[ \t\r\n]*$/D` test.
fn is_ws_with_nl(s: &str) -> bool {
    let mut has_nl = false;
    for c in s.chars() {
        match c {
            '\n' => has_nl = true,
            ' ' | '\t' | '\r' => {}
            _ => return false,
        }
    }
    has_nl
}

/// The number of trailing `\n` characters in `s`.
fn trailing_nl_count(s: &str) -> usize {
    s.chars().rev().take_while(|c| *c == '\n').count()
}

/// Whether a node can have trailing newlines migrated out of it. Mirrors
/// `MigrateTrailingNLs::canMigrateNLOutOfNode`.
///
/// `ancestor_frames` carries the metadata chain from the root (exclusive) down
/// to this node's parent (inclusive), with each frame recording the node's name,
/// whether it can migrate, and whether its previous sibling was a fostered
/// element. `is_last_child` mirrors `$node->nextSibling === null`.
fn can_migrate_nl_out_of_node(
    node: &Node,
    is_last_child: bool,
    prev_sibling_fostered: bool,
    ancestor_frames: &[Frame],
) -> bool {
    let name = node_name(node);
    // Mirrors `nodeName === 'table' || atTheTop($node)`. Since every element we
    // process here has a parent (the document root is never passed), `atTheTop`
    // is always false; the root is handled by `run`.
    if name == "table" {
        return false;
    }

    // `mw:Nowiki` content is a DOM fragment that PHP unpacks *after* this pass
    // (see `ExtensionHandler::onDocumentFragment` → `tunnelDOMThroughTokens`),
    // so its trailing newline is never subject to migration there. Our nowiki is
    // emitted as inline tokens, so we reproduce that opacity here: skip it.
    if is_nowiki_span(node) {
        return false;
    }

    // Don't allow migration out of a node inside a table whose preceding sibling
    // (before the `<table>`) is fostered content (mirrors `getTableParent` +
    // `previousSibling` fostered check).
    if table_has_fostered_predecessor(&name, prev_sibling_fostered, ancestor_frames) {
        return false;
    }

    let dp = node.dp.clone().unwrap_or_default();
    if dp.fostered {
        return false;
    }

    if node_ends_line_in_wt(node, &dp) || dp.auto_inserted_end {
        return true;
    }

    // Otherwise, only if this is the rightmost child and its parent ends a line
    // (recursively). `tmp.end_tsr` present forbids migration (mirrors the
    // explicit end tag case).
    is_last_child
        && dp.tmp.end_tsr.is_none()
        && !ancestor_frames.is_empty()
        && ancestor_frames.last().is_some_and(|f| f.can_migrate)
}

/// Determine whether `node` is a table or is nested inside a table whose
/// immediately-preceding sibling is fostered content. Mirrors
/// `getTableParent` + the `previousSibling instanceof Element &&
/// fostered` guard.
fn table_has_fostered_predecessor(
    name: &str,
    prev_sibling_fostered: bool,
    ancestor_frames: &[Frame],
) -> bool {
    // `getTableParent`: walk up through td/th/tr/tbody/thead/tfoot/caption to a
    // possible `<table>` ancestor. We reconstruct the relevant ancestor names
    // from `ancestor_frames` (root..parent).
    let mut cur_name = name.to_string();
    // frame index pointing at the parent (last frame), walking upward.
    let mut level = ancestor_frames.len();

    // Start: if the node itself is td/th, its table-parent lookup begins at its
    // parent (skip one level).
    if matches!(cur_name.as_str(), "td" | "th") {
        // parent is ancestor_frames[level - 1]
        if level == 0 {
            return false;
        }
        level -= 1;
        cur_name = ancestor_frames[level].name.clone();
    }
    if cur_name == "tr" {
        if level == 0 {
            return false;
        }
        level -= 1;
        cur_name = ancestor_frames[level].name.clone();
    }
    if matches!(cur_name.as_str(), "tbody" | "thead" | "tfoot" | "caption") {
        if level == 0 {
            return false;
        }
        level -= 1;
        cur_name = ancestor_frames[level].name.clone();
    }

    if cur_name != "table" {
        return false;
    }

    // The table is either `node` itself (level == ancestor_frames.len(), i.e. no
    // upward walk happened past the node) or an ancestor at `level`.
    if level == ancestor_frames.len() {
        // `node` itself is the table: its predecessor is the direct previous
        // sibling captured in `prev_sibling_fostered`.
        prev_sibling_fostered
    } else {
        ancestor_frames[level].prev_sibling_fostered
    }
}

/// A snapshot of a node's migration-relevant metadata, captured while descending
/// so the parent context is available without parent pointers.
#[derive(Clone)]
struct Frame {
    /// The node's HTML tag name (or `""` for non-elements).
    name: String,
    /// `canMigrateNLOutOfNode` for this node.
    can_migrate: bool,
    /// Whether this node's previous sibling is a fostered element.
    prev_sibling_fostered: bool,
}

impl Frame {
    fn none() -> Self {
        Frame {
            name: String::new(),
            can_migrate: false,
            prev_sibling_fostered: false,
        }
    }
}

/// Migrate trailing newlines out of `elt` and return the nodes to be inserted
/// immediately after `elt` in its parent. Mirrors step 2 of
/// `doMigrateTrailingNLs` for a single element.
fn migrate_out_of(elt: &mut Node) -> Vec<Node> {
    let name = node_name(elt);
    let is_td_th = name == "td" || name == "th";
    let children = std::mem::take(&mut elt.children);

    // 1. Walk backward over trailing zero-width elements -> migration barrier.
    let mut i = children.len();
    let mut migration_barrier: Option<usize> = None;
    while i > 0
        && matches!(children[i - 1].kind, NodeKind::Element(_))
        && has_zero_width_wt(&children[i - 1])
    {
        migration_barrier = Some(i - 1);
        i -= 1;
    }

    // 2. Walk backward over trailing Text/Comment nodes.
    let mut first_elt_to_migrate: Option<usize> = None;
    let mut partial_content = false;
    let mut found_nl = false;
    let mut tsr_correction: isize = 0;
    let mut j = i;
    while j > 0 {
        let idx = j - 1;
        match &children[idx].kind {
            NodeKind::Comment(value) => {
                if is_td_th {
                    break;
                }
                first_elt_to_migrate = Some(idx);
                tsr_correction +=
                    decoded_comment_length(value, comment_is_unclosed(&children, idx)) as isize;
            }
            NodeKind::Text(s) => {
                if !is_td_th && is_ws_with_nl(s) {
                    found_nl = true;
                    first_elt_to_migrate = Some(idx);
                    partial_content = false;
                    tsr_correction += s.len() as isize;
                } else if s.ends_with('\n') {
                    found_nl = true;
                    first_elt_to_migrate = Some(idx);
                    partial_content = true;
                    tsr_correction += trailing_nl_count(s) as isize;
                    break;
                } else {
                    break;
                }
            }
            _ => break,
        }
        j -= 1;
    }

    let Some(first) = first_elt_to_migrate else {
        elt.children = children;
        return Vec::new();
    };
    if !found_nl {
        elt.children = children;
        return Vec::new();
    }

    // The migrated range is `[first, end_exclusive)`, where `end_exclusive` is
    // the zero-width barrier (which stays) or the end of the list.
    let end_exclusive = migration_barrier.unwrap_or(children.len());

    // Rebuild `elt.children` as `[0..first) + (head, if partial) + [end_exclusive..)`,
    // and collect the migrated nodes `[first..end_exclusive)`.
    let mut migrated: Vec<Node> = children[first..end_exclusive].to_vec();
    let mut stays: Vec<Node> = children[0..first].to_vec();

    if partial_content {
        // Split the leftmost (and only partial) migrated text node: the
        // non-newline head stays in `elt`, the trailing newlines migrate out.
        if let NodeKind::Text(s) = &children[first].kind {
            let nl = trailing_nl_count(s);
            let head = &s[..s.len() - nl];
            let tail = &s[s.len() - nl..];
            if !head.is_empty() {
                stays.push(Node::text(head));
            }
            migrated[0].kind = NodeKind::Text(tail.to_string());
        }
    }

    stays.extend(children[end_exclusive..].iter().cloned());

    // 3. Adjust the TSR of any nodes at/after the migration barrier by
    //    `-tsrCorrection` (mirrors the trailing TSR-adjustment loop).
    if let Some(_barrier) = migration_barrier {
        // In the rebuilt `stays` list, the barrier is the first element of the
        // `children[end_exclusive..]` tail, i.e. at index `stays.len() - (children.len() - end_exclusive)`.
        let tail_len = children.len() - end_exclusive;
        let start_idx = stays.len() - tail_len;
        for node in &mut stays[start_idx..] {
            if tsr_correction != 0
                && let Some(dp) = node.dp.as_mut()
                && let Some(tsr) = dp.tsr.as_mut()
            {
                if let Some(start) = tsr.start.as_mut() {
                    *start = start.saturating_sub(tsr_correction as usize);
                }
                tsr.end = tsr.end.saturating_sub(tsr_correction as usize);
            }
        }
    }

    elt.children = stays;
    migrated
}

/// Recursively process an element's subtree. Returns this node's migration
/// frame so its parent can decide whether to migrate its trailing newlines.
///
/// `is_last_child` mirrors `$node->nextSibling === null`; `prev_sibling_fostered`
/// mirrors whether `$node->previousSibling` is a fostered element; and
/// `ancestor_frames` carries the metadata chain from the root (exclusive) to
/// this node's parent (inclusive).
fn process_node(
    node: &mut Node,
    is_last_child: bool,
    prev_sibling_fostered: bool,
    ancestor_frames: &mut Vec<Frame>,
) -> Frame {
    let frame = Frame {
        name: node_name(node),
        can_migrate: can_migrate_nl_out_of_node(
            node,
            is_last_child,
            prev_sibling_fostered,
            ancestor_frames,
        ),
        prev_sibling_fostered,
    };

    if matches!(node.kind, NodeKind::Element(_) | NodeKind::Document) {
        let children = std::mem::take(&mut node.children);
        ancestor_frames.push(frame.clone());
        node.children = process_children(children, ancestor_frames);
        ancestor_frames.pop();
    }
    frame
}

/// Process a node's children: recurse into each element child (bottom-up), then
/// migrate each element child's trailing newlines into this node (before the
/// child's next sibling).
fn process_children(mut children: Vec<Node>, ancestor_frames: &mut Vec<Frame>) -> Vec<Node> {
    let n = children.len();

    // 1. Recurse into element children, capturing each child's frame.
    let mut child_frames: Vec<Frame> = vec![Frame::none(); n];
    for i in 0..n {
        if matches!(children[i].kind, NodeKind::Element(_) | NodeKind::Document) {
            let is_last = i + 1 == n;
            let prev_fostered = prev_is_fostered_element(&children, i);
            child_frames[i] =
                process_node(&mut children[i], is_last, prev_fostered, ancestor_frames);
        }
    }

    // 2. Migrate trailing newlines out of each element child (left-to-right;
    //    each migration lands immediately after its child, which is exactly where
    //    the rebuilt list places it).
    let mut out: Vec<Node> = Vec::with_capacity(n);
    for (i, mut child) in children.into_iter().enumerate() {
        if child_frames[i].can_migrate && matches!(child.kind, NodeKind::Element(_)) {
            let migrated = migrate_out_of(&mut child);
            out.push(child);
            out.extend(migrated);
        } else {
            out.push(child);
        }
    }
    out
}

/// Run the pass over a document subtree. Faithful to `MigrateTrailingNLs::run`.
pub fn run(root: &mut Node) {
    if !matches!(root.kind, NodeKind::Element(_) | NodeKind::Document) {
        return;
    }
    // The root (document) is `atTheTop`; it never migrates its own trailing
    // newlines, but its children are processed normally.
    let mut frames = vec![Frame {
        name: node_name(root),
        can_migrate: false,
        prev_sibling_fostered: false,
    }];
    let children = std::mem::take(&mut root.children);
    root.children = process_children(children, &mut frames);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elt(kind: ElementKind) -> Node {
        Node::element(kind)
    }

    #[test]
    fn test_is_ws_with_nl() {
        assert!(is_ws_with_nl("\n"));
        assert!(is_ws_with_nl(" \t\n"));
        assert!(is_ws_with_nl(" \t\r\n\t "));
        assert!(is_ws_with_nl("\r\n"));
        assert!(!is_ws_with_nl("   "));
        assert!(!is_ws_with_nl(""));
        assert!(!is_ws_with_nl("a\n"));
        assert!(!is_ws_with_nl("\n a"));
    }

    #[test]
    fn test_trailing_nl_count() {
        assert_eq!(trailing_nl_count("a"), 0);
        assert_eq!(trailing_nl_count("a\n"), 1);
        assert_eq!(trailing_nl_count("a\n\n"), 2);
        assert_eq!(trailing_nl_count("\n\n"), 2);
    }

    #[test]
    fn test_has_zero_width_wt() {
        let mut el = elt(ElementKind::Span);
        let dp = DataParsoid {
            tsr: Some(crate::wikitext::tokens_v2::SourceRange::new(5, 5)),
            ..DataParsoid::default()
        };
        el.dp = Some(dp);
        assert!(has_zero_width_wt(&el));

        let mut el2 = elt(ElementKind::Span);
        el2.dp = Some(DataParsoid {
            tsr: Some(crate::wikitext::tokens_v2::SourceRange::new(5, 6)),
            ..DataParsoid::default()
        });
        assert!(!has_zero_width_wt(&el2));

        // A zero-width node with a text child is not zero-width.
        let mut el3 = elt(ElementKind::Span);
        el3.dp = Some(DataParsoid {
            tsr: Some(crate::wikitext::tokens_v2::SourceRange::new(5, 5)),
            ..DataParsoid::default()
        });
        el3.push_child(Node::text("x"));
        assert!(!has_zero_width_wt(&el3));

        // A `null` start (unknown) tsr — like a template end marker meta with
        // `tsr = [ null, end ]` — is NOT zero-width (mirrors PHP's
        // `$tsr->start === null` early-return), so migrate-nls won't cross it.
        let mut el4 = elt(ElementKind::Other("meta".to_string()));
        el4.dp = Some(DataParsoid {
            tsr: Some(crate::wikitext::tokens_v2::SourceRange::with_null_start(18)),
            ..DataParsoid::default()
        });
        assert!(!has_zero_width_wt(&el4));
    }

    #[test]
    fn test_migrates_trailing_nl_out_of_auto_inserted_italic() {
        // <i>b\n</i> where <i> has autoInsertedEnd: the trailing newline migrates out.
        let mut i = elt(ElementKind::Italic);
        i.dp = Some(DataParsoid {
            auto_inserted_end: true,
            ..DataParsoid::default()
        });
        i.push_child(Node::text("b\n"));

        let mut doc = Node::document();
        doc.push_child(i);

        run(&mut doc);

        // The <i> should now contain only "b", and the newline is a sibling after it.
        let i = &doc.children[0];
        assert!(matches!(&i.kind, NodeKind::Element(ElementKind::Italic)));
        assert!(
            matches!(
                i.children.first(),
                Some(c) if matches!(&c.kind, NodeKind::Text(t) if t == "b")
            ),
            "{i:?}"
        );
        assert!(
            matches!(
                doc.children.get(1),
                Some(c) if matches!(&c.kind, NodeKind::Text(t) if t == "\n")
            ),
            "{doc:?}"
        );
    }

    #[test]
    fn test_does_not_migrate_out_of_non_migratable_node() {
        // A <div> (not in the migration set, not auto-inserted) keeps its newline.
        let mut div = elt(ElementKind::Div);
        div.push_child(Node::text("x\n"));
        let mut doc = Node::document();
        doc.push_child(div);
        run(&mut doc);
        assert!(matches!(
            doc.children[0].children.first(),
            Some(c) if matches!(&c.kind, NodeKind::Text(t) if t == "x\n")
        ));
    }
}
