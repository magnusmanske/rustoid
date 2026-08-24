//! `PWrap` — DOM-level paragraph wrapper/fix-up pass, a faithful port of PHP
//! Parsoid's `src/Wt2Html/DOM/Processors/PWrap.php`.
//!
//! After the HTML5 tree builder produces a DOM, this pass re-wraps the children
//! of block containers in `<p>` elements, splitting splittable inline elements
//! (formatting elements) and hoisting "p-wrap optional" nodes (whitespace-only
//! text, comments, metadata tags) out of paragraph boundaries. This normalizes
//! the `<p>` contents that the token-level ParagraphWrapper leaves imperfect
//! (e.g. leading/trailing newlines and `<br/>` placement).

use crate::dom::node::{ElementKind, Node, NodeKind};

/// The HTML5 "formatting elements" set (used for the adoption agency and for
/// deciding which inline elements are splittable). Mirrors `DOMUtils::isFormattingElt`.
const FORMATTING_ELTS: &[&str] = &[
    "a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small", "strike", "strong", "tt", "u",
];

/// Metadata tags that need no p-wrapper of their own (a superset of
/// RemexCompatMunger's metadataElements). Mirrors `DOMUtils::isMetaDataTag`.
const METADATA_TAGS: &[&str] = &[
    "base", "link", "meta", "noscript", "script", "style", "template", "title",
];

/// Map an `ElementKind` to its lowercase HTML tag name (for classification).
fn element_tag(kind: &ElementKind) -> String {
    match kind {
        ElementKind::Document => "html".to_string(),
        ElementKind::Paragraph => "p".to_string(),
        ElementKind::Heading(1) => "h1".to_string(),
        ElementKind::Heading(2) => "h2".to_string(),
        ElementKind::Heading(3) => "h3".to_string(),
        ElementKind::Heading(4) => "h4".to_string(),
        ElementKind::Heading(5) => "h5".to_string(),
        ElementKind::Heading(6) => "h6".to_string(),
        ElementKind::Heading(n) => format!("h{n}"),
        ElementKind::Bold => "b".to_string(),
        ElementKind::Italic => "i".to_string(),
        ElementKind::Wikilink | ElementKind::ExtLink => "a".to_string(),
        ElementKind::Image => "figure".to_string(),
        ElementKind::Gallery => "ul".to_string(),
        ElementKind::Table => "table".to_string(),
        ElementKind::TableRow => "tr".to_string(),
        ElementKind::TableCell => "td".to_string(),
        ElementKind::TableHeader => "th".to_string(),
        ElementKind::TableCaption => "caption".to_string(),
        ElementKind::UnorderedList => "ul".to_string(),
        ElementKind::OrderedList => "ol".to_string(),
        ElementKind::ListItem => "li".to_string(),
        ElementKind::DefinitionList => "dl".to_string(),
        ElementKind::DefinitionTerm => "dt".to_string(),
        ElementKind::DefinitionDescription => "dd".to_string(),
        ElementKind::Preformatted => "pre".to_string(),
        ElementKind::HorizontalRule => "hr".to_string(),
        ElementKind::Transclusion | ElementKind::Annotation => "meta".to_string(),
        ElementKind::ExtensionTag => "span".to_string(),
        ElementKind::Div => "div".to_string(),
        ElementKind::Span => "span".to_string(),
        ElementKind::LineBreak => "br".to_string(),
        ElementKind::Comment => "comment".to_string(),
        ElementKind::RawHtml => "raw".to_string(),
        ElementKind::Section => "section".to_string(),
        ElementKind::InterlanguageLink | ElementKind::CategoryLink | ElementKind::Redirect => {
            "link".to_string()
        }
        ElementKind::TableOfContents => "div".to_string(),
        ElementKind::Indicator => "meta".to_string(),
        ElementKind::Figure => "figure".to_string(),
        ElementKind::FigCaption => "figcaption".to_string(),
        ElementKind::Other(tag) => tag.clone(),
    }
}

/// The HTML block-level elements that terminate/inhibit p-wrapping. Mirrors the
/// Remex block-node set used by `DOMUtils::isRemexBlockNode`.
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "ul"
    )
}

/// Does this node have a `typeof` matching `mw:Nowiki` or `mw:DOMFragment`?
fn is_nowiki_or_dom_fragment(node: &Node) -> bool {
    let Some(ty) = node.get_attr("typeof") else {
        return false;
    };
    ty.split_whitespace()
        .any(|t| t == "mw:Nowiki" || t == "mw:DOMFragment")
}

/// Is a p-wrapper optional for this node? (whitespace/comment/meta-tag/nowiki)
fn p_wrap_optional(node: &Node) -> bool {
    match &node.kind {
        NodeKind::Comment(_) => true,
        NodeKind::Text(s) => s.trim().is_empty(),
        NodeKind::Element(kind) if is_nowiki_or_dom_fragment(node) => {
            node.children.iter().all(p_wrap_optional)
        }
        NodeKind::Element(kind) => {
            let tag = element_tag(kind);
            METADATA_TAGS.contains(&tag.as_str())
        }
        NodeKind::Document => true,
    }
}

/// Is this a formatting element (splittable for p-wrapping)?
fn is_formatting_elt(node: &Node) -> bool {
    if let NodeKind::Element(kind) = &node.kind {
        let tag = element_tag(kind);
        FORMATTING_ELTS.contains(&tag.as_str())
    } else {
        false
    }
}

/// Does this node's subtree contain a block tag?
fn has_block_tag(node: &Node) -> bool {
    if let NodeKind::Element(kind) = &node.kind {
        let tag = element_tag(kind);
        if is_block_tag(&tag) {
            return true;
        }
    }
    node.children.iter().any(has_block_tag)
}

/// A split result: a node plus its `pwrap` classification (true=open-para,
/// false=close-para, null=agnostic).
struct Split {
    pwrap: Option<bool>,
    node: Node,
}

/// Split a node into p-wrap runs. Faithful port of `PWrap::split` (without the
/// transclusion/annotation range-aware `mergeRuns` data-parsoid bookkeeping,
/// which only affects template range width).
fn split(node: &Node) -> Vec<Split> {
    if p_wrap_optional(node) {
        return vec![Split {
            pwrap: None,
            node: node.clone(),
        }];
    }
    if matches!(node.kind, NodeKind::Text(_)) {
        return vec![Split {
            pwrap: Some(true),
            node: node.clone(),
        }];
    }
    if !is_formatting_elt(node) || node.children.is_empty() {
        // Block tag OR non-splittable/childless inline tag.
        let pwrap = Some(!has_block_tag(node));
        return vec![Split {
            pwrap,
            node: node.clone(),
        }];
    }

    // Splittable inline (formatting) tag: split children and merge runs.
    let mut splits: Vec<Split> = Vec::new();
    for child in &node.children {
        splits.extend(split(child));
    }
    merge_runs(node, splits)
}

/// Merge a contiguous run of split subtrees with identical pwrap properties,
/// cloning the formatting-elt wrapper `node` as needed. Faithful to
/// `PWrap::mergeRuns` (minus data-parsoid auto-insert flags).
fn merge_runs(_wrapper: &Node, splits: Vec<Split>) -> Vec<Split> {
    let mut ret: Vec<Split> = Vec::new();
    let mut i: isize = -1;
    for v in splits {
        if i < 0 {
            let mut wrapper = _wrapper.clone();
            wrapper.children.clear();
            ret.push(Split {
                pwrap: v.pwrap,
                node: wrapper,
            });
            i = 0;
        } else if ret[i as usize].pwrap.is_none() {
            ret[i as usize].pwrap = v.pwrap;
        } else if ret[i as usize].pwrap != v.pwrap && v.pwrap.is_some() {
            // New run, new clone of the wrapper.
            let mut wrapper = _wrapper.clone();
            wrapper.children.clear();
            ret.push(Split {
                pwrap: v.pwrap,
                node: wrapper,
            });
            i += 1;
        }
        if let NodeKind::Element(_) = ret[i as usize].node.kind {
            ret[i as usize].node.push_child(v.node);
        }
    }
    ret
}

/// Holds the currently-open `<p>` element during p-wrapping.
struct PWrapState {
    p: Option<Node>,
}

impl PWrapState {
    fn reset(&mut self) {
        self.p = None;
    }
}

/// Wrap children of `root` in `<p>` tags. Faithful port of `PWrap::pWrapDOM`.
fn p_wrap_dom(root: &mut Node) {
    let mut state = PWrapState { p: None };

    let children = std::mem::take(&mut root.children);
    let mut out: Vec<Node> = Vec::new();

    for c in children {
        if is_block_node(&c) {
            // Block node: reset the open paragraph and pass through.
            state.reset();
            out.push(c);
        } else {
            for v in split(&c) {
                match v.pwrap {
                    Some(false) => {
                        state.reset();
                        out.push(v.node);
                    }
                    None => {
                        if let Some(p) = state.p.as_mut() {
                            if let NodeKind::Element(_) = p.kind {
                                p.push_child(v.node);
                            }
                        } else {
                            out.push(v.node);
                        }
                    }
                    Some(true) => {
                        if state.p.is_none() {
                            state.p = Some(Node::element(ElementKind::Paragraph));
                        }
                        if let Some(p) = state.p.as_mut()
                            && let NodeKind::Element(_) = p.kind
                        {
                            p.push_child(v.node);
                        }
                    }
                }
            }
        }
    }

    if let Some(p) = state.p {
        out.push(p);
    }

    root.children = out;
}

/// Is this a Remex block node (a block element or an element that contains
/// non-inline content)? Faithful to `DOMUtils::isRemexBlockNode` for the common
/// element cases.
fn is_block_node(node: &Node) -> bool {
    if let NodeKind::Element(kind) = &node.kind {
        is_block_tag(&element_tag(kind))
    } else {
        false
    }
}

/// Recursively p-wrap inside elements with the given tag name (used for
/// `<blockquote>`). Faithful to `PWrap::pWrapInsideTag`.
fn p_wrap_inside_tag(root: &mut Node, tag_name: &str) {
    for child in &mut root.children {
        if let NodeKind::Element(kind) = &child.kind {
            if element_tag(kind) == tag_name {
                p_wrap_dom(child);
            } else {
                p_wrap_inside_tag(child, tag_name);
            }
        }
    }
}

/// Run the PWrap pass over the document, wrapping `<body>` children (and any
/// `<blockquote>` contents) in paragraphs. Faithful to `PWrap::run`.
/// Run the PWrap pass over the document, wrapping `<body>` children (and any
/// `<blockquote>` contents) in paragraphs. Faithful to `PWrap::run`.
pub fn run(root: &mut Node) {
    // Root is the document (fragment mode: `<html>` holding body content).
    // Apply p-wrapping to the synthetic `<html>` element's children (which are
    // the body content) and to any `<blockquote>` descendants.
    for child in root.children.iter_mut() {
        if let NodeKind::Element(kind) = &child.kind {
            let tag = element_tag(kind);
            if tag == "html" || tag == "body" || tag == "blockquote" {
                p_wrap_dom(child);
            } else {
                p_wrap_inside_tag(child, "blockquote");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_p_wrap_optional_whitespace() {
        assert!(p_wrap_optional(&Node::text("  \n ")));
        assert!(!p_wrap_optional(&Node::text("a")));
        assert!(p_wrap_optional(&Node::comment("c")));
    }

    #[test]
    fn test_formatting_elts() {
        assert!(is_formatting_elt(&Node::element(ElementKind::Bold)));
        assert!(is_formatting_elt(&Node::element(ElementKind::Italic)));
        assert!(!is_formatting_elt(&Node::element(ElementKind::Div)));
    }

    #[test]
    fn test_p_wrap_dom_basic() {
        // A div with a text child should NOT be split into a paragraph
        // (the text is already directly inside a block). This test exercises
        // the no-op when a block container's children are already well-formed.
        let mut root = Node::element(ElementKind::Other("body".to_string()));
        root.push_child(Node::text("hello"));
        p_wrap_dom(&mut root);
        // "hello" becomes wrapped in a <p>.
        assert_eq!(root.children.len(), 1);
        assert!(matches!(
            &root.children[0].kind,
            NodeKind::Element(ElementKind::Paragraph)
        ));
    }
}
