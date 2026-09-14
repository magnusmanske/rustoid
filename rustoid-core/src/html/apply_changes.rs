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
use crate::html::parse::{parse_fragment_in_context, parse_html};

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
                        // jQuery `.text(t)` sets `textContent`: for a *text* node
                        // (`contents()` yield) this replaces the node's data in
                        // place (the element keeps its identity); for an element
                        // (direct `text` without `contents()`) it replaces the
                        // children with a single text node.
                        match &mut el.kind {
                            NodeKind::Text(s) => *s = t.to_string(),
                            _ => {
                                el.children = vec![Node::text(t)];
                            }
                        }
                    }
                }
            }
            "html" => {
                let h = arg_str(change, method_idx + 1, "html value")?;
                for p in &targets {
                    // `$node->setAttribute`-style replacement; the fragment
                    // context is the target's own tag name.
                    let ctx = node_at_mut(body, p).map(|el| crate::html::wts_utils::node_name(el));
                    let Some(context) = ctx else { continue };
                    let new_children = parse_fragment_in_context(h, &context)?.children;
                    if let Some(el) = node_at_mut(body, p) {
                        el.children = new_children;
                    }
                }
            }
            "append" | "before" | "after" => {
                let h = arg_str(change, method_idx + 1, "insert html")?;
                apply_insertion(body, &targets, method, h)?;
            }
            "remove" => {
                // Optional selector restricting the removed set (PHP's
                // `remove( Node $node, ?string $optSelector )`).
                let opt_selector = change.get(method_idx + 1).and_then(|v| v.as_str());
                // Remove each matched node from its parent (deepest-first so
                // removal doesn't invalidate earlier indices).
                let mut paths = targets.clone();
                paths.sort_by_key(|p| std::cmp::Reverse(p.len()));
                remove_paths(body, &paths, opt_selector);
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
///
/// PHP uses `DOMCompat::querySelectorAll( $body, $selector )`, which (unlike
/// the JS `Element.querySelectorAll`) **includes the root element itself** when
/// it matches the rightmost compound. Only `body`'s own path is `[]`, so a
/// match on the root yields the empty path.
/// Find every element matching `selector` under `body`, returning their paths.
fn find_matches(body: &Node, selector: &str) -> Vec<Path> {
    let steps = parse_selector(selector);
    let mut out = Vec::new();
    let root_total = element_child_count(body);
    if matches_steps(body, &steps, Sib::new(0, root_total), &[], None) {
        out.push(Vec::new());
    }
    walk_steps(body, &steps, &mut Vec::new(), &mut Vec::new(), &mut out);
    out
}

/// A selector split into `(combinator, compound)` steps. The first step's
/// combinator is meaningless (it is always `Descendant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    /// ` ` — an ancestor anywhere above.
    Descendant,
    /// `+` — the immediately preceding element sibling.
    AdjacentSibling,
}

/// Split a selector into its compound steps, recording the combinator that
/// joins each compound to the one before it.
fn parse_selector(selector: &str) -> Vec<(Combinator, String)> {
    let mut steps: Vec<(Combinator, String)> = Vec::new();
    let mut current = String::new();
    let mut pending = Combinator::Descendant;
    for c in selector.chars() {
        if c == '+' {
            if !current.is_empty() {
                steps.push((pending, std::mem::take(&mut current)));
            }
            pending = Combinator::AdjacentSibling;
        } else if c.is_whitespace() {
            if !current.is_empty() {
                steps.push((pending, std::mem::take(&mut current)));
                // A following `+` overrides this; otherwise it is a descendant
                // combinator.
                pending = Combinator::Descendant;
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        steps.push((pending, current));
    }
    steps
}

/// Match a selector's steps against `node`, walking the ancestor stack and the
/// previous-element-sibling chain as the combinators require.
fn matches_steps(
    node: &Node,
    steps: &[(Combinator, String)],
    sib: Sib,
    ancestors: &[(&Node, Sib)],
    prev_sibling: Option<(&Node, Sib)>,
) -> bool {
    let Some(((_, last), _)) = steps.split_last() else {
        return false;
    };
    if !matches_compound(node, last, sib) {
        return false;
    }
    // Walk the remaining compounds right-to-left, following each step's
    // combinator.
    let mut ancestor_iter = ancestors.iter().rev().peekable();
    let mut sibling_cursor = prev_sibling;
    // `i` indexes a compound in `steps`, and `steps[i + 1].0` is the combinator
    // joining it to the compound on its right.
    let mut i = steps.len().saturating_sub(1);
    while i > 0 {
        i -= 1;
        let combinator = steps[i + 1].0;
        let (_, compound) = &steps[i];
        match combinator {
            Combinator::Descendant => {
                // The nearest matching ancestor, then any higher one.
                let mut matched = false;
                for (anc, anc_sib) in ancestor_iter.by_ref() {
                    if matches_compound(anc, compound, *anc_sib) {
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    return false;
                }
            }
            Combinator::AdjacentSibling => {
                // The immediately preceding element sibling must match. It shares
                // this node's parent, so its ancestor stack is the same.
                let Some((prev, prev_sib)) = sibling_cursor else {
                    return false;
                };
                if !matches_compound(prev, compound, prev_sib) {
                    return false;
                }
                sibling_cursor = None;
            }
        }
    }
    true
}

/// Descendant-combinator-only matching, retained for the compound-first fast
/// path in [`walk_steps`].
fn walk_steps<'a>(
    node: &'a Node,
    steps: &[(Combinator, String)],
    ancestors: &mut Vec<(&'a Node, Sib)>,
    path: &mut Vec<usize>,
    out: &mut Vec<Path>,
) {
    let total = element_child_count(node);
    let mut element_index = 0usize;
    let mut prev_element: Option<(&'a Node, Sib)> = None;
    for (i, child) in node.children.iter().enumerate() {
        let is_element = matches!(child.kind, NodeKind::Element(_));
        if is_element {
            let sib = Sib::new(element_index, total);
            if matches_steps(child, steps, sib, ancestors, prev_element) {
                let mut p = path.clone();
                p.push(i);
                out.push(p);
            }
            element_index += 1;
            ancestors.push((child, sib));
            path.push(i);
            walk_steps(child, steps, ancestors, path, out);
            path.pop();
            ancestors.pop();
            prev_element = Some((child, sib));
        } else {
            path.push(i);
            walk_steps(child, steps, ancestors, path, out);
            path.pop();
        }
    }
}

/// An element's position among its element siblings: `index` (0-based) and
/// `total` (the number of element siblings, including itself). Both are needed
/// for `:first-child`/`:last-child`/`:nth-child`.
#[derive(Clone, Copy)]
struct Sib {
    index: usize,
    total: usize,
}

impl Sib {
    fn new(index: usize, total: usize) -> Self {
        Self { index, total }
    }
}

/// The number of element children of `node`.
fn element_child_count(node: &Node) -> usize {
    node.children
        .iter()
        .filter(|c| matches!(c.kind, NodeKind::Element(_)))
        .count()
}

/// Match a single compound selector (e.g. `figcaption`, `.mw-default-size`,
/// `*[typeof="mw:File"]`, `li:nth-child(3)`) against a node.
fn matches_compound(node: &Node, compound: &str, sib: Sib) -> bool {
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
        return matches_pseudo(pseudo, sib);
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
fn matches_pseudo(pseudo: &str, sib: Sib) -> bool {
    let one_based = sib.index + 1;
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
        "first-child" => sib.index == 0,
        "last-child" => one_based == sib.total,
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
/// target. Faithful to the `$jquery[]` closures in PHP's `applyManualChanges`,
/// which wrap the fragment HTML in a `<div>` — or, inside a table, in a
/// `<table>`/`<tr>` so that `<td>`/`<tr>` children survive HTML5 fragment
/// parsing (whose "in body" mode drops them).
fn apply_insertion(body: &mut Node, targets: &[Path], method: &str, html: &str) -> Result<()> {
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
        // The fragment context depends on the parent element's tag name:
        //   'tbody' → parse `html` in a `<table>` and take that table's children
        //   'tr'    → parse `html` inside `<tbody><tr>…</tr></tbody>` and take
        //             the inner row's children
        //   else    → parse `html` in a plain `<div>`
        let parent_name =
            node_at_mut(body, parent_path).map(|n| crate::html::wts_utils::node_name(n));
        let new_nodes: Vec<Node> = match (method, parent_name.as_deref()) {
            ("before" | "after", Some("tbody")) => {
                // `setInnerHTML($tbl, $html)` on a fresh `<table>`; HTML5
                // synthesizes the `<tbody>`, so its children are the result.
                let tbl = parse_fragment_in_context(html, "table")?;
                tbl.children
                    .first()
                    .map(|tbody| tbody.children.clone())
                    .unwrap_or_default()
            }
            ("before" | "after", Some("tr")) => {
                // `setInnerHTML($tr, $html)` where `$tr` is an empty row of a
                // scratch table; the row's children are the result.
                parse_fragment_in_context(html, "tr")?.children
            }
            ("append", Some("tr")) => {
                // `setInnerHTML($tbl, $html)` then migrate `$tbl->firstChild`'s
                // children (the `<tbody>` HTML5 synthesizes) onto the row.
                let tbl = parse_fragment_in_context(html, "table")?;
                tbl.children
                    .first()
                    .map(|tbody| tbody.children.clone())
                    .unwrap_or_default()
            }
            _ => parse_fragment_in_context(html, "div")?.children,
        };
        let Some(parent) = node_at_mut(body, parent_path) else {
            continue;
        };
        match method {
            "append" => {
                if let Some(target) = parent.children.get_mut(idx) {
                    target.children.extend(new_nodes);
                }
            }
            "before" => splice_at(parent, idx, new_nodes),
            "after" => splice_at(parent, idx + 1, new_nodes),
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
///
/// `opt_selector` mirrors PHP's optional `$optSelector`: when present, an
/// additional `querySelectorAll` runs on the matched node, and — because text
/// nodes have no `querySelectorAll` in PHP — **any non-element node is kept**
/// unconditionally (the "text node hack!" in `Test::applyManualChanges`).
fn remove_paths(body: &mut Node, paths: &[Path], opt_selector: Option<&str>) {
    for path in paths {
        if path.is_empty() {
            continue;
        }
        if let Some(sel) = opt_selector {
            // Only elements are subject to the nested selector; text/comment
            // nodes sail through the hack and are removed unconditionally.
            let is_element =
                node_at_mut(body, path).is_some_and(|n| matches!(n.kind, NodeKind::Element(_)));
            if is_element
                && let Some(node) = node_at_mut(body, path)
                && find_matches(node, sel).is_empty()
            {
                continue;
            }
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
    fn test_adjacent_sibling_selector() {
        use crate::dom::node::ElementKind;
        // <tr><td>a</td><td>b</td><td>c</td></tr>
        let mut tr = Node::element(ElementKind::TableRow);
        for t in ["a", "b", "c"] {
            let mut td = Node::element(ElementKind::TableCell);
            td.push_child(Node::text(t));
            tr.push_child(td);
        }
        let mut doc = Node::document();
        doc.push_child(tr);

        // `td + td` matches every cell but the first.
        let paths = find_matches(&doc, "td + td");
        assert_eq!(paths.len(), 2, "{paths:?}");
        // Each match resolves to the 2nd and 3rd cell respectively.
        let names: Vec<&str> = paths
            .iter()
            .map(|p| {
                let td = p.iter().fold(&doc, |n, i| &n.children[*i]);
                match &td.children.first().map(|c| &c.kind) {
                    Some(NodeKind::Text(t)) => t.as_str(),
                    _ => "?",
                }
            })
            .collect();
        assert_eq!(names, vec!["b", "c"]);

        // A non-adjacent pair does not match, and a plain `td` matches all.
        assert!(find_matches(&doc, "td + p").is_empty());
        assert_eq!(find_matches(&doc, "td").len(), 3);
    }

    #[test]
    fn test_matches_compound_selector() {
        use crate::dom::node::ElementKind;
        let li = Node::element(ElementKind::ListItem);
        assert!(matches_compound(&li, "li", Sib::new(0, 8)));
        assert!(!matches_compound(&li, "p", Sib::new(0, 8)));
        assert!(matches_compound(&li, "li:nth-child(3)", Sib::new(2, 8)));
        assert!(!matches_compound(&li, "li:nth-child(3)", Sib::new(1, 8)));
        // Class + universal + attribute.
        let mut fig = Node::element(ElementKind::Other("figcaption".into()));
        fig.set_attr("class", "mw-default-size foo");
        assert!(matches_compound(
            &fig,
            "figcaption.mw-default-size",
            Sib::new(0, 8)
        ));
        assert!(!matches_compound(
            &fig,
            "figcaption.mw-bogus",
            Sib::new(0, 8)
        ));
        assert!(matches_compound(&fig, "*.mw-default-size", Sib::new(0, 8)));
        assert!(matches_compound(&fig, "*", Sib::new(0, 8)));
    }

    #[test]
    fn test_universal_attr_selector() {
        use crate::dom::node::ElementKind;
        let mut span = Node::element(ElementKind::Other("span".into()));
        span.set_attr("typeof", "mw:File");
        assert!(matches_compound(
            &span,
            "*[typeof=\"mw:File\"]",
            Sib::new(0, 8)
        ));
        assert!(!matches_compound(
            &span,
            "*[typeof=\"mw:File/Thumb\"]",
            Sib::new(0, 8)
        ));
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
