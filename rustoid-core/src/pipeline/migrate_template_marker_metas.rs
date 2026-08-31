//! MigrateTemplateMarkerMetas — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Processors/MigrateTemplateMarkerMetas.php`.
//!
//! This pass runs after p-wrapping and before `migrate-nls`. It uses simple
//! heuristics to move transclusion (`mw:Transclusion` / `mw:Param`) marker metas
//! toward a canonical position, so that the later `tplwrap` pass wraps the
//! narrowest sensible range. Specifically, when a start marker is the last
//! non-separator child (or an end marker is the first non-separator child) and
//! the two markers sit at different DOM depths, the deeper marker is migrated up
//! across a zero-width / auto-inserted tag barrier so both markers end up at the
//! same level.
//!
//! The `about → (start depth, end depth)` map is computed over the freshly-built
//! DOM (before DOM-level p-wrapping), mirroring PHP's
//! `transclusionMetaTagDepthMap` which is populated by `TreeMutationRelay` at
//! tree-build time.

use std::collections::HashMap;

use crate::dom::node::{Node, NodeKind};

fn node_name(node: &Node) -> String {
    crate::html::wts_utils::node_name(node)
}

/// `WTUtils::isTplStartMarkerMeta` — a `<meta>` whose `typeof` is `mw:Transclusion`
/// or `mw:Param`, *not* the `/End` form.
fn is_tpl_start_marker_meta(node: &Node) -> bool {
    node.get_attr("typeof").is_some_and(|ty| {
        ty.split_whitespace()
            .any(|t| (t == "mw:Transclusion" || t == "mw:Param") && !t.ends_with("/End"))
    })
}

/// `WTUtils::isTplEndMarkerMeta` — a `<meta>` whose `typeof` ends in `/End`.
fn is_tpl_end_marker_meta(node: &Node) -> bool {
    node.get_attr("typeof")
        .is_some_and(|ty| ty.split_whitespace().any(|t| t.ends_with("/End")))
}

/// Is this node any transclusion marker meta (start or end)?
fn is_tpl_marker_meta(node: &Node) -> bool {
    is_tpl_start_marker_meta(node) || is_tpl_end_marker_meta(node)
}

/// A separator node (whitespace text or comment), skipped when finding the
/// first/last content child. Mirrors `DiffDOMUtils::firstNonSepChild` /
/// `lastNonSepChild`.
fn is_separator(node: &Node) -> bool {
    match &node.kind {
        NodeKind::Comment(_) => true,
        NodeKind::Text(s) => s.trim().is_empty(),
        _ => false,
    }
}

fn has_literal_html_marker(node: &Node) -> bool {
    node.dp
        .as_ref()
        .is_some_and(|dp| dp.stx.as_deref() == Some("html"))
}

fn first_non_sep_index(children: &[Node]) -> Option<usize> {
    children.iter().position(|c| !is_separator(c))
}

fn last_non_sep_index(children: &[Node]) -> Option<usize> {
    children.iter().rposition(|c| !is_separator(c))
}

/// Compute the `about → (start depth, end depth)` map over a freshly-built DOM,
/// recording the depth of the last-seen start and end marker per `about`.
/// `depth` is the edge count from the document root (root = 0), matching
/// `DOMUtils::nodeDepth`.
pub fn collect_depths(root: &Node) -> HashMap<String, (usize, usize)> {
    let mut map = HashMap::new();
    collect_depths_impl(root, 0, &mut map);
    map
}

fn collect_depths_impl(node: &Node, depth: usize, map: &mut HashMap<String, (usize, usize)>) {
    if is_tpl_marker_meta(node)
        && let Some(about) = node.get_attr("about")
    {
        let entry = map.entry(about.to_string()).or_insert((0, 0));
        if is_tpl_end_marker_meta(node) {
            entry.1 = depth;
        } else {
            entry.0 = depth;
        }
    }
    for child in &node.children {
        collect_depths_impl(child, depth + 1, map);
    }
}

/// Run the migration pass over a document subtree, given the depth map.
pub fn run(root: &mut Node, depths: &HashMap<String, (usize, usize)>) {
    process_children(&mut root.children, depths);
}

/// Whether a node is at the top level (document or body fragment), where no
/// migration out should occur. Our fragment-mode tree builder produces `<html>`
/// as the structural child of the document; migration happens across element
/// boundaries *below* the document, mirroring PHP's `atTheTop` guard.
fn is_top_level(node: &Node) -> bool {
    matches!(node.kind, NodeKind::Document)
        || node_name(node) == "html"
        || node_name(node) == "body"
}

/// Recurse into `children`, migrating marker metas out of element children
/// across their own start/end barriers (into this same `children` list).
///
/// A child that is itself at the top level (`html`/`body`/document) is never
/// migrated out of — only its descendants are.
fn process_children(children: &mut Vec<Node>, depths: &HashMap<String, (usize, usize)>) {
    // Recurse into element children first (top-down), so nested migrations are
    // handled before we consider this level's migrations.
    for child in children.iter_mut() {
        if matches!(child.kind, NodeKind::Element(_) | NodeKind::Document) {
            process_children(&mut child.children, depths);
        }
    }

    let mut i = 0isize;
    while i >= 0 && (i as usize) < children.len() {
        let idx = i as usize;
        if !matches!(children[idx].kind, NodeKind::Element(_)) || is_top_level(&children[idx]) {
            i += 1;
            continue;
        }

        let fostered = children[idx].dp.as_ref().is_some_and(|dp| dp.fostered);

        // First-child migration.
        if let Some(first) = first_non_sep_index(&children[idx].children)
            && migrate_first_child(&children[idx].children[first], depths)
            && can_migrate_across_start(&children[idx])
        {
            let moved: Vec<Node> = children[idx].children.drain(0..=first).collect();
            let count = moved.len();
            let moved = mark_fostered(moved, fostered);
            // Insert the moved nodes before `children[idx]`.
            for m in moved.into_iter() {
                children.insert(idx, m);
            }
            i += count as isize;
        }

        // Last-child migration.
        let idx = i as usize;
        if idx >= children.len() {
            break;
        }
        if let Some(last) = last_non_sep_index(&children[idx].children)
            && migrate_last_child(&children[idx].children[last], depths)
            && can_migrate_across_end(&children[idx])
        {
            let moved: Vec<Node> = children[idx].children.drain(last..).collect();
            let moved = mark_fostered(moved, fostered);
            let insert_at = idx + 1;
            for m in moved.into_iter() {
                children.insert(insert_at, m);
            }
        }

        i += 1;
    }
}

fn mark_fostered(mut nodes: Vec<Node>, fostered: bool) -> Vec<Node> {
    if fostered {
        for node in &mut nodes {
            let dp = node.dp.get_or_insert_with(Default::default);
            dp.fostered = true;
        }
    }
    nodes
}

/// `migrateFirstChild(node, depths)` — should the first non-separator child
/// (a marker meta) migrate up out of `node`?
fn migrate_first_child(first_child: &Node, depths: &HashMap<String, (usize, usize)>) -> bool {
    if is_tpl_end_marker_meta(first_child) {
        return true;
    }
    if is_tpl_start_marker_meta(first_child)
        && let Some(about) = first_child.get_attr("about")
        && let Some(&(start, end)) = depths.get(about)
    {
        return start > end;
    }
    false
}

/// `migrateLastChild(node, depths)` — should the last non-separator child
/// (a marker meta) migrate up out of `node`?
fn migrate_last_child(last_child: &Node, depths: &HashMap<String, (usize, usize)>) -> bool {
    if is_tpl_start_marker_meta(last_child) {
        return true;
    }
    if is_tpl_end_marker_meta(last_child)
        && let Some(about) = last_child.get_attr("about")
        && let Some(&(start, end)) = depths.get(about)
    {
        return start < end;
    }
    false
}

/// Whether the start-tag barrier of `node` is zero-width or auto-inserted, so a
/// first-child marker can migrate up across it.
fn can_migrate_across_start(node: &Node) -> bool {
    let name = node_name(node);
    if let Some((st, _)) = crate::wikitext::consts::wt_tag_widths(&name)
        && st == Some(0)
        && !has_literal_html_marker(node)
    {
        return true;
    }
    node.dp.as_ref().is_some_and(|dp| dp.auto_inserted_start)
}

/// Whether the end-tag barrier of `node` is zero-width or auto-inserted, so a
/// last-child marker can migrate up across it.
fn can_migrate_across_end(node: &Node) -> bool {
    let name = node_name(node);
    if let Some((_, et)) = crate::wikitext::consts::wt_tag_widths(&name)
        && et == Some(0)
        && !has_literal_html_marker(node)
    {
        return true;
    }
    node.dp
        .as_ref()
        .is_some_and(|dp| dp.auto_inserted_end && name != "table")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::ElementKind;

    fn meta(ty: &str, about: &str) -> Node {
        let mut m = Node::element(ElementKind::Other("meta".to_string()));
        m.set_attr("typeof", ty);
        m.set_attr("about", about);
        m
    }

    #[test]
    fn test_marker_predicates() {
        let start = meta("mw:Transclusion", "#mwt1");
        assert!(is_tpl_start_marker_meta(&start));
        assert!(!is_tpl_end_marker_meta(&start));

        let end = meta("mw:Transclusion/End", "#mwt1");
        assert!(is_tpl_end_marker_meta(&end));
        assert!(!is_tpl_start_marker_meta(&end));
    }

    #[test]
    fn test_is_separator() {
        assert!(is_separator(&Node::text("  \n ")));
        assert!(!is_separator(&Node::text("x")));
        assert!(is_separator(&Node::comment("c")));
    }

    #[test]
    fn test_migrates_end_marker_out_of_p() {
        // A `<p>` ending in an end marker whose start marker is shallower in the
        // DOM migrates the end marker out (after the `<p>`). The `<p>` has
        // autoInsertedEnd, so migration across its end barrier is allowed.
        let mut p = Node::element(ElementKind::Paragraph);
        {
            let dp = p.dp.get_or_insert_with(Default::default);
            dp.auto_inserted_end = true;
        }
        p.push_child(Node::text("x"));
        p.push_child(meta("mw:Transclusion/End", "#mwt1"));

        let mut body = Node::element(ElementKind::Other("body".to_string()));
        // start meta shallow (depth 2), end meta deeper (depth 3).
        let start = meta("mw:Transclusion", "#mwt1");
        body.push_child(start);
        body.push_child(p);

        let mut doc = Node::document();
        doc.push_child(body);

        let mut depths = HashMap::new();
        // start at depth 2 (doc->body->start), end at depth 3 (doc->body->p->end).
        depths.insert("#mwt1".to_string(), (2, 3));

        run(&mut doc, &depths);

        // The end marker moved out of <p> to after it (as a body child).
        let body = &doc.children[0];
        assert_eq!(body.children.len(), 3, "{doc:?}");
        assert!(is_tpl_start_marker_meta(&body.children[0]));
        // body.children[1] is <p> with only text "x".
        assert!(matches!(
            &body.children[1].kind,
            NodeKind::Element(ElementKind::Paragraph)
        ));
        assert_eq!(body.children[1].children.len(), 1, "{doc:?}");
        assert!(is_tpl_end_marker_meta(&body.children[2]));
    }
}
