//! Pygments' `HtmlFormatter`, ported for the parts the extension uses.
//!
//! The formatter is where the token stream becomes HTML: each token is wrapped
//! in a `<span class="…">`, spans of the same class merge on one line, and a
//! token that contains a newline closes its span at the line break. Text tokens
//! (class `""`) are written bare.
//!
//! `_wrap_pre` also emits an empty `<span></span>` before the first line, so a
//! `<pre>` whose first line is empty is not collapsed by the HTML parser; that
//! span is part of the served output and therefore part of the port.

use rustoid_ast::dom::node::{ElementKind, Node, NodeKind};

/// One piece of a line.
enum Piece {
    Text(String),
    Span(&'static str, String),
}

/// The formatted lines of `tokens` as DOM nodes.
///
/// For a block (`inline == false`) the first node is Pygments' empty anchor
/// span; for an inline tag the trailing newlines become spaces and the text at
/// the element's outer edges is trimmed, as `SyntaxHighlight::syntaxHighlight`
/// does after Pygments returns.
pub fn nodes(tokens: &[(&'static str, &str)], inline: bool) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    let mut line: Vec<Piece> = Vec::new();

    for (class, value) in tokens {
        let class = (!class.is_empty()).then_some(*class);
        let parts: Vec<&str> = value.split('\n').collect();
        for (i, part) in parts.iter().enumerate() {
            if !part.is_empty() {
                line.push(match class {
                    Some(class) => Piece::Span(class, (*part).to_string()),
                    None => Piece::Text((*part).to_string()),
                });
            }
            if i + 1 < parts.len() {
                flush(&mut out, &mut line);
                out.push(Node::text("\n"));
            }
        }
    }
    flush(&mut out, &mut line);

    if inline {
        for node in &mut out {
            if let NodeKind::Text(text) = &mut node.kind
                && text == "\n"
            {
                *text = " ".to_string();
            }
        }
        trim_edges(&mut out);
    } else {
        let anchor = Node::element(ElementKind::Other("span".to_string()));
        out.insert(0, anchor);
    }
    out
}

/// Group the line's pieces into spans of one class and bare text.
fn flush(out: &mut Vec<Node>, line: &mut Vec<Piece>) {
    let mut i = 0;
    while i < line.len() {
        match &line[i] {
            Piece::Text(text) => {
                out.push(Node::text(text.clone()));
                i += 1;
            }
            Piece::Span(class, _) => {
                let mut text = String::new();
                while let Some(Piece::Span(other, piece)) = line.get(i) {
                    if other != class {
                        break;
                    }
                    text.push_str(piece);
                    i += 1;
                }
                let mut span = Node::element(ElementKind::Other("span".to_string()));
                span.set_attr("class", *class);
                span.push_child(Node::text(text));
                out.push(span);
            }
        }
    }
    line.clear();
}

/// `trim()` around the rendered string: only whitespace at the very edges of
/// the element's content is removed, so a span at either end protects its own.
fn trim_edges(out: &mut Vec<Node>) {
    if let Some(NodeKind::Text(text)) = out.first().map(|n| &n.kind) {
        let trimmed = text.trim_start().to_string();
        if let Some(first) = out.first_mut() {
            first.kind = NodeKind::Text(trimmed);
        }
    }
    if let Some(NodeKind::Text(text)) = out.last().map(|n| &n.kind) {
        let trimmed = text.trim_end().to_string();
        if let Some(last) = out.last_mut() {
            last.kind = NodeKind::Text(trimmed);
        }
    }
    out.retain(|node| !matches!(&node.kind, NodeKind::Text(t) if t.is_empty()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(nodes: &[Node]) -> String {
        let mut out = String::new();
        for node in nodes {
            match &node.kind {
                NodeKind::Text(text) => out.push_str(text),
                NodeKind::Element(ElementKind::Other(tag)) => {
                    out.push('<');
                    out.push_str(tag);
                    if let Some(class) = node.get_attr("class") {
                        out.push_str(&format!(" class=\"{class}\""));
                    }
                    out.push('>');
                    out.push_str(&html(&node.children));
                    out.push_str("</");
                    out.push_str(tag);
                    out.push('>');
                }
                _ => {}
            }
        }
        out
    }

    fn format(tokens: &[(&'static str, &str)]) -> String {
        html(&nodes(tokens, false))
    }

    #[test]
    fn adjacent_tokens_of_one_class_share_a_span() {
        assert_eq!(
            format(&[("nt", "Foo"), ("nt", " Bar"), ("", "\n")]),
            "<span></span><span class=\"nt\">Foo Bar</span>\n"
        );
    }

    #[test]
    fn a_span_ends_at_a_line_break() {
        assert_eq!(
            format(&[("nt", "a\nb"), ("", "\n")]),
            "<span></span><span class=\"nt\">a</span>\n<span class=\"nt\">b</span>\n"
        );
    }

    #[test]
    fn text_tokens_are_bare() {
        assert_eq!(format(&[("", "a\n")]), "<span></span>a\n");
        assert_eq!(format(&[("", "a\n\nb\n")]), "<span></span>a\n\nb\n");
    }

    #[test]
    fn an_inline_body_has_no_anchor_and_no_line_breaks() {
        assert_eq!(
            html(&nodes(&[("nt", "a"), ("", "\n"), ("nt", "b\n")], true)),
            "<span class=\"nt\">a</span> <span class=\"nt\">b</span>"
        );
    }
}
