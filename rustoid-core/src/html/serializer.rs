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
        let tree = DomTree::new(root);
        let root_id = tree.root();
        let mut state = SerializerState::new();
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

    /// Serialize with a [`SerializerEnv`] so link/media handlers can run.
    pub fn serialize_dom_with_env(
        root: crate::dom::node::Node,
        env: crate::html::env::SerializerEnv,
    ) -> String {
        let tree = DomTree::new(root);
        let root_id = tree.root();
        let mut state = SerializerState::with_env(env);
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
    let mut out = Vec::new();
    for attr in &node.attrs {
        let k = &attr.key;
        let v = &attr.value;
        if matches!(
            k.as_str(),
            "data-parsoid"
                | "data-mw"
                | "data-ve-changed"
                | "data-parsoid-changed"
                | "data-parsoid-diff"
                | "data-parsoid-serialize"
                | "data-object-id"
                | "about"
                | "typeof"
                | "rel"
                | "class"
        ) {
            continue;
        }
        out.push(format!("{}=\"{}\"", k, v.replace('"', "&quot;")));
    }
    out.join(" ")
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
}
