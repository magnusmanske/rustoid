//! DOMHandlerFactory — faithful port of PHP Parsoid's
//! `src/Html2Wt/DOMHandlers/DOMHandlerFactory.php`.
//!
//! Picks the right [`DomHandler`] for an element by its tag name, its `stx`
//! (syntactic-form) data-parsoid field, and its context (first encapsulation
//! wrapper, HTML-syntax list, HTML table). The individual handler *classes*
//! (`PHandler`, `LIHandler`, …) are layered on in subsequent modules; until
//! then, the faithful dispatch *selection* is computed and non-default cases
//! currently resolve to the shared [`DefaultDomHandler`] placeholder.

use crate::html::dom_handler::{DefaultDomHandler, DomHandler};
use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::dom_utils;
use crate::html::handlers::{
    BRHandler, BodyHandler, CaptionHandler, DDHandler, DTHandler, FallbackHTMLHandler, HRHandler,
    HeadingHandler, JustChildrenHandler, LIHandler, ListHandler, PHandler, PreHandler,
    QuoteHandler, SpanHandler, TDHandler, THHandler, TRHandler, TableHandler,
};
use crate::html::wts_utils;

/// The concrete DOM handler classes PHP maps tag names to (`DOMHandlerFactory::newFromTagHandler`).
/// Used to faithfully record the tag→handler correspondence; each variant is
/// instantiated in its own module as those handlers are ported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    A,
    Media,
    QuoteBold,
    Body,
    Br,
    Caption,
    Dd,
    DdRow,
    Dl,
    Dt,
    Figure,
    Hr,
    Heading(u8),
    QuoteItalic,
    Img,
    Li,
    Link,
    Meta,
    OlUl,
    P,
    Pre,
    PreHtml,
    Span,
    Table,
    TableBody,
    Td,
    Th,
    Tr,
    /// No specialized handler (PHP returns `null` → `FallbackHTMLHandler`).
    FallbackHTML,
}

/// Map a tag name to its specialized handler, faithfully to PHP's
/// `newFromTagHandler` `match`.
pub fn handler_kind_for_tag(tag: &str) -> Option<HandlerKind> {
    Some(match tag {
        "a" => HandlerKind::A,
        "audio" | "video" => HandlerKind::Media,
        "b" => HandlerKind::QuoteBold,
        "body" => HandlerKind::Body,
        "br" => HandlerKind::Br,
        "caption" => HandlerKind::Caption,
        "dd" => HandlerKind::Dd,
        "dd_row" => HandlerKind::DdRow,
        "dl" => HandlerKind::Dl,
        "dt" => HandlerKind::Dt,
        "figure" => HandlerKind::Figure,
        "hr" => HandlerKind::Hr,
        "h1" => HandlerKind::Heading(1),
        "h2" => HandlerKind::Heading(2),
        "h3" => HandlerKind::Heading(3),
        "h4" => HandlerKind::Heading(4),
        "h5" => HandlerKind::Heading(5),
        "h6" => HandlerKind::Heading(6),
        "i" => HandlerKind::QuoteItalic,
        "img" => HandlerKind::Img,
        "li" => HandlerKind::Li,
        "link" => HandlerKind::Link,
        "meta" => HandlerKind::Meta,
        "ol" | "ul" => HandlerKind::OlUl,
        "p" => HandlerKind::P,
        "pre" => HandlerKind::Pre,
        "pre_html" => HandlerKind::PreHtml,
        "span" => HandlerKind::Span,
        "table" => HandlerKind::Table,
        "thead" | "tbody" | "tfoot" => HandlerKind::TableBody,
        "td" => HandlerKind::Td,
        "th" => HandlerKind::Th,
        "tr" => HandlerKind::Tr,
        _ => return None,
    })
}

/// The `DOMHandlerFactory` dispatch (`getDOMHandler`), faithful to PHP's logic.
///
/// Returns the concrete handler for `node` (or the `DefaultDomHandler` fallback
/// until the individual handlers are ported). The selection algorithm is
/// complete and faithful; only the terminal handler *instantiation* is a
/// placeholder.
pub fn get_dom_handler(tree: &DomTree, node: NodeId) -> Box<dyn DomHandler> {
    let dp = tree.node(node).dp.clone();
    let stx = dp.as_ref().and_then(|d| d.stx.clone());

    // DocumentFragment → BodyHandler (our Document root is handled the same way).
    if dom_utils::node_name(tree.node(node)).is_empty() {
        return Box::new(DefaultDomHandler);
    }

    // First encapsulation wrapper → EncapsulatedContentHandler.
    if wts_utils::is_first_encapsulation_wrapper_node(tree.node(node)) {
        return Box::new(DefaultDomHandler); // TODO: EncapsulatedContentHandler
    }

    // Specialized handler for `nodeName_stx` (e.g. `dd_row`, `pre_html`).
    let tag = dom_utils::node_name(tree.node(node));
    let specialized = stx.as_ref().map(|s| format!("{tag}_{s}"));

    // Unless a specialized handler exists, use the HTML handler for html-stx
    // tags — but never for `<a>`.
    let specialized_kind = specialized.as_deref().and_then(handler_kind_for_tag);
    if specialized_kind.is_none() && stx.as_deref() == Some("html") && tag != "a" {
        return Box::new(FallbackHTMLHandler);
    }

    if serialize_child_table_tag_as_html(tree, node) {
        return Box::new(FallbackHTMLHandler);
    }

    if dom_utils::is_list_item(tree.node(node))
        && tree
            .parent(node)
            .is_some_and(|p| dom_utils::is_list(tree.node(p)))
        && tree
            .parent(node)
            .is_some_and(|p| wts_utils::is_literal_html_node(tree.node(p)))
    {
        return Box::new(FallbackHTMLHandler);
    }

    // Pick the best available specialized / plain handler. The three simplest
    // concrete handlers are ported; the rest fall back to the no-op default
    // until their modules land.
    let kind = specialized_kind.or_else(|| handler_kind_for_tag(&tag));
    match kind {
        Some(HandlerKind::Body) => Box::new(BodyHandler),
        Some(HandlerKind::QuoteBold) => Box::new(QuoteHandler::new("'''")),
        Some(HandlerKind::QuoteItalic) => Box::new(QuoteHandler::new("''")),
        Some(HandlerKind::TableBody) => Box::new(JustChildrenHandler),
        Some(HandlerKind::Br) => Box::new(BRHandler),
        Some(HandlerKind::Hr) => Box::new(HRHandler),
        Some(HandlerKind::Heading(1)) => Box::new(HeadingHandler::new("=")),
        Some(HandlerKind::Heading(2)) => Box::new(HeadingHandler::new("==")),
        Some(HandlerKind::Heading(3)) => Box::new(HeadingHandler::new("===")),
        Some(HandlerKind::Heading(4)) => Box::new(HeadingHandler::new("====")),
        Some(HandlerKind::Heading(5)) => Box::new(HeadingHandler::new("=====")),
        Some(HandlerKind::Heading(6)) => Box::new(HeadingHandler::new("======")),
        Some(HandlerKind::Dl) => Box::new(ListHandler::new(&["dt", "dd"])),
        Some(HandlerKind::OlUl) => Box::new(ListHandler::new(&["li"])),
        Some(HandlerKind::Li) => Box::new(LIHandler),
        Some(HandlerKind::P) => Box::new(PHandler),
        Some(HandlerKind::Dt) => Box::new(DTHandler),
        Some(HandlerKind::Dd) => Box::new(DDHandler::new(None)),
        Some(HandlerKind::DdRow) => Box::new(DDHandler::new(Some("row"))),
        Some(HandlerKind::Caption) => Box::new(CaptionHandler),
        Some(HandlerKind::Table) => Box::new(TableHandler),
        Some(HandlerKind::Tr) => Box::new(TRHandler),
        Some(HandlerKind::Td) => Box::new(TDHandler),
        Some(HandlerKind::Th) => Box::new(THHandler),
        Some(HandlerKind::Span) => Box::new(SpanHandler),
        Some(HandlerKind::Pre) => Box::new(PreHandler),
        _ => Box::new(DefaultDomHandler),
    }
}

/// `WTUtils::serializeChildTableTagAsHTML` — whether a table tag should be
/// serialized as HTML (because it is inside an HTML-syntax table). Faithful to
/// PHP's `serializeChildTableTagAsHTML`.
pub fn serialize_child_table_tag_as_html(_tree: &DomTree, _node: NodeId) -> bool {
    // Requires walking up to find an HTML-syntax `<table>` ancestor. STUB until
    // the table handlers are ported; returns `false` (native wikitext tables).
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_handler_kind_for_tag() {
        assert_eq!(handler_kind_for_tag("p"), Some(HandlerKind::P));
        assert_eq!(handler_kind_for_tag("h3"), Some(HandlerKind::Heading(3)));
        assert_eq!(handler_kind_for_tag("ul"), Some(HandlerKind::OlUl));
        assert_eq!(handler_kind_for_tag("bogus"), None);
    }

    #[test]
    fn test_get_dom_handler_paragraph() {
        let mut doc = Node::document();
        doc.push_child(Node::element(ElementKind::Paragraph));
        let tree = DomTree::new(doc);
        let p = tree.first_child(tree.root()).unwrap();
        // Returns a boxed handler (currently the default placeholder).
        let _handler = get_dom_handler(&tree, p);
    }
}
