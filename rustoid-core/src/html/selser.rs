//! Selective serialization (selser).
//!
//! Faithful port of PHP Parsoid's `Html2Wt\SelectiveSerializer`. Given the
//! edited DOM and the *original* (revision) DOM — with the revision wikitext —
//! it reuses the original wikitext source for unmodified regions of the DOM,
//! re-serializing only what a `DOMDiff` marks as changed.
//!
//! The entry point is [`selective_serialize_dom`], which mirrors
//! `SelectiveSerializer::serializeDOM`:
//!
//! 1. Wrap the direct text children of `<li>`/`<dd>` elements (in *both* DOMs)
//!    in `<span data-mw-selser-wrapper>` markers carrying a computed DSR, so
//!    `DOMDiff` can mark content changes at a finer granularity.
//! 2. Diff the old body against the new body via [`DomDiff`], annotating the new
//!    DOM in place.
//! 3. If nothing changed, return the revision wikitext verbatim; otherwise
//!    hand the annotated document to the selser-mode serializer.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::error::Result;
use crate::html::dom_diff::DomDiff;
use crate::html::dsr::{SelectiveUpdateData, is_valid_dsr};
use crate::html::parse::parse_html;

/// Wrap the direct text-node children of every descendant element named
/// `node_name` (used for `li` and `dd`) in `<span data-mw-selser-wrapper>`
/// markers, computing a (speculative) DSR for each. Faithful to
/// `SelectiveSerializer::wrapTextChildrenOfNode`.
///
/// The DSR relies on trimmed-whitespace metadata (`leadingWS`/`trailingWS`) from
/// the wt→html direction; on the original DOM these are accurate, on the edited
/// DOM they are speculative and are discarded by `DOMDiff` when the
/// `data-parsoid` attribute diverges.
fn wrap_text_children_of_node(body: &mut Node, node_name: &str) {
    let in_list_item = crate::html::dom_utils::is_list_item_name(node_name);
    collect_and_wrap(body, node_name, in_list_item);
}

/// Recursively walk the owned tree, wrapping the text children of every element
/// named `node_name`. This mirrors `querySelectorAll($body, $nodeName)` — all
/// descendants match, not just direct children — while avoiding aliasing `body`
/// (the list item is mutated only through its own `children` `&mut`).
fn collect_and_wrap(node: &mut Node, node_name: &str, in_list_item: bool) {
    if matches!(node.kind, NodeKind::Element(_))
        && crate::html::wts_utils::node_name(node) == node_name
    {
        wrap_one_list_item(node, in_list_item);
    }
    for child in &mut node.children {
        collect_and_wrap(child, node_name, in_list_item);
    }
}

/// Wrap the text-node children of a single `<li>`/`<dd>` element. Faithful to the
/// per-element body of `wrapTextChildrenOfNode`.
fn wrap_one_list_item(elt: &mut Node, in_list_item: bool) {
    // Skip items with `about` (template/extension content) and literal-HTML nodes.
    if crate::html::wts_utils::is_literal_html_node(elt) || elt.get_attr("about").is_some() {
        return;
    }

    // No point wrapping if there is no usable DSR on the list item itself.
    let Some(elt_dsr) = crate::html::wts_utils::get_dsr(elt) else {
        return;
    };
    if !is_valid_dsr(Some(&elt_dsr), false) {
        return;
    }

    // `$start = $eltDSR->innerStart()`: skip the leading (open) tag width.
    let mut start = elt_dsr.inner_start();

    let mut c = 0;
    while c < elt.children.len() {
        if c == 0 && !elt_dsr.has_valid_leading_ws() {
            // No accurate leading-WS width: cannot wrap the first text node.
            break;
        }
        if c == 0 {
            start += elt_dsr.leading_ws.max(0) as usize;
        }

        // The next sibling index, skipping over any encapsulated forest rooted at
        // this child (`WTUtils::skipOverEncapsulatedContent`), plus an extra step
        // when a trailing newline was split off (a `<span>` was inserted before the
        // current node, shifting subsequent indices by one).
        let next = skip_over_encapsulated_content(&elt.children, c).unwrap_or(elt.children.len());
        let next_opt = (next < elt.children.len()).then_some(next);
        let child = &elt.children[c];

        // The trailing-newline split inserts a node before index `c`, shifting
        // everything at/after `c` by one, so advance past it.
        let mut next_c = next;

        match &child.kind {
            NodeKind::Text(text) => {
                let text = text.clone();
                let mut len = text.len();
                // Don't wrap trailing newlines: single-line-context handling would
                // convert them into spaces and introduce dirty-diffs. Leave them
                // outside the wrapper to be handled as separator text.
                //
                // `$nl` is `null` when no trailing newline was split off.
                let (text, nls): (String, Option<String>) =
                    if len > 0 && text.as_bytes()[len - 1] == b'\n' {
                        let trimmed = text.trim_end_matches('\n').to_string();
                        let count = len - trimmed.len();
                        len = trimmed.len();
                        (trimmed, Some("\n".repeat(count)))
                    } else {
                        // Last child of the "original" item (or the item now ends
                        // in a nested inserted list): tack on the trailing-WS width.
                        if is_last_child_with_nested_list(&elt.children, next_opt, in_list_item) {
                            if !elt_dsr.has_valid_trailing_ws() {
                                break;
                            }
                            len += elt_dsr.trailing_ws.max(0) as usize;
                        }
                        (text, None)
                    };

                // Build `<span data-mw-selser-wrapper>` with DSR
                // `[start, start + len, 0, 0]`.
                let mut span = Node::element(ElementKind::Span);
                span.set_attr("data-mw-selser-wrapper", "");
                let dp = span.dp.get_or_insert_with(Default::default);
                // Faithful to
                // `$dp->dsr = new DomSourceRange($start, $start + $len, 0, 0)` and
                // `$dp->setTempFlag(TempData::IS_NEW, false)` — a non-null DSR makes
                // this node "not new" (see `is_new_elt`, which checks `dsr.is_none()`).
                dp.dsr = Some(crate::wikitext::tokens_v2::DomSourceRange {
                    start: Some(start),
                    end: Some(start + len),
                    open_width: Some(0),
                    close_width: Some(0),
                    leading_ws: 0,
                    trailing_ws: 0,
                });
                span.push_child(Node::text(text));

                // Faithful to PHP's three-way mutation:
                //   non-nl: `$elt->replaceChild($span, $c);`
                //       nl: `$elt->insertBefore($span, $c);` +
                //           `$c->nodeValue = $nl;` (the *same* text node keeps its
                //           position and becomes the newline run, so the captured
                //           `next` index stays correct).
                let nls_len = match nls {
                    None => {
                        elt.children[c] = span;
                        0
                    }
                    Some(nls) => {
                        let count = nls.len();
                        elt.children[c] = Node::text(nls);
                        elt.children.insert(c, span);
                        next_c += 1;
                        count
                    }
                };

                // `$start += $len;` then, in the `$nl` branch, `$start += $numOfNls;`.
                start += len + nls_len;
            }
            NodeKind::Comment(value) => {
                let unclosed = has_unclosed_comment_prev(&elt.children, c);
                start += crate::html::wts_utils::decoded_comment_length(value, unclosed);
            }
            NodeKind::Element(_) => {
                // No point wrapping following text if this child has no usable DSR.
                let Some(c_dsr) = crate::html::wts_utils::get_dsr(child) else {
                    break;
                };
                if !is_valid_dsr(Some(&c_dsr), false) {
                    break;
                }
                start = c_dsr.end.unwrap_or(start);
            }
            NodeKind::Document => {}
        }

        c = next_c;
    }
}

/// Decide whether the text node is the last child of the "original" item, or
/// the item now ends in a nested inserted list — either way, tack on the
/// trailing-WS width. Faithful to the `!$next || ($inListItem && isList($next)
/// && isNewElt($next))` test in `wrapTextChildrenOfNode`.
fn is_last_child_with_nested_list(
    children: &[Node],
    next_opt: Option<usize>,
    in_list_item: bool,
) -> bool {
    match next_opt {
        None => true,
        Some(next) => {
            let next_node = &children[next];
            in_list_item
                && crate::html::dom_utils::is_list(next_node)
                && next_node.dp.as_ref().is_none_or(|d| d.dsr.is_none())
        }
    }
}

/// `WTUtils::skipOverEncapsulatedContent` — return the index just past the
/// encapsulated forest (siblings sharing `about`), or `None` at end of list.
fn skip_over_encapsulated_content(children: &[Node], from: usize) -> Option<usize> {
    let about = children
        .get(from)
        .and_then(|n| n.get_attr("about"))
        .map(str::to_string);
    let Some(about) = about else {
        return (from + 1 < children.len()).then_some(from + 1);
    };
    let mut i = from + 1;
    while i < children.len() {
        let same = children[i]
            .get_attr("about")
            .map(|a| a == about)
            .unwrap_or(false);
        if !same {
            break;
        }
        i += 1;
    }
    (i < children.len()).then_some(i)
}

/// Whether the comment at `index` is immediately preceded by a
/// `mw:Placeholder/UnclosedComment` meta (which shortens the wikitext delimiter
/// from `<!--…-->` to just `<!--…`). Faithful to `decodedCommentLength`'s
/// `previousSibling` check.
fn has_unclosed_comment_prev(children: &[Node], index: usize) -> bool {
    if index == 0 {
        return false;
    }
    if !matches!(children[index - 1].kind, NodeKind::Element(_)) {
        return false;
    }
    crate::html::dom_utils::has_type_of(&children[index - 1], "mw:Placeholder/UnclosedComment")
}

/// Pre-process a DOM for selser by wrapping the text children of `<li>` and
/// `<dd>` elements. Faithful to `SelectiveSerializer::preprocessDOMForSelser`.
fn preprocess_dom_for_selser(body: &mut Node) {
    wrap_text_children_of_node(body, "li");
    wrap_text_children_of_node(body, "dd");
}

/// Selectively serialize an edited document, reusing the revision wikitext for
/// unmodified content. Faithful to `SelectiveSerializer::serializeDOM`.
///
/// * `doc` — the (edited) DOM to serialize (its `<body>` content is used).
/// * `selser_data` — carries the revision wikitext and the revision DOM (the
///   "old body" the diff compares against); the DOM is recovered from `rev_html`
///   if needed.
/// * `env` — the serializer environment (optional; `None` falls back to literal
///   HTML for links/media).
pub fn selective_serialize_dom(
    doc: &mut Node,
    selser_data: &mut SelectiveUpdateData,
    env: Option<crate::html::env::SerializerEnv>,
) -> Result<String> {
    // Populate the revision DOM from `rev_html` when it isn't already present.
    if selser_data.rev_dom.is_none()
        && let Some(rev_html) = selser_data.rev_html.as_deref()
    {
        selser_data.rev_dom = Some(Box::new(parse_html(rev_html)?));
    }

    // `$oldBody = DOMCompat::getBody($this->selserData->revDOM);`
    let old_body = match selser_data.rev_dom.take() {
        Some(old) => old,
        None => {
            // No revision DOM: nothing to diff against, so fall through to the
            // selser serializer (nothing can be reused).
            return Ok(
                crate::html::serializer::WikitextSerializer::serialize_dom_selser(
                    doc.clone(),
                    env,
                    &selser_data.rev_text,
                ),
            );
        }
    };
    let mut old_body = *old_body;

    // Pre-process both DOMs (selser-specific wrapping).
    preprocess_dom_for_selser(&mut old_body);
    preprocess_dom_for_selser(doc);

    // `$diff = (new DOMDiff($this->env))->diff($oldBody, $body);`
    let mut dom_diff = DomDiff::default();
    let changed = dom_diff.diff(&old_body, doc);

    let result = if !changed {
        // Nothing was modified: reuse the original source verbatim.
        selser_data.rev_text.clone()
    } else {
        // `$r = $this->wts->serializeDOM($doc, true);`
        crate::html::serializer::WikitextSerializer::serialize_dom_selser(
            doc.clone(),
            env,
            &selser_data.rev_text,
        )
    };

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::DataParsoid;

    fn li_with_dsr(start: usize, end: usize) -> Node {
        let mut li = Node::element(ElementKind::ListItem);
        li.dp = Some(DataParsoid {
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

    #[test]
    fn test_wrap_text_children_wraps_text() {
        let mut li = li_with_dsr(0, 5); // "*foo" -> open width 1, content "foo"
        li.push_child(Node::text("foo"));
        wrap_text_children_of_node(&mut li, "li");

        assert_eq!(li.children.len(), 1);
        let span = &li.children[0];
        assert!(span.get_attr("data-mw-selser-wrapper").is_some());
        // Span DSR: [1, 4, 0, 0] (innerStart=1, content "foo"=3 bytes).
        let dsr = span.dp.as_ref().and_then(|d| d.dsr.clone()).unwrap();
        assert_eq!(dsr.start, Some(1));
        assert_eq!(dsr.end, Some(4));
        assert_eq!(dsr.open_width, Some(0));
        assert_eq!(dsr.close_width, Some(0));
    }

    #[test]
    fn test_wrap_skips_about_items() {
        let mut li = li_with_dsr(0, 5);
        li.set_attr("about", "#mwt1");
        li.push_child(Node::text("foo"));
        wrap_text_children_of_node(&mut li, "li");
        // Not wrapped (still a bare text child).
        assert!(matches!(li.children[0].kind, NodeKind::Text(_)));
    }

    #[test]
    fn test_wrap_skips_literal_html() {
        let mut li = li_with_dsr(0, 5);
        li.dp.as_mut().unwrap().stx = Some("html".to_string());
        li.push_child(Node::text("foo"));
        wrap_text_children_of_node(&mut li, "li");
        assert!(matches!(li.children[0].kind, NodeKind::Text(_)));
    }

    #[test]
    fn test_wrap_trailing_newline_split() {
        let mut li = li_with_dsr(0, 8); // "*foo\n" -> "foo\n" content
        li.push_child(Node::text("foo\n"));
        wrap_text_children_of_node(&mut li, "li");

        // "foo" wrapped in a span, "\n" left as a trailing text node.
        assert_eq!(li.children.len(), 2);
        assert!(li.children[0].get_attr("data-mw-selser-wrapper").is_some());
        assert!(matches!(&li.children[1].kind, NodeKind::Text(t) if t == "\n"));
    }

    #[test]
    fn test_no_dsr_no_wrap() {
        let mut li = Node::element(ElementKind::ListItem);
        li.push_child(Node::text("foo"));
        wrap_text_children_of_node(&mut li, "li");
        // No DSR => skipped entirely.
        assert!(matches!(li.children[0].kind, NodeKind::Text(_)));
    }
}
