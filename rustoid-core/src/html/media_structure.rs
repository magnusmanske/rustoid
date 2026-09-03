//! MediaStructure — faithful port of PHP Parsoid's `src/Core/MediaStructure.php`.
//!
//! All media has a fixed DOM shape:
//!
//! ```text
//! <containerElt>
//!   <linkElt><mediaElt /></linkElt>
//!   <captionElt>…</captionElt>
//! </containerElt>
//! ```
//!
//! `MediaStructure::parse` extracts that structure leniently (handling broken or
//! non-Parsoid HTML). Elements are `NodeId`s into the serialization `DomTree`
//! arena, mirroring PHP's `Element` references.

use crate::html::dom_tree::{DomTree, NodeId, first_non_sep_child};
use crate::html::dom_utils;
use crate::html::wts_utils;

/// The extracted media structure. All four fields are `NodeId`s into the
/// `DomTree` (or `None` when the (optional) slot is absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaStructure {
    /// `figure` or `span`.
    pub container_elt: NodeId,
    /// `a` or `span` (optional).
    pub link_elt: Option<NodeId>,
    /// `img`, `audio`, `video`, or `span` (the media element itself).
    pub media_elt: NodeId,
    /// `figcaption` (optional).
    pub caption_elt: Option<NodeId>,
}

impl MediaStructure {
    /// Extract the fixed media structure, being as generous as possible with
    /// possibly-broken HTML. Faithful to `MediaStructure::parse`.
    pub fn parse(tree: &DomTree, node: NodeId) -> Option<MediaStructure> {
        let name = dom_utils::node_name(tree.node(node));
        if name != "figure" && !wts_utils::is_inline_media(tree.node(node)) {
            return None;
        }

        // Walk down through formatting elements to find the link/media element.
        let mut link_elt = node;
        loop {
            let child = first_non_sep_child(tree, link_elt)?;
            let child_name = dom_utils::node_name(tree.node(child));
            if child_name != "a" && crate::wikitext::consts::formatting_tags().contains(&child_name)
            {
                link_elt = child;
            } else {
                link_elt = child;
                break;
            }
        }

        // At this point `link_elt` is the first non-formatting child.
        let link_name = dom_utils::node_name(tree.node(link_elt));
        let (link_elt_opt, media_elt) = if link_name == "a" || link_name == "span" {
            // linkElt is the wrapper; its first non-sep child is the media elt.
            let media_elt = first_non_sep_child(tree, link_elt)?;
            (Some(link_elt), media_elt)
        } else {
            // Lenient: no link element; `link_elt` is the media element itself.
            (None, link_elt)
        };

        let media_name = dom_utils::node_name(tree.node(media_elt));
        if !matches!(media_name.as_str(), "audio" | "img" | "span" | "video") {
            return None;
        }

        // The caption is the first `figcaption` descendant of a `<figure>`.
        let caption_elt = if dom_utils::node_name(tree.node(node)) == "figure" {
            select_first_descendant_tag(tree, node, "figcaption")
        } else {
            None
        };

        Some(MediaStructure {
            container_elt: node,
            link_elt: link_elt_opt,
            media_elt,
            caption_elt,
        })
    }

    /// Was the media missing (rendered as a `<span>` red-link)? Faithful to
    /// `MediaStructure::isRedLink`.
    pub fn is_red_link(&self, tree: &DomTree) -> bool {
        dom_utils::node_name(tree.node(self.media_elt)) == "span"
    }

    /// The `resource` attribute of the media element (the file title), or `None`.
    pub fn get_resource(&self, tree: &DomTree) -> Option<String> {
        tree.node(self.media_elt)
            .get_attr("resource")
            .map(str::to_string)
    }

    /// The `alt` attribute of the media element, or `None`.
    pub fn get_alt(&self, tree: &DomTree) -> Option<String> {
        tree.node(self.media_elt)
            .get_attr("alt")
            .map(str::to_string)
    }

    /// The `href` of the link element, or `None`.
    pub fn get_media_url(&self, tree: &DomTree) -> Option<String> {
        self.link_elt
            .and_then(|l| tree.node(l).get_attr("href").map(str::to_string))
    }
}

/// Construct a `MediaStructure` for a bare media element (an `<img>`/`<audio>`/
/// `<video>` with no link/container wrapper), mirroring PHP's
/// `new MediaStructure($node)` in `ImgHandler`/`MediaHandler`.
pub fn cradle_media_structure(node: NodeId) -> MediaStructure {
    MediaStructure {
        container_elt: node,
        link_elt: None,
        media_elt: node,
        caption_elt: None,
    }
}

/// Depth-first search for the first descendant element with a given tag name.
fn select_first_descendant_tag(tree: &DomTree, id: NodeId, tag: &str) -> Option<NodeId> {
    fn walk(tree: &DomTree, id: NodeId, tag: &str) -> Option<NodeId> {
        let mut child = tree.first_child(id);
        while let Some(c) = child {
            if dom_utils::node_name(tree.node(c)) == tag {
                return Some(c);
            }
            if let Some(found) = walk(tree, c, tag) {
                return Some(found);
            }
            child = tree.next_sibling(c);
        }
        None
    }
    walk(tree, id, tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    fn media_doc() -> DomTree {
        // <figure typeof="mw:File/Thumb">
        //   <a href="..."><img resource="Foo.jpg" /></a>
        //   <figcaption>caption</figcaption>
        // </figure>
        let mut figure = Node::element(ElementKind::Figure);
        figure.set_attr("typeof", "mw:File/Thumb");
        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("href", "/wiki/Foo");
        let mut img = Node::element(ElementKind::Other("img".to_string()));
        img.set_attr("resource", "Foo.jpg");
        a.push_child(img);
        figure.push_child(a);
        let mut caption = Node::element(ElementKind::FigCaption);
        caption.push_child(Node::text("caption"));
        figure.push_child(caption);

        let mut doc = Node::document();
        doc.push_child(figure);
        DomTree::new(doc)
    }

    #[test]
    fn test_parse_media_structure() {
        let tree = media_doc();
        let figure_id = tree.first_child(tree.root()).unwrap();
        let ms = MediaStructure::parse(&tree, figure_id).unwrap();
        assert_eq!(ms.container_elt, figure_id);
        assert!(ms.link_elt.is_some());
        assert!(ms.caption_elt.is_some());
        assert_eq!(ms.get_resource(&tree).as_deref(), Some("Foo.jpg"));
        assert!(!ms.is_red_link(&tree));
        assert_eq!(ms.get_media_url(&tree).as_deref(), Some("/wiki/Foo"));
    }

    #[test]
    fn test_parse_rejects_non_figure() {
        let tree = DomTree::new(Node::document());
        assert!(MediaStructure::parse(&tree, tree.root()).is_none());
    }

    #[test]
    fn test_is_inline_media() {
        let mut span = Node::element(ElementKind::Span);
        span.set_attr("typeof", "mw:File");
        assert!(crate::html::wts_utils::is_inline_media(&span));
    }
}
