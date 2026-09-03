//! WikitextSerializer — faithful port of PHP Parsoid's
//! `src/Html2Wt/WikitextSerializer.php`.
//!
//! The orchestrator of html2wt serialization: it walks the DOM (via
//! [`SerializerState`] and the [`DomTree`] arena), delegating each element to
//! the [`DomHandler`](./dom_handler) chosen by [`get_dom_handler`], and emits
//! the result into `SerializerState::out`.
//!
//! This module is being ported bottom-up; the `walk_children` spine and the
//! `serialize_html_tag`/`serialize_html_end_tag` helpers are provided first so
//! the simple concrete handlers (`BodyHandler`, `JustChildrenHandler`,
//! `QuoteHandler`) can be exercised. The deep methods (`escapeWikitext`,
//! `serializeAttributes`, `serializeDOM`) are stubbed and layered on with
//! `WikitextEscapeHandlers`.

use crate::dom::node::{ElementKind, NodeKind};
use crate::html::dom_handler_factory::get_dom_handler;
use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::dom_utils;
use crate::html::serializer_state::SerializerState;

/// Walk the children of `node`, serializing each via its handler. This is the
/// shared spine for `SerializerState::serializeChildren` and stands in for the
/// `WikitextSerializer::serializeNode` walk (which handles text/comment
/// merging, separators, and selser) until that full walk is ported.
pub fn walk_children(tree: &DomTree, node: NodeId, state: &mut SerializerState) {
    let mut child = tree.first_child(node);
    while let Some(c) = child {
        serialize_node(tree, c, state);
        child = tree.next_sibling(c);
    }
}

/// Serialize a text node's value, splitting off a trailing `\n`-run into the
/// separator buffer and a leading `\n`-run out of it. Faithful to
/// `WikitextSerializer::serializeText` (the `SEPARATOR_SUFFIX_WITH_NLS_RE` /
/// `SEPARATOR_PREFIX_WITH_NLS_RE` splitting), which is what lets a table cell's
/// trailing `\n ` (`[1]\n `) round-trip as the `\n ` separator before the next
/// cell rather than as inline content.
pub fn serialize_text(text: &str, node: NodeId, tree: &DomTree, state: &mut SerializerState) {
    // `SEPARATOR_SUFFIX_WITH_NLS_RE = /\n[ \t\r\n]*$/D`: a trailing newline run.
    let mut body = text;
    let trailing = match body.rfind('\n') {
        Some(i)
            if body[i + 1..]
                .chars()
                .all(|c| c == ' ' || c == '\t' || c == '\r') =>
        {
            let tail = &body[i..];
            body = &body[..i];
            tail
        }
        _ => "",
    };

    // `SEPARATOR_PREFIX_WITH_NLS_RE = /^[ \t]*\n+[ \t\r\n]*/`: a leading newline run.
    if !state.in_indent_pre
        && let Some(idx) = first_nl_run_end(body)
    {
        state.append_sep(&body[..idx]);
        body = &body[idx..];
    }

    // Emit the (now separator-stripped) body.
    state.emit_chunk(body, node, tree);

    // Move the trailing newline run into the next separator (`$newSepMatch`).
    if !trailing.is_empty() && state.separator.src.is_none() {
        state.append_sep(trailing);
    }
}

/// Byte index just past the leading `[ \t]*\n+[ \t\r\n]*` run of `s`, or `None` if
/// there is no such leading run. Faithful to `SEPARATOR_PREFIX_WITH_NLS_RE`.
fn first_nl_run_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    let nl_start = i;
    while i < bytes.len() && matches!(bytes[i], b'\n' | b'\r') {
        i += 1;
    }
    if i == nl_start {
        return None; // no newline after the leading spaces/tabs
    }
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    Some(i)
}

/// Serialize a single node, delegating to its handler (or emitting text). This
/// is the faithful `WikitextSerializer::serializeNode` (non-selser): it handles
/// the text/comment/diff-marker branches, computes separator constraints before
/// and after each element, and delegates elements to the handler chosen by the
/// factory.
pub fn serialize_node(tree: &DomTree, node: NodeId, state: &mut SerializerState) {
    use crate::html::separators::Separators;

    let n = tree.node(node);
    match &n.kind {
        // Text: accumulate pure whitespace into the separator, else emit
        // (splitting a trailing `\n`-run back into the separator, mirroring
        // `serializeTextNode` / `serializeText`).
        NodeKind::Text(text) => {
            if !state.in_indent_pre && text.chars().all(|c| c.is_whitespace()) {
                state.append_sep(text);
            } else {
                state.needs_escaping = true;
                state.is_last_child =
                    crate::html::dom_tree::next_non_deleted_sibling(tree, node).is_none();
                serialize_text(text, node, tree, state);
                state.needs_escaping = false;
            }
        }
        // Comment: merge its wikitext form into the separator source.
        NodeKind::Comment(content) => {
            state.append_sep(&crate::html::wts_utils::comment_wt(content));
        }
        // Element: compute separator constraints and delegate to the handler.
        NodeKind::Element(_) | NodeKind::Document => {
            state.curr_node = Some(node);

            // Selser: ignore diff-marker metas, but clear unmodified-node state
            // (faithful to `serializeNode`'s diff-marker branch).
            if crate::html::diff_utils::DiffUtils::is_diff_marker(n, None) {
                state.update_modification_flags(node);
                state.update_sep(node);
                return;
            }

            // Selser: reuse the original wikitext source for an unmodified node
            // whose DSR is valid in the edited context (faithful to
            // `serializeNodeInternal`).
            let reused = state.selser_mode
                && !state.in_inserted_content
                && crate::html::wts_utils::orig_src_valid_in_edited_context(state, tree, node)
                && crate::html::wts_utils::get_dsr(n).is_some_and(|dsr| {
                    crate::html::dsr::is_valid_dsr(Some(&dsr), false)
                        && reuse_orig_src_has_positive_width(n, &dsr)
                });
            if reused {
                let unmodified = !crate::html::diff_utils::DiffUtils::has_diff_markers(n);
                if unmodified && let Some(dsr) = crate::html::wts_utils::get_dsr(n) {
                    state.curr_node_unmodified = true;

                    // `getOrigSrc` returns `None` only when no selser_data is
                    // configured; in that case fall through to the handler.
                    if let Some(out) = state.get_orig_src(&dsr.outer_range()) {
                        let suppress_slc =
                            crate::html::wts_utils::is_first_encapsulation_wrapper_node(n)
                                || crate::html::dom_utils::has_type_of(n, "mw:Nowiki")
                                || matches!(
                                    crate::html::wts_utils::node_name(n).as_str(),
                                    "dl" | "ul" | "ol" | "a"
                                );

                        if suppress_slc {
                            state.single_line_context.disable();
                        }
                        let env = state.env;
                        for ct in crate::html::constrained_text::from_sel_ser(
                            tree,
                            node,
                            &out,
                            env,
                            crate::html::constrained_text::FromSelSerOpts::default(),
                        ) {
                            state.emit_chunk(ct.text.clone(), ct.node, tree);
                        }
                        if suppress_slc {
                            state.single_line_context.pop();
                        }

                        return;
                    }
                    state.curr_node_unmodified = false;
                }
            }

            // Non-selser (or modified) path.
            state.curr_node_unmodified = false;

            // Before-constraints: prev non-sep sibling, or parent.
            let prev = crate::html::dom_tree::previous_non_sep_sibling(tree, node)
                .or_else(|| tree.parent(node));
            if let Some(prev) = prev {
                let mut prev_handler = get_dom_handler(tree, prev);
                let mut handler = get_dom_handler(tree, node);
                Separators::update_separator_constraints(
                    state,
                    tree,
                    prev,
                    prev_handler.as_mut(),
                    node,
                    handler.as_mut(),
                );
            }

            let mut handler = get_dom_handler(tree, node);
            handler.handle(tree, node, state);

            // After-constraints: next non-sep sibling, else parent.
            let next = crate::html::dom_tree::next_non_sep_sibling(tree, node)
                .or_else(|| tree.parent(node));
            if let Some(next) = next {
                let mut next_handler = get_dom_handler(tree, next);
                let mut handler = get_dom_handler(tree, node);
                Separators::update_separator_constraints(
                    state,
                    tree,
                    node,
                    handler.as_mut(),
                    next,
                    next_handler.as_mut(),
                );
            }

            state.update_modification_flags(node);
        }
    }
}

/// Faithful to the positive-width gate in `serializeNodeInternal`: the DSR must
/// have `end > start`, or (when width is zero) the node must be a `p`/`br`, an
/// auto-generated element, an `mw:Placeholder/StrippedTag`, fostered, or
/// misnested content.
fn reuse_orig_src_has_positive_width(
    node: &crate::dom::node::Node,
    dsr: &crate::html::dsr::DomSourceRange,
) -> bool {
    let (Some(start), Some(end)) = (dsr.start, dsr.end) else {
        return false;
    };
    if end > start {
        return true;
    }
    // Zero-width: allow the specific cases.
    let name = crate::html::wts_utils::node_name(node);
    if name == "p" || name == "br" {
        return true;
    }
    if node
        .data_mw
        .as_deref()
        .is_some_and(|dm| dm.contains("\"autoGenerated\":true"))
    {
        return true;
    }
    if crate::html::dom_utils::has_type_of(node, "mw:Placeholder/StrippedTag") {
        return true;
    }
    let dp = node.dp.as_ref();
    dp.is_some_and(|d| d.fostered || d.misnested.unwrap_or(false))
}

/// A minimal `WikitextSerializer` structure (the methods the handlers reference
/// are added in later modules; the walk is `walk_children`/`serialize_node`).
pub struct WikitextSerializer;

impl WikitextSerializer {
    /// Serialize the opening HTML tag for a literal-HTML node. Faithful to the
    /// non-selser, non-`autoInsertedStart` skeleton of `serializeHTMLTag`.
    pub fn serialize_html_tag(
        node: &crate::dom::node::Node,
        env: Option<crate::html::env::SerializerEnv>,
    ) -> String {
        let name = crate::html::wts_utils::node_name(node);
        let attrs = serialize_attributes(node, env);
        let suffix = if attrs.is_empty() {
            String::new()
        } else {
            format!(" {attrs}")
        };
        format!("<{name}{suffix}>")
    }

    /// Serialize the closing HTML tag for a literal-HTML node. Faithful to the
    /// non-`autoInsertedEnd` skeleton of `serializeHTMLEndTag`.
    pub fn serialize_html_end_tag(node: &crate::dom::node::Node) -> String {
        let name = crate::html::wts_utils::node_name(node);
        format!("</{name}>")
    }

    /// Serialize an owned DOM (`Node`) tree to wikitext, using the handler
    /// dispatch and the `SerializerState` walk. Faithful skeleton of
    /// `WikitextSerializer::serializeDOM` (no `DOMNormalizer`, no selser).
    ///
    /// This variant carries no environment, so link/media handlers fall back to
    /// literal HTML; use [`serialize_dom_with_env`] for full link serialization.
    pub fn serialize_dom(root: crate::dom::node::Node) -> String {
        Self::serialize_dom_internal(root, None, false, None, false)
    }

    /// Serialize with a [`SerializerEnv`] so link/media handlers can run.
    pub fn serialize_dom_with_env(
        root: crate::dom::node::Node,
        env: crate::html::env::SerializerEnv,
    ) -> String {
        Self::serialize_dom_internal(root, Some(env), false, None, false)
    }

    /// Serialize in selective-serialization (selser) mode: unmodified nodes with
    /// valid DSR reuse their original wikitext source instead of being
    /// re-serialized. Faithful to `serializeDOM($node, $selserMode = true)`.
    ///
    /// `original_wikitext` populates `SerializerState::selser_data` (the `revText`
    /// that `getOrigSrc` reads).
    pub fn serialize_dom_selser(
        root: crate::dom::node::Node,
        env: Option<crate::html::env::SerializerEnv>,
        original_wikitext: &str,
    ) -> String {
        Self::serialize_dom_internal(root, env, true, Some(original_wikitext), false)
    }

    /// Serialize a sub-DOM in attribute context (`domToWikitext` with
    /// `onSOL => false` + `inAttribute => true`): used to reconstruct a
    /// template-generated attribute key/value from its `data-mw.attribs` `html`.
    pub fn dom_to_wikitext_in_attribute(
        root: crate::dom::node::Node,
        env: crate::html::env::SerializerEnv,
    ) -> String {
        Self::serialize_dom_internal(root, Some(env), false, None, true)
    }

    /// The shared `serializeDOM` implementation. `selser` toggles the reuse-
    /// original-source branch; `selser_data` carries the revision wikitext for
    /// that branch. `in_attribute` serializes the fragment as a template-generated
    /// attribute value (`onSOL = false`, `inAttribute = true`).
    fn serialize_dom_internal(
        root: crate::dom::node::Node,
        env: Option<crate::html::env::SerializerEnv>,
        selser: bool,
        selser_rev_text: Option<&str>,
        in_attribute: bool,
    ) -> String {
        let mut root = root;
        // DOM normalization (quote-tag minimization / empty-tag stripping) runs
        // before serialization, faithfully mirroring `serializeDOM`'s
        // `DOMNormalizer::normalize` call. (Selser diff-marker bookkeeping is
        // layered on once the selser pipeline is fully wired.)
        crate::html::dom_normalizer::normalize(&mut root);
        let tree = DomTree::new(root);
        let root_id = tree.root();
        let mut state = match env {
            Some(env) => SerializerState::with_env(env),
            None => SerializerState::new(),
        };
        if selser {
            state.init_mode(true);
            state.selser_data =
                selser_rev_text.map(|t| crate::html::dsr::SelectiveUpdateData::new(t.to_string()));
        }
        if in_attribute {
            state.in_attribute = true;
            state.on_sol = false;
            state.at_start_of_output = false;
        }
        // Serialize the body content: `serializeDOM` extracts `<body>` (PHP's
        // `DOMCompat::getBody`), so skip a synthetic `<html>`/`<body>` wrapper
        // produced by our fragment-mode tree builder and serialize its children.
        let body = body_content_node(&tree, root_id);
        crate::html::serializer::walk_children(&tree, body, &mut state);
        state.flush_line();
        if let Some(redirect) = state.redirect_text.clone() {
            format!("{redirect}\n{}", state.out)
        } else {
            state.out
        }
    }
}

/// Resolve the node whose children are the document body content, mirroring
/// PHP's `WikitextSerializer::serializeDOM`, which extracts `<body>` from a
/// `Document` and otherwise serializes a `DocumentFragment` directly.
///
/// Our fragment-mode tree builder emits a synthetic `<html>` wrapper (see
/// `html5/tree_builder.rs::start_document`); both it and a literal `<body>` are
/// transparent for serialization, so we descend into the single such wrapper
/// and return it (or the root if there is none).
fn body_content_node(tree: &DomTree, root: NodeId) -> NodeId {
    if let Some(child) = tree.first_child(root)
        && let NodeKind::Element(kind) = &tree.node(child).kind
    {
        // Only treat it as structural if it is the sole child (a synthetic
        // wrapper has no siblings). PHP's `getBody` returns the body; a real
        // comment/text sibling would not be the body wrapper.
        let is_wrapper = matches!(
            kind,
            ElementKind::Other(tag) if tag == "html" || tag == "body"
        );
        if is_wrapper && tree.next_sibling(child).is_none() {
            return child;
        }
    }
    root
}

/// Serialize an element's attributes to an HTML attribute string. This is the
/// faithful port of `serializeAttributes`: `data-parsoid`/`data-mw`/RDFa are
/// stripped, node-data-id and auto-generated heading ids are dropped unless
/// reused, `mw-empty-elt` is stripped from flagged empty elements, and
/// template-generated keys/values are reconstructed from `data-mw.attribs`.
pub fn serialize_attributes_partial(
    node: &crate::dom::node::Node,
    env: Option<crate::html::env::SerializerEnv>,
) -> String {
    serialize_attributes(node, env)
}

fn serialize_attributes(
    node: &crate::dom::node::Node,
    env: Option<crate::html::env::SerializerEnv>,
) -> String {
    let mut out: Vec<String> = Vec::new();

    for attr in &node.attrs {
        let k = attr.key.as_str();
        let v = attr.value.as_str();

        // Unconditionally ignore (mirrors `IGNORED_ATTRIBUTES` + `data-mw`).
        if matches!(
            k,
            "data-parsoid"
                | "data-mw"
                | "data-ve-changed"
                | "data-parsoid-changed"
                | "data-parsoid-diff"
                | "data-parsoid-serialize"
                | "data-object-id"
        ) {
            continue;
        }

        // Ignore parsoid-like ids (`^mw[\w-]{2,}$`). They may have been left
        // behind by clients and shouldn't be serialized. Re-emit only when the
        // id was recovered from source and unmodified (faithful to
        // `CounterType::NODE_DATA_ID->matches`).
        if k == "id" && is_node_data_id(v) {
            if !node.dp.as_ref().is_none_or(|dp| dp.dsr.is_none()) {
                let v_info = crate::html::wts_utils::get_shadow_info(node, k, Some(v));
                if !v_info.modified && v_info.fromsrc && !v_info.value.is_empty() {
                    out.push(format!("id=\"{}\"", v_info.value.replace('"', "&quot;")));
                }
            }
            continue;
        }

        // Parsoid auto-generates ids for headings and they should be stripped,
        // except if this is a non-auto-generated (`reusedId`) id.
        if k == "id" && dom_utils::is_heading(node) {
            if node.dp.as_ref().and_then(|d| d.reused_id).unwrap_or(false) {
                let v_info = crate::html::wts_utils::get_shadow_info(node, k, Some(v));
                out.push(format!("id=\"{}\"", v_info.value.replace('"', "&quot;")));
            }
            continue;
        }

        // Strip Parsoid-inserted `class="mw-empty-elt"` markers (only for
        // flagged empty elements — `li`/`tbody`/`tr`/`p`).
        if k == "class"
            && crate::wikitext::consts::flagged_empty_elts()
                .contains(&crate::html::wts_utils::node_name(node))
        {
            let stripped = strip_mw_empty_elt_once(v);
            if stripped.is_empty() {
                continue;
            }
            if stripped != v {
                out.push(format!("class=\"{}\"", stripped.replace('"', "&quot;")));
                continue;
            }
        }

        // Strip other Parsoid-generated values (`about="#mwtN"`, `typeof` `mw:…`
        // tokens). Mirrors `$parsoidAttributes`.
        let parsoid_stripped: Option<String> = if k == "about" && is_transclusion_about(v) {
            Some(String::new())
        } else if k == "typeof" {
            let remaining: String = v
                .split_whitespace()
                .filter(|t| !t.starts_with("mw:"))
                .collect::<Vec<_>>()
                .join(" ");
            (remaining != v).then_some(remaining)
        } else {
            None
        };
        if let Some(rv) = parsoid_stripped {
            if !rv.is_empty() {
                out.push(format!("{k}=\"{}\"", rv.replace('"', "&quot;")));
            }
            continue;
        }

        // Regular attribute: honor shadow info (`a`/`sa`) plus template-generated
        // key/value reconstruction (`data-mw.attribs` `key.html`/`value.html`).
        let shadow = crate::html::wts_utils::get_shadow_info(node, k, Some(v));
        let value = shadow.value.as_str();
        let kk = get_attribute_key(node, k, env);
        let vv = get_attribute_value(node, k, env).unwrap_or_else(|| value.to_string());
        let kk = strip_data_x_prefix(&kk).to_string();
        if !vv.is_empty() {
            if !shadow.fromsrc {
                // Escaped from loaded attr value (not original source). Comments
                // and annotation tags embedded in the value are left unescaped.
                let escaped = escape_non_comment_segments(&vv);
                out.push(format!("{kk}=\"{escaped}\""));
            } else {
                out.push(format!("{kk}=\"{}\"", vv.replace('"', "&quot;")));
            }
        } else if kk.contains('{') || kk.contains('<') {
            // Templated / include / ext-tag generated attribute key.
            out.push(kk);
        } else {
            out.push(format!("{kk}=\"\""));
        }
    }

    // Sanitized-away attributes (`dataParsoid->a` / `dataParsoid->sa`): restore
    // any attribute present in the shadow maps but absent from the DOM (mirrors
    // the trailing recovery loop in PHP's `serializeAttributes`).
    if let Some(dp) = node.dp.as_ref()
        && let (Some(a), Some(sa)) = (dp.a.as_ref(), dp.sa.as_ref())
    {
        let mut keys: Vec<&String> = a.keys().collect();
        keys.sort();
        for key in keys {
            if node.get_attr(key).is_none() {
                if let Some(sv) = sa.get(key)
                    && !sv.is_empty()
                {
                    out.push(format!("{key}=\"{}\"", sv.replace('"', "&quot;")));
                } else {
                    out.push(key.to_string());
                }
            }
        }
    }

    out.join(" ")
}

/// `WikitextSerializer::getAttributeKey` — reconstruct a template-generated
/// attribute key from `data-mw.attribs[i].key.html`. Returns the reconstructed
/// wikitext, or `key` unchanged when there is no matching generated key.
fn get_attribute_key(
    node: &crate::dom::node::Node,
    key: &str,
    env: Option<crate::html::env::SerializerEnv>,
) -> String {
    for (k_str, k_html) in attribs_key_value_html(node) {
        if k_str == key
            && let Some(html) = k_html
        {
            return dom_to_wikitext_from_html(html, env);
        }
    }
    key.to_string()
}

/// `WikitextSerializer::getAttributeValue` — reconstruct a template-generated
/// attribute *value* from `data-mw.attribs[i].value.html`. Returns `None` when
/// there is no matching generated value (so the caller falls back to the shadow
/// value).
fn get_attribute_value(
    node: &crate::dom::node::Node,
    key: &str,
    env: Option<crate::html::env::SerializerEnv>,
) -> Option<String> {
    for (k_str, v_html) in attribs_value_html(node) {
        if k_str == key
            && let Some(html) = v_html
        {
            return Some(dom_to_wikitext_from_html(html, env));
        }
    }
    None
}

/// Iterate over `data-mw.attribs`, yielding `(keyString, key.html?)` for each
/// entry (mirrors `getAttributeKey` walking `$tplAttrs`).
fn attribs_key_value_html(
    node: &crate::dom::node::Node,
) -> impl Iterator<Item = (String, Option<String>)> {
    let attribs = parse_attribs(node);
    attribs.into_iter().filter_map(|entry| {
        let mut kv = entry.as_array()?.iter();
        let key_obj = kv.next()?;
        let key_str = data_mw_value_txt(key_obj)?;
        let key_html = data_mw_value_html(key_obj);
        Some((key_str, key_html))
    })
}

/// Iterate over `data-mw.attribs`, yielding `(keyString, value.html?)` for each
/// entry (mirrors `getAttributeValue` walking `$tplAttrs`).
fn attribs_value_html(
    node: &crate::dom::node::Node,
) -> impl Iterator<Item = (String, Option<String>)> {
    let attribs = parse_attribs(node);
    attribs.into_iter().filter_map(|entry| {
        let mut kv = entry.as_array()?.iter();
        let key_obj = kv.next()?;
        let value_obj = kv.next()?;
        let key_str = data_mw_value_txt(key_obj)?;
        let value_html = data_mw_value_html(value_obj);
        Some((key_str, value_html))
    })
}

/// Parse `node.data_mw` (a raw JSON string) and return its `attribs` array (an
/// array of `[key, value]` pairs). Empty when absent/malformed.
fn parse_attribs(node: &crate::dom::node::Node) -> Vec<serde_json::Value> {
    let Some(dm) = node.data_mw.as_deref() else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(dm) else {
        return Vec::new();
    };
    json.get("attribs")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
}

/// The plain-text key string of a `data-mw.attribs` key object (`getKeyString`).
fn data_mw_value_txt(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(o) => o.get("txt").and_then(|t| t.as_str()).map(str::to_string),
        _ => None,
    }
}

/// The `html` field of a `data-mw.attribs` key/value object, if present.
fn data_mw_value_html(v: &serde_json::Value) -> Option<String> {
    v.get("html").and_then(|h| h.as_str()).map(str::to_string)
}

/// Serialize a template-generated attribute key/value `html` fragment back to
/// wikitext (`domToWikitext` with `onSOL => false` + `inAttribute => true`).
fn dom_to_wikitext_from_html(html: String, env: Option<crate::html::env::SerializerEnv>) -> String {
    let Ok(root) = crate::html::parse::parse_html(&html) else {
        return String::new();
    };
    match env {
        Some(env) => WikitextSerializer::dom_to_wikitext_in_attribute(root, env),
        None => WikitextSerializer::serialize_dom(root),
    }
}

/// Remove a leading `data-x-` prefix (case-insensitive), mirroring PHP's
/// `preg_replace('/^data-x-/i', '', $kk, 1)`.
fn strip_data_x_prefix(kk: &str) -> &str {
    const PREFIX: &str = "data-x-";
    if kk.len() >= PREFIX.len() && kk[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        &kk[PREFIX.len()..]
    } else {
        kk
    }
}

/// Escape the non-comment/non-annotation segments of an attribute value that
/// does *not* come from source (`fromsrc == false`), leaving wikitext comments
/// (`<!-- … -->`) and annotation tags untouched. Faithful to PHP's
/// `preg_split(commentsOrAnnotationsRE, …, PREG_SPLIT_DELIM_CAPTURE)` then
/// escaping only the even (non-delimiter) segments via `escapeWtEntities` +
/// `>`/`"` replacement.
fn escape_non_comment_segments(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("<!--") {
        // Emit the pre-comment segment (escaped).
        out.push_str(&escape_attr_segment(&rest[..start]));
        // Find the comment end; emit the comment unescaped.
        match rest[start..].find("-->") {
            Some(rel_end) => {
                let abs_end = start + rel_end + 3;
                out.push_str(&rest[start..abs_end]);
                rest = &rest[abs_end..];
            }
            None => {
                // Unterminated comment: emit the rest unescaped and stop.
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(&escape_attr_segment(rest));
    out
}

/// Escape a single non-comment attribute segment: `>` → `&gt;`, `"` → `&quot;`
/// (the `escapeWtEntities` entity-escape is a no-op here since we don't
/// entity-decode).
fn escape_attr_segment(seg: &str) -> String {
    seg.replace('>', "&gt;").replace('"', "&quot;")
}

/// `CounterType::NODE_DATA_ID::matches` → `/^mw[\w-]{2,}$/D` (the `mw` prefix
/// followed by at least two word-/hyphen characters, e.g. `mw-xy`).
fn is_node_data_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if !id.starts_with("mw") {
        return false;
    }
    bytes[2..].len() >= 2
        && bytes[2..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

/// `CounterType::TRANSCLUSION_ABOUT::matches` → `/^#mwt\d+$/D`: an `about` value
/// that is exactly `#mwt` followed by digits (the transclusion id).
fn is_transclusion_about(v: &str) -> bool {
    let Some(rest) = v.strip_prefix("#mwt") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Remove a single `mw-empty-elt` class token (with an optional adjacent
/// space), mirroring `preg_replace('/\b ?mw-empty-elt\b/', '', $v, 1)`.
fn strip_mw_empty_elt_once(v: &str) -> String {
    let Some(start) = v.find("mw-empty-elt") else {
        return v.to_string();
    };
    // Extend back over a single preceding space (word boundary already ensured
    // by the token being boxed by non-`\w` characters in practice).
    let mut s = start;
    if s > 0 && &v[s - 1..s] == " " {
        s -= 1;
    }
    let end = start + "mw-empty-elt".len();
    format!("{}{}", &v[..s], &v[end..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_serialize_html_tag() {
        // `mw-empty-elt` is only stripped from flagged empty elements (`p`,
        // `li`, `tr`, `tbody`). A `p` strips its `class="mw-empty-elt"`; other
        // data-* attributes are always ignored.
        let mut p = Node::element(ElementKind::Paragraph);
        p.set_attr("class", "mw-empty-elt");
        p.set_attr("style", "color:red");
        assert_eq!(
            WikitextSerializer::serialize_html_tag(&p, None),
            "<p style=\"color:red\">"
        );

        // A `div` is not a flagged empty element, so its `mw-empty-elt` class
        // is preserved (faithful to `FlaggedEmptyElts`).
        let mut div = Node::element(ElementKind::Div);
        div.set_attr("class", "mw-empty-elt");
        assert_eq!(
            WikitextSerializer::serialize_html_tag(&div, None),
            "<div class=\"mw-empty-elt\">"
        );
    }

    #[test]
    fn test_serialize_attributes_restores_sanitized_attr() {
        // An attribute that was sanitized away from the DOM but is present in
        // `dataParsoid->a`/`sa` is restored from its source value.
        let mut div = Node::element(ElementKind::Div);
        div.set_attr("style", "color:red");
        let mut a = std::collections::HashMap::new();
        a.insert("align".to_string(), "left".to_string());
        let mut sa = std::collections::HashMap::new();
        sa.insert("align".to_string(), "left".to_string());
        div.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            a: Some(a),
            sa: Some(sa),
            ..Default::default()
        });
        assert_eq!(
            serialize_attributes(&div, None),
            "style=\"color:red\" align=\"left\""
        );
    }

    #[test]
    fn test_parse_attribs_and_data_mw_helpers() {
        // `data-mw.attribs` is a JSON array of `[key, value]` pairs where each
        // key/value is a string or `{txt, html, uneditable}` object.
        let dm = r#"{"attribs":[[{"txt":"style","html":"<b>x</b>"},{"html":""}]]}"#;
        let mut div = Node::element(ElementKind::Div);
        div.data_mw = Some(dm.to_string());

        let attribs = parse_attribs(&div);
        assert_eq!(attribs.len(), 1);

        // keyString = `.txt`; key.html / value.html are extracted separately.
        let keys: Vec<(String, Option<String>)> = attribs_key_value_html(&div).collect();
        assert_eq!(keys[0].0, "style");
        assert_eq!(keys[0].1.as_deref(), Some("<b>x</b>"));

        let vals: Vec<(String, Option<String>)> = attribs_value_html(&div).collect();
        assert_eq!(vals[0].0, "style");
        assert_eq!(vals[0].1.as_deref(), Some(""));
    }

    #[test]
    fn test_strip_data_x_prefix() {
        assert_eq!(strip_data_x_prefix("data-x-foo"), "foo");
        assert_eq!(strip_data_x_prefix("DATA-X-foo"), "foo");
        assert_eq!(strip_data_x_prefix("foo"), "foo");
    }

    #[test]
    fn test_is_node_data_id() {
        assert!(is_node_data_id("mw-xy"));
        assert!(is_node_data_id("mwA1"));
        assert!(is_node_data_id("mw__"));
        assert!(is_node_data_id("mw-x")); // two trailing chars ("-x")
        assert!(!is_node_data_id("mwx")); // only one trailing char
        assert!(!is_node_data_id("x-mw-xy"));
    }

    #[test]
    fn test_is_transclusion_about() {
        assert!(is_transclusion_about("#mwt1"));
        assert!(is_transclusion_about("#mwt123"));
        assert!(!is_transclusion_about("#mwt"));
        assert!(!is_transclusion_about("#mwt1 foo"));
        assert!(!is_transclusion_about("#mwtx"));
    }

    #[test]
    fn test_walk_children_emits_text() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("hi"));
        doc.push_child(p);

        let tree = DomTree::new(doc);
        let p_id = tree.first_child(tree.root()).unwrap();
        // Walking the paragraph's children buffers the text on the current line.
        let mut state = SerializerState::new();
        walk_children(&tree, p_id, &mut state);
        // The faithful path buffers into `curr_line` and flushes at line end.
        assert_eq!(state.curr_line.text, "hi");
        state.flush_line();
        assert_eq!(state.out, "hi");
    }

    #[test]
    fn test_serialize_dom_heading() {
        let mut doc = Node::document();
        let mut h2 = Node::element(ElementKind::Heading(2));
        h2.push_child(Node::text("foo"));
        h2.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(0),
                end: Some(7),
                open_width: Some(2),
                close_width: Some(2),
            }),
            ..Default::default()
        });
        doc.push_child(h2);

        let wt = WikitextSerializer::serialize_dom(doc);
        assert_eq!(wt, "==foo==");
    }

    #[test]
    fn test_serialize_dom_italic() {
        let mut doc = Node::document();
        let mut i = Node::element(ElementKind::Italic);
        i.push_child(Node::text("foo"));
        doc.push_child(i);

        let wt = WikitextSerializer::serialize_dom(doc);
        assert_eq!(wt, "''foo''");
    }

    #[test]
    fn test_serialize_dom_two_paragraphs() {
        // <p>a</p><p>b</p> → "a\n\nb" (paragraphs are separated by two newlines).
        let mut doc = Node::document();
        let mut p1 = Node::element(ElementKind::Paragraph);
        p1.push_child(Node::text("a"));
        let mut p2 = Node::element(ElementKind::Paragraph);
        p2.push_child(Node::text("b"));
        doc.push_child(p1);
        doc.push_child(p2);

        let wt = WikitextSerializer::serialize_dom(doc);
        assert_eq!(wt, "a\n\nb");
    }

    #[test]
    fn test_serialize_dom_escapes_sol_markup() {
        // A text node whose first char is SOL-sensitive (`*`) must be protected
        // so it serializes back to the same literal text rather than a list.
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("*foo"));
        doc.push_child(p);

        let wt = WikitextSerializer::serialize_dom(doc);
        assert_eq!(wt, "<nowiki/>*foo");
    }

    #[test]
    fn test_serialize_dom_escapes_transclusion() {
        // `{{foo}}` serializes as text, so it must be nowiki-protected to avoid
        // re-parsing as a template transclusion.
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("{{foo}}"));
        doc.push_child(p);

        let wt = WikitextSerializer::serialize_dom(doc);
        assert_eq!(wt, "<nowiki>{{foo}}</nowiki>");
    }

    #[test]
    fn test_serialize_dom_with_env_wikilink() {
        // A wikilink serializes to `[[Foo]]` when an environment is present.
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("rel", "mw:WikiLink");
        a.set_attr("href", "./Foo");
        a.push_child(Node::text("Foo"));
        p.push_child(a);
        doc.push_child(p);

        let config = crate::mock::MockSiteConfig::new();
        let title = crate::title::Title::new_main("Test");
        let env = crate::html::env::SerializerEnv::new(&config, &title);
        let wt = WikitextSerializer::serialize_dom_with_env(doc, env);
        assert_eq!(wt, "[[Foo]]");
    }

    #[test]
    fn test_serialize_dom_with_env_figure() {
        // A thumbnail `<figure>` (with the description-page `<a>` link, as real
        // Parsoid emits) serializes to `[[File:Example.jpg|thumb|caption]]`.
        let mut doc = Node::document();
        let mut figure = Node::element(ElementKind::Figure);
        figure.set_attr("typeof", "mw:File/Thumb");
        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("href", "./File:Example.jpg");
        let mut img = Node::element(ElementKind::Other("img".to_string()));
        img.set_attr("resource", "./File:Example.jpg");
        a.push_child(img);
        figure.push_child(a);
        let mut caption = Node::element(ElementKind::FigCaption);
        caption.push_child(Node::text("A caption"));
        figure.push_child(caption);
        doc.push_child(figure);

        let config = crate::mock::MockSiteConfig::new();
        let title = crate::title::Title::new_main("Test");
        let env = crate::html::env::SerializerEnv::new(&config, &title);
        let wt = WikitextSerializer::serialize_dom_with_env(doc, env);
        assert_eq!(wt, "[[File:Example.jpg|thumb|A caption]]");
    }

    #[test]
    fn test_serialize_dom_selser_reuses_orig_src() {
        // A heading with a valid DSR reuses the original `===Heading===` source
        // verbatim in selser mode even though the handler would normally
        // re-serialize it (with the same result, but the selser branch exercises
        // the `getOrigSrc` + `fromSelSer` path).
        let original = "===Heading===";
        let mut doc = Node::document();
        let mut h3 = Node::element(ElementKind::Heading(3));
        h3.push_child(Node::text("Heading"));
        h3.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(0),
                end: Some(13),
                open_width: Some(3),
                close_width: Some(3),
            }),
            ..Default::default()
        });
        doc.push_child(h3);

        let config = crate::mock::MockSiteConfig::new();
        let title = crate::title::Title::new_main("Test");
        let env = crate::html::env::SerializerEnv::new(&config, &title);
        let wt = WikitextSerializer::serialize_dom_selser(doc, Some(env), original);
        assert_eq!(wt, original);
    }

    #[test]
    fn test_first_nl_run_end() {
        // Leading `[ \t]*\n+[ \t\r\n]*` runs.
        assert_eq!(first_nl_run_end("\n "), Some(2));
        assert_eq!(first_nl_run_end(" \n \t"), Some(4));
        assert_eq!(first_nl_run_end("\t\n\nbar"), Some(3));
        // No newline → None.
        assert_eq!(first_nl_run_end(" "), None);
        assert_eq!(first_nl_run_end("foo"), None);
        // Space/newline mixed then content.
        assert_eq!(first_nl_run_end("\nfoo"), Some(1));
    }

    #[test]
    fn test_serialize_text_splits_trailing_nl_into_separator() {
        // A table cell's text `[1]\n ` keeps `[1]` inline and moves `\n ` into
        // the separator (the `Newlines reset separator state` round-trip).
        let mut doc = Node::document();
        let mut td = Node::element(ElementKind::TableCell);
        td.push_child(Node::text("[1]\n "));
        doc.push_child(td);

        let tree = DomTree::new(doc);
        let td_id = tree.first_child(tree.root()).unwrap();
        let text_id = tree.first_child(td_id).unwrap();

        let mut state = SerializerState::new();
        serialize_text("[1]\n ", text_id, &tree, &mut state);
        assert_eq!(state.separator.src.as_deref(), Some("\n "));
        state.flush_line();
        assert_eq!(state.out, "[1]");
    }

    #[test]
    fn test_serialize_text_leading_nl_becomes_separator() {
        // A leading newline run is re-emitted as a separator before the body
        // (faithful to `SEPARATOR_PREFIX_WITH_NLS_RE` splitting).
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("\nfoo"));
        doc.push_child(p);

        let tree = DomTree::new(doc);
        let p_id = tree.first_child(tree.root()).unwrap();
        let text_id = tree.first_child(p_id).unwrap();

        let mut state = SerializerState::new();
        serialize_text("\nfoo", text_id, &tree, &mut state);
        state.flush_line();
        assert_eq!(state.out, "\nfoo");
    }
}
