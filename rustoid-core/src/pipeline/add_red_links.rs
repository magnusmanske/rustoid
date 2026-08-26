//! AddRedLinks — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Processors/AddRedLinks.php`.
//!
//! Marks links whose targets do not exist (`missing`) with `class="new"`,
//! a `red-link-title` i18n attribute, `typeof="mw:LocalizedAttrs"`, and a
//! `?action=edit&redlink=1` query string on the href.

use crate::dom::node::{ElementKind, Node, NodeKind};

/// Add red links to an AST. `known_pages` is the set of page titles (in
/// prefixed form) that exist; any `rel="mw:WikiLink"` link whose `title` is
/// not in this set — and is not the page itself — is marked as a red link.
///
/// `page_title` is the prefixed title of the page being parsed, used for the
/// self-link check (mirrors PHP's `$env->getContextTitle()->getPrefixedText()`).
pub fn run(node: &mut Node, known_pages: &std::collections::HashSet<String>, page_title: &str) {
    for child in &mut node.children {
        run(child, known_pages, page_title);
    }
    if !matches!(node.kind, NodeKind::Element(ElementKind::Wikilink)) {
        return;
    }
    if !rel_has(node, "mw:WikiLink") {
        return;
    }

    let Some(title) = node.get_attr("title").map(str::to_string) else {
        return;
    };

    // A page with empty `title` (e.g. `[[]]`) cannot be a valid red link.
    if title.is_empty() {
        return;
    }

    // Self-links (and links to the current page) are not red links.
    if title == page_title {
        return;
    }

    // Only mark as red when the target is known to be missing.
    if known_pages.contains(&title) {
        return;
    }

    // `a->removeAttribute('class')` mirrors PHP's pb2pb refresh reset.
    node.attrs.retain(|a| a.key != "class");

    let mut classes = vec!["new".to_string()];
    add_class(node, &mut classes);

    // Red-link title i18n: `data-mw-i18n` + `typeof="mw:LocalizedAttrs"`.
    let i18n = format!(
        "{{\"title\":{{\"lang\":\"x-page\",\"key\":\"red-link-title\",\"params\":[\"{title}\"]}}}}"
    );
    node.set_attr("data-mw-i18n", i18n);
    add_typeof(node, "mw:LocalizedAttrs");

    // Append `?action=edit&redlink=1` to the href query string.
    if let Some(href) = node.get_attr("href").map(str::to_string) {
        let sep = if href.contains('?') { '&' } else { '?' };
        node.set_attr("href", format!("{href}{sep}action=edit&redlink=1"));
    }
}

fn rel_has(node: &Node, token: &str) -> bool {
    node.get_attr("rel")
        .map(|r| r.split_whitespace().any(|t| t == token))
        .unwrap_or(false)
}

fn add_class(node: &mut Node, classes: &mut Vec<String>) {
    let existing: Vec<String> = node
        .get_attr("class")
        .map(|c| c.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let mut all = existing;
    all.append(classes);
    node.set_attr("class", all.join(" "));
}

/// Add a token to a node's `typeof` attribute (whitespace-separated).
fn add_typeof(node: &mut Node, token: &str) {
    let mut tokens: Vec<String> = node
        .get_attr("typeof")
        .map(|t| t.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if !tokens.iter().any(|t| t == token) {
        tokens.push(token.to_string());
    }
    node.set_attr("typeof", tokens.join(" "));
}

/// Collect the prefixed page title from a title string, for `known_pages`
/// matching. The link `title` attribute is already the prefixed form.
pub fn link_title_from_attr(node: &Node) -> Option<String> {
    node.get_attr("title").map(str::to_string)
}

/// Collect the set of `rel="mw:WikiLink"` link titles in the subtree, for
/// batching existence checks (mirrors PHP's `getPageInfo` batching).
pub fn collect_wikilink_titles(node: &Node, out: &mut Vec<String>) {
    if matches!(node.kind, NodeKind::Element(ElementKind::Wikilink))
        && rel_has(node, "mw:WikiLink")
        && let Some(t) = link_title_from_attr(node)
        && !t.is_empty()
    {
        out.push(t);
    }
    for child in &node.children {
        collect_wikilink_titles(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn wikilink(title: &str) -> Node {
        let mut n = Node::element(ElementKind::Wikilink);
        n.set_attr("rel", "mw:WikiLink");
        n.set_attr("title", title);
        n.set_attr("href", format!("./{}", title.replace(' ', "_")));
        n.push_child(Node::text(title));
        n
    }

    #[test]
    fn test_missing_link_becomes_red() {
        let mut doc = Node::document();
        doc.push_child(wikilink("Test"));
        let known = HashSet::new();
        run(&mut doc, &known, "TestPage");
        let a = &doc.children[0];
        assert_eq!(a.get_attr("class"), Some("new"));
        assert_eq!(a.get_attr("typeof"), Some("mw:LocalizedAttrs"));
        assert_eq!(a.get_attr("href"), Some("./Test?action=edit&redlink=1"));
        assert!(
            a.get_attr("data-mw-i18n")
                .unwrap()
                .contains("red-link-title")
        );
    }

    #[test]
    fn test_existing_link_not_red() {
        let mut doc = Node::document();
        doc.push_child(wikilink("Test"));
        let mut known = HashSet::new();
        known.insert("Test".to_string());
        run(&mut doc, &known, "TestPage");
        assert_eq!(doc.children[0].get_attr("class"), None);
    }

    #[test]
    fn test_self_link_not_red() {
        let mut doc = Node::document();
        doc.push_child(wikilink("TestPage"));
        let known = HashSet::new();
        run(&mut doc, &known, "TestPage");
        assert_eq!(doc.children[0].get_attr("class"), None);
    }
}
