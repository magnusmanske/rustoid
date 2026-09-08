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
/// trail/prefix into the link, recording the moved source in `DataParsoid->tail`
/// / `DataParsoid->prefix` and (when the link sits inside a transclusion
/// encapsulation) migrating the moved source into `data-mw.parts` and correcting
/// DSR offsets.
fn handle_children_siblings(parent: &mut Node, config: &dyn SiteConfig) {
    let trail = config
        .link_trail_regex()
        .and_then(|r| regex::Regex::new(r).ok());
    let prefix = config
        .link_prefix_regex()
        .and_then(|r| regex::Regex::new(r).ok());

    let mut i = 0usize;
    while i < parent.children.len() {
        if !(matches!(
            parent.children[i].kind,
            NodeKind::Element(ElementKind::Wikilink)
        ) && is_wikilink_rel(&parent.children[i]))
        {
            i += 1;
            continue;
        }

        let about = about_of(&parent.children[i]);
        let link_no_typeof = parent.children[i].get_attr("typeof").is_none();

        // --- Link prefix (adjacent text before the link) ---
        if let Some(re) = &prefix {
            let mut prefix_text = String::new();
            let mut data_mw_correction = String::new();
            let mut dsr_correction = 0usize;
            loop {
                if i == 0 {
                    break;
                }
                let (content, from_tpl, remove) = match match_neighbour(
                    parent,
                    i - 1,
                    re,
                    about.as_deref(),
                    link_no_typeof,
                    false,
                ) {
                    Some(n) => n,
                    None => break,
                };
                if content.is_empty() {
                    break;
                }
                if remove {
                    parent.children.remove(i - 1);
                    i -= 1;
                }
                prefix_text = format!("{content}{prefix_text}");
                if !from_tpl {
                    data_mw_correction = format!("{content}{data_mw_correction}");
                    dsr_correction += content.len();
                }
            }
            if !prefix_text.is_empty() {
                prepend_text_to_node(&mut parent.children[i], &prefix_text);
                set_dp_prefix(&mut parent.children[i], &prefix_text);
                apply_dsr_correction_for_tpl(&mut parent.children[i], true, dsr_correction);
                if !data_mw_correction.is_empty() {
                    migrate_data_mw_parts(&mut parent.children[i], &data_mw_correction, true);
                }
            }
        }

        // --- Link trail (adjacent text after the link) ---
        if let Some(re) = &trail {
            let mut trail_text = String::new();
            let mut data_mw_correction = String::new();
            let mut dsr_correction = 0usize;
            loop {
                if i + 1 >= parent.children.len() {
                    break;
                }
                let (content, from_tpl, remove) = match match_neighbour(
                    parent,
                    i + 1,
                    re,
                    about.as_deref(),
                    link_no_typeof,
                    true,
                ) {
                    Some(n) => n,
                    None => break,
                };
                if content.is_empty() {
                    break;
                }
                if remove {
                    parent.children.remove(i + 1);
                }
                trail_text.push_str(&content);
                if !from_tpl {
                    data_mw_correction.push_str(&content);
                    dsr_correction += content.len();
                }
            }
            if !trail_text.is_empty() {
                append_text_to_node(&mut parent.children[i], &trail_text);
                set_dp_tail(&mut parent.children[i], &trail_text);
                apply_dsr_correction_for_tpl(&mut parent.children[i], false, dsr_correction);
                if !data_mw_correction.is_empty() {
                    migrate_data_mw_parts(&mut parent.children[i], &data_mw_correction, false);
                }
            }
        }

        i += 1;
    }
}

/// Inspect the single sibling at `idx` (relative to the link) and, when it is a
/// matching trail/prefix text (possibly a same-`about` single-child `<span>` to
/// unwrap), return `(matched_content, from_tpl, remove)`. A partial match (only
/// the leading/trailing portion matches) returns the matched portion and leaves
/// the remainder written back into the sibling (`remove == false`). A full match
/// returns `remove == true` so the caller drops the sibling. `None` means the
/// sibling is not a mergeable text neighbour.
///
/// `is_forward` selects trail (next sibling) vs prefix (previous sibling);
/// `link_no_typeof` and `base_about` feed the `unwrappedSpan` decision.
fn match_neighbour(
    parent: &mut Node,
    idx: usize,
    re: &regex::Regex,
    base_about: Option<&str>,
    link_no_typeof: bool,
    is_forward: bool,
) -> Option<(String, bool, bool)> {
    let neighbour = &parent.children[idx];
    let from_tpl = neighbour.get_attr("about").is_some();

    // `unwrappedSpan`: a `<span>` from the same transclusion (same `about`),
    // not literal HTML, single text child that wraps the actual text.
    let unwrap = matches!(neighbour.kind, NodeKind::Element(ElementKind::Span))
        && !crate::html::dom_utils::is_literal_html_node(neighbour)
        && from_tpl
        && base_about.is_some()
        && neighbour.get_attr("about") == base_about
        && neighbour.children.len() == 1
        && matches!(neighbour.children[0].kind, NodeKind::Text(_))
        && (neighbour.get_attr("typeof").is_none() || (!is_forward && link_no_typeof));

    // The text to test is the unwrapped span's child, else the sibling itself.
    let text: Option<&str> = if unwrap {
        match &neighbour.children[0].kind {
            NodeKind::Text(t) => Some(t),
            _ => None,
        }
    } else {
        match &neighbour.kind {
            NodeKind::Text(t) => Some(t),
            _ => None,
        }
    };
    let text = text?;

    let m: Option<(usize, String)> = if is_forward {
        // Trail: `^`-anchored leading match.
        re.find(text)
            .filter(|m| m.start() == 0)
            .map(|m| (m.start(), m.as_str().to_string()))
    } else {
        // Prefix: `$`-anchored trailing match — match the regex against the
        // reversed text (equivalent to `/[charset]+$/Du`), then reverse back.
        let rev: String = text.chars().rev().collect();
        re.find(&rev).filter(|m| m.start() == 0).map(|m| {
            let matched_rev = &rev[..m.end()];
            let matched: String = matched_rev.chars().rev().collect();
            (text.len() - matched.len(), matched)
        })
    };
    let (start, matched) = m?;
    if matched.is_empty() {
        return None;
    }

    if matched == text {
        // Entire node matches → remove it (and, implicitly, any unwrapped span).
        Some((matched, from_tpl, true))
    } else {
        // Partial match: the remainder stays in the (unwrapped) sibling.
        let remaining = if is_forward {
            text[matched.len()..].to_string()
        } else {
            text[..start].to_string()
        };
        if unwrap {
            let child = &mut parent.children[idx].children[0];
            child.kind = NodeKind::Text(remaining);
        } else {
            let sibling = &mut parent.children[idx];
            sibling.kind = NodeKind::Text(remaining);
        }
        Some((matched, from_tpl, false))
    }
}

/// The `about` value of a link node when it is itself a transclusion
/// encapsulation forest root (mirrors `WTUtils::isEncapsulatedDOMForestRoot` +
/// `getAttribute($aNode, 'about')` in `getLinkTrail`/`getLinkPrefix`).
fn about_of(node: &Node) -> Option<String> {
    let about = node.get_attr("about")?;
    about
        .strip_prefix("#mwt")
        .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))?;
    Some(about.to_string())
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

/// Correct the DSR offsets of the transclusion wrapper around `link` after moving
/// `dsr_correction` bytes of trail (`end` direction) or prefix (`start` direction)
/// into it. Mirrors PHP `HandleLinkNeighbours::handler`, which swaps `$dp` for the
/// first encapsulation wrapper node's data-parsoid when the link is inside a
/// transclusion, then adjusts `dsr.start`/`openWidth` (prefix) or
/// `dsr.end`/`closeWidth` (trail).
fn apply_dsr_correction_for_tpl(link: &mut Node, is_prefix: bool, dsr_correction: usize) {
    if dsr_correction == 0 {
        return;
    }
    // The link may itself be the encapsulation root; otherwise the correction is
    // recorded on the first encapsulation wrapper ancestor, but with this flat
    // children model we only carry DSR on the link's own data-parsoid unless the
    // link *is* the wrapper. PHP applies the correction to `$firstTplNode` (the
    // wrapper's) data-parsoid; here the wrapper is the link node itself when it
    // is a forest root, so we correct `link.dp` directly.
    let mut dp = link.dp.clone().unwrap_or_default();
    if let Some(dsr) = dp.dsr.as_mut() {
        if is_prefix {
            if let Some(start) = dsr.start.as_mut() {
                *start = start.saturating_sub(dsr_correction);
            }
            if let Some(ow) = dsr.open_width.as_mut() {
                *ow += dsr_correction;
            }
        } else {
            if let Some(end) = dsr.end.as_mut() {
                *end += dsr_correction;
            }
            if let Some(cw) = dsr.close_width.as_mut() {
                *cw += dsr_correction;
            }
        }
    }
    link.data_parsoid = dp.to_data_parsoid_json();
    link.dp = Some(dp);
}

/// Append (`is_prefix == false`) or prepend (true) `text` to the link's
/// `data-mw.parts` array, mirroring PHP's `$dataMW->parts[]`/`array_unshift`. The
/// migration only happens when the link is inside a `mw:Transclusion`
/// encapsulation (its own `data-mw.parts` or that of the wrapper).
fn migrate_data_mw_parts(link: &mut Node, text: &str, is_prefix: bool) {
    let Some(data_mw) = link.data_mw.as_deref() else {
        return;
    };
    let mut json: serde_json::Value =
        serde_json::from_str(data_mw).unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    let Some(parts) = json.get_mut("parts").and_then(|p| p.as_array_mut()) else {
        return;
    };
    let value = serde_json::Value::String(text.to_string());
    if is_prefix {
        if !parts.first().is_some_and(|f| *f == value) {
            parts.insert(0, value);
        }
    } else if !parts.last().is_some_and(|f| *f == value) {
        parts.push(value);
    }
    link.data_mw = Some(json.to_string());
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

    fn split_trail_match(text: &str, re: &regex::Regex) -> (String, String) {
        let Some(m) = re.find(text) else {
            return (String::new(), text.to_string());
        };
        if m.start() != 0 {
            return (String::new(), text.to_string());
        }
        (m.as_str().to_string(), text[m.end()..].to_string())
    }

    fn text_of(node: &Node) -> &str {
        match &node.kind {
            NodeKind::Text(t) => t,
            _ => "",
        }
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

    #[test]
    fn test_link_trail_merges_plain_after_forest_root() {
        // `{{1x|[[Foo]]}}l`: the link is a forest root (`about` + `typeof`), the
        // trailing `l` is a plain text sibling *outside* the transclusion. The
        // trail merges into the link and is appended to `data-mw.parts`.
        let config = MockSiteConfig::new();

        let mut a = link();
        a.set_attr("about", "#mwt1");
        a.set_attr("typeof", "mw:Transclusion");
        a.data_mw = Some(
            r#"{"parts":[{"template":{"target":{"wt":"1x","href":"./Template:1x"},"params":{"1":{"wt":"[[Foo]]"}},"i":0}}]}"#
                .to_string(),
        );

        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(a);
        p.push_child(Node::text("l"));

        run(&mut p, &config);

        assert_eq!(p.children.len(), 1);
        let link = &p.children[0];
        assert_eq!(
            link.children.last().unwrap().kind,
            NodeKind::Text("l".to_string())
        );
        assert_eq!(link.dp.as_ref().unwrap().tail.as_deref(), Some("l"));
        let json: serde_json::Value =
            serde_json::from_str(link.data_mw.as_deref().unwrap()).unwrap();
        let parts = json["parts"].as_array().unwrap();
        assert_eq!(parts.last().unwrap().as_str(), Some("l"));
    }

    #[test]
    fn test_link_trail_unwraps_same_about_span() {
        // `{{1x|[[Foo]]l}}`: the trailing `l` is *inside* the transclusion, so it
        // is wrapped in a same-`about` single-child span. The trail must merge
        // into the link through that span (fromTpl=true) *without* migrating
        // `data-mw.parts`.
        let config = MockSiteConfig::new();

        let mut a = link();
        a.set_attr("about", "#mwt1");
        a.set_attr("typeof", "mw:Transclusion");
        a.data_mw = Some(
            r#"{"parts":[{"template":{"target":{"wt":"1x","href":"./Template:1x"},"params":{"1":{"wt":"[[Foo]]l"}},"i":0}}]}"#
                .to_string(),
        );

        let mut span = Node::element(ElementKind::Span);
        span.set_attr("about", "#mwt1");
        span.push_child(Node::text("l"));

        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(a);
        p.push_child(span);

        run(&mut p, &config);

        assert_eq!(p.children.len(), 1);
        let link = &p.children[0];
        assert_eq!(
            link.children.last().unwrap().kind,
            NodeKind::Text("l".to_string())
        );
        assert_eq!(link.dp.as_ref().unwrap().tail.as_deref(), Some("l"));
        // No parts migration: the trail came from inside the template.
        let json: serde_json::Value =
            serde_json::from_str(link.data_mw.as_deref().unwrap()).unwrap();
        let parts = json["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
    }
}
