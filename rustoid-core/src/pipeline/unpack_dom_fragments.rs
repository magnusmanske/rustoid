//! UnpackDOMFragments — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Handlers/UnpackDOMFragments.php`.
//!
//! Placeholder elements carrying `typeof="mw:DOMFragment"` (and a stashed
//! sub-fragment in `Node.fragment`, mirroring PHP's `data-parsoid.html`) are
//! replaced by their content children during finalize. Typeof/about/data-mw
//! metadata on the placeholder is transferred to the fragment's first
//! (span-wrapped) child.
//!
//! When the placeholder's parent is an `<a>` and the fragment itself contains an
//! `<a>` (a `[[…]]`/`[[File:…]]` link inside an external link), the two anchors
//! would nest illegally. PHP (`UnpackDOMFragments::hasBadNesting`) repairs this
//! by serializing the parent and re-parsing it through the HTML parser, which
//! runs the adoption-agency algorithm: the inner `<a>` is hoisted out of the
//! outer `<a>` as a following sibling, and every hoisted element is marked
//! `misnested` (with a zero-width DSR). We reproduce that well-defined result
//! directly on the DOM (see [`fix_nested_anchors`]), preserving the node data a
//! serialize/reparse round-trip would otherwise lose.

use crate::dom::node::{Node, NodeKind};

/// Whether a node's `typeof` contains the `mw:DOMFragment` token.
fn has_dom_fragment_type(node: &Node) -> bool {
    node.get_attr("typeof")
        .is_some_and(|t| t.split_whitespace().any(|ty| ty == "mw:DOMFragment"))
}

/// Whether a node's `typeof` contains `mw:Transclusion`.
fn has_transclusion_type(node: &Node) -> bool {
    node.get_attr("typeof")
        .is_some_and(|t| t.split_whitespace().any(|ty| ty == "mw:Transclusion"))
}

/// Add whitespace-separated token(s) to a node's `typeof` attribute.
fn add_typeof(node: &mut Node, token: &str) {
    let mut tokens: Vec<String> = node
        .get_attr("typeof")
        .map(|t| t.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    for tok in token.split_whitespace() {
        if !tokens.iter().any(|t| t == tok) {
            tokens.push(tok.to_string());
        }
    }
    node.set_attr("typeof", tokens.join(" "));
}

/// The HTML tag name of a node (`""` for non-elements).
fn node_name(node: &Node) -> String {
    crate::html::wts_utils::node_name(node)
}

/// Whether `node` or any descendant is an `<a>` element. Mirrors
/// `DOMUtils::treeHasElement($fragment, 'a')`.
fn tree_has_element(node: &Node, name: &str) -> bool {
    if node_name(node) == name {
        return true;
    }
    node.children.iter().any(|c| tree_has_element(c, name))
}

/// Whether an element is a "block" container. A block container holding the
/// nested `<a>` is hoisted *as a unit* (mirroring HTML5's `figure`/`div`/etc.
/// handling, which reconstructs the active formatting elements — the outer
/// `<a>` — inside the block before inserting it), whereas an inline `<span>`
/// shell stays inside the outer anchor while only the nested `<a>` is hoisted.
fn is_block_container(name: &str) -> bool {
    matches!(
        name,
        "figure"
            | "div"
            | "p"
            | "table"
            | "ul"
            | "ol"
            | "dl"
            | "section"
            | "blockquote"
            | "pre"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
    )
}

/// Mark `node` and every element in its subtree as misnested. Mirrors
/// `UnpackDOMFragments::markMisnested`, which sets `dp->misnested = true` and a
/// zero-width DSR (`[offset, offset]`) on each hoisted element, so the selser
/// serializer can reuse the (empty) source via the `misnested` flag rather than
/// re-serializing it as a redlink.
fn mark_misnested(node: &mut Node, offset: Option<usize>) {
    if matches!(node.kind, NodeKind::Element(_)) {
        let dp = node.dp.get_or_insert_with(Default::default);
        dp.misnested = Some(true);
        dp.dsr = Some(crate::wikitext::tokens_v2::DomSourceRange {
            start: offset,
            end: offset,
            open_width: None,
            close_width: None,
            ..Default::default()
        });
        if let Some(json) = &mut node.data_parsoid
            && let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(json)
            && let Some(map) = obj.as_object_mut()
        {
            map.insert("misnested".to_string(), serde_json::Value::Bool(true));
            *json = obj.to_string();
        }
    }
    for child in &mut node.children {
        mark_misnested(child, offset);
    }
}

/// Unpack every `mw:DOMFragment` placeholder in the subtree, replacing it in
/// place with its stashed fragment children (the well-formed case).
///
/// The bad-nesting repair (`<a>` inside `<a>`) is deferred to
/// [`fix_bad_nesting`], which must run *after* `AddMediaInfo` (mirroring PHP's
/// `media` … `linkneighbours+dom-unpack` order) so the foster-out operates on
/// already-resolved media rather than detaching a broken-media anchor first.
pub fn run(node: &mut Node) {
    run_inner(node);
}

/// Repair illegal `<a>`-inside-`<a>` nesting created by unpacking a DOM fragment
/// whose parent is an `<a>`. Must run after `AddMediaInfo`.
pub fn fix_bad_nesting(node: &mut Node) {
    fix_nested_anchors(node);
    // Fostering a block media container (`<figure>`) out of an `<a>` leaves it
    // (incorrectly) as a direct child of a wrapping `<p>`; a block cannot live
    // inside a `<p>`, so hoist it out (mirroring HTML5's block-in-`<p>` split,
    // which the token stage couldn't see while the fragment was still opaque).
    split_paragraphs_on_block(node);
}

/// Whether a node is a block-level element (`<figure>`/`<div>`/`<table>`/…).
fn is_block_node(node: &Node) -> bool {
    is_block_container(&node_name(node))
}

/// Whether a run of sibling nodes has no visible text content (only empty
/// inline elements like a fostered-out `<a>`, whitespace, and comments). Used to
/// decide whether the leading run of a `<p>` should be hoisted wholesale when a
/// following block is hoisted (mirroring HTML5, where an `<a>` whose content was
/// fostered out leaves nothing p-worth-wrapping).
fn is_trivially_empty(run: &[Node]) -> bool {
    run.iter().all(|n| match &n.kind {
        NodeKind::Text(t) => t.trim().is_empty(),
        NodeKind::Comment(_) => true,
        NodeKind::Element(_) => is_trivially_empty(&n.children),
        NodeKind::Document => false,
    })
}

/// Top-down: split every `<p>` containing a direct block-level child into the
/// `<p>` (holding the leading inline run) followed by its block (and trailing)
/// siblings at the parent level. When the leading inline run is itself
/// trivially empty (e.g. an `<a>` left empty by the misnest foster), the `<p>`
/// is dropped entirely. Mirrors the tree builder's block-in-`<p>` split.
fn split_paragraphs_on_block(node: &mut Node) {
    // Recurse first.
    for child in &mut node.children {
        split_paragraphs_on_block(child);
    }

    // Rebuild this level, expanding `<p>`s that contain a direct block child.
    let mut rebuilt: Vec<Node> = Vec::with_capacity(node.children.len());
    for child in std::mem::take(&mut node.children) {
        if node_name(&child) == "p"
            && let Some(i) = child.children.iter().position(is_block_node)
        {
            let mut p = child;
            let hoisted: Vec<Node> = p.children.drain(i..).collect();
            // Drop the `<p>` if its leading run is trivially empty (only the
            // emptied `<a>` + whitespace); otherwise keep it.
            if !is_trivially_empty(&p.children) {
                rebuilt.push(p);
                rebuilt.extend(hoisted);
            } else {
                // Hoist the leading (empty) run too, dropping the `<p>`.
                rebuilt.extend(p.children);
                rebuilt.extend(hoisted);
            }
        } else {
            rebuilt.push(child);
        }
    }
    node.children = rebuilt;
}

/// Bottom-up splice of fragment content. Bad nesting is repaired afterward by
/// [`fix_nested_anchors`], which needs the full (post-splice) tree.
fn run_inner(node: &mut Node) {
    let mut new_children: Vec<Node> = Vec::with_capacity(node.children.len());
    for mut child in std::mem::take(&mut node.children) {
        run_inner(&mut child);
        if has_dom_fragment_type(&child) {
            let mut fragment = child
                .fragment
                .take()
                .map(|f| *f)
                .unwrap_or_else(Node::document);
            let mut kids = std::mem::take(&mut fragment.children);
            if kids.is_empty() {
                // A leaf fragment (e.g. a bare text node) is itself the content.
                transfer_metadata(&child, std::slice::from_mut(&mut fragment));
                new_children.push(fragment);
            } else {
                transfer_metadata(&child, &mut kids);
                new_children.extend(kids);
            }
        } else {
            new_children.push(child);
        }
    }
    node.children = new_children;
}

/// Transfer `typeof`/`data-mw`/`about`/`data-parsoid` metadata from a
/// `mw:DOMFragment` placeholder onto its (span-wrapped) first fragment child.
/// Mirrors the transclusion/fostered transfer in PHP's `UnpackDOMFragments::handler`.
fn transfer_metadata(placeholder: &Node, kids: &mut [Node]) {
    let is_transclusion = has_transclusion_type(placeholder);
    let about = placeholder.get_attr("about").map(str::to_string);
    let dmw = placeholder.data_mw.clone();
    // The placeholder's computed DSR (`open_width`/`close_width` come from the
    // extension token's `extTagOffsets`) transfers to the first fragment child
    // (e.g. the gallery `<ul>`), so selser's `getOrigSrc(innerRange)` can recover
    // the original body (preserving blank lines).
    let dp = placeholder.dp.clone();
    for (i, child) in kids.iter_mut().enumerate() {
        if i == 0 {
            if is_transclusion {
                add_typeof(child, "mw:Transclusion");
            }
            if let Some(d) = &dmw {
                child.data_mw = Some(d.clone());
            }
            if let Some(d) = &dp {
                // Merge the placeholder's DSR (the computed range over the
                // `<gallery>…</gallery>` source) into the child's `dp`.
                let child_dp = child.dp.get_or_insert_with(Default::default);
                if let Some(dsr) = &d.dsr {
                    child_dp.dsr = Some(dsr.clone());
                }
                child_dp.tsr = d.tsr.clone();
                child_dp.ext_tag_offsets = d.ext_tag_offsets.clone();
                if child_dp.src.is_none() {
                    child_dp.src = d.src.clone();
                }
            }
        }
        if let Some(ab) = &about {
            child.set_attr("about", ab.clone());
        }
    }
}

/// Top-down repair of `<a>` elements whose subtree contains a nested `<a>`. Runs
/// after fragments are unpacked so the nested media/link anchor is a real
/// element. Reproduces the adoption-agency result:
///
/// * Inline (`<span>`) media container: the nested `<a>` (and following
///   siblings) are hoisted out after the outer `<a>`; the `<span>` shell stays
///   (empty) inside it.
/// * Block (`<figure>`) media container: the whole container is hoisted out, the
///   outer `<a>` is emptied, and a copy of the outer `<a>` is reconstructed as
///   the container's first child.
///
/// Every hoisted element is marked `misnested`.
fn fix_nested_anchors(node: &mut Node) {
    // Recurse into children first so deeper nesting is repaired before we
    // inspect this level.
    for child in &mut node.children {
        fix_nested_anchors(child);
    }

    // Rebuild this level's children, expanding each `<a>`-containing-`<a>` into
    // the (possibly emptied) `<a>` followed by its hoisted siblings.
    let mut rebuilt: Vec<Node> = Vec::with_capacity(node.children.len());
    for child in std::mem::take(&mut node.children) {
        if node_name(&child) == "a" && child.children.iter().any(|c| tree_has_element(c, "a")) {
            let mut outer = child; // the `<a>` itself
            let kids = std::mem::take(&mut outer.children);
            let hoisted = foster_anchors(&mut outer, kids);
            rebuilt.push(outer);
            rebuilt.extend(hoisted);
        } else {
            rebuilt.push(child);
        }
    }
    node.children = rebuilt;
}

/// Foster the nested `<a>`(s) out of the outer anchor `parent`, returning the
/// nodes to place immediately after it. `parent` is left holding any content
/// that stays inside (the empty inline shell, or nothing for a hoisted block).
fn foster_anchors(parent: &mut Node, fragment_kids: Vec<Node>) -> Vec<Node> {
    let mut kept: Vec<Node> = Vec::new();
    let mut hoisted: Vec<Node> = Vec::new();

    // The zero-width DSR offset for hoisted (misnested) nodes is the outer
    // anchor's source end (PHP `markMisnested`'s `$newOffset = $linkNode->dsr->end`).
    let offset = parent
        .dp
        .as_ref()
        .and_then(|d| d.dsr.as_ref())
        .and_then(|d| d.end);

    // A copy of the outer `<a>` (attributes only) used to reconstruct the anchor
    // inside a hoisted block container (HTML5 formatting-element reconstruction).
    let anchor_copy = || {
        let mut copy = Node::element(match &parent.kind {
            NodeKind::Element(k) => k.clone(),
            _ => crate::dom::node::ElementKind::Span,
        });
        copy.attrs.clone_from(&parent.attrs);
        copy.data_parsoid.clone_from(&parent.data_parsoid);
        copy.data_mw.clone_from(&parent.data_mw);
        copy.dp.clone_from(&parent.dp);
        copy
    };

    for child in fragment_kids {
        let child_is_anchor = node_name(&child) == "a";
        if child_is_anchor {
            // A bare nested `<a>` is hoisted out directly.
            let mut child = child;
            mark_misnested(&mut child, offset);
            hoisted.push(child);
        } else if tree_has_element(&child, "a") {
            if is_block_container(&node_name(&child)) {
                let mut container = child;
                let mut copy = anchor_copy();
                mark_misnested(&mut copy, offset);
                container.children.insert(0, copy);
                mark_misnested(&mut container, offset);
                hoisted.push(container);
            } else {
                // Inline container: keep the shell inside the outer `<a>`, hoist
                // the nested `<a>` (and following siblings) out.
                split_inline_anchor(child, offset, &mut kept, &mut hoisted);
            }
        } else {
            kept.push(child);
        }
    }

    parent.children = kept;
    hoisted
}

/// Split an inline `<span>` media container: the nested `<a>` (and everything
/// after it) is hoisted out and marked misnested; the (now empty) shell stays.
fn split_inline_anchor(
    mut container: Node,
    offset: Option<usize>,
    kept: &mut Vec<Node>,
    hoisted: &mut Vec<Node>,
) {
    let Some(i) = container
        .children
        .iter()
        .position(|c| tree_has_element(c, "a"))
    else {
        kept.push(container);
        return;
    };
    let drained = container.children.drain(i..).collect::<Vec<_>>();
    for mut child in drained {
        mark_misnested(&mut child, offset);
        hoisted.push(child);
    }
    kept.push(container);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, NodeKind};

    fn fragment_placeholder(kind: ElementKind) -> Node {
        let mut n = Node::element(kind);
        n.set_attr("typeof", "mw:DOMFragment");
        n
    }

    #[test]
    fn test_unpacks_simple_fragment() {
        let mut frag = Node::document();
        frag.push_child(Node::text("hello"));
        let mut ph = fragment_placeholder(ElementKind::Other("span".into()));
        ph.fragment = Some(Box::new(frag));

        let mut doc = Node::document();
        doc.push_child(ph);
        run(&mut doc);

        assert_eq!(doc.children.len(), 1);
        assert_eq!(doc.children[0].kind, NodeKind::Text("hello".into()));
    }

    #[test]
    fn test_transfers_about_and_typeof() {
        let mut frag = Node::document();
        let mut span = Node::element(ElementKind::Span);
        span.push_child(Node::text("x"));
        frag.push_child(span);

        let mut ph = fragment_placeholder(ElementKind::Other("span".into()));
        ph.set_attr("about", "#mwt1");
        add_typeof(&mut ph, "mw:Transclusion");
        ph.data_mw = Some("{\"name\":\"pre\"}".into());
        ph.fragment = Some(Box::new(frag));

        let mut doc = Node::document();
        doc.push_child(ph);
        run(&mut doc);

        let child = &doc.children[0];
        assert_eq!(child.get_attr("about"), Some("#mwt1"));
        assert!(
            child
                .get_attr("typeof")
                .unwrap()
                .contains("mw:Transclusion")
        );
        assert_eq!(child.data_mw.as_deref(), Some("{\"name\":\"pre\"}"));
    }

    #[test]
    fn test_non_fragment_passes_through() {
        let mut doc = Node::document();
        doc.push_child(Node::text("plain"));
        run(&mut doc);
        assert_eq!(doc.children.len(), 1);
        assert_eq!(doc.children[0].kind, NodeKind::Text("plain".into()));
    }

    #[test]
    fn test_inline_media_fostered_out_of_anchor() {
        // The inline case: `<a extlink><span><a file><img></a></span></a>` →
        // `<a extlink><span></span></a><a file><img></a>`.
        let mut img = Node::element(ElementKind::Image);
        img.set_attr("src", "x");
        let mut file_a = Node::element(ElementKind::Wikilink);
        file_a.set_attr("href", "./File:F.jpg");
        file_a.push_child(img);
        let mut span = Node::element(ElementKind::Span);
        span.push_child(file_a);
        let mut ext = Node::element(ElementKind::ExtLink);
        ext.set_attr("rel", "mw:ExtLink");
        ext.push_child(span);

        let mut doc = Node::document();
        doc.push_child(ext);
        run(&mut doc);
        fix_bad_nesting(&mut doc);

        // doc.children = [<a extlink><span></span></a>, <a file><img></a>]
        assert_eq!(doc.children.len(), 2);
        assert_eq!(node_name(&doc.children[0]), "a");
        assert_eq!(node_name(&doc.children[1]), "a");
        // The outer `<a>` holds the empty span; the hoisted `<a>` holds the img.
        assert_eq!(doc.children[0].children.len(), 1);
        assert_eq!(node_name(&doc.children[0].children[0]), "span");
        assert!(doc.children[0].children[0].children.is_empty());
        assert_eq!(doc.children[1].children.len(), 1);
        assert_eq!(node_name(&doc.children[1].children[0]), "img");
        // The hoisted `<a>` and its `<img>` are marked misnested.
        assert!(doc.children[1].dp.as_ref().unwrap().misnested == Some(true));
    }

    #[test]
    fn test_misnested_dsr_is_zero_width_at_outer_end() {
        // The outer `<a>`'s `dsr.end` becomes the zero-width DSR of the hoisted
        // (misnested) `<a>`, so the selser serializer can reuse-empty via the
        // `misnested` flag (PHP `markMisnested` sets `dsr = [end, end]`).
        let mut img = Node::element(ElementKind::Image);
        img.set_attr("src", "x");
        let mut file_a = Node::element(ElementKind::Wikilink);
        file_a.set_attr("href", "./File:F.jpg");
        file_a.push_child(img);
        let mut span = Node::element(ElementKind::Span);
        span.push_child(file_a);
        let mut ext = Node::element(ElementKind::ExtLink);
        ext.set_attr("rel", "mw:ExtLink");
        // The outer anchor source ends at 56.
        ext.dp = Some(crate::wikitext::tokens_v2::DataParsoid {
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(2),
                end: Some(56),
                open_width: Some(20),
                close_width: None,
                leading_ws: 0,
                trailing_ws: 0,
            }),
            ..Default::default()
        });
        ext.push_child(span);

        let mut doc = Node::document();
        doc.push_child(ext);
        run(&mut doc);
        fix_bad_nesting(&mut doc);

        // The hoisted `<a>` gets a zero-width DSR [56, 56].
        let hoisted_dsr = doc.children[1].dp.as_ref().and_then(|d| d.dsr.as_ref());
        assert_eq!(hoisted_dsr.map(|d| d.start), Some(Some(56)));
        assert_eq!(hoisted_dsr.map(|d| d.end), Some(Some(56)));
        assert!(doc.children[1].dp.as_ref().unwrap().misnested == Some(true));
    }

    #[test]
    fn test_thumb_media_fostered_out_of_anchor() {
        // The block case: `<a extlink><figure><a file><img></a><figcaption/></figure></a>`
        // → `<a extlink></a><figure><a extlink></a><a file><img></a><figcaption/></figure>`.
        let mut img = Node::element(ElementKind::Image);
        img.set_attr("src", "x");
        let mut file_a = Node::element(ElementKind::Wikilink);
        file_a.set_attr("href", "./File:F.jpg");
        file_a.push_child(img);
        let mut fig = Node::element(ElementKind::Figure);
        fig.push_child(file_a);
        let mut cap = Node::element(ElementKind::FigCaption);
        cap.push_child(Node::text("123"));
        fig.push_child(cap);
        let mut ext = Node::element(ElementKind::ExtLink);
        ext.set_attr("rel", "mw:ExtLink");
        ext.push_child(fig);

        let mut doc = Node::document();
        doc.push_child(ext);
        run(&mut doc);
        fix_bad_nesting(&mut doc);

        // doc.children = [<a extlink></a>, <figure>…</figure>]
        assert_eq!(doc.children.len(), 2);
        assert_eq!(node_name(&doc.children[0]), "a");
        assert!(doc.children[0].children.is_empty());
        assert_eq!(node_name(&doc.children[1]), "figure");
        // The figure holds: a reconstructed empty `<a>`, the file `<a>` + img,
        // and the figcaption.
        let fig_children = &doc.children[1].children;
        assert_eq!(fig_children.len(), 3);
        assert_eq!(node_name(&fig_children[0]), "a");
        assert!(fig_children[0].children.is_empty());
        assert_eq!(node_name(&fig_children[1]), "a");
        assert_eq!(node_name(&fig_children[1].children[0]), "img");
        assert_eq!(node_name(&fig_children[2]), "figcaption");
    }

    #[test]
    fn test_bare_nested_anchor_fostered_out() {
        // A bare nested `<a>` (link text, not media) is hoisted out directly:
        // `<a extlink><a wikilink>Foo</a></a>` → `<a extlink></a><a wikilink>Foo</a>`.
        let mut inner = Node::element(ElementKind::Wikilink);
        inner.set_attr("href", "./Foo");
        inner.push_child(Node::text("Foo"));
        let mut ext = Node::element(ElementKind::ExtLink);
        ext.set_attr("rel", "mw:ExtLink");
        ext.push_child(inner);

        let mut doc = Node::document();
        doc.push_child(ext);
        run(&mut doc);
        fix_bad_nesting(&mut doc);

        assert_eq!(doc.children.len(), 2);
        assert_eq!(node_name(&doc.children[0]), "a");
        assert!(doc.children[0].children.is_empty());
        assert_eq!(node_name(&doc.children[1]), "a");
        assert_eq!(
            doc.children[1].children[0].kind,
            NodeKind::Text("Foo".into())
        );
    }

    #[test]
    fn test_block_split_out_of_paragraph() {
        // A `<p>` whose only content is an emptied `<a>` and a fostered block
        // `<figure>` is dropped, hoisting both to the parent.
        // `<p><a></a><figure><figcaption>x</figcaption></figure></p>`
        //   → `<a></a><figure><figcaption>x</figcaption></figure>`.
        let mut a = Node::element(ElementKind::ExtLink);
        a.set_attr("rel", "mw:ExtLink");
        let mut fig = Node::element(ElementKind::Figure);
        let mut cap = Node::element(ElementKind::FigCaption);
        cap.push_child(Node::text("x"));
        fig.push_child(cap);
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(a);
        p.push_child(fig);

        let mut doc = Node::document();
        doc.push_child(p);
        fix_bad_nesting(&mut doc);

        // The `<p>` is dropped; its children surface at the top level.
        assert_eq!(doc.children.len(), 2);
        assert_eq!(node_name(&doc.children[0]), "a");
        assert_eq!(node_name(&doc.children[1]), "figure");
    }

    #[test]
    fn test_paragraph_kept_for_leading_inline_text() {
        // A `<p>` with real leading text before a block keeps the `<p>` (holding
        // the text) and hoists only the block.
        // `<p>hi<figure>x</figure></p>` → `<p>hi</p><figure>x</figure>`.
        let mut fig = Node::element(ElementKind::Figure);
        fig.push_child(Node::text("x"));
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("hi"));
        p.push_child(fig);

        let mut doc = Node::document();
        doc.push_child(p);
        fix_bad_nesting(&mut doc);

        assert_eq!(doc.children.len(), 2);
        assert_eq!(node_name(&doc.children[0]), "p");
        assert_eq!(
            doc.children[0].children[0].kind,
            NodeKind::Text("hi".into())
        );
        assert_eq!(node_name(&doc.children[1]), "figure");
    }
}
