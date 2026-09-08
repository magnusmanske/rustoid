//! HandleLinkNeighbours — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Handlers/HandleLinkNeighbours.php`.
//!
//! Moves link-trail (`[[Foo]]l` → the trailing `l` attaches to the link) and
//! link-prefix text into their adjacent `mw:WikiLink` anchor, recording the
//! moved source in `DataParsoid->tail` / `DataParsoid->prefix` for
//! round-tripping. The trail is determined by `SiteConfig::linkTrailRegex`
//! (enwiki: `^[a-z]+`), the prefix by `SiteConfig::linkPrefixRegex` (typically
//! absent for wikis without an RTL script).

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::traits::SiteConfig;

/// Apply link-neighbour handling to the subtree rooted at `node`.
///
/// We operate on each element's `children` list so that a link and its adjacent
/// text siblings (both direct children of the same parent, e.g. a `<p>`) can be
/// merged. Mirrors PHP's DOM traversal, which runs `handler` on every `<a>`.
pub fn run(node: &mut Node, config: &dyn SiteConfig) {
    // First recurse into children (siblings get merged at their own parent).
    recurse_before_merge(node, config);

    // Then walk this node's immediate children, merging link trails/prefixes.
    handle_children_siblings(node, config);
}

/// Recurse into each child (running the same neighbour merge at that level).
fn recurse_before_merge(node: &mut Node, config: &dyn SiteConfig) {
    for child in &mut node.children {
        run(child, config);
    }
}

/// For each wikilink `<a>` among `parent`'s children, consume an adjacent text
/// trail/prefix into the link.
fn handle_children_siblings(parent: &mut Node, config: &dyn SiteConfig) {
    let trail = config
        .link_trail_regex()
        .and_then(|r| regex::Regex::new(r).ok());
    let prefix = config
        .link_prefix_regex()
        .and_then(|r| regex::Regex::new(r).ok());

    let children = &mut parent.children;
    let mut i = 0usize;
    while i < children.len() {
        if !(matches!(children[i].kind, NodeKind::Element(ElementKind::Wikilink))
            && is_wikilink_rel(&children[i]))
        {
            i += 1;
            continue;
        }

        // Link prefix: text immediately *before* this link.
        if let Some(re) = &prefix
            && i > 0
            && matches!(children[i - 1].kind, NodeKind::Text(_))
        {
            let (keep, moved) = split_prefix_match(text_of(&children[i - 1]), re);
            if !moved.is_empty() {
                // Move the matched suffix of the previous text into the link.
                prepend_text_to_node(&mut children[i], &moved);
                set_dp_prefix(&mut children[i], &moved);
                if keep.is_empty() {
                    children.remove(i - 1);
                    i -= 1;
                } else {
                    set_text(&mut children[i - 1], &keep);
                }
            }
        }

        // Link trail: text immediately *after* this link.
        if let Some(re) = &trail
            && i + 1 < children.len()
            && matches!(children[i + 1].kind, NodeKind::Text(_))
        {
            let (moved, keep) = split_trail_match(text_of(&children[i + 1]), re);
            if !moved.is_empty() {
                append_text_to_node(&mut children[i], &moved);
                set_dp_tail(&mut children[i], &moved);
                if keep.is_empty() {
                    children.remove(i + 1);
                    // `i` is not advanced past the link; the next sibling
                    // (if any) is handled on the *next* iteration after the
                    // loop's `i += 1` below — but we must not skip it. Re-scan
                    // this index by not advancing. We handle that by
                    // continuing the loop below without `i += 1`.
                } else {
                    set_text(&mut children[i + 1], &keep);
                    // Advance past the (now-trailing) text sibling.
                    i += 1;
                }
                continue;
            }
        }

        i += 1;
    }
}

/// Split a text string into `(matched_lead, remaining)` where `matched_lead` is
/// the longest leading portion matched by the trail regex. Returns
/// `("", original)` when there is no match.
fn split_trail_match(text: &str, re: &regex::Regex) -> (String, String) {
    let Some(m) = re.find(text) else {
        return (String::new(), text.to_string());
    };
    // PHP matches `$matches[0]` at the *start* of the sibling; only a leading
    // match is a trail.
    if m.start() != 0 {
        return (String::new(), text.to_string());
    }
    (m.as_str().to_string(), text[m.end()..].to_string())
}

/// Split a text string into `(remaining, matched_tail)` where `matched_tail` is
/// the trailing portion matched by the prefix regex (applied to the reversed
/// text, matching PHP's "content will be reversed"). For simplicity we match
/// the regex against the suffix. Returns `(original, "")` when no match.
fn split_prefix_match(text: &str, re: &regex::Regex) -> (String, String) {
    // PHP reverses the neighbour text and matches the (reversed) prefix regex,
    // then un-reverses the captured source. A prefix is a *suffix* of the text
    // sibling; find the longest suffix matching by anchoring the regex at the
    // end of the reversed string (i.e. the start of our reversed copy).
    let rev: String = text.chars().rev().collect();
    let Some(m) = re.find(&rev) else {
        return (text.to_string(), String::new());
    };
    if m.start() != 0 {
        return (text.to_string(), String::new());
    }
    let matched_rev = &rev[..m.end()];
    let matched: String = matched_rev.chars().rev().collect();
    let remaining_len = text.len() - matched.len();
    (text[..remaining_len].to_string(), matched)
}

fn text_of(node: &Node) -> &str {
    match &node.kind {
        NodeKind::Text(t) => t,
        _ => "",
    }
}

fn set_text(node: &mut Node, text: &str) {
    node.kind = NodeKind::Text(text.to_string());
}

/// Prepend `text` to the link's first child (or add a leading text child).
fn prepend_text_to_node(link: &mut Node, text: &str) {
    link.children.insert(0, Node::text(text));
}

/// Append `text` as a trailing text child of the link.
fn append_text_to_node(link: &mut Node, text: &str) {
    link.children.push(Node::text(text));
}

/// Record the trail source in the link's `DataParsoid` and refresh the
/// serialized `data-parsoid` string.
fn set_dp_tail(link: &mut Node, tail: &str) {
    let mut dp = link.dp.clone().unwrap_or_default();
    match dp.tail.as_mut() {
        Some(existing) => existing.push_str(tail),
        None => dp.tail = Some(tail.to_string()),
    }
    link.data_parsoid = dp.to_data_parsoid_json();
    link.dp = Some(dp);
}

/// Record the prefix source in the link's `DataParsoid`.
fn set_dp_prefix(link: &mut Node, prefix: &str) {
    let mut dp = link.dp.clone().unwrap_or_default();
    match dp.prefix.as_mut() {
        Some(existing) => {
            existing.insert_str(0, prefix);
        }
        None => dp.prefix = Some(prefix.to_string()),
    }
    link.data_parsoid = dp.to_data_parsoid_json();
    link.dp = Some(dp);
}

fn is_wikilink_rel(node: &Node) -> bool {
    node.get_attr("rel")
        .map(|r| {
            let mut tokens = r.split_whitespace();
            matches!(
                tokens.next(),
                Some("mw:WikiLink") | Some("mw:WikiLink/Interwiki")
            ) && tokens.next().is_none()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    fn link() -> Node {
        let mut a = Node::element(ElementKind::Wikilink);
        a.set_attr("rel", "mw:WikiLink");
        a.set_attr("href", "./Foo");
        a.set_attr("title", "Foo");
        a.push_child(Node::text("Foo"));
        a
    }

    #[test]
    fn test_split_trail_simple() {
        let re = regex::Regex::new("^[a-z]+").unwrap();
        assert_eq!(
            split_trail_match("xxx, ...", &re),
            ("xxx".to_string(), ", ...".to_string())
        );
        assert_eq!(
            split_trail_match("XXX", &re),
            (String::new(), "XXX".to_string())
        );
    }

    #[test]
    fn test_link_trail_merged_into_link() {
        let config = MockSiteConfig::new();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(link());
        p.push_child(Node::text("xxx rest"));
        p.push_child(Node::text("!!!"));

        run(&mut p, &config);

        // The `xxx` trail moved into the link; `rest !!!` remains as siblings.
        let children = &p.children;
        assert_eq!(children.len(), 3);
        assert!(matches!(
            children[0].kind,
            NodeKind::Element(ElementKind::Wikilink)
        ));
        let link = &children[0];
        // Last child of the link is the trail text.
        let tail_text = match &link.children.last().unwrap().kind {
            NodeKind::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert_eq!(tail_text, "xxx");
        assert_eq!(link.dp.as_ref().unwrap().tail.as_deref(), Some("xxx"));
        assert_eq!(text_of(&children[1]), " rest");
        assert_eq!(text_of(&children[2]), "!!!");
    }
}
