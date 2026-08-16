//! TreeBuilderStage — drives the TokenTransform3 (line-based) handlers and
//! converts the resulting token stream into the format-agnostic AST.
//!
//! Mirrors the structure of PHP Parsoid's `TreeBuilderStage` (which itself is
//! a thin driver over the `TreeBuilder`), minus the full HTML5 tree
//! construction (which is layered in below).
//!
//! The TT3 handlers run in the following order (mirroring Parsoid's
//! `PipelineFactory`):
//!   PreHandler → QuoteTransformer → ListHandler → ParagraphWrapper

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::wikitext::tokens_v2::{Item, ParsoidToken};

use super::list_handler::ListHandler;
use super::paragraph_wrapper_v2::ParagraphWrapper;
use super::pre_handler::PreHandler;
use super::quote_transformer_v2::QuoteTransformer;

/// Run the TokenTransform3 (line-based) handlers over a token stream.
///
/// This is the token-level half of tree building; the resulting `Vec<Item>` is
/// then handed to a token→AST converter (not yet wired) to produce the DOM.
pub struct TreeBuilderStage {
    inline_context: bool,
}

impl TreeBuilderStage {
    pub fn new(inline_context: bool) -> Self {
        Self { inline_context }
    }

    /// Run the TT3 handlers in order and return the transformed token stream.
    pub fn process(&self, tokens: Vec<Item>) -> Vec<Item> {
        let mut out = tokens;

        // 1. PreHandler (indent-pre detection).
        let mut pre_handler = PreHandler::with_options(self.inline_context);
        out = pre_handler.run(out);

        // 2. QuoteTransformer (mw-quote → b/i).
        out = QuoteTransformer::transform(out);

        // 3. ListHandler (listItem → ul/ol/li).
        let mut list_handler = ListHandler::new();
        out = list_handler.run(out);

        // 4. ParagraphWrapper (wrap content in <p>).
        let mut pw = ParagraphWrapper::with_options(self.inline_context);
        out = pw.wrap(out);

        out
    }

    /// Run the TT3 handlers and convert the result to an AST.
    pub fn to_ast(&self, tokens: Vec<Item>) -> Node {
        let tokens = self.process(tokens);
        token_stream_to_ast(&tokens)
    }
}

impl Default for TreeBuilderStage {
    fn default() -> Self {
        Self::new(false)
    }
}

/// Convert a transformed token stream into the format-agnostic `Node` AST.
///
/// This mirrors the token→DOM construction of PHP Parsoid's `TreeBuilder` for
/// the core element kinds, using a simple open-element stack.
pub fn token_stream_to_ast(tokens: &[Item]) -> Node {
    let mut doc = Node::document();
    // Stack of currently-open elements (the top is where content is added).
    let mut stack: Vec<Node> = Vec::new();

    for item in tokens {
        match item {
            Item::Str(s) => {
                push_text(&mut doc, &mut stack, s);
            }
            Item::Tok(tok) => match tok {
                ParsoidToken::Tag(t) => {
                    open_element(&mut stack, &t.name, &t.attribs, &t.data_parsoid);
                }
                ParsoidToken::EndTag(t) => {
                    close_element(&mut doc, &mut stack, &t.name);
                }
                ParsoidToken::SelfclosingTag(t) => {
                    selfclosing(&mut doc, &mut stack, &t.name, &t.attribs, &t.data_parsoid);
                }
                ParsoidToken::Comment(c) => {
                    let comment = Node::comment(&c.value);
                    push_node(&mut doc, &mut stack, comment);
                }
                ParsoidToken::Nl(_) => {
                    // Newlines between blocks are dropped by the tree builder.
                }
                _ => {}
            },
        }
    }

    // Close any remaining open elements.
    while let Some(node) = stack.pop() {
        push_node(&mut doc, &mut stack, node);
    }

    doc
}

/// Map a tag name to an `ElementKind`.
fn element_kind(name: &str) -> ElementKind {
    match name {
        "h1" => ElementKind::Heading(1),
        "h2" => ElementKind::Heading(2),
        "h3" => ElementKind::Heading(3),
        "h4" => ElementKind::Heading(4),
        "h5" => ElementKind::Heading(5),
        "h6" => ElementKind::Heading(6),
        "p" => ElementKind::Paragraph,
        "b" => ElementKind::Bold,
        "i" => ElementKind::Italic,
        "pre" => ElementKind::Preformatted,
        "table" => ElementKind::Table,
        "tr" => ElementKind::TableRow,
        "td" | "th" => ElementKind::TableCell,
        "caption" => ElementKind::TableCaption,
        "ul" => ElementKind::UnorderedList,
        "ol" => ElementKind::OrderedList,
        "li" | "dt" => ElementKind::ListItem,
        "dd" => ElementKind::DefinitionDescription,
        "dl" => ElementKind::DefinitionList,
        "div" => ElementKind::Div,
        "span" => ElementKind::Span,
        "blockquote" => ElementKind::Div,
        "section" => ElementKind::Section,
        other => ElementKind::Other(other.to_string()),
    }
}

fn open_element(
    stack: &mut Vec<Node>,
    name: &str,
    attrs: &[crate::wikitext::tokens_v2::KV],
    dp: &crate::wikitext::tokens_v2::DataParsoid,
) {
    let mut node = Node::element(element_kind(name));
    copy_attribs(&mut node, attrs);
    node.data_parsoid = dp.to_data_parsoid_json();
    stack.push(node);
}

fn close_element(doc: &mut Node, stack: &mut Vec<Node>, name: &str) {
    // Pop until we close the matching element name.
    let mut closed = Vec::new();
    while let Some(node) = stack.pop() {
        if let NodeKind::Element(kind) = &node.kind
            && kind_name_matches(kind, name)
        {
            // This node's children are already populated; attach it.
            push_node(doc, stack, node);
            for n in closed.into_iter().rev() {
                push_node(doc, stack, n);
            }
            return;
        }
        closed.push(node);
    }
    // No matching open element: reattach whatever we popped.
    for n in closed.into_iter().rev() {
        push_node(doc, stack, n);
    }
}

fn kind_name_matches(kind: &ElementKind, name: &str) -> bool {
    match (kind, name) {
        (ElementKind::Heading(n), "h1") => *n == 1,
        (ElementKind::Heading(n), "h2") => *n == 2,
        (ElementKind::Heading(n), "h3") => *n == 3,
        (ElementKind::Heading(n), "h4") => *n == 4,
        (ElementKind::Heading(n), "h5") => *n == 5,
        (ElementKind::Heading(n), "h6") => *n == 6,
        (ElementKind::Paragraph, "p") => true,
        (ElementKind::Bold, "b") => true,
        (ElementKind::Italic, "i") => true,
        (ElementKind::Preformatted, "pre") => true,
        (ElementKind::Table, "table") => true,
        (ElementKind::TableRow, "tr") => true,
        (ElementKind::TableCell, "td") | (ElementKind::TableCell, "th") => true,
        (ElementKind::TableCaption, "caption") => true,
        (ElementKind::UnorderedList, "ul") => true,
        (ElementKind::OrderedList, "ol") => true,
        (ElementKind::ListItem, "li") | (ElementKind::ListItem, "dt") => true,
        (ElementKind::DefinitionDescription, "dd") => true,
        (ElementKind::DefinitionList, "dl") => true,
        (ElementKind::Div, "div") | (ElementKind::Div, "blockquote") => true,
        (ElementKind::Span, "span") => true,
        _ => false,
    }
}

fn selfclosing(
    doc: &mut Node,
    stack: &mut [Node],
    name: &str,
    attrs: &[crate::wikitext::tokens_v2::KV],
    dp: &crate::wikitext::tokens_v2::DataParsoid,
) {
    let kind = match name {
        "hr" => ElementKind::HorizontalRule,
        "br" => ElementKind::LineBreak,
        "wikilink" => ElementKind::Wikilink,
        "extlink" => ElementKind::ExtLink,
        "urllink" => ElementKind::ExtLink,
        "mw:redirect" => ElementKind::Redirect,
        other => ElementKind::Other(other.to_string()),
    };
    let mut node = Node::element(kind);
    copy_attribs(&mut node, attrs);
    node.data_parsoid = dp.to_data_parsoid_json();
    push_node(doc, stack, node);
}

fn copy_attribs(node: &mut Node, attrs: &[crate::wikitext::tokens_v2::KV]) {
    for kv in attrs {
        if let Some(k) = kv.key.as_str() {
            // `data-mw` is a first-class field, not a regular attribute.
            if k == "data-mw" {
                if let Some(v) = kv.value.as_str() {
                    node.data_mw = Some(v.to_string());
                }
                continue;
            }
            if k == "data-parsoid" {
                continue;
            }
            match kv.value.as_str() {
                Some(v) => node.set_attr(k, v),
                None => node.set_attr(k, ""),
            }
        }
    }
}

fn push_text(doc: &mut Node, stack: &mut [Node], text: &str) {
    if text.is_empty() {
        return;
    }
    push_node(doc, stack, Node::text(text.to_string()));
}

fn push_node(doc: &mut Node, stack: &mut [Node], node: Node) {
    if let Some(top) = stack.last_mut() {
        top.push_child(node);
    } else {
        doc.push_child(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};

    fn tokenize(wikitext: &str) -> Vec<Item> {
        let options = TokenizerOptions::default();
        let mut tokenizer = PegTokenizer::new(wikitext, &options);
        tokenizer
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|e| match e {
                crate::wikitext::tokens_v2::Either::Left(s) => Item::Str(s),
                crate::wikitext::tokens_v2::Either::Right(t) => Item::Tok(t),
            })
            .collect()
    }

    #[test]
    fn test_process_plain_text() {
        let stage = TreeBuilderStage::new(false);
        let out = stage.process(tokenize("hello world"));
        assert!(!out.is_empty());
    }

    #[test]
    fn test_process_heading() {
        let stage = TreeBuilderStage::new(false);
        let out = stage.process(tokenize("== Heading ==\n"));
        // Should contain an h2 tag after TT3.
        assert!(out.iter().any(|it| {
            matches!(it, Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::Tag(t)) if t.name == "h2")
        }));
    }

    #[test]
    fn test_process_bold() {
        let stage = TreeBuilderStage::new(false);
        let out = stage.process(tokenize("'''bold'''"));
        // Should contain a <b> tag (from quote transformer).
        assert!(out.iter().any(|it| {
            matches!(it, Item::Tok(crate::wikitext::tokens_v2::ParsoidToken::Tag(t)) if t.name == "b")
        }));
    }

    #[test]
    fn test_to_ast_heading() {
        use crate::dom::node::{ElementKind, NodeKind};

        let stage = TreeBuilderStage::new(false);
        let doc = stage.to_ast(tokenize("== Heading ==\n"));

        // The document should contain an h2 element.
        assert!(
            doc.children
                .iter()
                .any(|n| { matches!(&n.kind, NodeKind::Element(ElementKind::Heading(2))) })
        );
    }

    #[test]
    fn test_to_ast_bold() {
        let stage = TreeBuilderStage::new(false);
        let doc = stage.to_ast(tokenize("'''bold'''"));

        // The document should contain a bold element (possibly nested in <p>).
        assert!(contains_bold(&doc), "expected a bold element: {doc:?}");
    }

    fn contains_bold(node: &Node) -> bool {
        use crate::dom::node::{ElementKind, NodeKind};
        if let NodeKind::Element(ElementKind::Bold) = &node.kind {
            return true;
        }
        node.children.iter().any(contains_bold)
    }

    #[test]
    fn test_to_ast_wikilink() {
        use crate::dom::node::{ElementKind, NodeKind};

        let stage = TreeBuilderStage::new(false);
        let doc = stage.to_ast(tokenize("[[Main Page]]"));

        assert!(doc.children.iter().any(|n| {
            matches!(&n.kind, NodeKind::Element(ElementKind::Wikilink))
                || n.children
                    .iter()
                    .any(|c| matches!(&c.kind, NodeKind::Element(ElementKind::Wikilink)))
        }));
    }
}
