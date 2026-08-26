//! ComputeDSR — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Processors/ComputeDSR.php`.
//!
//! DSR ("DOM Source Range") records, for each element, the source offset range
//! of the original wikitext that generated it, plus the widths of its opening
//! and closing tags. It is derived bottom-up (last child → first child) from
//! each token's TSR ("Tag Source Range") and the statically-known tag widths in
//! `Consts::$WtTagWidths`.
//!
//! The pass reads the structured token `DataParsoid` carried on each `Node`
//! (`Node::dp`, preserving `tsr`/`src`/`tmp.end_tsr`/`fostered`/etc.) and
//! writes the computed `dsr` back into `dp.dsr`, which is later serialized into
//! `data-parsoid`. The html2wt serializer's `getOrigSrc`/`buildSep` read this to
//! recover exact source.
//!
//! Only the top-level page runs this pass (never template content).

use crate::dom::node::{Node, NodeKind};
use crate::wikitext::consts;
use crate::wikitext::tokens_v2::{DataParsoid, DomSourceRange};

const WT_TAGS_WITH_LIMITED_TSR: &[&str] = &[
    "b", "i", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "dl", "li", "dt", "dd", "table",
    "caption", "tr", "td", "th", "hr", "br", "pre",
];

fn node_name(node: &Node) -> String {
    crate::html::wts_utils::node_name(node)
}

fn is_wt_tag_with_limited_tsr(name: &str) -> bool {
    WT_TAGS_WITH_LIMITED_TSR.contains(&name)
}

fn has_type_of(node: &Node, ty: &str) -> bool {
    node.get_attr("typeof")
        .is_some_and(|v| v.split_whitespace().any(|t| t == ty))
}

fn match_placeholder(node: &Node) -> bool {
    node.get_attr("typeof").is_some_and(|v| {
        v.split_whitespace()
            .any(|t| t == "mw:Placeholder" || t.starts_with("mw:Placeholder/"))
    })
}

fn is_placeholder_or_lang_variant(node: &Node) -> bool {
    has_type_of(node, "mw:Placeholder") || has_type_of(node, "mw:LanguageVariant")
}

fn has_literal_html_marker(dp: &DataParsoid) -> bool {
    dp.stx.as_deref() == Some("html")
}

fn tsr_spans_tag_dom(node: &Node, dp: &DataParsoid) -> bool {
    !(is_wt_tag_with_limited_tsr(&node_name(node))
        || is_placeholder_or_lang_variant(node)
        || has_literal_html_marker(dp))
}

fn is_quote_elt(node: &Node) -> bool {
    crate::html::wts_utils::is_quote_elt(node)
}

fn is_a_tag_from_wiki_link_syntax(node: &Node) -> bool {
    node.get_attr("rel") == Some("mw:WikiLink")
}

fn is_a_tag_from_ext_link_syntax(node: &Node) -> bool {
    node.get_attr("rel") == Some("mw:ExtLink")
}

fn is_a_tag_from_url_or_magic_syntax(node: &Node) -> bool {
    node.get_attr("rel").is_some_and(|rel| {
        matches!(rel, "mw:ExtLink" | "mw:WikiLink")
            && node.dp.as_ref().is_some_and(|dp| {
                dp.stx.as_deref() == Some("url") || dp.stx.as_deref() == Some("magiclink")
            })
    })
}

fn has_expanded_attrs_type(node: &Node) -> bool {
    has_type_of(node, "mw:ExpandedAttrs")
}

fn compute_a_tag_width(node: &Node, dp: &DataParsoid) -> Option<(Option<usize>, Option<usize>)> {
    if is_a_tag_from_wiki_link_syntax(node) && !has_expanded_attrs_type(node) {
        if dp.stx.as_deref() == Some("piped") {
            let pipe_len = dp.first_pipe_src.as_deref().unwrap_or("|").len();
            let href = dp
                .sa
                .as_ref()
                .and_then(|sa| sa.get("href"))
                .cloned()
                .unwrap_or_default();
            Some((Some(2 + href.len() + pipe_len), Some(2)))
        } else {
            Some((Some(2), Some(2)))
        }
    } else if dp.tsr.is_some() && is_a_tag_from_ext_link_syntax(node) {
        let content_start = dp
            .tmp
            .ext_link_content_offsets
            .as_ref()
            .map(|sr| sr.start)
            .unwrap_or(0);
        let start = dp.tsr.as_ref().map(|sr| sr.start).unwrap_or(0);
        Some((Some(content_start.saturating_sub(start)), Some(1)))
    } else if is_a_tag_from_url_or_magic_syntax(node) {
        Some((Some(0), Some(0)))
    } else {
        None
    }
}

fn compute_tag_widths(
    mut st_width: Option<usize>,
    mut et_width: Option<usize>,
    node: &Node,
    dp: &DataParsoid,
) -> (Option<usize>, Option<usize>) {
    if let Some(offs) = &dp.ext_tag_offsets {
        return (offs.open_width, offs.close_width);
    }

    if has_literal_html_marker(dp) {
        if dp.self_close == Some(true) {
            et_width = Some(0);
        }
    } else if has_type_of(node, "mw:LanguageVariant") {
        st_width = Some(2);
        et_width = Some(2);
    } else {
        let name = node_name(node);
        if name == "tr" && dp.start_tag_src.is_none() {
            st_width = Some(0);
            et_width = Some(0);
        } else {
            let wt_tag_width = consts::wt_tag_widths(&name);
            if st_width.is_none() {
                if name == "a"
                    && let Some((a_st, _)) = compute_a_tag_width(node, dp)
                {
                    st_width = a_st;
                }
                if st_width.is_none()
                    && let Some((static_st, _)) = wt_tag_width
                {
                    st_width = static_st;
                }
            }
            if et_width.is_none()
                && let Some((_, static_et)) = wt_tag_width
            {
                et_width = static_et;
            }
        }
    }

    (st_width, et_width)
}

/// The RTL DSR recursion, faithful to `ComputeDSR::computeNodeDSR`.
///
/// Processes `node`'s children from last to first, writing `dp.dsr` on each
/// element, and returns `(cs, e)` — the child-start and (possibly extended) end
/// offsets for the caller to merge.
fn compute_node_dsr(
    node: &mut Node,
    s: Option<usize>,
    mut e: Option<usize>,
    mut dsr_correction: usize,
) -> (Option<usize>, Option<usize>) {
    if e.is_none() && node.children.is_empty() {
        e = s;
    }

    let mut ce = e;
    let mut cs = ce;

    let mut i = node.children.len();
    while i > 0 {
        i -= 1;
        let orig_ce = ce;

        // Snapshot the next (already-processed, to-the-right) sibling's
        // stripped-tag absorption info before mutably borrowing `node.children[i]`.
        let absorbed = if i + 1 < node.children.len() {
            let next = &node.children[i + 1];
            let next_name = node_name(next);
            if let Some(ndp) = next.dp.as_ref()
                && ndp.src.is_some()
                && has_type_of(next, "mw:Placeholder/StrippedTag")
                && consts::wt_quote_tags().contains(&ndp.name.clone().unwrap_or_default())
                && consts::wt_quote_tags().contains(&next_name)
            {
                ndp.src.as_ref().map(|s| s.len())
            } else {
                None
            }
        } else {
            None
        };

        // Now process this child.
        let child = &mut node.children[i];

        // $endTSR → $ce.
        if let NodeKind::Element(_) = child.kind
            && let Some(end_tsr_end) = child
                .dp
                .as_ref()
                .and_then(|d| d.tmp.end_tsr.as_ref())
                .map(|tsr| tsr.end)
        {
            ce = Some(end_tsr_end);
        }

        // Stripped-tag absorption (b/i quotes): the next sibling is a stripped
        // quote placeholder, and this child is also a quote element.
        if let (Some(correction), true) = (absorbed, is_quote_elt(child)) {
            ce = ce.map(|c| c + correction);
            dsr_correction = correction;
        }

        let mut fostered = false;
        match child.kind {
            NodeKind::Text(ref t) => {
                cs = ce.map(|c| c.saturating_sub(t.len()));
            }
            NodeKind::Comment(ref c) => {
                // `<!--` (4) + body + `-->` (3); wikitext-decoding preserves the
                // body length for our encoded comments.
                cs = ce.map(|cc| cc.saturating_sub(c.len() + 7));
            }
            NodeKind::Document => {}
            NodeKind::Element(_) => {
                let dp = child.dp.clone().unwrap_or_default();
                let tsr = dp.tsr.clone();

                let mut st_width: Option<usize> = None;
                let mut et_width: Option<usize> = None;

                // AutoInsertedEnd correction for quote elements.
                if ce.is_some() && dp.auto_inserted_end && is_quote_elt(child) {
                    let correction = 3 + node_name(child).len();
                    if correction == dsr_correction {
                        ce = ce.map(|c| c.saturating_sub(correction));
                        dsr_correction = 0;
                    }
                }

                let is_meta = node_name(child) == "meta";
                if is_meta {
                    if let Some(tsr) = &tsr {
                        // Meta-marker tags (templates/extensions) and other
                        // meta-tags alike reset cs/ce to the (top-level) tsr.
                        cs = Some(tsr.start);
                        ce = Some(tsr.end);
                    } else if has_type_of(child, "mw:IndentPreWS") {
                        cs = ce.map(|c| c.saturating_sub(1));
                    } else if match_placeholder(child) && ce.is_some() && dp.src.is_some() {
                        cs = ce.map(|c| c.saturating_sub(dp.src.as_ref().unwrap().len()));
                    }
                    if let Some(offs) = &dp.ext_tag_offsets {
                        st_width = offs.open_width;
                        et_width = offs.close_width;
                    }
                } else if has_type_of(child, "mw:Entity") && ce.is_some() && dp.src.is_some() {
                    cs = ce.map(|c| c.saturating_sub(dp.src.as_ref().unwrap().len()));
                } else {
                    if match_placeholder(child) && ce.is_some() && dp.src.is_some() {
                        cs = ce.map(|c| c.saturating_sub(dp.src.as_ref().unwrap().len()));
                    } else {
                        if let Some(end_tsr) =
                            child.dp.as_ref().and_then(|d| d.tmp.end_tsr.as_ref())
                        {
                            et_width = Some(end_tsr.length());
                        }
                        if let Some(tsr) = &tsr {
                            if !dp.auto_inserted_start {
                                cs = Some(tsr.start);
                                if tsr_spans_tag_dom(child, &dp) {
                                    if tsr.end > 0 {
                                        ce = Some(tsr.end);
                                    }
                                } else {
                                    st_width = Some(tsr.end.saturating_sub(tsr.start));
                                }
                            }
                        } else if s.is_some() {
                            cs = s;
                        }

                        (st_width, et_width) = compute_tag_widths(st_width, et_width, child, &dp);

                        if dp.auto_inserted_start {
                            st_width = Some(0);
                        }
                        if dp.auto_inserted_end {
                            et_width = Some(0);
                        }

                        let ccs = cs.zip(st_width).map(|(c, w)| c + w);
                        let cce = ce.zip(et_width).map(|(c, w)| c.saturating_sub(w));

                        let is_fragment_wrapper = dp.dom_fragment_src.is_some();
                        let is_nonpiped_wikilink = is_a_tag_from_wiki_link_syntax(child)
                            && dp.stx.as_deref() != Some("piped");

                        let new_dsr = if is_fragment_wrapper
                            || has_type_of(child, "mw:LanguageVariant")
                            || is_nonpiped_wikilink
                        {
                            (ccs, cce)
                        } else {
                            compute_node_dsr(child, ccs, cce, dsr_correction)
                        };

                        if let (Some(sw), Some(new_start)) = (st_width, new_dsr.0) {
                            let new_cs = new_start.saturating_sub(sw);
                            if cs.is_none() || (tsr.is_none() && new_cs < cs.unwrap()) {
                                cs = Some(new_cs);
                            }
                        }
                        if let (Some(ew), Some(new_end)) = (et_width, new_dsr.1) {
                            let new_ce = new_end + ew;
                            if new_ce > ce.unwrap_or(0) {
                                ce = Some(new_ce);
                            }
                        }
                    }
                }

                fostered = dp.fostered;

                if cs.is_some() || ce.is_some() {
                    let dsr = if fostered {
                        let o = orig_ce.unwrap_or(0);
                        DomSourceRange {
                            start: Some(o),
                            end: Some(o),
                            open_width: None,
                            close_width: None,
                        }
                    } else {
                        DomSourceRange {
                            start: cs,
                            end: ce,
                            open_width: st_width,
                            close_width: et_width,
                        }
                    };
                    child.dp.get_or_insert_with(DataParsoid::default).dsr = Some(dsr);
                }
            }
        }

        if fostered {
            ce = orig_ce;
        } else {
            ce = cs;
        }
    }

    if cs.is_none() {
        cs = s;
    }

    (cs, e)
}

/// Compute DSR for every node of a document subtree, writing `dp.dsr` on each
/// element. Faithful to `ComputeDSR::run` (top-level page only).
///
/// NOTE: the forward sibling-start/end propagation pass (`propagateRight` in PHP)
/// is not yet ported; it requires a second left-to-right traversal and is only
/// relevant when a child's `ce` shifts left/right relative to a right sibling.
pub fn run(root: &mut Node, source: &str) {
    let end = source.len();
    compute_node_dsr(root, Some(0), Some(end), 0);

    if let NodeKind::Element(_) = root.kind {
        let dsr = DomSourceRange {
            start: Some(0),
            end: Some(end),
            open_width: Some(0),
            close_width: Some(0),
        };
        root.dp.get_or_insert_with(DataParsoid::default).dsr = Some(dsr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};
    use crate::wikitext::tokens_v2::{DataParsoid, SourceRange};

    fn dtp_with_tsr(start: usize, end: usize) -> DataParsoid {
        DataParsoid {
            tsr: Some(SourceRange::new(start, end)),
            ..Default::default()
        }
    }

    #[test]
    fn test_simple_text_paragraph_dsr() {
        // "hello" → <p>hello</p> with a <p> of open/close width 0.
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.dp = Some(dtp_with_tsr(0, 5));
        p.children.push(Node::text("hello"));
        doc.children.push(p);

        run(&mut doc, "hello");

        let p = &doc.children[0];
        let dsr = p.dp.as_ref().unwrap().dsr.as_ref().unwrap();
        assert_eq!(dsr.start, Some(0));
        assert_eq!(dsr.end, Some(5));
        assert_eq!(dsr.open_width, Some(0));
        assert_eq!(dsr.close_width, Some(0));
    }

    #[test]
    fn test_bold_quote_dsr() {
        // "'''bold'''" → <b>bold</b> with open/close width 3.
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.dp = Some(dtp_with_tsr(0, 11));
        let mut b = Node::element(ElementKind::Bold);
        b.dp = Some(dtp_with_tsr(0, 3));
        b.children.push(Node::text("bold"));
        p.children.push(b);
        doc.children.push(p);

        run(&mut doc, "'''bold'''");

        let b = &doc.children[0].children[0];
        let dsr = b.dp.as_ref().unwrap().dsr.as_ref().unwrap();
        assert_eq!(dsr.start, Some(0));
        assert_eq!(dsr.end, Some(11));
        assert_eq!(dsr.open_width, Some(3));
        assert_eq!(dsr.close_width, Some(3));
    }
}
