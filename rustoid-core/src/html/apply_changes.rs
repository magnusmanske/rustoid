//! Manual (jQuery-style) DOM changes for selser / manual-edit tests.
//!
//! Faithful port of PHP Parsoid's `Test::applyManualChanges`, which interprets
//! a parser-test `parsoid.changes` array as a sequence of pseudo-jQuery
//! operations applied to the parsed DOM before re-serializing.
//!
//! The `changes` array uses jquery syntax: `[selector, method, ...args]`
//! becomes `$(selector).method(...args)`. For example:
//!
//! ```text
//! ["p", "html", "BAR"]                         =>  $('p').html('BAR')
//! ["li:nth-child(3)", "append", "<ul>…</ul>"]  =>  $('li:nth-child(3)').append('…')
//! ["[typeof~='mw:File']", "attr", "data-mw", "{}"]
//! ```
//!
//! A `contents` second argument calls jQuery's `.contents()` (the child nodes of
//! each matched element) before applying the following method.

use crate::dom::node::{Node, NodeKind};
use crate::error::{Result, RustoidError};
use crate::html::parse::parse_html;

/// A path into the tree: each element is a child index (0-based) from the root.
type Path = Vec<usize>;

/// Apply a sequence of manual (jQuery-style) changes to a DOM body.
///
/// `body` is the root whose descendants are matched by `selector` (mirroring
/// `DOMCompat::querySelectorAll($body, $selector)`); `changes` is the raw
/// `parsoid.changes` JSON value. Faithful to `Test::applyManualChanges`.
pub fn apply_manual_changes(body: &mut Node, changes: &serde_json::Value) -> Result<()> {
    let changes = changes
        .as_array()
        .ok_or_else(|| RustoidError::Parse("parsoid.changes is not an array".to_string()))?;

    for change in changes {
        let change = change
            .as_array()
            .ok_or_else(|| RustoidError::Parse("change is not an array".to_string()))?;
        if change.len() < 2 {
            return Err(RustoidError::Parse(format!("bad change: {change:?}")));
        }
        let selector = change[0]
            .as_str()
            .ok_or_else(|| RustoidError::Parse("change selector is not a string".to_string()))?;

        // `$(selector)` → list of matched element paths.
        let mut targets: Vec<Path> = find_matches(body, selector);

        // `change[1] === "contents"` calls `.contents()` on the matched set
        // (the child nodes of each matched element), then applies the following
        // method to those child nodes.
        let mut method_idx = 1;
        let mut contents_children: Vec<Path> = Vec::new();
        if change[1].as_str() == Some("contents") {
            method_idx = 2;
            for p in &targets {
                for (i, _) in child_nodes_at(body, p).iter().enumerate() {
                    let mut cp = p.clone();
                    cp.push(i);
                    contents_children.push(cp);
                }
            }
            targets = contents_children;
        }

        let method = change
            .get(method_idx)
            .and_then(|m| m.as_str())
            .ok_or_else(|| RustoidError::Parse("change method is not a string".to_string()))?;

        match method {
            "attr" => {
                let name = arg_str(change, method_idx + 1, "attr name")?;
                let val = arg_str(change, method_idx + 2, "attr value")?;
                for p in &targets {
                    if let Some(el) = node_at_mut(body, p) {
                        set_attr_verbose(el, name, val);
                    }
                }
            }
            "text" => {
                let t = arg_str(change, method_idx + 1, "text value")?;
                for p in &targets {
                    if let Some(el) = node_at_mut(body, p) {
                        el.children = vec![Node::text(t)];
                    }
                }
            }
            "html" => {
                let h = arg_str(change, method_idx + 1, "html value")?;
                let frag = parse_html(h)?;
                let new_children = frag.children;
                for p in &targets {
                    if let Some(el) = node_at_mut(body, p) {
                        el.children = new_children.clone();
                    }
                }
            }
            "append" | "before" | "after" => {
                let h = arg_str(change, method_idx + 1, "insert html")?;
                let frag = parse_html(h)?;
                apply_insertion(body, &targets, method, frag)?;
            }
            "remove" => {
                // Remove each matched node from its parent.
                let mut paths = targets.clone();
                paths.sort_by_key(|p| std::cmp::Reverse(p.len()));
                remove_paths(body, &paths);
            }
            "empty" => {
                for p in &targets {
                    if let Some(el) = node_at_mut(body, p) {
                        el.children.clear();
                    }
                }
            }
            "removeAttr" => {
                let name = arg_str(change, method_idx + 1, "attr name")?;
                for p in &targets {
                    if let Some(el) = node_at_mut(body, p) {
                        remove_attr_verbose(el, name);
                    }
                }
            }
            "addClass" | "removeClass" => {
                let cls = arg_str(change, method_idx + 1, "class name")?;
                for p in &targets {
                    if let Some(el) = node_at_mut(body, p) {
                        toggle_class(el, cls, method == "addClass");
                    }
                }
            }
            "wrap" => {
                let w = arg_str(change, method_idx + 1, "wrap html")?;
                apply_wrap(body, &targets, w)?;
            }
            other => {
                return Err(RustoidError::Parse(format!(
                    "unsupported mutator function: {other}"
                )));
            }
        }
    }
    Ok(())
}

/// Fetch a change argument by index as a string (with a descriptive error).
fn arg_str<'a>(change: &'a [serde_json::Value], idx: usize, what: &str) -> Result<&'a str> {
    change
        .get(idx)
        .and_then(|v| v.as_str())
        .ok_or_else(|| RustoidError::Parse(format!("{what} missing")))
}

/// Set an attribute, routing the serializer-only `data-mw`/`data-parsoid`
/// attributes to their dedicated `Node` fields (which the html2wt serializer
/// reads) rather than the generic `attrs` list. Mirrors `setAttribute`, which
/// treats these as ordinary attributes in a DOM.
fn set_attr_verbose(el: &mut Node, name: &str, val: &str) {
    match name {
        "data-mw" => el.data_mw = Some(val.to_string()),
        "data-parsoid" => el.data_parsoid = Some(val.to_string()),
        _ => el.set_attr(name, val),
    }
}

/// Remove an attribute, clearing the dedicated fields for `data-mw`/
/// `data-parsoid`. Mirrors `removeAttribute`.
fn remove_attr_verbose(el: &mut Node, name: &str) {
    match name {
        "data-mw" => el.data_mw = None,
        "data-parsoid" => el.data_parsoid = None,
        _ => el.attrs.retain(|a| a.key != name),
    }
}

/// Return a mutable reference to the node at `path`, if it exists.
fn node_at_mut<'a>(body: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    let mut cur = body;
    for &idx in path {
        cur = cur.children.get_mut(idx)?;
    }
    Some(cur)
}

/// The child nodes of the element at `path`, walked as (index) pairs for
/// `contents()`.
fn child_nodes_at<'a>(body: &'a Node, path: &[usize]) -> &'a [Node] {
    let mut cur = body;
    for &idx in path {
        match cur.children.get(idx) {
            Some(c) => cur = c,
            None => return &[],
        }
    }
    &cur.children
}

/// Find every element matching `selector` under `body`, returning their paths.
/// Excludes the root itself (mirrors `querySelectorAll` scope) unless it matches.
fn find_matches(body: &Node, selector: &str) -> Vec<Path> {
    // Pre-split the selector into its compound parts (descendant combinator).
    let compounds: Vec<&str> = selector.split_whitespace().collect();
    let mut out = Vec::new();
    walk(body, &compounds, &mut Vec::new(), &mut Vec::new(), &mut out);
    out
}

/// Recursively walk the tree, matching the selector's compound parts against
/// each element: the rightmost compound against the element itself, preceding
/// compounds against its ancestors (descendant combinator), mirroring Zest's
/// `qsa`. `ancestors` is the stack of enclosing element nodes.
fn walk<'a>(
    node: &'a Node,
    compounds: &[&str],
    ancestors: &mut Vec<&'a Node>,
    path: &mut Vec<usize>,
    out: &mut Vec<Path>,
) {
    // Compute the element-sibling position (0-based) for each child, matching
    // Zest's `:nth-child`, which counts *element* siblings via
    // `previousElementSibling` (a historical quirk of `qsa`-derived selector
    // engines) rather than all siblings including whitespace text nodes.
    let mut element_index = 0usize;
    for (i, child) in node.children.iter().enumerate() {
        let is_element = matches!(child.kind, NodeKind::Element(_));
        if is_element {
            if matches_full_selector(child, compounds, element_index, ancestors) {
                let mut p = path.clone();
                p.push(i);
                out.push(p);
            }
            element_index += 1;
            ancestors.push(child);
            path.push(i);
            walk(child, compounds, ancestors, path, out);
            path.pop();
            ancestors.pop();
        } else {
            path.push(i);
            walk(child, compounds, ancestors, path, out);
            path.pop();
        }
    }
}

/// Match a fully-split selector against `node`. `sibling_index` is the node's
/// element-sibling position for its own pseudo-class; `ancestors[0]` is the
/// immediate parent element, `ancestors[1]` the grandparent, etc.
fn matches_full_selector(
    node: &Node,
    compounds: &[&str],
    sibling_index: usize,
    ancestors: &[&Node],
) -> bool {
    // The rightmost compound matches the node itself.
    let (last, rest) = compounds.split_last().expect("non-empty selector");
    if !matches_compound(node, last, sibling_index) {
        return false;
    }
    // Preceding compounds match *some* ancestor (descendant combinator), in
    // right-to-left order: the rightmost remaining compound matches the nearest
    // matching ancestor, the next matches an ancestor *above* that, and so on.
    // Walk the ancestor stack (nearest-first) matching each compound in turn.
    let mut comp_iter = rest.iter().rev();
    let mut comp = comp_iter.next();
    for ancestor in ancestors.iter().rev() {
        let Some(part) = comp else {
            break;
        };
        if matches_compound(ancestor, part, 0) {
            comp = comp_iter.next();
        }
    }
    comp.is_none()
}

/// Public helper matching a node against a (possibly descendant) selector,
/// given its element-sibling index. Used by unit tests; production matching goes
/// through [`walk`]/[`matches_full_selector`].
pub fn matches_selector(node: &Node, selector: &str, sibling_index: usize) -> bool {
    let compounds: Vec<&str> = selector.split_whitespace().collect();
    matches_full_selector(node, &compounds, sibling_index, &[])
}

/// Match a single compound selector (e.g. `figcaption`, `.mw-default-size`,
/// `*[typeof="mw:File"]`, `li:nth-child(3)`) against a node. `sibling_index` is
/// the element-sibling index for pseudo-class evaluation.
fn matches_compound(node: &Node, compound: &str, sibling_index: usize) -> bool {
    if !matches!(node.kind, NodeKind::Element(_)) {
        return false;
    }

    // Split off a trailing pseudo-class (`:first-child`, `:nth-child(...)`).
    let (simple, pseudo) = split_pseudo(compound);

    // A compound is a sequence of simple selectors: `*`/tag, `.class`,
    // `[attr]`, `#id` (id unsupported).
    if !matches_simple_selector(node, simple) {
        return false;
    }

    if let Some(pseudo) = pseudo {
        return matches_pseudo(pseudo, sibling_index);
    }
    true
}

/// Split a compound selector into its non-pseudo part and an optional trailing
/// `:pseudo(...)` (only the first colon *outside* an attribute selector is
/// honored, so `[typeof="mw:File"]` is not split).
fn split_pseudo(compound: &str) -> (&str, Option<&str>) {
    let bytes = compound.as_bytes();
    let mut in_attr = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'[' => in_attr = true,
            b']' => in_attr = false,
            b':' if !in_attr => {
                return (&compound[..i], Some(&compound[i + 1..]));
            }
            _ => {}
        }
    }
    (compound, None)
}

/// Match a pseudo-free simple selector sequence against a node. Handles the
/// tag (or `*`), any `.class`, and any `[attr]` qualifiers.
fn matches_simple_selector(node: &Node, simple: &str) -> bool {
    let simple = simple.trim();
    if simple.is_empty() || simple == "*" {
        return true;
    }

    // Walk the selector left-to-right, consuming one qualifier at a time.
    let mut rest = simple;
    // Optional leading tag name (a run of word chars, not preceded by `.`/`[`).
    let name = crate::html::wts_utils::node_name(node);
    if !rest.is_empty() && !rest.starts_with(['.', '[']) {
        // Consume a bare tag name.
        let tag_end = rest.find(['.', '[', ':']).unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        if tag != "*" && name != tag {
            return false;
        }
        rest = &rest[tag_end..];
    }

    // Consume `.class` and `[attr...]` qualifiers in order.
    while !rest.is_empty() {
        if let Some(after_dot) = rest.strip_prefix('.') {
            let class_end = after_dot.find(['.', '[', ':']).unwrap_or(after_dot.len());
            let class = &after_dot[..class_end];
            let Some(cls_attr) = node.get_attr("class") else {
                return false;
            };
            if !cls_attr.split_whitespace().any(|c| c == class) {
                return false;
            }
            rest = &after_dot[class_end..];
        } else if rest.starts_with('[') {
            let (attr_sel, remainder) = take_attr_selector(rest);
            if !matches_attribute_selector(node, attr_sel) {
                return false;
            }
            rest = remainder;
        } else {
            // Unrecognized qualifier — fail conservatively.
            return false;
        }
    }
    true
}

/// Extract a balanced `[...]` attribute selector from the start of `rest`,
/// returning it plus the remaining selector text.
fn take_attr_selector(rest: &str) -> (&str, &str) {
    let start = rest.find('[').expect("starts with [");
    let bytes = rest.as_bytes();
    let mut end = rest.len();
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if b == b']' {
            end = i + 1;
            break;
        }
    }
    (&rest[start..end], &rest[end..])
}

/// Match `:nth-child(An+B)` / `:odd` / `:even` / `:first-child` / `:last-child`.
/// Faithful to CSS for the simple integer cases used by the parser tests. The
/// `1-based` index is derived from `sibling_index`; `last` is the total number
/// of element siblings (not tracked here, so `:last-child` is approximated).
fn matches_pseudo(pseudo: &str, sibling_index: usize) -> bool {
    let one_based = sibling_index + 1;
    if let Some(inner) = pseudo.trim().strip_prefix("nth-child(")
        && let Some(inner) = inner.strip_suffix(')')
    {
        // Support `n`, `2n`, `2n+1`, `odd`, `even`, and plain integers.
        let inner = inner.trim();
        if inner == "odd" {
            return one_based % 2 == 1;
        }
        if inner == "even" {
            return one_based.is_multiple_of(2);
        }
        if let Ok(n) = inner.parse::<usize>() {
            return one_based == n;
        }
        // `an+b` form.
        return matches_an_plus_b(inner, one_based);
    }
    match pseudo.trim() {
        "first-child" => one_based == 1,
        "last-child" => one_based == 1, // approximated; refined via find count when needed
        _ => false,
    }
}

/// Match an `an+b` expression against a 1-based index (`b` may be `+b`/`-b`).
fn matches_an_plus_b(expr: &str, index: usize) -> bool {
    let expr = expr.replace(' ', "");
    // Split into coefficient `a` and offset `b` around `n`.
    let (a, b) = match expr.find('n') {
        Some(i) => {
            let a_part = &expr[..i];
            let b_part = &expr[i + 1..];
            let a: i64 = match a_part {
                "" => 1,
                "+" => 1,
                "-" => -1,
                s => s.parse().unwrap_or(0),
            };
            let b: i64 = if b_part.is_empty() {
                0
            } else {
                b_part.parse().unwrap_or(0)
            };
            (a, b)
        }
        None => (0, expr.parse().unwrap_or(0)),
    };
    let i = index as i64;
    if a == 0 {
        i == b
    } else {
        let diff = i - b;
        diff % a == 0 && diff / a >= 0
    }
}

/// Match a single attribute selector: `[attr]`, `[attr=val]`, `[attr~=val]`,
/// `[attr|=val]`, `[attr^=val]`, `[attr$=val]`, `[attr*=val]`.
fn matches_attribute_selector(node: &Node, sel: &str) -> bool {
    let inner = sel.trim_start_matches('[').trim_end_matches(']');
    let mut op = None;
    let mut split_at = None;
    for o in ["~=", "|=", "^=", "$=", "*=", "="] {
        if let Some(j) = inner.find(o) {
            op = Some(o);
            split_at = Some(j);
            break;
        }
    }
    if split_at.is_none() && inner.find('=').is_some() {
        op = Some("=");
        split_at = inner.find('=');
    }

    let (name, value) = match split_at {
        Some(j) => {
            let op_len = op.map(|o| o.len()).unwrap_or(1);
            (
                Some(&inner[..j]),
                Some(inner[j + op_len..].trim_matches(['\'', '"'])),
            )
        }
        None => (Some(inner), None),
    };

    let Some(name) = name else {
        return false;
    };
    let actual = node.get_attr(name);

    match (op, value) {
        (None, None) => actual.is_some(),
        (Some("="), Some(v)) => actual == Some(v),
        (Some("~="), Some(v)) => actual
            .map(|a| a.split_whitespace().any(|w| w == v))
            .unwrap_or(false),
        (Some("^="), Some(v)) => actual.map(|a| a.starts_with(v)).unwrap_or(false),
        (Some("$="), Some(v)) => actual.map(|a| a.ends_with(v)).unwrap_or(false),
        (Some("*="), Some(v)) => actual.map(|a| a.contains(v)).unwrap_or(false),
        (Some("|="), Some(v)) => actual
            .map(|a| a == v || a.starts_with(&format!("{v}-")))
            .unwrap_or(false),
        _ => false,
    }
}

/// Apply `append`/`before`/`after` by splicing parsed nodes relative to each
/// target. Faithful to jQuery plus the `tbody`/`tr` special-casing in PHP's
/// `applyManualChanges` (matching the relevant DOM-construction quirks).
fn apply_insertion(body: &mut Node, targets: &[Path], method: &str, frag: Node) -> Result<()> {
    // Process targets in reverse order so earlier indices stay valid as we
    // splice; each target is resolved fresh.
    let mut ordered = targets.to_vec();
    ordered.sort();
    for path in ordered.iter().rev() {
        if path.is_empty() {
            continue;
        }
        let parent_path = &path[..path.len() - 1];
        let idx = path[path.len() - 1];
        let Some(parent) = node_at_mut(body, parent_path) else {
            continue;
        };
        let new_nodes = frag.children.clone();
        match method {
            "append" => {
                let target_children = parent.children.get_mut(idx).map(|t| &mut t.children);
                if let Some(children) = target_children {
                    children.extend(new_nodes);
                }
            }
            "before" => {
                splice_at(parent, idx, new_nodes);
            }
            "after" => {
                splice_at(parent, idx + 1, new_nodes);
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn splice_at(parent: &mut Node, idx: usize, new_nodes: Vec<Node>) {
    let idx = idx.min(parent.children.len());
    for (offset, n) in new_nodes.into_iter().enumerate() {
        parent.children.insert(idx + offset, n);
    }
}

/// Remove the nodes at `paths` (each a child path). Paths must be sorted
/// deepest-first so removal doesn't invalidate earlier indices.
fn remove_paths(body: &mut Node, paths: &[Path]) {
    for path in paths {
        if path.is_empty() {
            continue;
        }
        let parent_path = &path[..path.len() - 1];
        let idx = path[path.len() - 1];
        if let Some(parent) = node_at_mut(body, parent_path)
            && idx < parent.children.len()
        {
            parent.children.remove(idx);
        }
    }
}

fn toggle_class(el: &mut Node, cls: &str, add: bool) {
    match el.attrs.iter_mut().find(|a| a.key == "class") {
        Some(a) => {
            let mut classes: Vec<String> = a.value.split_whitespace().map(str::to_string).collect();
            if add {
                if !classes.iter().any(|c| c == cls) {
                    classes.push(cls.to_string());
                }
            } else {
                classes.retain(|c| c != cls);
            }
            a.value = classes.join(" ");
        }
        None if add => {
            el.set_attr("class", cls);
        }
        None => {}
    }
}

/// Wrap each target in the first element of the parsed `wrap` HTML. Faithful to
/// jQuery's `.wrap()`, which wraps each element (using the deepest element then
/// the outermost).
fn apply_wrap(body: &mut Node, targets: &[Path], wrap: &str) -> Result<()> {
    let frag = parse_html(wrap)?;
    let mut ordered = targets.to_vec();
    ordered.sort();
    for path in ordered.iter().rev() {
        if path.is_empty() {
            continue;
        }
        let parent_path = &path[..path.len() - 1];
        let idx = path[path.len() - 1];
        let Some(parent) = node_at_mut(body, parent_path) else {
            continue;
        };
        // Build the wrapper and move the target into its innermost child.
        let wrapper = build_wrapper(&frag);
        if let Some(target) = parent.children.get_mut(idx) {
            let target = std::mem::replace(target, Node::text(""));
            let mut w = wrapper.clone();
            innermost_mut(&mut w).children = vec![target];
            parent.children[idx] = w;
        }
    }
    Ok(())
}

/// Clone the wrapper structure (a chain of single-child elements) down to the
/// innermost leaf, mirroring jQuery's wrap (deepest-first).
fn build_wrapper(frag: &Node) -> Node {
    frag.clone()
}

fn innermost_mut(node: &mut Node) -> &mut Node {
    let mut cur = node;
    loop {
        let has = cur
            .children
            .iter()
            .any(|c| matches!(c.kind, NodeKind::Element(_)));
        if !has {
            return cur;
        }
        cur = cur
            .children
            .iter_mut()
            .find(|c| matches!(c.kind, NodeKind::Element(_)))
            .expect("has element child");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_matches_compound_selector() {
        use crate::dom::node::ElementKind;
        let li = Node::element(ElementKind::ListItem);
        assert!(matches_compound(&li, "li", 0));
        assert!(!matches_compound(&li, "p", 0));
        assert!(matches_compound(&li, "li:nth-child(3)", 2));
        assert!(!matches_compound(&li, "li:nth-child(3)", 1));
        // Class + universal + attribute.
        let mut fig = Node::element(ElementKind::Other("figcaption".into()));
        fig.set_attr("class", "mw-default-size foo");
        assert!(matches_compound(&fig, "figcaption.mw-default-size", 0));
        assert!(!matches_compound(&fig, "figcaption.mw-bogus", 0));
        assert!(matches_compound(&fig, "*.mw-default-size", 0));
        assert!(matches_compound(&fig, "*", 0));
    }

    #[test]
    fn test_universal_attr_selector() {
        use crate::dom::node::ElementKind;
        let mut span = Node::element(ElementKind::Other("span".into()));
        span.set_attr("typeof", "mw:File");
        assert!(matches_compound(&span, "*[typeof=\"mw:File\"]", 0));
        assert!(!matches_compound(&span, "*[typeof=\"mw:File/Thumb\"]", 0));
    }

    #[test]
    fn test_descendant_selector() {
        use crate::dom::node::ElementKind;
        let mut body = Node::document();
        let mut figure = Node::element(ElementKind::Other("figure".into()));
        let mut img = Node::element(ElementKind::Other("img".into()));
        img.set_attr("width", "170");
        figure.push_child(img);
        body.push_child(figure);
        let matches = find_matches(&body, "figure img");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_apply_attr_change() {
        use crate::dom::node::ElementKind;
        let mut body = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("BAR"));
        body.push_child(p);

        let changes = json!([["p", "attr", "data-x", "y"]]);
        apply_manual_changes(&mut body, &changes).unwrap();
        let p = &body.children[0];
        assert_eq!(p.get_attr("data-x"), Some("y"));
    }
}
