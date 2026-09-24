//! `DedupeStyles` — faithful port of Parsoid's
//! `src/Wt2Html/DOM/Handlers/DedupeStyles.php`.
//!
//! A `<templatestyles>` tag inlines its stylesheet, and the same stylesheet is
//! commonly requested by many templates on one page. The live service emits each
//! unique stylesheet **once**, as a `<style data-mw-deduplicate="TemplateStyles:r…">`,
//! and replaces every later request for the same key with a
//! `<link rel="mw-deduplicated-inline-style" href="mw-data:TemplateStyles:r…">`.
//! So the CSS is shipped once while every occurrence still records which
//! stylesheet it wanted.
//!
//! The pass runs as part of the `fixups` traverser — `MigrateTrailingCategories,
//! TableFixups,DedupeStyles` — i.e. after `TableFixups` and after extension
//! post-processing, and only on the top-level document.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::html::dom_utils::is_fosterable_position_element;
use crate::html::wts_utils::node_name;
use std::collections::HashSet;

/// Run the pass over the document rooted at `root`.
///
/// `seen` is the document-scoped set of keys already emitted (PHP's
/// `$env->styleTagKeys`).
pub fn run(root: &mut Node) {
    let mut seen: HashSet<String> = HashSet::new();
    visit(root, &mut seen);
}

/// Pre-order walk, mirroring `DOMTraverser`'s dispatch for the `style` handler.
///
/// The handler is registered for node name `style`, so only those nodes are
/// inspected; every node is still descended into. A node that is replaced is not
/// descended into, which is what the traverser's "handler returned a node"
/// semantics amount to here (the replacement is a childless `<link>`).
fn visit(node: &mut Node, seen: &mut HashSet<String>) {
    let mut i = 0;
    while i < node.children.len() {
        // A style node whose key is already in `seen` is a duplicate; `insert`
        // records a first sighting and reports whether this was one.
        let is_duplicate = match &node.children[i].kind {
            NodeKind::Element(_) if node_name(&node.children[i]) == "style" => node.children[i]
                .get_attr("data-mw-deduplicate")
                .is_some_and(|key| !seen.insert(key.to_string())),
            _ => false,
        };
        if is_duplicate {
            if is_fosterable_position_element(node) {
                // Inside a table structure a sibling `<link>` cannot be inserted
                // without disturbing the table, so the style's text is dropped
                // instead (the CSS itself was already emitted with the first
                // occurrence).
                node.children[i].children.clear();
            } else {
                let style = std::mem::replace(&mut node.children[i], Node::text(""));
                node.children[i] = deduplicated_link(&style);
            }
        } else {
            visit(&mut node.children[i], seen);
        }
        i += 1;
    }
}

/// Build the `<link>` that stands in for a duplicate `<style>`.
///
/// The metadata is copied from the source node rather than reconstructed: the
/// first occurrence's `data-mw` carries `"body":{"extsrc":""}` while later ones
/// may omit it, so re-serialising it here would drop or invent a field.
///
/// Attribute order (`rel`, `href`, `about`, `typeof`, then `data-mw`) is the
/// order the live service serves and is reproduced exactly.
fn deduplicated_link(style: &Node) -> Node {
    let mut link = Node::element(ElementKind::Other("link".to_string()));
    link.set_attr("rel", "mw-deduplicated-inline-style");
    link.set_attr(
        "href",
        format!(
            "mw-data:{}",
            style.get_attr("data-mw-deduplicate").unwrap_or("")
        ),
    );
    link.set_attr("about", style.get_attr("about").unwrap_or(""));
    link.set_attr("typeof", style.get_attr("typeof").unwrap_or(""));
    // A templatestyles node carries its `data-mw` as a plain attribute: it is
    // inserted through a DOM-fragment placeholder, not the token-stash path that
    // moves `data-mw` into the `data_mw` field, so the field is usually empty.
    // Copy whichever form the source has, last, so it lands after `typeof` — the
    // position the live service uses.
    if let Some(dmw) = style.get_attr("data-mw") {
        link.set_attr("data-mw", dmw);
    } else {
        link.data_mw = style.data_mw.clone();
    }
    link.dp = style.dp.clone();
    link
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(key: &str, about: &str, css: &str) -> Node {
        let mut n = Node::element(ElementKind::Other("style".to_string()));
        n.set_attr("data-mw-deduplicate", key);
        n.set_attr("typeof", "mw:Extension/templatestyles");
        n.set_attr("about", about);
        // As the pipeline actually produces it: `data-mw` is a plain attribute.
        n.set_attr("data-mw", r#"{"name":"templatestyles"}"#);
        n.push_child(Node::text(css));
        n
    }

    #[test]
    fn later_duplicate_becomes_a_link() {
        let mut div = Node::element(ElementKind::Div);
        div.push_child(style("TemplateStyles:r1", "#mwt1", "a{}"));
        div.push_child(style("TemplateStyles:r1", "#mwt2", "a{}"));
        div.push_child(style("TemplateStyles:r2", "#mwt3", "b{}"));
        run(&mut div);

        assert_eq!(node_name(&div.children[0]), "style");
        assert_eq!(node_name(&div.children[1]), "link");
        assert_eq!(node_name(&div.children[2]), "style");

        let link = &div.children[1];
        assert_eq!(
            link.get_attr("href"),
            Some("mw-data:TemplateStyles:r1"),
            "href carries the dedup key"
        );
        assert_eq!(link.get_attr("rel"), Some("mw-deduplicated-inline-style"));
        assert_eq!(link.get_attr("about"), Some("#mwt2"));
        assert_eq!(
            link.get_attr("data-mw"),
            Some(r#"{"name":"templatestyles"}"#),
            "data-mw is copied verbatim"
        );
        assert!(link.children.is_empty(), "the link has no children");
    }

    #[test]
    fn data_mw_is_copied_from_the_field_when_there_is_no_attribute() {
        let mut div = Node::element(ElementKind::Div);
        let mut a = style("TemplateStyles:r1", "#mwt1", "a{}");
        a.attrs.retain(|attr| attr.key != "data-mw");
        a.data_mw = Some(r#"{"name":"templatestyles","body":{"extsrc":""}}"#.to_string());
        let mut b = style("TemplateStyles:r1", "#mwt2", "a{}");
        b.attrs.retain(|attr| attr.key != "data-mw");
        b.data_mw = Some(r#"{"name":"templatestyles"}"#.to_string());
        div.push_child(a);
        div.push_child(b);
        run(&mut div);

        assert_eq!(
            div.children[1].data_mw.as_deref(),
            Some(r#"{"name":"templatestyles"}"#)
        );
    }

    #[test]
    fn duplicate_in_fosterable_position_is_emptied_not_replaced() {
        // `tr` is a fosterable position (Consts::FosterablePosition).
        let mut tr = Node::element(ElementKind::TableRow);
        tr.push_child(style("TemplateStyles:r1", "#mwt1", "a{}"));
        tr.push_child(style("TemplateStyles:r1", "#mwt2", "a{}"));
        run(&mut tr);

        assert_eq!(node_name(&tr.children[0]), "style");
        assert_eq!(
            node_name(&tr.children[1]),
            "style",
            "a fosterable duplicate stays a style tag"
        );
        assert!(
            tr.children[1].children.is_empty(),
            "but its text content is emptied"
        );
    }
}
