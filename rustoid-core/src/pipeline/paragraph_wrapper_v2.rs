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
    ///
    /// Mirrors PHP `LineBasedHandler::process`'s dispatch order:
    /// NlTk → onNewline, EOFTk → onEnd, CompoundTk → onCompoundTk (whose
    /// `null` result falls through to onAny), otherwise → onAny.
    fn on_token(&mut self, token: Item) -> Option<Vec<Item>> {
        match &token {
            Item::Tok(ParsoidToken::Nl(_)) => self.on_newline_or_eof(token),
            Item::Tok(ParsoidToken::Eof(_)) => self.on_newline_or_eof(token),
            Item::Tok(ParsoidToken::List(_))
            | Item::Tok(ParsoidToken::IndentPre(_))
            | Item::Tok(ParsoidToken::EmptyLine(_)) => {
                let res = self.on_compound_tk(token.clone());
                if res.is_none() {
                    // onCompoundTk returned null → dispatch to onAny.
                    self.on_any(token)
                } else {
                    res
                }
            }
            _ => self.on_any(token),
        }
    }

    /// Handle compound tokens (ListTk / IndentPreTk / EmptyLineTk).
    ///
    /// Faithful port of PHP `ParagraphWrapper::onCompoundTk`:
    /// - DL-DD lists are flattened and their nested tokens re-wrapped;
    /// - IndentPreTk / EmptyLineTk / non-DL-DD ListTk return `None` so the
    ///   caller dispatches the token to `onAny` (PHP returns `null` in each of
    ///   these cases).
    fn on_compound_tk(&mut self, token: Item) -> Option<Vec<Item>> {
        if let Item::Tok(ParsoidToken::List(t)) = &token {
            if t.is_dl_dd_list() {
                let nested = t.get_nested_tokens().to_vec();
                return Some(self.wrap(nested));
            }
            return None;
        }
        // IndentPreTk / EmptyLineTk: fall through to onAny.
        None
    }

    /// Undo an indent-pre when it appears in a block element or blockquote.
    ///
    /// Faithful port of PHP `ParagraphWrapper::undoIndentPre`: re-emit the
    /// nested tokens (skipping `<pre>`, `</pre>`, and converting the
    /// `mw:IndentPreWS` meta to a space) through the normal line handlers.
    fn undo_indent_pre(
        &mut self,
        ipre: &crate::wikitext::tokens_v2::IndentPreTk,
    ) -> Option<Vec<Item>> {
        let mut ret = if self.new_line_count == 0 {
            // `flushBuffers('')` — flush the pending buffer, holding nothing.
            self.flush_buffers(Item::Str(String::new()))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let nested = ipre.get_nested_tokens().to_vec();
        let n = nested.len();
        let mut i = 1; // skip the <pre>
        while i < n {
            let token = nested[i].clone();
            let is_ws = matches!(&token, Item::Tok(ParsoidToken::SelfclosingTag(tk))
                if tk.attribs.iter().any(|kv| kv.key.as_str() == Some("typeof") && kv.value.as_str() == Some("mw:IndentPreWS")));
            let is_nl = matches!(&token, Item::Tok(ParsoidToken::Nl(_)));
            let is_pre_end =
                matches!(&token, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "pre");

            if is_ws {
                self.nl_ws_tokens.push(Item::Str(" ".to_string()));
            } else if is_pre_end {
                // Skip `</pre>`.
            } else if is_nl {
                if let Some(res) = self.on_newline_or_eof(token) {
                    ret.extend(res);
                }
            } else {
                if let Some(res) = self.on_any(token) {
                    ret.extend(res);
                }
            }
            i += 1;
        }

        Some(ret)
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
            // It's a newline. Faithful to PHP `onNewlineOrEOF`: reset the
            // current line (this also updates `in_block_elem` from
            // `curr_line_block_tag_open`, so content after a block-level open
            // tag on a previous line is treated as inside a block and not
            // p-wrapped), then buffer the newline.
            self.reset_curr_line();
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

        // List token (ListTk): skip nested tokens, treat as a block tag.
        if matches!(token, Item::Tok(ParsoidToken::List(_))) {
            self.curr_line_block_tag_seen = true;
            return self.process_buffers(token, true);
        }

        // IndentPreTk: skip nested tokens, unless nested in a block or
        // blockquote, in which case the pre is undone.
        if let Item::Tok(ParsoidToken::IndentPre(t)) = &token {
            let ipre = t.clone();
            if self.in_block_elem || self.in_blockquote {
                return self.undo_indent_pre(&ipre);
            }
            self.curr_line_block_tag_seen = true;
            return self.process_buffers(token, true);
        }

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

    /// Flush buffers with a token, holding the token in `curr_line_tokens`
    /// rather than emitting it. Mirrors PHP `flushBuffers` exactly.
    fn flush_buffers(&mut self, token: Item) -> Option<Vec<Item>> {
        // Assert: PHP requires newLineCount === 0 here (callers guarantee it).
        self.curr_line_tokens.push(token);
        let mut res_toks = std::mem::take(&mut self.token_buffer);
        let nl_ws_tokens = std::mem::take(&mut self.nl_ws_tokens);
        self.reset_buffers();
        res_toks.extend(nl_ws_tokens);
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
    ///
    /// Faithful port of PHP `ParagraphWrapper::openPTag`, including the
    /// transclusion/annotation range bookkeeping that keeps `mw:Transclusion`
    /// markers and other SOL-transparent tokens *outside* the `<p>`.
    fn open_p_tag(&mut self, out: &mut Vec<Item>) {
        if self.has_open_p_tag {
            return;
        }
        let mut tpl_start_index: Option<usize> = None;
        // `i` is the eventual splice offset; default to the very end so that,
        // when `out` is entirely SOL-transparent, `<p>` opens at the end.
        let mut insert_at = out.len();
        for (i, t) in out.iter().enumerate() {
            if let Item::Tok(ParsoidToken::SelfclosingTag(meta)) = t
                && meta.name == "meta"
            {
                let meta_type = transclusion_meta_type(t);
                if meta_type.as_deref() == Some("mw:Transclusion") {
                    // Start tag; remember it and keep scanning forward.
                    tpl_start_index = Some(i);
                    continue;
                } else if meta_type
                    .as_deref()
                    .is_some_and(|ty| ty.starts_with("mw:Transclusion/"))
                {
                    // End tag; clear any pending start index.
                    tpl_start_index = None;
                    continue;
                } else if is_annotation_start_item(t) {
                    break;
                }
            }
            // Not a transclusion meta; stop at the first non-SOL-transparent,
            // non-newline token.
            if !self.is_sol_transparent_item(t) && !matches!(t, Item::Tok(ParsoidToken::Nl(_))) {
                insert_at = i;
                break;
            }
        }
        if let Some(start) = tpl_start_index {
            insert_at = start;
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
    ///
    /// Faithful port of PHP `ParagraphWrapper::closeOpenPTag`, including the
    /// transclusion/annotation range bookkeeping.
    fn close_open_p_tag(&mut self, out: &mut Vec<Item>) {
        if !self.has_open_p_tag {
            return;
        }
        let mut tpl_end_index: Option<usize> = None;
        let mut insert_at = out.len();
        for i in (0..out.len()).rev() {
            let t = &out[i];
            if let Item::Tok(ParsoidToken::SelfclosingTag(meta)) = t
                && meta.name == "meta"
            {
                let meta_type = transclusion_meta_type(t);
                if meta_type.as_deref() == Some("mw:Transclusion") {
                    // Start tag; do not include it or anything after.
                    tpl_end_index = None;
                    continue;
                } else if meta_type
                    .as_deref()
                    .is_some_and(|ty| ty.starts_with("mw:Transclusion/"))
                {
                    // End tag; leave it (and anything after) out.
                    tpl_end_index = Some(i);
                    continue;
                } else if is_annotation_end_item(t) {
                    break;
                }
            }
            if !self.is_sol_transparent_item(t) && !matches!(t, Item::Tok(ParsoidToken::Nl(_))) {
                insert_at = i + 1;
                break;
            }
        }
        if let Some(end) = tpl_end_index {
            insert_at = end;
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

/// Return the `typeof` value of a `<meta>`, or `None` if the item is not a
/// `meta` self-closing tag that carries a transclusion-related `typeof`.
/// Mirrors `TokenUtils::matchTypeOf`'s access to the `typeof` attribute
/// (without the regex: callers do prefix checks here).
fn transclusion_meta_type(item: &Item) -> Option<String> {
    match item {
        Item::Tok(ParsoidToken::SelfclosingTag(meta)) if meta.name == "meta" => meta
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("typeof"))
            .and_then(|kv| kv.value.as_str())
            .and_then(|v| {
                v.split_whitespace()
                    .find(|ty| *ty == "mw:Transclusion" || ty.starts_with("mw:Transclusion/"))
                    .map(|ty| ty.to_string())
            }),
        _ => None,
    }
}

/// Does this item look like an annotation start meta token?
/// Mirrors `TokenUtils::isAnnotationStartToken` (matched against the
/// `mw:Annotation/<type>` regexp, excluding `/End`).
fn is_annotation_start_item(item: &Item) -> bool {
    match item {
        Item::Tok(ParsoidToken::SelfclosingTag(meta)) if meta.name == "meta" => meta
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("typeof"))
            .and_then(|kv| kv.value.as_str())
            .is_some_and(|v| {
                v.split_whitespace()
                    .any(|ty| ty.starts_with("mw:Annotation/") && !ty.ends_with("/End"))
            }),
        _ => false,
    }
}

/// Does this item look like an annotation end meta token?
/// Mirrors `TokenUtils::isAnnotationEndToken`.
fn is_annotation_end_item(item: &Item) -> bool {
    match item {
        Item::Tok(ParsoidToken::SelfclosingTag(meta)) if meta.name == "meta" => meta
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("typeof"))
            .and_then(|kv| kv.value.as_str())
            .is_some_and(|v| {
                v.split_whitespace()
                    .any(|ty| ty.starts_with("mw:Annotation/") && ty.ends_with("/End"))
            }),
        _ => false,
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

    #[test]
    fn test_sol_transparent_link_followed_by_newline() {
        // "#REDIRECT [[Main Page]]\nA newline" — the SOL-transparent redirect
        // <link> must be flushed with exactly one literal newline before the
        // following paragraph (no spurious blank line).
        let link = Item::Tok(ParsoidToken::SelfclosingTag(SelfclosingTagTk::new(
            "link",
            vec![],
            DataParsoid::default(),
        )));
        // The ParagraphWrapper only inspects the tag name, not `rel`; mutate via
        // a helper is unnecessary — a bare <link> is already SOL-transparent.
        let mut wrapper = ParagraphWrapper::new();
        let out = wrapper.wrap(vec![link, nl(), text("A newline"), eof()]);

        // Expected token sequence:
        //   <link> nl <p> "A newline" </p> eof
        let names: Vec<String> = out
            .iter()
            .map(|it| match it {
                Item::Tok(t) => format!("{t:?}"),
                Item::Str(s) => format!("Str({s:?})"),
            })
            .collect();

        let newline_count = out
            .iter()
            .filter(|it| matches!(it, Item::Tok(ParsoidToken::Nl(_))))
            .count();
        assert_eq!(
            newline_count, 1,
            "expected a single newline, got {:?}",
            names
        );

        let p_open = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "p"));
        assert!(p_open, "expected <p> open, got {:?}", names);
    }

    #[test]
    fn test_list_compound_passes_through() {
        // A non-DL-DD ListTk must pass through unchanged (not wrapped in <p>).
        let list = Item::Tok(ParsoidToken::List(crate::wikitext::tokens_v2::ListTk::new()));
        let mut wrapper = ParagraphWrapper::new();
        let out = wrapper.wrap(vec![list, eof()]);

        // Output must contain the List token and no <p>.
        let list_count = out
            .iter()
            .filter(|it| matches!(it, Item::Tok(ParsoidToken::List(_))))
            .count();
        assert_eq!(list_count, 1, "expected the List token to pass through");
        let p_count = out
            .iter()
            .filter(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "p"))
            .count();
        assert_eq!(p_count, 0, "expected no <p> wrapping around a list");
    }
}
