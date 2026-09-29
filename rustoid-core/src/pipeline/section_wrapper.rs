//! Section wrapping — DOM post-processor that wraps wikitext headings in
//! `<section data-mw-section-id="N">` elements, with an always-present lead
//! `<section data-mw-section-id="0">`.
//!
//! Faithful port of the core of PHP Parsoid's
//! `src/Wt2Html/DOM/Processors/WrapSectionsState.php`. The full PHP algorithm
//! also reconciles section boundaries with template/extension encapsulation
//! wrappers and computes TOC metadata; those responsibilities depend on the
//! DataParsoid DSR/Section/transclusion infrastructure that is layered in
//! separately. This module implements the heading-wrapping portion, which is
//! self-contained and directly observable in output HTML.

use crate::dom::node::{ElementKind, Node, NodeKind};

/// A currently-open section plus its heading level.
struct OpenSection {
    level: u8,
    node: Node,
}

/// Wrap the children of `body` (the `<body>` element produced by the tree
/// builder) in `<section>` wrappers, in place.
pub fn wrap_sections(body: &mut Node) {
    let mut state = SectionNumber(0);
    let children = std::mem::take(&mut body.children);
    body.children = wrap_level(&children, &mut state, true);
}

/// Running section id counter.
struct SectionNumber(usize);

impl SectionNumber {
    fn next(&mut self) -> usize {
        self.0 += 1;
        self.0
    }
}

fn new_section(id: &str) -> Node {
    let mut section = Node::element(ElementKind::Section);
    section.set_attr("data-mw-section-id", id);
    // A wrapper Parsoid created, so it has a `data-parsoid` slot even though the
    // slot is empty and nothing is emitted for it — and therefore it takes a node
    // id. The live service serves `<section data-mw-section-id="0" id="mwAQ">`,
    // which is the document's *first* id, ahead of every element inside it.
    // Without this the section is skipped and everything after it shifts by one.
    section.empty_dp_slot = true;
    section
}

/// Process a list of sibling nodes, wrapping headings in `<section>` elements.
///
/// Returns the new list of top-level siblings (sections and any unwrapped
/// non-heading content). `at_top` indicates this is the `<body>` level, where
/// the always-present lead section is created.
fn wrap_level(children: &[Node], counter: &mut SectionNumber, at_top: bool) -> Vec<Node> {
    // Open heading-sections, outermost first; `stack.last()` is the innermost,
    // the one that subsequent sibling content belongs to.
    let mut stack: Vec<OpenSection> = Vec::new();
    let mut out: Vec<Node> = Vec::new();
    // The lead section is created only at the top level, is always present, and
    // always comes first; content before the first heading belongs to it. It is
    // kept in the output but never on the stack: a heading opens a *new*
    // top-level section, it does not nest inside the lead.
    if at_top {
        out.push(new_section("0"));
    }

    for child in children {
        if let Some(level) = heading_level(child) {
            // A heading of this level ends every open section that cannot nest
            // it. Each closed section is attached to its parent, or emitted as a
            // top-level sibling; a section opened for an earlier heading and
            // then popped here must be *kept*, not dropped — on `Israel` the
            // `==References==` section nested `===Sources===`, and the following
            // `==External links==` popped it back off the stack.
            close_above(&mut stack, &mut out, level);
            let mut section = new_section(&counter.next().to_string());
            section.push_child(child.clone());
            stack.push(OpenSection {
                level,
                node: section,
            });
        } else {
            // Non-heading content belongs to the innermost open section, or
            // (before the first heading, at top level) the lead section at
            // `out[0]`, or — below the top level, where there is no lead — to the
            // output as-is.
            let transformed = transform_subtree(child, counter);
            if let Some(open) = stack.last_mut() {
                open.node.push_child(transformed);
            } else if at_top && let Some(lead) = out.first_mut() {
                // Before the first heading, at the top level, content belongs to
                // the always-present lead section at `out[0]`. Below the top
                // level there is no lead, so it is emitted as-is — appending it
                // to `out[0]` there would nest it inside whatever came first.
                lead.push_child(transformed);
            } else {
                out.push(transformed);
            }
        }
    }

    // Commit every still-open section, innermost first.
    close_above(&mut stack, &mut out, 0);
    out
}

/// Close every open section that cannot contain a heading of `level`: pop it,
/// attach it to its parent section if one is still open, else emit it as a
/// top-level sibling. `level == 0` closes them all (a heading level is never 0).
fn close_above(stack: &mut Vec<OpenSection>, out: &mut Vec<Node>, level: u8) {
    while stack.last().is_some_and(|s| s.level >= level) {
        let Some(section) = stack.pop() else {
            break;
        };
        match stack.last_mut() {
            Some(parent) => parent.node.push_child(section.node),
            None => out.push(section.node),
        }
    }
}

/// Recurse into a non-heading element's children, wrapping any nested headings.
fn transform_subtree(node: &Node, counter: &mut SectionNumber) -> Node {
    let mut cloned = node.clone();
    if matches!(cloned.kind, NodeKind::Element(_)) && !cloned.children.is_empty() {
        let children = std::mem::take(&mut cloned.children);
        cloned.children = wrap_level(&children, counter, false);
    }
    cloned
}

/// Whether `node` is a wikitext heading that should be wrapped in a section.
/// HTML headings (emitted from literal `<h2>` markup) are not wrapped; they are
/// identified by `stx:"html"` in their `data-parsoid`.
fn heading_level(node: &Node) -> Option<u8> {
    let NodeKind::Element(ElementKind::Heading(level)) = node.kind else {
        return None;
    };
    if let Some(dp) = &node.data_parsoid
        && dp.contains("\"stx\":\"html\"")
    {
        return None;
    }
    Some(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heading(level: u8) -> Node {
        Node::element(ElementKind::Heading(level))
    }

    fn text(s: &str) -> Node {
        Node::text(s)
    }

    fn para() -> Node {
        Node::element(ElementKind::Paragraph)
    }

    /// Collect the `data-mw-section-id` values of direct section children.
    fn section_ids(node: &Node) -> Vec<String> {
        node.children
            .iter()
            .filter(|c| matches!(c.kind, NodeKind::Element(ElementKind::Section)))
            .map(|c| c.get_attr("data-mw-section-id").unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn test_lead_section_always_present() {
        let mut body = Node::element(ElementKind::Other("body".to_string()));
        body.push_child(para());
        wrap_sections(&mut body);
        assert_eq!(section_ids(&body), vec!["0".to_string()]);
    }

    #[test]
    fn test_single_heading() {
        let mut body = Node::element(ElementKind::Other("body".to_string()));
        body.push_child(text("lead"));
        body.push_child(heading(2));
        wrap_sections(&mut body);
        assert_eq!(section_ids(&body), vec!["0", "1"]);
    }

    #[test]
    fn test_nested_headings() {
        let mut body = Node::element(ElementKind::Other("body".to_string()));
        body.push_child(heading(2));
        body.push_child(text("a"));
        body.push_child(heading(3));
        body.push_child(text("b"));
        wrap_sections(&mut body);
        // Top-level sections: 0 (lead), 1 (the h2). The h3 nested inside 1.
        assert_eq!(section_ids(&body), vec!["0", "1"]);
        let sec1 = &body.children[1];
        assert_eq!(section_ids(sec1), vec!["2".to_string()]);
    }

    #[test]
    fn test_sibling_headings() {
        let mut body = Node::element(ElementKind::Other("body".to_string()));
        body.push_child(heading(2));
        body.push_child(text("a"));
        body.push_child(heading(2));
        body.push_child(text("b"));
        wrap_sections(&mut body);
        // Lead (0), h2 (1), h2 (2) as siblings.
        assert_eq!(section_ids(&body), vec!["0", "1", "2"]);
    }

    #[test]
    fn test_section_nesting_a_deeper_heading_survives_a_later_sibling() {
        // h2 (References) nests h3 (Sources); the following h2 (External links)
        // ends both. The References section must remain, as must Sources inside
        // it — an earlier version dropped a section that had been pushed onto the
        // nesting stack and was then popped, losing the whole References section
        // of `Israel`.
        let mut body = Node::element(ElementKind::Other("body".to_string()));
        body.push_child(heading(2)); // Notes
        body.push_child(text("n"));
        body.push_child(heading(2)); // References
        body.push_child(text("r"));
        body.push_child(heading(3)); // Sources
        body.push_child(text("s"));
        body.push_child(heading(2)); // External links
        body.push_child(text("e"));
        wrap_sections(&mut body);

        // Lead (0), Notes (1), References (2), External links (4); Sources is
        // numbered 3 in document order but nested inside References.
        assert_eq!(section_ids(&body), vec!["0", "1", "2", "4"]);
        let references = &body.children[2];
        assert_eq!(section_ids(references), vec!["3".to_string()]);
    }
}
