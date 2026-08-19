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
    section
}

/// Process a list of sibling nodes, wrapping headings in `<section>` elements.
///
/// Returns the new list of top-level siblings (sections and any unwrapped
/// non-heading content). `at_top` indicates this is the `<body>` level, where
/// the always-present lead section is created.
fn wrap_level(children: &[Node], counter: &mut SectionNumber, at_top: bool) -> Vec<Node> {
    // Stack of open parent heading-sections. `stack.last()` is the innermost
    // enclosing section, and `current` is the most-recently-opened section,
    // into which subsequent sibling content is appended.
    let mut stack: Vec<OpenSection> = Vec::new();
    let mut current: Option<OpenSection> = None;

    let mut out: Vec<Node> = Vec::new();
    // The lead section is created only at the top level, is always present,
    // and always comes first; content before the first heading belongs to it.
    if at_top {
        out.push(new_section("0"));
    }

    for child in children {
        if let Some(level) = heading_level(child) {
            // Pop parent sections that cannot nest this level.
            while stack.last().is_some_and(|s| level <= s.level) {
                stack.pop();
            }

            // If the currently-open heading section can nest this level, it
            // becomes a parent; otherwise it is now closed and committed to the
            // output as a sibling.
            match current.take() {
                Some(cur) if level > cur.level => {
                    stack.push(cur);
                }
                Some(cur) => {
                    out.push(cur.node);
                }
                None => {}
            }

            // Open the new heading section.
            let mut section = new_section(&counter.next().to_string());
            section.push_child(child.clone());
            current = Some(OpenSection {
                level,
                node: section,
            });
        } else {
            // Content that is not a heading belongs to the innermost open
            // section, or (before the first heading, at top level) the lead
            // section held at out[0].
            let transformed = transform_subtree(child, counter);
            if current.is_none() && at_top {
                if let Some(lead) = out.first_mut() {
                    lead.push_child(transformed);
                }
            } else if let Some(cur) = current.as_mut() {
                cur.node.push_child(transformed);
            } else {
                out.push(transformed);
            }
        }
    }

    // Commit any still-open section (nesting back into its parents).
    if let Some(leaf) = current.take() {
        attach_sections(leaf, &mut stack, &mut out);
    } else {
        while let Some(parent) = stack.pop() {
            out.insert(0, parent.node);
        }
    }

    out
}

/// Attach `leaf` (a still-open section) into its parent section, recursively
/// walking back up the stack of opened sections. Each popped parent is pushed
/// into its own parent (or `out`), then the leaf is pushed as the last child of
/// the nearest parent.
fn attach_sections(leaf: OpenSection, stack: &mut Vec<OpenSection>, out: &mut Vec<Node>) {
    let mut child = leaf.node;

    // Walk up the stack, making each parent contain its child.
    while let Some(mut parent) = stack.pop() {
        parent.node.push_child(child);
        child = parent.node;
    }

    // The outermost opened section becomes a top-level sibling.
    out.push(child);
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
}
