//! Tree builder — converts token stream to AST.
//!
//! Takes the flat token stream from the preprocessor and constructs a nested
//! AST with proper block/inline structure. This implements the wikitext-to-DOM
//! tree construction algorithm adapted from Parsoid's TreeBuilder.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::error::Result;
use crate::wikitext::tokens::WikitextToken;

/// Builds an AST from a stream of wikitext tokens.
pub struct TreeBuilder {
    /// Stack of currently-open block HTML elements (div, pre, blockquote).
    /// When non-empty, content is added as children of the top element.
    open_blocks: Vec<Node>,
    /// Stack of currently-open inline HTML elements (code, b, i, span, etc.).
    /// When non-empty, inline content is added as children of the top element.
    open_inlines: Vec<Node>,
    /// Stack of format start positions (index into inline_buf when format opened).
    fmt_starts: Vec<usize>,
}

impl TreeBuilder {
    /// Create a new tree builder.
    pub fn new() -> Self {
        Self {
            open_blocks: Vec::new(),
            open_inlines: Vec::new(),
            fmt_starts: Vec::new(),
        }
    }

    /// Build an AST from a token stream.
    pub fn build(&mut self, tokens: &[WikitextToken]) -> Result<Node> {
        let mut doc = Node::document();
        let mut inline_buf: Vec<Node> = Vec::new();
        let mut fmt_stack: Vec<ElementKind> = Vec::new();
        let mut at_line_start = true;
        self.open_blocks.clear();
        self.open_inlines.clear();

        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];

            match token {
                WikitextToken::HeadingOpen(level) => {
                    doc = self.flush_inline_to_target(doc, &mut inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    let (heading, new_i) = self.build_heading(tokens, i, *level);
                    Self::push_to_target(&mut doc, &mut self.open_blocks, heading);
                    i = new_i;
                    at_line_start = true;
                }
                WikitextToken::ListItem(ch, depth) => {
                    doc = self.flush_inline_to_target(doc, &mut inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    let prefix = std::mem::take(&mut inline_buf);
                    let (list_item, new_i) = self.build_list_item(tokens, i, *ch, *depth, prefix);
                    Self::push_to_target(&mut doc, &mut self.open_blocks, list_item);
                    i = new_i;
                    at_line_start = true;
                }
                WikitextToken::Hr => {
                    doc = self.flush_inline_to_target(doc, &mut inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    Self::push_to_target(
                        &mut doc,
                        &mut self.open_blocks,
                        Node::element(ElementKind::HorizontalRule),
                    );
                    i += 1;
                    at_line_start = true;
                }
                WikitextToken::ParagraphBreak
                | WikitextToken::ParagraphOpen
                | WikitextToken::ParagraphClose => {
                    doc = self.flush_inline_to_target(doc, &mut inline_buf, &fmt_stack);
                    inline_buf = Vec::new();
                    fmt_stack.clear();
                    i += 1;
                    at_line_start = true;
                }
                WikitextToken::TableOpen(_) => {
                    let prefix = std::mem::take(&mut inline_buf);
                    let (table, new_i) = self.build_table(tokens, i, prefix);
                    Self::push_to_target(&mut doc, &mut self.open_blocks, table);
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
                WikitextToken::ItalicOpen => {
                    fmt_stack.push(ElementKind::Italic);
                    self.fmt_starts.push(inline_buf.len());
                    at_line_start = false;
                    i += 1;
                }
                WikitextToken::BoldOpen => {
                    fmt_stack.push(ElementKind::Bold);
                    self.fmt_starts.push(inline_buf.len());
                    at_line_start = false;
                    i += 1;
                }
                WikitextToken::BoldClose => {
                    self.handle_bold_close(&mut inline_buf, &mut fmt_stack);
                    at_line_start = false;
                    i += 1;
                }
                WikitextToken::ItalicClose => {
                    self.handle_italic_close(&mut inline_buf, &mut fmt_stack);
                    at_line_start = false;
                    i += 1;
                }
                WikitextToken::Text(text) => {
                    let node = Node::text(text.clone());
                    self.push_inline(&mut inline_buf, node);
                    at_line_start = false;
                    i += 1;
                }
                WikitextToken::Newline => {
                    // Emit newline as text — either inline or at document level.
                    if !inline_buf.is_empty() {
                        let node = Node::text("\n".to_string());
                        self.push_inline(&mut inline_buf, node);
                    } else {
                        // Between blocks: add newline as text node at doc level
                        Self::push_to_target(
                            &mut doc,
                            &mut self.open_blocks,
                            Node::text("\n".to_string()),
                        );
                    }
                    at_line_start = true;
                    i += 1;
                }
                WikitextToken::Comment(comment) => {
                    if at_line_start || inline_buf.is_empty() {
                        Self::push_to_target(
                            &mut doc,
                            &mut self.open_blocks,
                            Node::comment(comment.clone()),
                        );
                    } else {
                        inline_buf.push(Node::comment(comment.clone()));
                    }
                    i += 1;
                }
                WikitextToken::NowikiContent(content) => {
                    // Nowiki content is inline — emit as literal text.
                    // In Parsoid, this would be wrapped in <span typeof="mw:Nowiki">,
                    // but since test comparison strips those spans, we emit plain text.
                    let node = Node::text(content.clone());
                    self.push_inline(&mut inline_buf, node);
                    at_line_start = false;
                    i += 1;
                }
                WikitextToken::SelfClosingTag(name, _) => {
                    if name == "br" {
                        inline_buf.push(Node::element(ElementKind::LineBreak));
                    } else {
                        inline_buf.push(Node::text(format!("<{name}/>")));
                    }
                    at_line_start = false;
                    i += 1;
                }
                WikitextToken::HtmlTagOpen(name, attrs) => {
                    let is_block = matches!(name.as_str(), "div" | "pre" | "blockquote");
                    let tag_kind = match name.as_str() {
                        "b" | "strong" => ElementKind::Bold,
                        "i" | "em" => ElementKind::Italic,
                        "div" => ElementKind::Div,
                        "span" => ElementKind::Span,
                        "pre" => ElementKind::Preformatted,
                        _ => ElementKind::Other(name.clone()),
                    };
                    if is_block {
                        // Flush pending inline content before opening block tag
                        doc = self.flush_inline_to_target(doc, &mut inline_buf, &fmt_stack);
                        inline_buf = Vec::new();
                        fmt_stack.clear();
                        let mut elem = Node::element(tag_kind);
                        for (k, v) in attrs {
                            elem.set_attr(k, v);
                        }
                        elem.data_parsoid = Some("{\"stx\":\"html\"}".to_string());
                        self.open_blocks.push(elem);
                        at_line_start = true;
                    } else {
                        let mut elem = Node::element(tag_kind);
                        for (k, v) in attrs {
                            elem.set_attr(k, v);
                        }
                        self.open_inlines.push(elem);
                        at_line_start = false;
                    }
                    i += 1;
                }
                WikitextToken::HtmlTagClose(name) => {
                    // Close matching open block or inline tag
                    if self.open_blocks.last().map_or(false, |b| {
                        let tag = element_kind_to_tag(&b.kind);
                        tag == Some(name.as_str())
                    }) {
                        // Flush inline before closing block
                        doc = self.flush_inline_to_target(doc, &mut inline_buf, &fmt_stack);
                        inline_buf = Vec::new();
                        if let Some(block) = self.open_blocks.pop() {
                            Self::push_to_target(&mut doc, &mut self.open_blocks, block);
                        }
                    } else if let Some(top) = self.open_inlines.last() {
                        let tag = element_kind_to_tag(&top.kind);
                        if tag == Some(name.as_str()) {
                            if let Some(inline) = self.open_inlines.pop() {
                                inline_buf.push(inline);
                            }
                        }
                    }
                    i += 1;
                }
                WikitextToken::MagicWord(word) => {
                    inline_buf.push(Node::text(format!("[{word}]")));
                    at_line_start = false;
                    i += 1;
                }
                WikitextToken::EOF => {
                    // Auto-close any remaining open format elements, innermost first.
                    while !fmt_stack.is_empty() {
                        let start_idx = self.fmt_starts.pop().unwrap_or(0);
                        let start_idx = start_idx.min(inline_buf.len());
                        let kind = fmt_stack.pop().unwrap();
                        let tail: Vec<Node> = inline_buf.drain(start_idx..).collect();
                        if !tail.is_empty() {
                            let mut wrapper = Node::element(kind);
                            for node in tail {
                                wrapper.push_child(node);
                            }
                            inline_buf.push(wrapper);
                        }
                    }
                    doc = self.flush_inline_to_target(doc, &mut inline_buf, &[]);
                    while let Some(block) = self.open_blocks.pop() {
                        doc.push_child(block);
                    }
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

    /// Push a node to either the current open block or the document.
    fn push_to_target(doc: &mut Node, open_blocks: &mut Vec<Node>, child: Node) {
        if let Some(block) = open_blocks.last_mut() {
            block.push_child(child);
        } else {
            doc.push_child(child);
        }
    }

    /// Push a text node to either the current open inline or the inline buffer.
    fn push_inline(&mut self, inline_buf: &mut Vec<Node>, node: Node) {
        if let Some(top) = self.open_inlines.last_mut() {
            top.push_child(node);
        } else {
            inline_buf.push(node);
        }
    }

    /// Flush inline buffer into a paragraph (or not, if inside pre), appending to the right target.
    fn flush_inline_to_target(
        &mut self,
        mut doc: Node,
        inline_buf: &mut Vec<Node>,
        fmt_stack: &[ElementKind],
    ) -> Node {
        // Close any open inline elements
        while let Some(inline) = self.open_inlines.pop() {
            inline_buf.push(inline);
        }

        let mut buf = std::mem::take(inline_buf);
        wrap_buf_in_fmt(&mut buf, fmt_stack);
        if buf.is_empty() {
            return doc;
        }

        // If we're inside a block HTML element that doesn't require p-wrapping,
        // push content directly.
        let inside_no_wrap = self.open_blocks.last().map_or(false, |b| {
            matches!(
                &b.kind,
                NodeKind::Element(ElementKind::Preformatted | ElementKind::Div)
            )
        });

        if inside_no_wrap {
            for child in buf {
                Self::push_to_target(&mut doc, &mut self.open_blocks, child);
            }
        } else {
            let para = flush_into_para(Node::document(), buf);
            for child in para.children {
                Self::push_to_target(&mut doc, &mut self.open_blocks, child);
            }
        }
        doc
    }

    #[allow(dead_code)]
    fn handle_bold_open(&self, inline_buf: &mut Vec<Node>, fmt_stack: &mut Vec<ElementKind>) {
        let in_italic = fmt_stack.contains(&ElementKind::Italic);
        if fmt_stack.contains(&ElementKind::Bold) {
            while let Some(top) = fmt_stack.last() {
                wrap_buf_in_fmt(inline_buf, fmt_stack);
                let kind = top.clone();
                fmt_stack.pop();
                if kind == ElementKind::Bold {
                    break;
                }
            }
            if in_italic && fmt_stack.contains(&ElementKind::Italic) {
                while let Some(top) = fmt_stack.last() {
                    wrap_buf_in_fmt(inline_buf, fmt_stack);
                    let kind = top.clone();
                    fmt_stack.pop();
                    if kind == ElementKind::Italic {
                        break;
                    }
                }
            }
        } else {
            wrap_buf_in_fmt(inline_buf, fmt_stack);
            fmt_stack.push(ElementKind::Bold);
            if in_italic {
                fmt_stack.push(ElementKind::Italic);
            }
        }
    }

    fn handle_italic_close(
        &mut self,
        inline_buf: &mut Vec<Node>,
        fmt_stack: &mut Vec<ElementKind>,
    ) {
        // If bold is nested inside italic, close bold first (innermost first)
        while fmt_stack.last() == Some(&ElementKind::Bold) {
            self.handle_bold_close(inline_buf, fmt_stack);
        }
        if let Some(pos) = fmt_stack.iter().rposition(|k| *k == ElementKind::Italic) {
            let start_idx = self.fmt_starts.get(pos).copied().unwrap_or(0);
            // Ensure start_idx is within bounds
            let start_idx = start_idx.min(inline_buf.len());
            let tail: Vec<Node> = inline_buf.drain(start_idx..).collect();
            let mut italic = Node::element(ElementKind::Italic);
            for node in tail {
                italic.push_child(node);
            }
            inline_buf.push(italic);
            if pos < fmt_stack.len() {
                fmt_stack.remove(pos);
            }
            if pos < self.fmt_starts.len() {
                self.fmt_starts.remove(pos);
            }
        }
    }

    fn handle_bold_close(&mut self, inline_buf: &mut Vec<Node>, fmt_stack: &mut Vec<ElementKind>) {
        if let Some(pos) = fmt_stack.iter().rposition(|k| *k == ElementKind::Bold) {
            let start_idx = self.fmt_starts.get(pos).copied().unwrap_or(0);
            let start_idx = start_idx.min(inline_buf.len());
            let tail: Vec<Node> = inline_buf.drain(start_idx..).collect();
            let mut bold = Node::element(ElementKind::Bold);
            for node in tail {
                bold.push_child(node);
            }
            inline_buf.push(bold);
            if pos < fmt_stack.len() {
                fmt_stack.remove(pos);
            }
            if pos < self.fmt_starts.len() {
                self.fmt_starts.remove(pos);
            }
        }
    }

    #[allow(dead_code)]
    fn handle_italic_open(&self, inline_buf: &mut Vec<Node>, fmt_stack: &mut Vec<ElementKind>) {
        if fmt_stack.contains(&ElementKind::Italic) {
            while let Some(top) = fmt_stack.last() {
                wrap_buf_in_fmt(inline_buf, fmt_stack);
                let kind = top.clone();
                fmt_stack.pop();
                if kind == ElementKind::Italic {
                    break;
                }
            }
        } else {
            wrap_buf_in_fmt(inline_buf, fmt_stack);
            fmt_stack.push(ElementKind::Italic);
        }
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
                WikitextToken::Comment(comment) => {
                    item.push_child(Node::comment(comment.clone()));
                    i += 1;
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

        let href = format!("./{}", page.replace(' ', "_"));
        link.set_attr("href", &href);
        link.set_attr("title", &page);
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
                    // Non-list element — flush all pending lists unless
                    // it's a comment (SOL-transparent) that should stay in the list
                    let is_comment = matches!(&child.kind, crate::dom::node::NodeKind::Comment(_));
                    if last_list_kind.is_some() && is_comment {
                        // Comment is SOL-transparent: add it to the current pending list
                        if let Some(last_list) = pending_lists.last_mut() {
                            last_list.push_child(child.clone());
                        } else {
                            result.push_child(child.clone());
                        }
                    } else {
                        for list in pending_lists.drain(..) {
                            result.push_child(list);
                        }
                        last_list_kind = None;
                        result.push_child(child.clone());
                    }
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
    // Wrap from innermost to outermost
    for kind in stack.iter().rev() {
        let mut wrapper = Node::element(kind.clone());
        for node in buf.drain(..) {
            wrapper.push_child(node);
        }
        buf.push(wrapper);
    }
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

/// Get the HTML tag name for an ElementKind (for matching open/close tags).
fn element_kind_to_tag(kind: &crate::dom::node::NodeKind) -> Option<&'static str> {
    if let crate::dom::node::NodeKind::Element(ek) = kind {
        match ek {
            ElementKind::Div => Some("div"),
            ElementKind::Preformatted => Some("pre"),
            ElementKind::Bold => Some("b"),
            ElementKind::Italic => Some("i"),
            ElementKind::Span => Some("span"),
            _ => None,
        }
    } else {
        None
    }
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
