//! Tree builder — converts token stream to AST.
//!
//! Takes the flat token stream from the preprocessor and constructs a nested
//! AST with proper block/inline structure. This implements the wikitext-to-DOM
//! tree construction algorithm adapted from Parsoid's TreeBuilder.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::error::Result;
use crate::wikitext::tokens::WikitextToken;

/// Builds an AST from a stream of wikitext tokens.
pub struct TreeBuilder;

impl TreeBuilder {
    /// Create a new tree builder.
    pub fn new() -> Self {
        Self
    }

    /// Build an AST from a token stream.
    pub fn build(&mut self, tokens: &[WikitextToken]) -> Result<Node> {
        let mut doc = Node::document();
        let mut inline_buf: Vec<Node> = Vec::new();
        let mut fmt_stack: Vec<ElementKind> = Vec::new();
        let mut at_line_start = true;

        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];

            // Helper: take inline_buf and wrap in paragraph if non-empty
            // (defined as a repeated pattern, not a closure)

            // Handle block-level token types
            match token {
                WikitextToken::HeadingOpen(level) => {
                    doc = self.flush_inline_into_para(doc, inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    let (heading, new_i) = self.build_heading(tokens, i, *level);
                    doc.push_child(heading);
                    i = new_i;
                    at_line_start = true;
                }
                WikitextToken::ListItem(ch, depth) => {
                    doc = self.flush_inline_into_para(doc, inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    let prefix = std::mem::take(&mut inline_buf);
                    let (list_item, new_i) = self.build_list_item(tokens, i, *ch, *depth, prefix);
                    doc.push_child(list_item);
                    i = new_i;
                    at_line_start = true;
                }
                WikitextToken::Hr => {
                    doc = self.flush_inline_into_para(doc, inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    doc.push_child(Node::element(ElementKind::HorizontalRule));
                    i += 1;
                    at_line_start = true;
                }
                WikitextToken::ParagraphBreak => {
                    doc = self.flush_inline_into_para(doc, inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    i += 1;
                    at_line_start = true;
                }
                WikitextToken::TableOpen(_) => {
                    let prefix = std::mem::take(&mut inline_buf);
                    let (table, new_i) = self.build_table(tokens, i, prefix);
                    doc.push_child(table);
                    i = new_i;
                }
                WikitextToken::WikilinkOpen => {
                    let (link_node, new_i) = self.build_wikilink(tokens, i);
                    inline_buf.push(link_node);
                    i = new_i;
                }
                WikitextToken::ExtLinkOpen(url) => {
                    let (link_node, new_i) = self.build_extlink(tokens, i, url);
                    inline_buf.push(link_node);
                    i = new_i;
                }
                WikitextToken::BoldOpen => {
                    // If inside Italic (''''' = bold+italic), this BoldOpen also opens Italic
                    let in_italic = fmt_stack.contains(&ElementKind::Italic);
                    if fmt_stack.contains(&ElementKind::Bold) {
                        // Close: unwind until Bold
                        while let Some(top) = fmt_stack.last() {
                            wrap_buf_in_fmt(&mut inline_buf, &fmt_stack);
                            let kind = top.clone();
                            fmt_stack.pop();
                            if kind == ElementKind::Bold {
                                break;
                            }
                        }
                        // If we were in italic too and just unwound past it, close italic
                        if in_italic && fmt_stack.contains(&ElementKind::Italic) {
                            while let Some(top) = fmt_stack.last() {
                                wrap_buf_in_fmt(&mut inline_buf, &fmt_stack);
                                let kind = top.clone();
                                fmt_stack.pop();
                                if kind == ElementKind::Italic {
                                    break;
                                }
                            }
                        }
                    } else {
                        wrap_buf_in_fmt(&mut inline_buf, &fmt_stack);
                        fmt_stack.push(ElementKind::Bold);
                        if in_italic {
                            fmt_stack.push(ElementKind::Italic);
                        }
                    }
                    i += 1;
                }
                WikitextToken::ItalicOpen => {
                    if fmt_stack.contains(&ElementKind::Italic) {
                        while let Some(top) = fmt_stack.last() {
                            wrap_buf_in_fmt(&mut inline_buf, &fmt_stack);
                            let kind = top.clone();
                            fmt_stack.pop();
                            if kind == ElementKind::Italic {
                                break;
                            }
                        }
                    } else {
                        wrap_buf_in_fmt(&mut inline_buf, &fmt_stack);
                        fmt_stack.push(ElementKind::Italic);
                    }
                    i += 1;
                }
                WikitextToken::Text(text) => {
                    inline_buf.push(Node::text(text.clone()));
                    i += 1;
                }
                WikitextToken::Comment(comment) => {
                    if at_line_start || inline_buf.is_empty() {
                        doc.push_child(Node::comment(comment.clone()));
                    } else {
                        inline_buf.push(Node::comment(comment.clone()));
                    }
                    i += 1;
                }
                WikitextToken::NowikiContent(content) => {
                    // Nowiki content is a block-level preformatted element
                    doc = self.flush_inline_into_para(doc, inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    let mut pre = Node::element(ElementKind::Preformatted);
                    pre.push_child(Node::text(content.clone()));
                    doc.push_child(pre);
                    at_line_start = true;
                    i += 1;
                }
                WikitextToken::SelfClosingTag(name, _) => {
                    if name == "br" {
                        inline_buf.push(Node::element(ElementKind::LineBreak));
                    } else {
                        inline_buf.push(Node::text(format!("<{name}/>")));
                    }
                    i += 1;
                }
                WikitextToken::HtmlTagOpen(name, _) => {
                    let is_block = matches!(name.as_str(), "div" | "pre" | "blockquote");
                    let tag_kind = match name.as_str() {
                        "b" | "strong" => ElementKind::Bold,
                        "i" | "em" => ElementKind::Italic,
                        "div" => ElementKind::Div,
                        "span" => ElementKind::Span,
                        "pre" => ElementKind::Preformatted,
                        "code" | "tt" => ElementKind::Span,
                        _ => ElementKind::Other(name.clone()),
                    };
                    if is_block {
                        // Block-level HTML tag: flush pending paragraph first
                        doc = self.flush_inline_into_para(doc, inline_buf, &fmt_stack);
                        inline_buf = Vec::new();
                        fmt_stack.clear();
                        doc.push_child(Node::element(tag_kind));
                        at_line_start = true;
                    } else {
                        inline_buf.push(Node::element(tag_kind));
                        at_line_start = false;
                    }
                    i += 1;
                }
                WikitextToken::HtmlTagClose(_name) => {
                    // Close tags just mark end of element; we push a close marker
                    // that is handled in post-processing (Phase 6).
                    // For now, ignore closing HTML tags in tree building.
                    i += 1;
                }
                WikitextToken::MagicWord(word) => {
                    inline_buf.push(Node::text(format!("[{word}]")));
                    i += 1;
                }
                WikitextToken::EOF => {
                    // Flush remaining inline content
                    doc = self.flush_inline_into_para(doc, inline_buf, &fmt_stack);
                    break;
                }
                _ => {
                    i += 1;
                }
            }
        }

        // Post-process: group list items into proper list wrappers
        let doc = self.group_lists(doc);

        Ok(doc)
    }

    /// Flush inline buffer into a paragraph, appending to doc.
    fn flush_inline_into_para(
        &self,
        doc: Node,
        inline_buf: Vec<Node>,
        fmt_stack: &[ElementKind],
    ) -> Node {
        // Wrap any remaining content in open formatting elements
        let mut buf = inline_buf;
        wrap_buf_in_fmt(&mut buf, fmt_stack);
        flush_into_para(doc, buf)
    }

    /// Build a heading element: consume tokens until the end of the heading line.
    fn build_heading(
        &mut self,
        tokens: &[WikitextToken],
        start: usize,
        level: u8,
    ) -> (Node, usize) {
        let mut heading = Node::element(ElementKind::Heading(level));
        let mut i = start + 1; // past HeadingOpen

        while i < tokens.len() {
            match &tokens[i] {
                WikitextToken::HeadingOpen(_) | WikitextToken::HeadingClose => {
                    i += 1;
                    break; // End of heading content
                }
                WikitextToken::Text(text) => {
                    heading.push_child(Node::text(text.clone()));
                    i += 1;
                }
                WikitextToken::WikilinkOpen => {
                    let (link, new_i) = self.build_wikilink(tokens, i);
                    heading.push_child(link);
                    i = new_i;
                }
                WikitextToken::ExtLinkOpen(url) => {
                    let (link, new_i) = self.build_extlink(tokens, i, url);
                    heading.push_child(link);
                    i = new_i;
                }
                WikitextToken::BoldOpen => {
                    heading.push_child(Node::element(ElementKind::Bold));
                    i += 1;
                }
                WikitextToken::ItalicOpen => {
                    heading.push_child(Node::element(ElementKind::Italic));
                    i += 1;
                }
                WikitextToken::Newline | WikitextToken::ParagraphBreak => {
                    break; // End of heading at newline
                }
                WikitextToken::EOF => break,
                _ => {
                    i += 1;
                }
            }
        }

        // Strip trailing equals from last text node, and leading/trailing whitespace
        if let Some(last) = heading.children.last_mut()
            && let NodeKind::Text(ref mut text) = last.kind
        {
            *text = text.trim_end_matches('=').trim().to_string();
            if text.is_empty() {
                heading.children.pop();
            }
        }
        // Also strip leading space from first text node
        if let Some(first) = heading.children.first_mut()
            && let NodeKind::Text(ref mut text) = first.kind
        {
            *text = text.trim_start().to_string();
            if text.is_empty() {
                heading.children.remove(0);
            }
        }

        (heading, i)
    }

    /// Build a list item, consuming tokens until the next list marker or block.
    fn build_list_item(
        &mut self,
        tokens: &[WikitextToken],
        start: usize,
        _ch: char,
        _depth: u8,
        prefix_children: Vec<Node>,
    ) -> (Node, usize) {
        let kind = if _ch == '#' {
            ElementKind::OrderedList
        } else if _ch == ';' || _ch == ':' {
            ElementKind::DefinitionList
        } else {
            ElementKind::UnorderedList
        };

        let mut list = Node::element(kind);
        let mut item = Node::element(ElementKind::ListItem);

        // Add any prefix inline content
        for child in prefix_children {
            item.push_child(child);
        }

        let mut i = start + 1; // past ListItem token

        while i < tokens.len() {
            match &tokens[i] {
                WikitextToken::Newline | WikitextToken::ParagraphBreak | WikitextToken::EOF => {
                    i += 1;
                    break;
                }
                WikitextToken::Text(text) => {
                    item.push_child(Node::text(text.clone()));
                    i += 1;
                }
                WikitextToken::BoldOpen => {
                    item.push_child(Node::element(ElementKind::Bold));
                    i += 1;
                }
                WikitextToken::ItalicOpen => {
                    item.push_child(Node::element(ElementKind::Italic));
                    i += 1;
                }
                WikitextToken::WikilinkOpen => {
                    let (link, new_i) = self.build_wikilink(tokens, i);
                    item.push_child(link);
                    i = new_i;
                }
                WikitextToken::ExtLinkOpen(url) => {
                    let (link, new_i) = self.build_extlink(tokens, i, url);
                    item.push_child(link);
                    i = new_i;
                }
                _ => {
                    i += 1;
                }
            }
        }

        list.push_child(item);

        // Trim leading space from first text node
        if let Some(first) = list.children.first_mut()
            && let Some(first_item) = first.children.first_mut()
            && let NodeKind::Text(ref mut text) = first_item.kind
        {
            *text = text.trim_start().to_string();
        }

        (list, i)
    }

    /// Build a wikilink: [[Page|Display]]
    fn build_wikilink(&mut self, tokens: &[WikitextToken], start: usize) -> (Node, usize) {
        let mut link = Node::element(ElementKind::Wikilink);
        let mut page = String::new();
        let mut display = String::new();
        let mut after_pipe = false;
        let mut i = start + 1; // past WikilinkOpen

        while i < tokens.len() {
            match &tokens[i] {
                WikitextToken::WikilinkClose => {
                    i += 1;
                    break;
                }
                WikitextToken::WikilinkPipe => {
                    after_pipe = true;
                    i += 1;
                }
                WikitextToken::Text(text) => {
                    if after_pipe {
                        display.push_str(text);
                    } else {
                        page.push_str(text);
                    }
                    i += 1;
                }
                _ => {
                    i += 1;
                }
            }
        }

        link.set_attr("href", &page);
        if display.is_empty() {
            // Display is the page title with namespace prefix removed if same namespace
            // For simplicity, use the full page name
            display = page.clone();
            // Strip namespace prefix for display if it matches current context
            if let Some(colon_pos) = display.find(':') {
                display = display[colon_pos + 1..].to_string();
            }
        }
        link.push_child(Node::text(display));

        (link, i)
    }

    /// Build an external link: [url text]
    fn build_extlink(
        &mut self,
        tokens: &[WikitextToken],
        start: usize,
        url: &str,
    ) -> (Node, usize) {
        let mut link = Node::element(ElementKind::ExtLink);
        link.set_attr("href", url);

        let mut display = String::new();
        let mut i = start + 1;

        while i < tokens.len() {
            match &tokens[i] {
                WikitextToken::ExtLinkClose => {
                    i += 1;
                    break;
                }
                WikitextToken::Text(text) => {
                    display.push_str(text);
                    i += 1;
                }
                WikitextToken::EOF => break,
                _ => {
                    i += 1;
                }
            }
        }

        if display.is_empty() {
            // Auto-numbered link
            display = format!("[{url}]");
        }
        link.push_child(Node::text(display));

        (link, i)
    }

    /// Build a table from {|...|}
    fn build_table(
        &mut self,
        tokens: &[WikitextToken],
        start: usize,
        prefix_children: Vec<Node>,
    ) -> (Node, usize) {
        let mut table = Node::element(ElementKind::Table);
        if let WikitextToken::TableOpen(attrs) = &tokens[start] {
            for (key, val) in attrs {
                table.set_attr(key, val);
            }
        }

        // Add any foster content (fostering — content moved out of table)
        for child in prefix_children {
            if !matches!(child.kind, crate::dom::node::NodeKind::Text(ref t) if t.trim().is_empty())
            {
                table.push_child(child);
            }
        }

        let mut i = start + 1;
        let mut current_row: Option<Node> = None;
        let mut current_cell_buf = String::new();

        let flush_cell = |row: &mut Node, buf: &mut String| {
            if !buf.trim().is_empty() || !row.children.is_empty() {
                let mut cell = Node::element(ElementKind::TableCell);
                cell.push_child(Node::text(buf.trim().to_string()));
                row.push_child(cell);
            }
            buf.clear();
        };

        while i < tokens.len() {
            match &tokens[i] {
                WikitextToken::TableClose => {
                    if let Some(mut row) = current_row.take() {
                        flush_cell(&mut row, &mut current_cell_buf);
                        table.push_child(row);
                    }
                    i += 1;
                    break;
                }
                WikitextToken::TableRow => {
                    if let Some(mut row) = current_row.take() {
                        flush_cell(&mut row, &mut current_cell_buf);
                        table.push_child(row);
                    }
                    current_row = Some(Node::element(ElementKind::TableRow));
                    i += 1;
                }
                WikitextToken::TableCell | WikitextToken::TableCaption => {
                    if current_row.is_none() {
                        current_row = Some(Node::element(ElementKind::TableRow));
                    }
                    if let Some(ref mut row) = current_row {
                        flush_cell(row, &mut current_cell_buf);
                    }
                    i += 1;
                }
                WikitextToken::Text(text) => {
                    current_cell_buf.push_str(text);
                    i += 1;
                }
                WikitextToken::Newline => {
                    i += 1;
                }
                WikitextToken::EOF => break,
                _ => {
                    i += 1;
                }
            }
        }

        (table, i)
    }

    /// Post-process: group consecutive list items into proper parent list elements.
    fn group_lists(&self, doc: Node) -> Node {
        let mut result = Node::document();
        let mut pending_lists: Vec<Node> = Vec::new();
        let mut last_list_kind: Option<ElementKind> = None;

        for child in doc.children {
            match &child.kind {
                crate::dom::node::NodeKind::Element(kind)
                    if matches!(
                        kind,
                        ElementKind::UnorderedList
                            | ElementKind::OrderedList
                            | ElementKind::DefinitionList
                    ) =>
                {
                    if last_list_kind.as_ref() == Some(kind) {
                        // Same kind — merge into current list
                        if let Some(last_list) = pending_lists.last_mut() {
                            // Transfer items from this list to the running list
                            for item in &child.children {
                                last_list.push_child(item.clone());
                            }
                        }
                    } else {
                        // Different kind or first — start new list
                        // Flush previous pending
                        for list in pending_lists.drain(..) {
                            result.push_child(list);
                        }
                        last_list_kind = Some(kind.clone());
                        pending_lists.push(child.clone());
                    }
                }
                _ => {
                    // Non-list element — flush all pending lists
                    for list in pending_lists.drain(..) {
                        result.push_child(list);
                    }
                    last_list_kind = None;
                    result.push_child(child.clone());
                }
            }
        }

        // Flush remaining
        for list in pending_lists {
            result.push_child(list);
        }

        result
    }
}

/// Wrap a flat node buffer into the innermost formatting element.
fn wrap_buf_in_fmt(buf: &mut Vec<Node>, stack: &[ElementKind]) {
    if buf.is_empty() || stack.is_empty() {
        return;
    }
    let kind = stack.last().unwrap().clone();
    let mut wrapper = Node::element(kind);
    for node in buf.drain(..) {
        wrapper.push_child(node);
    }
    buf.push(wrapper);
}

/// Flush a buffer of nodes into a paragraph, appending to a document.
fn flush_into_para(mut doc: Node, buf: Vec<Node>) -> Node {
    if !buf.is_empty() {
        let has_content = buf.iter().any(|n| {
            matches!(&n.kind, crate::dom::node::NodeKind::Text(t) if !t.trim().is_empty())
                || matches!(&n.kind, crate::dom::node::NodeKind::Element(_))
        });
        if has_content {
            let mut p = Node::element(ElementKind::Paragraph);
            for child in buf {
                p.push_child(child);
            }
            doc.push_child(p);
        }
    }
    doc
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_text_to_ast() {
        let mut builder = TreeBuilder::new();
        let tokens = vec![
            WikitextToken::Text("Hello, world!".to_string()),
            WikitextToken::EOF,
        ];
        let doc = builder.build(&tokens).unwrap();
        assert!(!doc.children.is_empty());
    }

    #[test]
    fn test_heading() {
        let mut builder = TreeBuilder::new();
        let tokens = vec![
            WikitextToken::HeadingOpen(2),
            WikitextToken::Text("Title".to_string()),
            WikitextToken::HeadingOpen(2), // Close marker
            WikitextToken::EOF,
        ];
        let doc = builder.build(&tokens).unwrap();
        assert_eq!(doc.children.len(), 1);
        assert!(matches!(
            doc.children[0].kind,
            crate::dom::node::NodeKind::Element(ElementKind::Heading(2))
        ));
    }

    #[test]
    fn test_paragraph_wrapping() {
        let mut builder = TreeBuilder::new();
        let tokens = vec![
            WikitextToken::Text("First".to_string()),
            WikitextToken::ParagraphBreak,
            WikitextToken::Text("Second".to_string()),
            WikitextToken::EOF,
        ];
        let doc = builder.build(&tokens).unwrap();
        assert_eq!(doc.children.len(), 2);
        assert!(matches!(
            doc.children[0].kind,
            crate::dom::node::NodeKind::Element(ElementKind::Paragraph)
        ));
    }

    #[test]
    fn test_wikilink() {
        let mut builder = TreeBuilder::new();
        let tokens = vec![
            WikitextToken::WikilinkOpen,
            WikitextToken::Text("Main Page".to_string()),
            WikitextToken::WikilinkClose,
            WikitextToken::EOF,
        ];
        let doc = builder.build(&tokens).unwrap();
        // Should have a Wikilink element with href attr
        assert!(!doc.children.is_empty());
    }
}
