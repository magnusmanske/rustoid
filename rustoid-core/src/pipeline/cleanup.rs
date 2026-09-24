//! `CleanUp` — DOM-level cleanup pass, a faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Handlers/CleanUp.php`.
//!
//! Currently this implements the empty-element handling (`handleEmptyElements`):
//! "flagged" empty elements (those listed in `Consts::$Output['FlaggedEmptyElts']`
//! — `p`, `li`, `tbody`, `tr`) that contain only whitespace, comments, and
//! rendering-transparent nodes are marked with the `mw-empty-elt` class so that
//! wikis can style/hide them, and empty `mw-empty-elt` wrapper spans are removed.
//!
//! This mirrors the empty-`<p>` handling in Parsoid's `CleanUp` pass, which runs
//! after tree building and p-wrapping.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::html::wts_utils::element_tag;
use crate::wikitext::tokens_v2::DataParsoid;

/// Whether `dp` carries a literal-HTML (`stx: html`) marker. Faithful to
/// `WTUtils::hasLiteralHTMLMarker`.
fn has_literal_html_marker(dp: Option<&DataParsoid>) -> bool {
    dp.is_some_and(|dp| dp.stx.as_deref() == Some("html"))
}

/// The "flagged empty elements" the cleanup pass inspects (`Consts::$Output['FlaggedEmptyElts']`).
fn flagged_empty_elts() -> &'static [&'static str] {
    &["li", "tbody", "tr", "p"]
}

fn has_class(node: &Node, class: &str) -> bool {
    node.get_attr("class")
        .is_some_and(|c| c.split_whitespace().any(|t| t == class))
}

/// `WTUtils::isRenderingTransparentNode` — comments, SOL-transparent links
/// (category/redirect/language page-property links), non-HTML metas, and
/// fallback-id spans render nothing, so a flagged empty element containing only
/// these is still empty.
///
/// The SOL-transparent link case is load-bearing: a `<p>` whose whole content is
/// an empty `mw:Nowiki` wrapper plus a category link is empty, and the wiki
/// serves it as `<p class="mw-empty-elt">`. Without it `List of sovereign
/// states` differs at byte 2 — on the `<p>` itself.
fn is_rendering_transparent(node: &Node) -> bool {
    if matches!(node.kind, NodeKind::Comment(_)) {
        return true;
    }
    if crate::html::wts_utils::is_sol_transparent_link(node)
        || crate::html::wts_utils::is_fallback_id_span(node)
    {
        return true;
    }
    if let NodeKind::Element(kind) = &node.kind {
        let typeof_attr = node.get_attr("typeof").unwrap_or("");
        let has_excluded_typeof = typeof_attr
            .split_whitespace()
            .any(|t| t.starts_with("mw:Annotation/") || t == "mw:DOMFragment");
        if has_excluded_typeof {
            return false;
        }
        if element_tag(kind) == "meta" {
            return node
                .data_parsoid
                .as_deref()
                .is_none_or(|dp| !dp.contains("\"stx\":\"html\""));
        }
    }
    false
}

/// Whether a node is a nowiki or DOMFragment wrapper (whose contents should be
/// unwrapped when checking for emptiness).
fn is_nowiki_or_dom_fragment(node: &Node) -> bool {
    node.get_attr("typeof").is_some_and(|ty| {
        ty.split_whitespace()
            .any(|t| t == "mw:Nowiki" || t == "mw:DOMFragment")
    })
}

/// Whether a node carries the parsoid-added `wrapper` temp flag.
fn is_wrapper(node: &Node) -> bool {
    node.data_parsoid
        .as_deref()
        .is_some_and(|dp| dp.contains("\"wrapper\":true"))
}

/// Mirrors `CleanUp::isEmptyNode`: return true if `node` has only comments,
/// whitespace text, rendering-transparent nodes, nowiki/DOM-fragment wrappers
/// wrapping empty content, and flagged empty elements wrapping empty content.
fn is_empty_node(node: &Node, has_rt_nodes: &mut bool) -> bool {
    for child in &node.children {
        match &child.kind {
            NodeKind::Comment(_) => continue,
            NodeKind::Text(s) => {
                if !s.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
                    return false;
                }
            }
            NodeKind::Element(kind) => {
                let tag = element_tag(kind);
                if flagged_empty_elts().contains(&tag.as_str()) {
                    if is_empty_node(child, has_rt_nodes) {
                        continue;
                    }
                    return false;
                }
                if is_rendering_transparent(child) || has_class(child, "mw-empty-elt") {
                    *has_rt_nodes = true;
                    continue;
                }
                if (is_nowiki_or_dom_fragment(child) || is_wrapper(child))
                    && is_empty_node(child, has_rt_nodes)
                {
                    continue;
                }
                return false;
            }
            NodeKind::Document => return false,
        }
    }
    true
}

/// Whether a node is a "first encapsulation wrapper" (carries `about` + a
/// transclusion/extension `typeof`). Used to avoid deleting a wrapper that
/// anchors an about-chain.
fn is_first_encapsulation_wrapper(node: &Node) -> bool {
    node.get_attr("about").is_some()
        && node.get_attr("typeof").is_some_and(|ty| {
            ty.split_whitespace().any(|t| {
                t == "mw:Transclusion"
                    || t == "mw:Param"
                    || t.starts_with("mw:Transclusion/")
                    || t.starts_with("mw:Extension/")
            })
        })
}

/// Handle an empty element, adding the `mw-empty-elt` class or removing
/// deletable `mw-empty-elt` spans. Mirrors `CleanUp::handleEmptyElements`.
fn handle_empty_element(node: &mut Node) {
    let tag = match &node.kind {
        NodeKind::Element(kind) => element_tag(kind),
        _ => return,
    };

    // Remove deletable `mw-empty-elt` wrapper spans (those which are empty, or
    // carry only a single IEW child), unless they anchor an about-chain.
    if tag == "span" && has_class(node, "mw-empty-elt") {
        if is_first_encapsulation_wrapper(node) {
            return;
        }
        let deletable = node.children.is_empty()
            || (node.children.len() == 1
                && matches!(&node.children[0].kind, NodeKind::Text(t) if t.trim().is_empty()));
        if deletable {
            node.children.clear();
        }
        return;
    }

    if !flagged_empty_elts().contains(&tag.as_str()) {
        return;
    }

    let mut has_rt_nodes = false;
    if !is_empty_node(node, &mut has_rt_nodes) {
        return;
    }

    // After removing the empty-element class, a flagged element is only
    // "empty" (and hence marked) if it carries no meaningful attributes.
    // For `<p>` this mirrors the legacy parser: an empty `<p>` with only
    // `data-parsoid`/`stx` (parsoid-added) attributes is still markable.
    for attr in &node.attrs {
        if attr.key != "data-parsoid" && attr.key != "stx" {
            return;
        }
    }

    // Add the `mw-empty-elt` class (merging with any existing `class`).
    let existing = node.get_attr("class").map(str::to_string);
    let merged = match existing {
        Some(c) if !c.split_whitespace().any(|t| t == "mw-empty-elt") => {
            format!("{c} mw-empty-elt")
        }
        Some(c) => c,
        None => "mw-empty-elt".to_string(),
    };
    node.set_attr("class", merged);
}

/// Run the `CleanUp` empty-element pass over the document.
pub fn run(root: &mut Node) {
    handle_empty_element(root);
    trim_whitespace(root);
    for child in &mut root.children {
        run(child);
    }
}

/// Drop the `data-parsoid` of nodes inside a transclusion that the service keeps
/// none for. Faithful to `CleanUp::markDiscardableDataParsoid`, and run where PHP
/// runs it: as the cleanup traverser's *store* step, so it is called just before
/// the page-bundle ids are allocated.
///
/// Inside an encapsulation range only the **first** node and the **last** keep
/// their metadata, plus any node whose `data-parsoid` carries no `stx` and any
/// node in native (extension) content; everything else loses it. The service does
/// not key those nodes, and the ids are positional, so keeping one shifts every
/// id after it — the module-output category link beside a `#ifeq` one was the
/// visible case.
///
/// Only `data-parsoid` goes, not the structured `dp` and not `data-mw`: later
/// passes and the round-trip serializers read `dp`, and a node with `data-mw` is
/// keyed by it whatever its `data-parsoid` says.
pub fn mark_discardable_data_parsoid(root: &mut Node) {
    discard_in_siblings(&mut root.children, false);
}

/// One sibling list: find each encapsulation range and mark its interior.
fn discard_in_siblings(children: &mut [Node], in_native: bool) {
    let mut i = 0;
    while i < children.len() {
        if !is_first_encapsulation_wrapper(&children[i]) {
            let inside = in_native || is_native_ext(&children[i]);
            discard_in_siblings(&mut children[i].children, inside);
            i += 1;
            continue;
        }
        // A range runs from here through the last following element sibling with
        // the same `about` (mirrors `WTUtils::getAboutSiblings`).
        let about = children[i].get_attr("about").map(str::to_string);
        let mut last = i;
        while last + 1 < children.len() && children[last + 1].get_attr("about") == about.as_deref()
        {
            last += 1;
        }
        let span = last - i + 1;
        for (offset, child) in children[i..=last].iter_mut().enumerate() {
            let inside = in_native || is_native_ext(child);
            discard_node(child, offset == 0, offset + 1 == span, inside);
            // Every descendant sits inside the range but is neither its first nor
            // its last node, so it never gets the boundary exemptions.
            discard_in_range(&mut child.children, inside);
        }
        i = last + 1;
    }
}

/// Mark the marked-state of every descendant of a range member.
fn discard_in_range(children: &mut [Node], in_native: bool) {
    for child in children.iter_mut() {
        let inside = in_native || is_native_ext(child);
        discard_node(child, false, false, inside);
        discard_in_range(&mut child.children, inside);
    }
}

/// Discard `node`'s `data-parsoid` unless it is a boundary node, native content,
/// or an `stx`-bearing last node or heading (which the serializer needs `stx` on).
fn discard_node(node: &mut Node, is_first: bool, is_last: bool, in_native: bool) {
    if !node.kind.is_element() || is_first || in_native {
        return;
    }
    let has_stx = node.dp.as_ref().is_some_and(|dp| dp.stx.is_some())
        || node
            .data_parsoid
            .as_deref()
            .is_some_and(|json| json.contains("\"stx\""));
    let is_heading = matches!(node.kind, NodeKind::Element(ElementKind::Heading(_)));
    if has_stx && (is_last || is_heading) {
        return;
    }
    node.data_parsoid = None;
}

/// Whether the node is an extension's tag. Used for `CleanUp::inNativeContent`,
/// conservatively: rustoid's site config does not distinguish native from
/// non-native extension tags, so *any* extension content is treated as native,
/// which discards less than the reference rather than more.
fn is_native_ext(node: &Node) -> bool {
    node.get_attr("typeof").is_some_and(|ty| {
        ty.split_whitespace()
            .any(|t| t.starts_with("mw:Extension/"))
    })
}

/// Trim leading/trailing `[ \t]` whitespace from a trimmable-WS element, removing
/// pure-whitespace text children and `mw:DisplaySpace` elements, and recording the
/// trimmed widths in `dp.dsr.leading_ws`/`trailing_ws` (or `-1` when the widths
/// cannot be reliably tracked). Faithful to `CleanUp::trimWhiteSpace`.
fn trim_whitespace(node: &mut Node) {
    let tag = match &node.kind {
        NodeKind::Element(kind) => element_tag(kind),
        _ => return,
    };

    // Faithful to the guard in `finalCleanup`: only wikitext markup (not literal
    // HTML) with trimmable whitespace is trimmed.
    if !crate::wikitext::consts::wikitext_tags_with_trimmable_ws().contains(&tag)
        || has_literal_html_marker(node.dp.as_ref())
    {
        return;
    }

    // We need a DSR to record the trimmed widths on.
    if node.dp.as_ref().and_then(|d| d.dsr.as_ref()).is_none() {
        return;
    }

    // --- Trim leading whitespace (first line) ---
    let mut trimmed_len = 0usize;
    let mut update_dsr = true;
    let mut skipped = false;

    let mut break_index = node.children.len();
    let mut i = 0usize;
    while i < node.children.len() {
        let pure_ws_text = matches!(&node.children[i].kind, NodeKind::Text(t) if !t.is_empty() && t.bytes().all(|b| matches!(b, b' ' | b'\t')));
        let display_space = matches!(&node.children[i].kind, NodeKind::Element(_))
            && crate::html::dom_utils::has_type_of(&node.children[i], "mw:DisplaySpace");

        if pure_ws_text {
            let len = match &node.children[i].kind {
                NodeKind::Text(t) => t.len(),
                _ => 0,
            };
            trimmed_len += len;
            update_dsr = !skipped;
            node.children.remove(i);
        } else if display_space {
            trimmed_len += 1;
            update_dsr = !skipped;
            node.children.remove(i);
        } else if !crate::html::wts_utils::is_rendering_transparent_node(&node.children[i]) {
            break_index = i;
            break;
        } else {
            skipped = true;
            i += 1;
        }
    }

    if break_index < node.children.len() {
        let child = &mut node.children[break_index];
        if let NodeKind::Text(t) = &mut child.kind {
            let byte_len = t.bytes().take_while(|b| matches!(b, b' ' | b'\t')).count();
            if byte_len > 0 {
                update_dsr = !skipped;
                trimmed_len += byte_len;
                *t = t[byte_len..].to_string();
            }
        }
    }

    let leading_ws = if update_dsr { trimmed_len as isize } else { -1 };

    // --- Trim trailing whitespace (last line) ---
    let mut trimmed_len = 0usize;
    let mut update_dsr = true;
    let mut skipped = false;

    let mut break_index = node.children.len();
    let mut i = node.children.len();
    while i > 0 {
        i -= 1;
        let pure_ws_text = matches!(&node.children[i].kind, NodeKind::Text(t) if !t.is_empty() && t.bytes().all(|b| matches!(b, b' ' | b'\t')));
        if pure_ws_text {
            let len = match &node.children[i].kind {
                NodeKind::Text(t) => t.len(),
                _ => 0,
            };
            trimmed_len += len;
            update_dsr = !skipped;
            node.children.remove(i);
            break_index = i;
        } else if !crate::html::wts_utils::is_rendering_transparent_node(&node.children[i]) {
            break_index = i;
            break;
        } else {
            skipped = true;
        }
    }

    if break_index < node.children.len() {
        let child = &mut node.children[break_index];
        if let NodeKind::Text(t) = &mut child.kind {
            // Faithful to `/^([\s\S]*\S)([ \t]+)$/D`: strip a trailing `[ \t]+`
            // run only when the character *immediately before* that run is
            // non-whitespace (`\S`). An intervening `\n` (or other whitespace)
            // means the run is not trimmed (it is separator text, not content).
            let trailing_count = t.len() - t.trim_end_matches([' ', '\t']).len();
            if trailing_count > 0 {
                let prefix = &t[..t.len() - trailing_count];
                if let Some(last) = prefix.chars().next_back()
                    && !last.is_whitespace()
                {
                    update_dsr = !skipped;
                    trimmed_len += trailing_count;
                    *t = prefix.to_string();
                }
            }
        }
    }

    let trailing_ws = if update_dsr { trimmed_len as isize } else { -1 };

    if let Some(dp) = node.dp.as_mut()
        && let Some(dsr) = dp.dsr.as_mut()
    {
        dsr.leading_ws = leading_ws;
        dsr.trailing_ws = trailing_ws;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::ElementKind;

    #[test]
    fn test_empty_p_gets_mw_empty_elt() {
        let mut p = Node::element(ElementKind::Paragraph);
        // Whitespace-only child text (IEW) does not prevent emptiness.
        p.push_child(Node::text("  \n "));

        let mut root = Node::document();
        root.push_child(p);
        run(&mut root);

        assert_eq!(
            root.children[0].get_attr("class"),
            Some("mw-empty-elt"),
            "got: {:?}",
            root.children[0]
        );
    }

    /// Inside a transclusion range, an interior `stx`-bearing node loses its
    /// `data-parsoid`, its `stx`-bearing *last* sibling keeps it, and the head is
    /// never touched. This is the `Module:SDcat` category link: the service does
    /// not key it, and keeping its key shifts every later id.
    #[test]
    fn discard_keeps_only_the_range_boundaries() {
        fn stx_link() -> Node {
            let mut n = Node::element(ElementKind::CategoryLink);
            n.dp = Some(DataParsoid {
                stx: Some("simple".to_string()),
                ..Default::default()
            });
            n.data_parsoid = Some("{\"dsr\":[0,1,0,0],\"stx\":\"simple\"}".to_string());
            n
        }
        let mut head = Node::element(ElementKind::Transclusion);
        head.set_attr("about", "#mwt1");
        head.set_attr("typeof", "mw:Transclusion");
        head.dp = Some(DataParsoid {
            stx: Some("simple".to_string()),
            ..Default::default()
        });
        head.data_parsoid = Some("{\"dsr\":[0,1,0,0]}".to_string());
        let mut interior = stx_link();
        interior.set_attr("about", "#mwt1");
        let mut last = stx_link();
        last.set_attr("about", "#mwt1");

        let mut root = Node::document();
        root.push_child(head);
        root.push_child(interior);
        root.push_child(last);
        mark_discardable_data_parsoid(&mut root);

        assert!(root.children[0].data_parsoid.is_some(), "head keeps its dp");
        assert!(
            root.children[1].data_parsoid.is_none(),
            "interior loses its dp"
        );
        assert!(root.children[2].data_parsoid.is_some(), "last keeps its dp");
    }

    /// A range member's *descendant* is interior even though it is not a
    /// boundary of the range.
    #[test]
    fn discard_reaches_into_a_range_member() {
        let mut head = Node::element(ElementKind::Transclusion);
        head.set_attr("about", "#mwt1");
        head.set_attr("typeof", "mw:Transclusion");
        let mut member = Node::element(ElementKind::Span);
        member.set_attr("about", "#mwt1");
        let mut inner = Node::element(ElementKind::CategoryLink);
        inner.dp = Some(DataParsoid {
            stx: Some("simple".to_string()),
            ..Default::default()
        });
        inner.data_parsoid = Some("{\"dsr\":[0,1,0,0]}".to_string());
        member.push_child(inner);

        let mut root = Node::document();
        root.push_child(head);
        root.push_child(member);
        mark_discardable_data_parsoid(&mut root);

        assert!(root.children[1].children[0].data_parsoid.is_none());
    }

    /// Outside any range nothing is discarded.
    #[test]
    fn discard_leaves_nodes_outside_a_range_alone() {
        let mut link = Node::element(ElementKind::CategoryLink);
        link.dp = Some(DataParsoid {
            stx: Some("simple".to_string()),
            ..Default::default()
        });
        link.data_parsoid = Some("{\"dsr\":[0,1,0,0]}".to_string());
        let mut root = Node::document();
        root.push_child(link);
        mark_discardable_data_parsoid(&mut root);
        assert!(root.children[0].data_parsoid.is_some());
    }

    #[test]
    fn test_nonempty_p_untouched() {
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("hello"));

        let mut root = Node::document();
        root.push_child(p);
        run(&mut root);

        assert_eq!(root.children[0].get_attr("class"), None);
    }

    #[test]
    fn test_empty_p_with_comment_and_render_transparent() {
        // A comment and a rendering-transparent meta do not make the p non-empty.
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::comment("c"));

        let mut root = Node::document();
        root.push_child(p);
        run(&mut root);

        assert_eq!(root.children[0].get_attr("class"), Some("mw-empty-elt"));
    }

    fn li_with_dsr(children: Vec<Node>, start: usize, end: usize) -> Node {
        let mut li = Node::element(ElementKind::ListItem);
        for c in children {
            li.push_child(c);
        }
        li.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(start),
                end: Some(end),
                open_width: Some(1),
                close_width: Some(0),
                leading_ws: 0,
                trailing_ws: 0,
            }),
            ..Default::default()
        });
        li
    }

    /// Trim trailing `[ \t]` from "foo " → "foo", `trailing_ws = 1`.
    #[test]
    fn test_trim_trailing_space() {
        let mut li = li_with_dsr(vec![Node::text("foo ")], 0, 5);
        trim_whitespace(&mut li);
        let dsr = li.dp.as_ref().unwrap().dsr.as_ref().unwrap();
        assert_eq!(dsr.trailing_ws, 1);
        assert_eq!(dsr.leading_ws, 0);
        assert!(matches!(&li.children[0].kind, NodeKind::Text(t) if t == "foo"));
    }

    /// A trailing `[ \t]` run preceded by a `\n` is *not* trimmed (faithful to
    /// `/^([\s\S]*\S)([ \t]+)$/D` — the `\n` separates the non-ws char from the
    /// trailing run). This preserves the separator space between table cells.
    /// (The *leading* space is still trimmed, per the leading regex.)
    #[test]
    fn test_trim_trailing_space_after_newline_is_untouched() {
        let mut li = li_with_dsr(vec![Node::text(" [1]\n ")], 0, 6);
        trim_whitespace(&mut li);
        let dsr = li.dp.as_ref().unwrap().dsr.as_ref().unwrap();
        assert_eq!(dsr.leading_ws, 1); // leading space before `[` trimmed
        assert_eq!(dsr.trailing_ws, 0); // trailing space after `\n` NOT trimmed
        assert!(matches!(&li.children[0].kind, NodeKind::Text(t) if t == "[1]\n "));
    }

    /// A rendering-transparent node (comment) in the middle makes the trimmed
    /// widths unreliable → `-1` (faithful to the `$skipped` flag).
    #[test]
    fn test_trim_trailing_with_comment_is_invalid() {
        let mut li = li_with_dsr(
            vec![Node::text("c "), Node::comment("c2"), Node::text(" ")],
            0,
            14,
        );
        trim_whitespace(&mut li);
        let dsr = li.dp.as_ref().unwrap().dsr.as_ref().unwrap();
        assert_eq!(dsr.trailing_ws, -1);
        // "c " trailing space stripped → "c"; the trailing " " text node removed.
        assert_eq!(li.children.len(), 2);
        assert!(matches!(&li.children[0].kind, NodeKind::Text(t) if t == "c"));
        assert!(matches!(&li.children[1].kind, NodeKind::Comment(c) if c == "c2"));
    }
}
