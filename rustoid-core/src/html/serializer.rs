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

/// Serialize a single node, delegating to its handler (or emitting text). This
/// is the faithful `WikitextSerializer::serializeNode` (non-selser): it handles
/// the text/comment/diff-marker branches, computes separator constraints before
/// and after each element, and delegates elements to the handler chosen by the
/// factory.
pub fn serialize_node(tree: &DomTree, node: NodeId, state: &mut SerializerState) {
    use crate::html::separators::Separators;

    let n = tree.node(node);
    match &n.kind {
        // Text: accumulate pure whitespace into the separator, else emit.
        NodeKind::Text(text) => {
            if !state.in_indent_pre && text.chars().all(|c| c.is_whitespace()) {
                state.append_sep(text);
            } else {
                state.needs_escaping = true;
                state.is_last_child =
                    crate::html::dom_tree::next_non_deleted_sibling(tree, node).is_none();
                state.emit_chunk(text.clone(), node, tree);
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
    pub fn serialize_html_tag(node: &crate::dom::node::Node) -> String {
        let name = crate::html::wts_utils::node_name(node);
        let attrs = serialize_attributes(node);
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
        Self::serialize_dom_internal(root, None, false, None)
    }

    /// Serialize with a [`SerializerEnv`] so link/media handlers can run.
    pub fn serialize_dom_with_env(
        root: crate::dom::node::Node,
        env: crate::html::env::SerializerEnv,
    ) -> String {
        Self::serialize_dom_internal(root, Some(env), false, None)
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
        Self::serialize_dom_internal(root, env, true, Some(original_wikitext))
    }

    /// The shared `serializeDOM` implementation. `selser` toggles the reuse-
    /// original-source branch; `selser_data` carries the revision wikitext for
    /// that branch.
    fn serialize_dom_internal(
        root: crate::dom::node::Node,
        env: Option<crate::html::env::SerializerEnv>,
        selser: bool,
        selser_rev_text: Option<&str>,
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

/// Serialize an element's attributes to an HTML attribute string. This is a
/// faithful-enough skeleton of `serializeAttributes` for the IGNORED list and
/// the plain (non-shadow) attribute case; `data-parsoid`/`data-mw` and RDFa
/// `about`/`typeof`/`rel` attributes are stripped.
pub fn serialize_attributes_partial(node: &crate::dom::node::Node) -> String {
    serialize_attributes(node)
}

fn serialize_attributes(node: &crate::dom::node::Node) -> String {
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

        // Parsoid-generated heading ids are stripped unless they were explicitly
        // reused (mirrors the `id` + `isHeading` branch). Our `Node` has no
        // `reusedId` signal yet, so we conservatively drop only a `data-object-id`
        // style id here and keep real ids elsewhere.
        if k == "id" && v.starts_with("mw-") && dom_utils::is_heading(node) {
            continue;
        }

        // Strip Parsoid-inserted `class="mw-empty-elt"` markers.
        if k == "class" {
            let stripped = v.replace("mw-empty-elt", "").trim().to_string();
            if stripped.is_empty() {
                continue;
            }
            if stripped != v {
                out.push(format!("class=\"{}\"", stripped.replace('"', "&quot;")));
                continue;
            }
        }

        // Strip Parsoid-generated `about`/`typeof` RDFa values (mirrors the
        // `$parsoidAttributes` regex strip): `about="#mwt…"` and the `mw:…`
        // tokens in `typeof` are removed, and any remainder is kept.
        let parsoid_stripped: Option<String> = if k == "about" && v.starts_with("#mwt") {
            Some(trim_parsoid_about(v))
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

        // Regular attribute: honor shadow info (`a`/`sa`) and the `data-x-`
        // attribute-key prefix strip, mirroring the faithful path.
        let shadow = crate::html::wts_utils::get_shadow_info(node, k, Some(v));
        let kk = k.trim_start_matches("data-x-");
        let vv = shadow.value.as_str();
        if !vv.is_empty() {
            if !shadow.fromsrc {
                // Escaped from loaded attr value (not original source). Comments
                // and annotation tags embedded in the value are left unescaped
                // (faithful to `preg_split(commentsOrAnnotationsRE)` walking the
                // odd (delimiter) segments unescaped).
                let escaped = escape_non_comment_segments(vv);
                out.push(format!("{kk}=\"{escaped}\""));
            } else {
                out.push(format!("{kk}=\"{}\"", vv.replace('"', "&quot;")));
            }
        } else if kk.contains('{') || kk.contains('<') {
            // Templated / include / ext-tag generated attribute key.
            out.push(kk.to_string());
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

/// Strip the Parsoid transclusion counter from an `about="#mwtN"` value,
/// mirroring `CounterType::TRANSCLUSION_ABOUT` + `preg_replace` in PHP:
/// `#mwt3` → `` and `#mwt3 xfoo` → `xfoo` (the trailing id suffix survives).
fn trim_parsoid_about(v: &str) -> String {
    // `#mwt` followed by digits is the transclusion id; strip that leading run.
    let rest = &v[4..]; // skip "#mwt"
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_serialize_html_tag() {
        let mut div = Node::element(ElementKind::Div);
        div.set_attr("class", "mw-empty-elt");
        div.set_attr("style", "color:red");
        // data-* and class are stripped.
        assert_eq!(
            WikitextSerializer::serialize_html_tag(&div),
            "<div style=\"color:red\">"
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
            serialize_attributes(&div),
            "style=\"color:red\" align=\"left\""
        );
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
        // A `<figure>` with a resource serializes to `[[File:…|caption]]`.
        let mut doc = Node::document();
        let mut figure = Node::element(ElementKind::Figure);
        figure.set_attr("typeof", "mw:File/Thumb");
        let mut img = Node::element(ElementKind::Other("img".to_string()));
        img.set_attr("resource", "Example.jpg");
        figure.push_child(img);
        let mut caption = Node::element(ElementKind::FigCaption);
        caption.push_child(Node::text("A caption"));
        figure.push_child(caption);
        doc.push_child(figure);

        let config = crate::mock::MockSiteConfig::new();
        let title = crate::title::Title::new_main("Test");
        let env = crate::html::env::SerializerEnv::new(&config, &title);
        let wt = WikitextSerializer::serialize_dom_with_env(doc, env);
        assert_eq!(wt, "[[Example.jpg|A caption]]");
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
}
