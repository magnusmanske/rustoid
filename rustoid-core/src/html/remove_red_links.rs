//! `RemoveRedLinks` — faithful port of `src/Html2Wt/RemoveRedLinks.php`.
//!
//! MediaWiki renders a link to a non-existent page with a `?action=edit&redlink=1`
//! query string and a `new` class. Parsoid's html2wt canonicalization strips
//! those two query parameters (unless selser, where the previous revision is
//! reused) so the link serializes back to its original wikitext.

use crate::dom::node::Node;

/// Strip the `action=edit` / `redlink=1` query parameters from a red link's
/// `href`. Returns the replacement href when one is needed.
///
/// Faithful to `RemoveRedLinks::handler`, including the parameter-order
/// preservation (PHP mutates the parsed query array in place) and the
/// "empty query means no `?`" rule.
pub fn remove_red_link_href(href: &str) -> Option<String> {
    let qm_pos = href.find('?')?;
    // Everything before `?` is the base; parse the query string.
    let (base, query) = href.split_at(qm_pos);
    let query = &query[1..];

    // PHP's `parse_str` splits on `&` (and `;` in older versions, but modern PHP
    // uses `&` only) and percent-decodes both keys and values.
    let mut drop_params = false;
    let mut kept: Vec<String> = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_val) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = decode_url_component(raw_key);
        let val = decode_url_component(raw_val);
        // PHP: `if ( isset($queryElts['action']) && $queryElts['action'] === 'edit' ) unset(...)`
        // then the same for `redlink === '1'`. NOTE: `parse_str` *overwrites*
        // duplicate keys, keeping the last value, so a later duplicate that does
        // not match does not resurrect an unmatched earlier one.
        let is_action_edit = key == "action" && val == "edit";
        let is_redlink_one = key == "redlink" && val == "1";
        if is_action_edit || is_redlink_one {
            drop_params = true;
            continue;
        }
        kept.push(pair.to_string());
    }

    if !drop_params {
        return None;
    }

    if kept.is_empty() {
        // Avoids the insertion of `?` on an empty query string.
        Some(base.to_string())
    } else {
        Some(format!("{base}?{}", kept.join("&")))
    }
}

/// Percent-decode a URL component. `+` becomes a space (form decoding, matching
/// PHP's `parse_str`).
fn decode_url_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Walk the tree applying [`remove_red_link_href`] to every `<a rel="mw:WikiLink"
/// class="new">` element. Faithful to the `DOMTraverser` handler registration in
/// `ContentModelHandler::canonicalizeDOM`.
///
/// The traverser returns `true` (keep descending) for every node, so a full walk
/// is equivalent.
pub fn remove_red_links(body: &mut Node) {
    fn walk(node: &mut Node) {
        handle(node);
        for child in &mut node.children {
            walk(child);
        }
    }

    fn handle(node: &mut Node) {
        if node.get_attr("href").is_none() {
            return;
        }
        if !matches!(node.kind, crate::dom::node::NodeKind::Element(_)) {
            return;
        }
        if crate::html::wts_utils::node_name(node) != "a" {
            return;
        }
        if crate::html::dom_utils::match_rel(node, "^mw:WikiLink$").is_none() {
            return;
        }
        let has_new = node
            .get_attr("class")
            .is_some_and(|c| c.split_whitespace().any(|t| t == "new"));
        if !has_new {
            return;
        }
        let Some(href) = node.get_attr("href") else {
            return;
        };
        if let Some(new_href) = remove_red_link_href(href) {
            node.set_attr("href", &new_href);
        }
    }

    walk(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_action_and_redlink() {
        assert_eq!(
            remove_red_link_href("./Eau?action=edit&redlink=1").as_deref(),
            Some("./Eau")
        );
    }

    #[test]
    fn keeps_other_params() {
        assert_eq!(
            remove_red_link_href("./Foo?action=edit&redlink=1&x=2").as_deref(),
            Some("./Foo?x=2")
        );
    }

    #[test]
    fn no_query_is_untouched() {
        assert_eq!(remove_red_link_href("./Eau"), None);
    }

    #[test]
    fn unrelated_query_is_untouched() {
        assert_eq!(remove_red_link_href("./Foo?a=b"), None);
    }

    #[test]
    fn non_edit_action_is_untouched() {
        assert_eq!(remove_red_link_href("./Foo?action=history"), None);
    }

    #[test]
    fn other_redlink_value_is_untouched() {
        assert_eq!(remove_red_link_href("./Foo?redlink=0"), None);
    }
}
