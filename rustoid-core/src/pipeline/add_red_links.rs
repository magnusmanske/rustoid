//! AddRedLinks — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Processors/AddRedLinks.php`.
//!
//! Marks links whose targets do not exist (`missing` && !`known`) with
//! `class="new"`, a `red-link-title` i18n attribute, `typeof="mw:LocalizedAttrs"`,
//! and a `?action=edit&redlink=1` query string on the href. Self-links become
//! `mw-selflink` / `mw-selflink-fragment`, redirects get `mw-redirect`, and any
//! extra `linkclasses` reported by the page-info are applied.

use std::collections::HashMap;

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::traits::PageInfo;

/// Apply red-link/self-link/redirect marking to an AST.
///
/// `page_info` maps prefixed target titles to their resolved metadata (from
/// `DataSource::get_page_info`). `page_title` is the prefixed title of the page
/// being parsed, used for the self-link check (mirrors PHP's `getContextTitle()`).
pub fn run(node: &mut Node, page_info: &HashMap<String, PageInfo>, page_title: &str) {
    for child in &mut node.children {
        run(child, page_info, page_title);
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

    // Clear any existing class (mirrors `$a->removeAttribute('class')` at the
    // top of the per-link loop).
    node.attrs.retain(|a| a.key != "class");

    let info = page_info.get(&title);
    let missing = info.map(|i| i.missing && !i.known).unwrap_or(false);

    if missing && title != page_title {
        add_class(node, "new");
        // Red-link title i18n: `data-mw-i18n` + `typeof="mw:LocalizedAttrs"`.
        // The params array is JSON-encoded so backslashes/quotes in the title
        // round-trip (mirrors I18nInfo::toJsonArray + JSON serialization). The
        // literal object keeps the `lang`/`key`/`params` key order PHP emits
        // (`serde_json`'s `json!` would sort the keys).
        let title_json = serde_json::to_string(&title).unwrap_or_else(|_| "\"\"".to_string());
        let i18n = format!(
            "{{\"title\":{{\"lang\":\"x-page\",\"key\":\"red-link-title\",\"params\":[{title_json}]}}}}"
        );
        node.set_attr("data-mw-i18n", i18n);
        add_typeof(node, "mw:LocalizedAttrs");

        // Append `?action=edit&redlink=1` to the href query string, keeping the
        // fragment *after* the query (PHP reassembles the URL correctly).
        if let Some(href) = node.get_attr("href").map(str::to_string) {
            let (base, fragment) = split_fragment(&href);
            let sep = if base.contains('?') { '&' } else { '?' };
            let new_href = format!("{base}{sep}action=edit&redlink=1");
            node.set_attr(
                "href",
                match fragment {
                    Some(f) => format!("{new_href}#{f}"),
                    None => new_href,
                },
            );
        }
    } else if title == page_title {
        // Self-link: `mw-selflink-fragment` when the href carries a fragment,
        // otherwise `mw-selflink selflink`. The `title` is removed either way.
        let has_fragment = node
            .get_attr("href")
            .map(|h| h.contains('#'))
            .unwrap_or(false);
        if has_fragment {
            add_class(node, "mw-selflink-fragment");
        } else {
            add_class(node, "mw-selflink");
            add_class(node, "selflink");
        }
        node.attrs.retain(|a| a.key != "title");
    }

    // Redirect and extra link classes.
    if let Some(info) = info {
        if info.redirect {
            add_class(node, "mw-redirect");
        }
        for class in &info.linkclasses {
            add_class(node, class);
        }
    }
}

/// Split an href into `(base_query_part, fragment)` on the first `#`.
fn split_fragment(href: &str) -> (&str, Option<&str>) {
    match href.split_once('#') {
        Some((base, frag)) => (base, Some(frag)),
        None => (href, None),
    }
}

fn rel_has(node: &Node, token: &str) -> bool {
    node.get_attr("rel")
        .map(|r| r.split_whitespace().any(|t| t == token))
        .unwrap_or(false)
}

fn add_class(node: &mut Node, class: &str) {
    let existing: Vec<String> = node
        .get_attr("class")
        .map(|c| c.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if existing.iter().any(|c| c == class) {
        return;
    }
    let mut all = existing;
    all.push(class.to_string());
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

/// Collect the prefixed page title from a title string, for page-info lookups.
/// The link `title` attribute is already the prefixed form.
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
    use std::collections::HashMap;

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
        let info = HashMap::new();
        // In the real pipeline `get_page_info` returns an entry for every title
        // (with `missing` computed); here an absent entry mirrors an unresolved
        // title that is missing.
        let mut info = info;
        info.insert(
            "Test".to_string(),
            PageInfo {
                missing: true,
                known: false,
                ..Default::default()
            },
        );
        run(&mut doc, &info, "TestPage");
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
        let mut info = HashMap::new();
        info.insert(
            "Test".to_string(),
            PageInfo {
                missing: false,
                known: true,
                ..Default::default()
            },
        );
        run(&mut doc, &info, "TestPage");
        assert_eq!(doc.children[0].get_attr("class"), None);
    }

    #[test]
    fn test_self_link_not_red() {
        let mut doc = Node::document();
        doc.push_child(wikilink("TestPage"));
        let info = HashMap::new();
        run(&mut doc, &info, "TestPage");
        let a = &doc.children[0];
        assert_eq!(a.get_attr("class"), Some("mw-selflink selflink"));
        assert_eq!(a.get_attr("title"), None);
    }

    #[test]
    fn test_redirect_adds_class() {
        let mut doc = Node::document();
        doc.push_child(wikilink("Redirected"));
        let mut info = HashMap::new();
        info.insert(
            "Redirected".to_string(),
            PageInfo {
                missing: false,
                known: true,
                redirect: true,
                ..Default::default()
            },
        );
        run(&mut doc, &info, "TestPage");
        assert_eq!(doc.children[0].get_attr("class"), Some("mw-redirect"));
    }
}
