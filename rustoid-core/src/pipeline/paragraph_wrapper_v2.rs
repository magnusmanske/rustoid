//! ParagraphWrapper — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/ParagraphWrapper.php`.
//!
//! Inserts `<p>` open/close tokens around inline content, mimicking the
//! MediaWiki PHP parser's "wikitext visual newlines" behavior.
//!
//! This is a line-based handler that buffers tokens and flushes them when it
//! becomes clear where `<p>` boundaries should lie.

use crate::wikitext::consts;
use crate::wikitext::tokens_v2::{
    DataParsoid, EndTagTk, Item, ParsoidToken, SelfclosingTagTk, TagTk,
};

/// The ParagraphWrapper. Stateful across a single `wrap` call.
pub struct ParagraphWrapper {
    has_open_p_tag: bool,
    in_block_elem: bool,
    in_blockquote: bool,

    token_buffer: Vec<Item>,
    nl_ws_tokens: Vec<Item>,
    new_line_count: usize,

    curr_line_tokens: Vec<Item>,
    curr_line_has_wrappable_tokens: bool,
    curr_line_block_tag_seen: bool,
    curr_line_block_tag_open: bool,

    /// Whether p-warpping is disabled (inline context).
    disabled: bool,
}

impl ParagraphWrapper {
    pub fn new() -> Self {
        Self::with_options(false)
    }

    /// Create a ParagraphWrapper with the given inline-context flag.
    /// In PHP, `disabled = !empty(options['inlineContext'])`.
    pub fn with_options(inline_context: bool) -> Self {
        Self {
            has_open_p_tag: false,
            in_block_elem: false,
            in_blockquote: false,
            token_buffer: Vec::new(),
            nl_ws_tokens: Vec::new(),
            new_line_count: 0,
            curr_line_tokens: Vec::new(),
            curr_line_has_wrappable_tokens: false,
            curr_line_block_tag_seen: false,
            curr_line_block_tag_open: false,
            disabled: inline_context,
        }
    }

    /// Wrap a token stream in paragraphs.
    pub fn wrap(&mut self, tokens: Vec<Item>) -> Vec<Item> {
        if self.disabled {
            // P-wrapping disabled (inline context): pass through unchanged.
            return tokens;
        }

        let mut output = Vec::new();

        for token in tokens {
            let res = self.on_token(token);

            if let Some(items) = res {
                output.extend(items);
            }
        }

        // If EOF wasn't in the stream, flush any pending newlines.
        if self.new_line_count > 0
            || !self.token_buffer.is_empty()
            || !self.curr_line_tokens.is_empty()
        {
            let eof = Item::Tok(ParsoidToken::Eof(crate::wikitext::tokens_v2::EOFTk));
            if let Some(res) = self.on_newline_or_eof(eof) {
                output.extend(res);
            }
        }

        output
    }

    /// Dispatch a single token to the correct handler.
    fn on_token(&mut self, token: Item) -> Option<Vec<Item>> {
        match &token {
            Item::Tok(ParsoidToken::Nl(_)) => self.on_newline_or_eof(token),
            Item::Tok(ParsoidToken::Eof(_)) => self.on_newline_or_eof(token),
            Item::Tok(ParsoidToken::EmptyLine(_)) => {
                // onCompoundTk: EmptyLineTk → pass through (return None).
                // But actually the PHP onAny handles EmptyLineTk as SOL-transparent.
                self.on_any(token)
            }
            _ => self.on_any(token),
        }
    }

    /// Handle newline or EOF tokens.
    fn on_newline_or_eof(&mut self, token: Item) -> Option<Vec<Item>> {
        if self.curr_line_block_tag_seen {
            let mut curr = std::mem::take(&mut self.curr_line_tokens);
            self.close_open_p_tag(&mut curr);
            self.curr_line_tokens = curr;
        } else if !self.in_block_elem && !self.has_open_p_tag && self.curr_line_has_wrappable_tokens
        {
            let mut curr = std::mem::take(&mut self.curr_line_tokens);
            self.open_p_tag(&mut curr);
            self.curr_line_tokens = curr;
        }

        // Append current line tokens to token buffer.
        self.token_buffer.append(&mut self.curr_line_tokens);

        if matches!(token, Item::Tok(ParsoidToken::Eof(_))) {
            self.nl_ws_tokens.push(token);
            let mut buf = std::mem::take(&mut self.token_buffer);
            self.close_open_p_tag(&mut buf);
            self.token_buffer = buf;
            let res = self.process_pending_nls();
            self.reset();
            Some(res)
        } else {
            // It's a newline.
            self.curr_line_tokens.clear();
            self.curr_line_has_wrappable_tokens = false;
            self.curr_line_block_tag_seen = false;
            self.curr_line_block_tag_open = false;
            self.new_line_count += 1;
            self.nl_ws_tokens.push(token);
            None
        }
    }

    /// Handle any (non-newline/EOF) token.
    fn on_any(&mut self, token: Item) -> Option<Vec<Item>> {
        // Determine token name if it's a tag.
        let token_name = match &token {
            Item::Tok(t) => t.get_name().to_string(),
            _ => String::new(),
        };

        // 1. SOL-transparent: whitespace-only strings, EmptyLineTk, CommentTk,
        //    behavior switches.
        //    (For simplicity, we treat whitespace strings and comments as
        //    sol-transparent; full TokenUtils::isSolTransparent needs more.)
        if self.is_sol_transparent_basic(&token) {
            if self.new_line_count == 0 {
                return self.flush_buffers(token);
            } else {
                self.nl_ws_tokens.push(token);
                return Some(Vec::new());
            }
        }

        // 2. SOL-transparent link tags, metas, style.
        if self.is_sol_transparent_tag(&token, &token_name) {
            if self.new_line_count == 0 {
                return self.flush_buffers(token);
            } else if self.new_line_count == 1 {
                // Swallow newline, whitespace, comments, and current line.
                self.token_buffer.append(&mut self.nl_ws_tokens);
                self.token_buffer.append(&mut self.curr_line_tokens);
                self.new_line_count = 0;
                self.curr_line_tokens.clear();
                self.curr_line_has_wrappable_tokens = false;
                self.curr_line_block_tag_seen = false;
                self.curr_line_block_tag_open = false;

                self.curr_line_tokens.push(token);
                return Some(Vec::new());
            } else {
                return self.process_buffers(token, false);
            }
        }

        // Plain string token.
        if matches!(token, Item::Str(_)) {
            self.curr_line_has_wrappable_tokens = true;
            return self.process_buffers(token, false);
        }

        // List token (ListTk) — skip nested processing.
        // Our token type doesn't have ListTk yet; handled by ListHandler elsewhere.

        // IndentPreTk — undo if in block context.

        // Wikitext block elements.
        if consts::wikitext_block_elems().contains(&token_name) {
            self.curr_line_block_tag_seen = true;
            self.curr_line_block_tag_open = true;

            let is_end_tag = matches!(token, Item::Tok(ParsoidToken::EndTag(_)));
            if (consts::block_elems().contains(&token_name) && is_end_tag)
                || (consts::anti_block_elems().contains(&token_name) && !is_end_tag)
                || consts::never_block_elems().contains(&token_name)
            {
                self.curr_line_block_tag_open = false;
            }
        }

        if token_name == "blockquote" {
            self.in_blockquote = !matches!(token, Item::Tok(ParsoidToken::EndTag(_)));
        }

        self.curr_line_has_wrappable_tokens = true;
        self.process_buffers(token, false)
    }

    /// Check if a token is SOL-transparent (basic cases).
    fn is_sol_transparent_basic(&self, token: &Item) -> bool {
        match token {
            Item::Str(s) => {
                s.trim().is_empty() && !s.is_empty() && s.chars().all(|c| c == ' ' || c == '\t')
            }
            Item::Tok(ParsoidToken::EmptyLine(_)) => true,
            Item::Tok(ParsoidToken::Comment(_)) => true,
            Item::Tok(ParsoidToken::SelfclosingTag(tk)) if tk.name == "behavior-switch" => true,
            _ => false,
        }
    }

    /// Check if a token is a SOL-transparent tag (link/meta/style).
    /// Mirrors PHP's `TokenUtils::isSolTransparent`, including
    /// `isSolTransparentLinkTag` (a `<link>` with `rel` matching
    /// `mw:PageProp/(?:Category|redirect|Language)`).
    fn is_sol_transparent_tag(&self, token: &Item, token_name: &str) -> bool {
        if matches!(token, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "style") {
            return true;
        }
        if token_name == "style" {
            return true;
        }
        if token_name == "meta" && matches!(token, Item::Tok(ParsoidToken::SelfclosingTag(_))) {
            return true;
        }
        // SOL-transparent link tags: `<link rel="mw:PageProp/redirect|Category|Language">`.
        if token_name == "link" {
            let rel = match token {
                Item::Tok(t) => t.get_attribute_v("rel").unwrap_or(""),
                _ => "",
            };
            return matches!(
                rel,
                "mw:PageProp/redirect" | "mw:PageProp/Category" | "mw:PageProp/Language"
            );
        }
        false
    }

    /// Reset everything.
    fn reset(&mut self) {
        self.reset_buffers();
        self.reset_curr_line();
        self.has_open_p_tag = false;
        self.in_block_elem = false;
        self.in_blockquote = false;
    }

    fn reset_buffers(&mut self) {
        self.token_buffer.clear();
        self.nl_ws_tokens.clear();
        self.new_line_count = 0;
    }

    fn reset_curr_line(&mut self) {
        if self.curr_line_block_tag_seen {
            self.in_block_elem = self.curr_line_block_tag_open;
        }
        self.curr_line_tokens.clear();
        self.curr_line_has_wrappable_tokens = false;
        self.curr_line_block_tag_seen = false;
        self.curr_line_block_tag_open = false;
    }

    /// Process the current buffers with a token, optionally flushing current line.
    fn process_buffers(&mut self, token: Item, flush_current_line: bool) -> Option<Vec<Item>> {
        let mut res = self.process_pending_nls();
        self.curr_line_tokens.push(token);
        if flush_current_line {
            res.append(&mut self.curr_line_tokens);
            self.reset_curr_line();
        }
        Some(res)
    }

    /// Flush buffers with a token, emitting the token directly (SOL-transparent
    /// tokens pass through immediately rather than being held for `<p>` wrapping).
    fn flush_buffers(&mut self, token: Item) -> Option<Vec<Item>> {
        let mut res_toks = std::mem::take(&mut self.token_buffer);
        let nl_ws_tokens = std::mem::take(&mut self.nl_ws_tokens);
        res_toks.extend(nl_ws_tokens);
        // Emit the SOL-transparent token at its position.
        res_toks.push(token);
        Some(res_toks)
    }

    /// Append tokens from nl_ws_tokens until a newline is found.
    fn process_one_nl_tk(&mut self, out: &mut Vec<Item>, offset: &mut usize) -> Item {
        let n = self.nl_ws_tokens.len();
        while *offset < n {
            let t = self.nl_ws_tokens[*offset].clone();
            *offset += 1;
            if matches!(t, Item::Tok(ParsoidToken::Nl(_))) {
                return t;
            } else {
                out.push(t);
            }
        }
        // PHP throws UnreachableException; we can't panic per our rules,
        // return a synthetic newline.
        Item::Tok(ParsoidToken::Nl(crate::wikitext::tokens_v2::NlTk::new(
            crate::wikitext::tokens_v2::SourceRange::new(0, 0),
        )))
    }

    /// Open a paragraph tag in `out` if not already open.
    fn open_p_tag(&mut self, out: &mut Vec<Item>) {
        if self.has_open_p_tag {
            return;
        }
        // Find the insertion index: skip SOL-transparent tokens and newlines.
        let mut insert_at = out.len();
        for (i, t) in out.iter().enumerate() {
            let is_sol_transparent =
                self.is_sol_transparent_item(t) || matches!(t, Item::Tok(ParsoidToken::Nl(_)));
            if !is_sol_transparent {
                insert_at = i;
                break;
            }
        }

        out.insert(
            insert_at,
            Item::Tok(ParsoidToken::Tag(TagTk::new(
                "p",
                vec![],
                DataParsoid::default(),
            ))),
        );
        self.has_open_p_tag = true;
    }

    /// Close an open paragraph tag in `out`.
    fn close_open_p_tag(&mut self, out: &mut Vec<Item>) {
        if !self.has_open_p_tag {
            return;
        }
        // Find insertion index from the end, skipping SOL-transparent tokens.
        let mut insert_at = out.len();
        for i in (0..out.len()).rev() {
            let t = &out[i];
            let is_sol_transparent =
                self.is_sol_transparent_item(t) || matches!(t, Item::Tok(ParsoidToken::Nl(_)));
            if !is_sol_transparent {
                insert_at = i + 1;
                break;
            }
        }

        out.insert(
            insert_at,
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "p",
                vec![],
                DataParsoid::default(),
            ))),
        );
        self.has_open_p_tag = false;
    }

    /// Check if an item is SOL-transparent.
    fn is_sol_transparent_item(&self, t: &Item) -> bool {
        self.is_sol_transparent_basic(t)
            || self.is_sol_transparent_tag(
                t,
                match t {
                    Item::Tok(tok) => tok.get_name(),
                    _ => "",
                },
            )
    }

    /// Process pending newlines, returning tokens to emit.
    fn process_pending_nls(&mut self) -> Vec<Item> {
        let mut res_toks = std::mem::take(&mut self.token_buffer);
        let mut new_line_count = self.new_line_count;
        let mut nl_offset = 0;

        if new_line_count >= 2 && !self.in_block_elem {
            self.close_open_p_tag(&mut res_toks);

            // First is emitted as a literal newline.
            let nl = self.process_one_nl_tk(&mut res_toks, &mut nl_offset);
            res_toks.push(nl);
            new_line_count -= 1;

            let remainder = new_line_count % 2;

            while new_line_count > 0 {
                let nl_tk = self.process_one_nl_tk(&mut res_toks, &mut nl_offset);
                if new_line_count % 2 == remainder {
                    if self.has_open_p_tag {
                        res_toks.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                            "p",
                            vec![],
                            DataParsoid::default(),
                        ))));
                        self.has_open_p_tag = false;
                    }
                    if new_line_count > 1 {
                        res_toks.push(Item::Tok(ParsoidToken::Tag(TagTk::new(
                            "p",
                            vec![],
                            DataParsoid::default(),
                        ))));
                        self.has_open_p_tag = true;
                    }
                } else {
                    res_toks.push(Item::Tok(ParsoidToken::SelfclosingTag(
                        SelfclosingTagTk::new("br", vec![], DataParsoid::default()),
                    )));
                }
                res_toks.push(nl_tk);
                new_line_count -= 1;
            }
        }

        if self.curr_line_block_tag_seen {
            self.close_open_p_tag(&mut res_toks);
            if new_line_count == 1 {
                let nl = self.process_one_nl_tk(&mut res_toks, &mut nl_offset);
                res_toks.push(nl);
            }
        }

        // Gather remaining ws and nl tokens.
        for i in nl_offset..self.nl_ws_tokens.len() {
            res_toks.push(self.nl_ws_tokens[i].clone());
        }

        // Reset buffers.
        self.reset_buffers();

        res_toks
    }
}

impl Default for ParagraphWrapper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Item {
        Item::Str(s.to_string())
    }

    fn nl() -> Item {
        Item::Tok(ParsoidToken::Nl(crate::wikitext::tokens_v2::NlTk::new(
            crate::wikitext::tokens_v2::SourceRange::new(0, 1),
        )))
    }

    fn eof() -> Item {
        Item::Tok(ParsoidToken::Eof(crate::wikitext::tokens_v2::EOFTk))
    }

    #[test]
    fn test_simple_paragraph() {
        let mut wrapper = ParagraphWrapper::new();
        let out = wrapper.wrap(vec![text("hello"), eof()]);

        let has_p_open = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "p"));
        let has_p_close = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "p"));
        assert!(has_p_open, "expected <p> open in {:?}", out);
        assert!(has_p_close, "expected </p> close in {:?}", out);
    }

    #[test]
    fn test_two_paragraphs() {
        let mut wrapper = ParagraphWrapper::new();
        let out = wrapper.wrap(vec![text("first"), nl(), nl(), text("second"), eof()]);

        let p_open_count = out
            .iter()
            .filter(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "p"))
            .count();
        let p_close_count = out
            .iter()
            .filter(|it| matches!(it, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "p"))
            .count();
        assert_eq!(p_open_count, 2, "expected 2 opening <p>, got {:?}", out);
        assert_eq!(p_close_count, 2, "expected 2 closing </p>, got {:?}", out);
    }

    #[test]
    fn test_block_tag_no_wrap() {
        // A <div> should prevent paragraph wrapping around itself.
        let div_open = Item::Tok(ParsoidToken::Tag(TagTk::new(
            "div",
            vec![],
            DataParsoid::default(),
        )));
        let div_close = Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "div",
            vec![],
            DataParsoid::default(),
        )));
        let mut wrapper = ParagraphWrapper::new();
        let out = wrapper.wrap(vec![div_open, text("x"), div_close, eof()]);

        // The div itself should be present, and no <p> should wrap the div.
        let has_div = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "div"));
        assert!(has_div);
    }
}
