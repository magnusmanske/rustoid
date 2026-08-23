//! Heading anchor (`id`) generation — faithful port of the core of PHP
//! Parsoid's `src/Wt2Html/DOM/Handlers/Headings.php` (`genAnchors` / the safe
//! heading transform).
//!
//! Wikitext headings (`== Hello world ==`) get an HTML `id` attribute derived
//! from their (stripped, whitespace-normalized) text content, so that section
//! fragment links (`#Hello_world`) continue to work. This runs regardless of
//! section wrapping.

use crate::dom::node::{ElementKind, Node, NodeKind};

/// Assign `id` attributes to wikitext headings, mirroring PHP's `genAnchors`.
pub fn gen_anchors(ast: &mut Node) {
    for child in &mut ast.children {
        gen_anchors_rec(child);
    }
}

fn gen_anchors_rec(node: &mut Node) {
    if let NodeKind::Element(ElementKind::Heading(_)) = node.kind {
        assign_heading_id(node);
    }
    for child in &mut node.children {
        gen_anchors_rec(child);
    }
}

/// Compute and set the `id` attribute on a heading element.
fn assign_heading_id(node: &mut Node) {
    // Skip HTML headings (emitted from literal `<h2>` markup), which already
    // carry the user's id (or none) and are not derived from text content.
    if let Some(dp) = &node.data_parsoid
        && dp.contains("\"stx\":\"html\"")
    {
        return;
    }
    // Do not overwrite an explicit id (mirrors PHP's early return).
    if node.get_attr("id").is_some() {
        return;
    }

    let text = text_content(node);
    let normalized = crate::sanitizer::normalize_section_name_whitespace(&text);
    let anchor_id = crate::sanitizer::escape_id_for_attribute(&normalized);
    if !anchor_id.is_empty() {
        node.set_attr("id", anchor_id);
    }
}

/// Concatenate the text content of `node`, ignoring tags (mirrors `textContent`).
fn text_content(node: &Node) -> String {
    let mut out = String::new();
    collect_text(node, &mut out);
    out
}

fn collect_text(node: &Node, out: &mut String) {
    match &node.kind {
        NodeKind::Text(t) => out.push_str(t),
        NodeKind::Element(_) | NodeKind::Document => {
            for child in &node.children {
                collect_text(child, out);
            }
        }
        NodeKind::Comment(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heading(text: &str) -> Node {
        let mut h = Node::element(ElementKind::Heading(2));
        h.push_child(Node::text(text));
        h
    }

    #[test]
    fn test_simple_heading_id() {
        let mut ast = Node::document();
        ast.push_child(heading("hi"));
        gen_anchors(&mut ast);
        let id = ast.children[0].get_attr("id").map(str::to_string);
        assert_eq!(id.as_deref(), Some("hi"));
    }

    #[test]
    fn test_heading_id_space_to_underscore() {
        let mut ast = Node::document();
        ast.push_child(heading("Hello world"));
        gen_anchors(&mut ast);
        let id = ast.children[0].get_attr("id").map(str::to_string);
        assert_eq!(id.as_deref(), Some("Hello_world"));
    }

    #[test]
    fn test_heading_skips_explicit_id() {
        let mut html_heading = Node::element(ElementKind::Heading(2));
        html_heading.set_attr("id", "already-set");
        let mut ast = Node::document();
        ast.push_child(html_heading);
        gen_anchors(&mut ast);
        assert_eq!(ast.children[0].get_attr("id"), Some("already-set"));
    }
}
