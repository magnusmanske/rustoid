//! AST → wikitext serializer.
//!
//! Converts our AST back to wikitext for round-tripping (Phase 7).

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::error::Result;

/// Serialize an AST document to wikitext.
pub fn ast_to_wikitext(node: &Node) -> Result<String> {
    let mut buf = String::new();
    serialize_node(node, &mut buf, 0, &mut Context::default())?;
    Ok(buf)
}

#[derive(Default)]
struct Context {
    /// Current list nesting depth.
    list_depth: usize,
}

fn serialize_node(node: &Node, buf: &mut String, _depth: usize, ctx: &mut Context) -> Result<()> {
    match &node.kind {
        NodeKind::Document => {
            for child in &node.children {
                serialize_node(child, buf, _depth, ctx)?;
            }
        }
        NodeKind::Element(kind) => {
            serialize_element(kind, node, buf, ctx)?;
        }
        NodeKind::Text(text) => {
            buf.push_str(text);
        }
        NodeKind::Comment(content) => {
            buf.push_str(&format!("<!--{content}-->"));
        }
    }
    Ok(())
}

fn serialize_element(
    kind: &ElementKind,
    node: &Node,
    buf: &mut String,
    ctx: &mut Context,
) -> Result<()> {
    match kind {
        ElementKind::Document => {
            for child in &node.children {
                serialize_node(child, buf, 0, ctx)?;
            }
        }
        ElementKind::Paragraph => {
            // Check if this is a wrapper paragraph we can skip
            serialize_children(node, buf, ctx)?;
            buf.push('\n');
        }
        ElementKind::Heading(level) => {
            let eq = "=".repeat(*level as usize);
            buf.push_str(&eq);
            serialize_children(node, buf, ctx)?;
            buf.push_str(&eq);
            buf.push('\n');
        }
        ElementKind::Bold => {
            buf.push_str("'''");
            serialize_children(node, buf, ctx)?;
            buf.push_str("'''");
        }
        ElementKind::Italic => {
            buf.push_str("''");
            serialize_children(node, buf, ctx)?;
            buf.push_str("''");
        }
        ElementKind::Wikilink => {
            buf.push_str("[[");
            if let Some(href) = node.get_attr("href") {
                buf.push_str(href);
            }
            // Check if children provide display text different from href
            let display = collect_text(node);
            let href = node.get_attr("href").unwrap_or("");
            if !display.is_empty() && display != href {
                buf.push('|');
                buf.push_str(&display);
            }
            buf.push_str("]]");
        }
        ElementKind::ExtLink => {
            let href = node.get_attr("href").unwrap_or("");
            buf.push('[');
            buf.push_str(href);
            let display = collect_text(node);
            if !display.is_empty() && display != format!("[{href}]") {
                buf.push(' ');
                buf.push_str(&display);
            }
            buf.push(']');
        }
        ElementKind::UnorderedList | ElementKind::OrderedList => {
            let marker = match kind {
                ElementKind::OrderedList => '#',
                _ => '*',
            };
            ctx.list_depth += 1;
            for child in &node.children {
                if matches!(child.kind, NodeKind::Element(ElementKind::ListItem)) {
                    let prefix = marker.to_string().repeat(ctx.list_depth);
                    buf.push_str(&prefix);
                    buf.push(' ');
                    serialize_children(child, buf, ctx)?;
                    buf.push('\n');
                }
            }
            ctx.list_depth -= 1;
        }
        ElementKind::ListItem => {
            // Handled by parent list
            serialize_children(node, buf, ctx)?;
        }
        ElementKind::Table => {
            buf.push_str("{|");
            for attr in &node.attrs {
                if !attr.value.is_empty() {
                    buf.push_str(&format!(" {}=\"{}\"", attr.key, attr.value));
                } else {
                    buf.push_str(&format!(" {}", attr.key));
                }
            }
            buf.push('\n');
            serialize_children(node, buf, ctx)?;
            buf.push_str("|}\n");
        }
        ElementKind::TableRow => {
            buf.push_str("|-\n");
            serialize_children(node, buf, ctx)?;
        }
        ElementKind::TableCell => {
            buf.push('|');
            serialize_children(node, buf, ctx)?;
            buf.push('\n');
        }
        ElementKind::TableCaption => {
            buf.push_str("|+ ");
            serialize_children(node, buf, ctx)?;
            buf.push('\n');
        }
        ElementKind::Preformatted => {
            buf.push_str("<pre>");
            serialize_children(node, buf, ctx)?;
            buf.push_str("</pre>\n");
        }
        ElementKind::HorizontalRule => {
            buf.push_str("----\n");
        }
        ElementKind::Div => {
            buf.push_str("<div");
            for attr in &node.attrs {
                buf.push_str(&format!(" {}=\"{}\"", attr.key, attr.value));
            }
            buf.push('>');
            serialize_children(node, buf, ctx)?;
            buf.push_str("</div>\n");
        }
        ElementKind::Span => {
            buf.push_str("<span");
            for attr in &node.attrs {
                buf.push_str(&format!(" {}=\"{}\"", attr.key, attr.value));
            }
            buf.push('>');
            serialize_children(node, buf, ctx)?;
            buf.push_str("</span>");
        }
        ElementKind::LineBreak => {
            buf.push_str("<br/>\n");
        }
        ElementKind::Image => {
            buf.push_str("[[");
            if let Some(src) = node.get_attr("src") {
                buf.push_str(src);
            }
            buf.push_str("]]");
        }
        ElementKind::Transclusion => {
            // Reconstruct from data-mw if available
            if let Some(ref data_mw) = node.data_mw
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(data_mw)
                && let Some(parts) = json.get("parts").and_then(|p| p.as_array())
            {
                let mut wikitext = String::new();
                for part in parts {
                    if let Some(tpl) = part.get("template")
                        && let Some(target) = tpl.get("target").and_then(|t| t.get("wt"))
                    {
                        wikitext.push_str("{{");
                        wikitext.push_str(target.as_str().unwrap_or(""));
                        if let Some(params) = tpl.get("params")
                            && let Some(obj) = params.as_object()
                        {
                            for (key, val) in obj {
                                wikitext.push('|');
                                wikitext.push_str(key);
                                wikitext.push('=');
                                wikitext
                                    .push_str(val.get("wt").and_then(|v| v.as_str()).unwrap_or(""));
                            }
                        }
                        wikitext.push_str("}}");
                    }
                }
                if !wikitext.is_empty() {
                    buf.push_str(&wikitext);
                    return Ok(());
                }
            }
            // Fallback: serialize children
            serialize_children(node, buf, ctx)?;
        }
        ElementKind::ExtensionTag => {
            // Reconstruct from data-mw
            if let Some(ref data_mw) = node.data_mw
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(data_mw)
                && let Some(name) = json.get("name").and_then(|n| n.as_str())
            {
                buf.push('<');
                buf.push_str(name);
                if let Some(attrs) = json.get("attrs").and_then(|a| a.as_object()) {
                    for (k, v) in attrs {
                        buf.push(' ');
                        buf.push_str(k);
                        buf.push_str("=\"");
                        buf.push_str(v.as_str().unwrap_or(""));
                        buf.push('"');
                    }
                }
                if let Some(body) = json.get("body").and_then(|b| b.get("extsrc")) {
                    buf.push('>');
                    buf.push_str(body.as_str().unwrap_or(""));
                    buf.push_str("</");
                    buf.push_str(name);
                    buf.push('>');
                } else {
                    buf.push_str("/>");
                }
                return Ok(());
            }
            // Fallback
            buf.push_str("<ext/>");
        }
        ElementKind::Other(tag) => {
            buf.push('<');
            buf.push_str(tag);
            buf.push('>');
            serialize_children(node, buf, ctx)?;
            buf.push_str("</");
            buf.push_str(tag);
            buf.push('>');
        }
        _ => {
            // Unknown element — serialize children only
            serialize_children(node, buf, ctx)?;
        }
    }
    Ok(())
}

fn serialize_children(node: &Node, buf: &mut String, ctx: &mut Context) -> Result<()> {
    for child in &node.children {
        serialize_node(child, buf, 0, ctx)?;
    }
    Ok(())
}

/// Collect all text content from a node's children.
fn collect_text(node: &Node) -> String {
    let mut text = String::new();
    for child in &node.children {
        match &child.kind {
            NodeKind::Text(t) => text.push_str(t),
            NodeKind::Element(ElementKind::Bold) => {
                text.push_str("'''");
                text.push_str(&collect_text(child));
                text.push_str("'''");
            }
            NodeKind::Element(ElementKind::Italic) => {
                text.push_str("''");
                text.push_str(&collect_text(child));
                text.push_str("''");
            }
            _ => {
                text.push_str(&collect_text(child));
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::Node;

    #[test]
    fn test_simple_text() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("Hello world"));
        doc.push_child(p);

        let wikitext = ast_to_wikitext(&doc).unwrap();
        assert_eq!(wikitext.trim(), "Hello world");
    }

    #[test]
    fn test_bold() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        let mut b = Node::element(ElementKind::Bold);
        b.push_child(Node::text("bold"));
        p.push_child(b);
        doc.push_child(p);

        let wikitext = ast_to_wikitext(&doc).unwrap();
        assert!(wikitext.contains("'''bold'''"));
    }

    #[test]
    fn test_wikilink() {
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        let mut link = Node::element(ElementKind::Wikilink);
        link.set_attr("href", "Main Page");
        link.push_child(Node::text("Main Page"));
        p.push_child(link);
        doc.push_child(p);

        let wikitext = ast_to_wikitext(&doc).unwrap();
        assert!(wikitext.contains("[[Main Page]]"));
    }

    #[test]
    fn test_heading() {
        let mut doc = Node::document();
        let mut h = Node::element(ElementKind::Heading(2));
        h.push_child(Node::text("Title"));
        doc.push_child(h);

        let wikitext = ast_to_wikitext(&doc).unwrap();
        assert!(wikitext.contains("==Title=="));
    }

    #[test]
    fn test_roundtrip_simple() {
        // Parse HTML, serialize to wikitext, verify
        let html = "<p>Hello <b>world</b></p>";
        let ast = crate::html::parse::parse_html(html).unwrap();
        let wikitext = ast_to_wikitext(&ast).unwrap();
        assert!(wikitext.contains("'''world'''"));
        assert!(wikitext.contains("Hello"));
    }
}
