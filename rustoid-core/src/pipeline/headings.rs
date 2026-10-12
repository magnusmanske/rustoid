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

/// Make ids unique across the document, mirroring PHP's `dedupeHeadingIds`.
///
/// Two headings can carry the same text — `Order of the British Empire` has two
/// "Sovereign and Grand Master" sections — and `genAnchors` would give both the
/// same id. The walk registers every `id` it meets (lower-cased) in document
/// order, and a heading — or the `mw:FallbackId` span inside one — whose id is
/// already taken is suffixed `_2`, `_3`, …. The case folding and the suffix
/// search are kept verbatim from PHP, because they decide fragment URLs that
/// exist outside the wiki.
pub fn dedupe_heading_ids(ast: &mut Node) {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for child in &mut ast.children {
        dedupe_rec(child, &mut seen);
    }
}

fn dedupe_rec(node: &mut Node, seen: &mut std::collections::HashMap<String, usize>) {
    if let Some(orig) = node.get_attr("id").map(str::to_string) {
        let key = orig.to_lowercase();
        if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(key.clone()) {
            e.insert(1);
        } else {
            // Only headings and legacy fallback spans are renamed; any other
            // element that reuses an id is left as it is (`IE 7` uniqueness is
            // only enforced for headings, so that external links keep working).
            let heading = matches!(node.kind, NodeKind::Element(ElementKind::Heading(_)));
            let fallback = node
                .get_attr("typeof")
                .is_some_and(|t| t.split_whitespace().any(|x| x == "mw:FallbackId"));
            if heading || fallback {
                let current = seen.get(&key).copied().unwrap_or(0);
                let mut suffix = current + 1;
                seen.insert(key.clone(), suffix);
                while seen.contains_key(&format!("{key}_{suffix}")) {
                    suffix += 1;
                    seen.insert(key.clone(), suffix);
                }
                node.set_attr("id", format!("{orig}_{suffix}"));
                seen.insert(format!("{key}_{suffix}"), 1);
            }
        }
    }
    for child in &mut node.children {
        dedupe_rec(child, seen);
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
        node.set_attr("id", anchor_id.clone());
    }

    // When the primary (html5) id differs from the legacy fallback — the text
    // holds a character html5 keeps but legacy escapes, such as `'` or `=` —
    // Parsoid inserts an empty `<span typeof="mw:FallbackId" id="…">` as the
    // heading's first child, so old fragment links still resolve. Mirrors
    // `Headings::genAnchors`'s `$fallbackId` handling.
    let fallback_id = crate::sanitizer::escape_id_for_attribute_fallback(&normalized);
    if fallback_id.is_empty() || fallback_id == anchor_id {
        return;
    }
    let mut span = Node::element(ElementKind::Span);
    span.set_attr("id", fallback_id);
    span.set_attr("typeof", "mw:FallbackId");
    // A zero-width `dsr` at the heading's inner start, so the span round-trips
    // to the empty source range Parsoid gives it.
    if let Some(dsr) = node.dp.as_ref().and_then(|dp| dp.dsr.as_ref()) {
        let offset = dsr.inner_start();
        span.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(offset),
                end: Some(offset),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    node.children.insert(0, span);
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

    #[test]
    fn test_duplicate_heading_ids_are_suffixed() {
        let mut ast = Node::document();
        ast.push_child(heading("Foo"));
        ast.push_child(heading("Foo"));
        ast.push_child(heading("Foo"));
        gen_anchors(&mut ast);
        dedupe_heading_ids(&mut ast);
        let ids: Vec<_> = ast
            .children
            .iter()
            .map(|h| h.get_attr("id").map(str::to_string))
            .collect();
        assert_eq!(
            ids,
            vec![
                Some("Foo".to_string()),
                Some("Foo_2".to_string()),
                Some("Foo_3".to_string())
            ]
        );
    }

    #[test]
    fn test_duplicate_heading_ids_skip_a_taken_suffix() {
        // A `Foo_2` id elsewhere in the document pushes the heading's suffix to
        // `_3`, which is why the suffix search has to consult every id, not just
        // the heading counter.
        let mut taken = Node::element(ElementKind::Span);
        taken.set_attr("id", "Foo_2");
        let mut ast = Node::document();
        ast.push_child(heading("Foo"));
        ast.push_child(taken);
        ast.push_child(heading("Foo"));
        gen_anchors(&mut ast);
        dedupe_heading_ids(&mut ast);
        assert_eq!(ast.children[0].get_attr("id"), Some("Foo"));
        assert_eq!(ast.children[2].get_attr("id"), Some("Foo_3"));
    }
}
