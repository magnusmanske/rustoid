//! PreHandler — faithful port of PHP Parsoid's `src/Wt2Html/TT/PreHandler.php`.
//!
//! Inserts `<pre>` blocks for indented content (whitespace at start of line),
//! using the documented 6-state finite state machine.
//!
//! States: SOL, PRE, PRE_COLLECT, SOL_AFTER_PRE, MULTILINE_PRE, IGNORE.

use crate::wikitext::consts;
use crate::wikitext::tokens_v2::{
    DataParsoid, EndTagTk, IndentPreTk, Item, KV, KeyValue, ParsoidToken, SelfclosingTagTk,
    SourceRange, TagTk,
};

/// FSM states (mirroring PHP's integer constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Sol,
    Pre,
    PreCollect,
    SolAfterPre,
    MultilinePre,
    Ignore,
}

/// The PreHandler. Stateful across a single `run` call.
pub struct PreHandler {
    state: State,
    pre_tsr: Option<SourceRange>,
    tokens: Vec<Item>,
    curr_line_pre_toks: Vec<Item>,
    ws_tk_index: isize,
    on_any_enabled: bool,
    disabled: bool,
}

impl PreHandler {
    pub fn new() -> Self {
        Self::with_options(false)
    }

    /// Create a PreHandler with the given inline-context flag.
    /// In PHP, `disabled` is set when `inlineContext` is present.
    pub fn with_options(inline_context: bool) -> Self {
        let mut h = Self {
            state: State::Sol,
            pre_tsr: None,
            tokens: Vec::new(),
            curr_line_pre_toks: Vec::new(),
            ws_tk_index: -1,
            on_any_enabled: true,
            disabled: inline_context,
        };
        if inline_context {
            h.disabled = true;
        } else {
            h.disabled = false;
            h.reset();
        }
        h
    }

    /// Reset the FSM state.
    fn reset(&mut self) {
        self.state = State::Sol;
        self.pre_tsr = Some(SourceRange::new(0, 0));
        self.tokens.clear();
        self.curr_line_pre_toks.clear();
        self.ws_tk_index = -1;
        self.on_any_enabled = true;
    }

    /// Run the PreHandler over a token stream.
    pub fn run(&mut self, tokens: Vec<Item>) -> Vec<Item> {
        if self.disabled {
            return tokens;
        }

        let mut saw_eof = false;
        let mut output = Vec::new();
        for token in tokens {
            if matches!(token, Item::Tok(ParsoidToken::Eof(_))) {
                saw_eof = true;
            }
            let res = self.on_token(token);
            if let Some(mut items) = res {
                output.append(&mut items);
            }
        }

        // If no EOF was seen in the input, flush like EOF.
        if !saw_eof {
            let eof = Item::Tok(ParsoidToken::Eof(crate::wikitext::tokens_v2::EOFTk));
            if let Some(items) = self.on_eof(eof) {
                output.extend(items);
            }
        }

        output
    }

    /// Dispatch a token to the right handler.
    fn on_token(&mut self, token: Item) -> Option<Vec<Item>> {
        match &token {
            Item::Tok(ParsoidToken::Nl(_)) => self.on_newline(token),
            Item::Tok(ParsoidToken::Eof(_)) => self.on_eof(token),
            _ if self.on_any_enabled => self.on_any(token),
            _ => {
                // onAny disabled (IGNORE state): pass through.
                Some(vec![token])
            }
        }
    }

    /// Create the indent-pre whitespace meta token.
    fn new_indent_pre_ws() -> ParsoidToken {
        let mut tk = SelfclosingTagTk::new("meta", vec![], DataParsoid::default());
        tk.attribs.push(KV {
            key: KeyValue::Str("typeof".to_string()),
            value: KeyValue::Str("mw:IndentPreWS".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
        ParsoidToken::SelfclosingTag(tk)
    }

    /// Does this token represent an indent-pre whitespace meta-token?
    fn is_indent_pre_ws(item: &Item) -> bool {
        match item {
            Item::Tok(ParsoidToken::SelfclosingTag(tk)) => tk.attribs.iter().any(|kv| {
                kv.key.as_str() == Some("typeof") && kv.value.as_str() == Some("mw:IndentPreWS")
            }),
            _ => false,
        }
    }

    /// Switch the FSM to IGNORE state.
    fn move_to_ignore_state(&mut self) {
        self.on_any_enabled = false;
        self.state = State::Ignore;
    }

    /// Wrap buffered tokens in `<pre>..</pre>`.
    fn gen_pre(&mut self) -> Vec<Item> {
        let mut ret = Vec::new();

        let n = self.tokens.len();
        if n > 0 {
            // Find the index of the last token to wrap (skip sol-transparent).
            let mut i = n - 1;
            while i > 0 {
                let is_sol_transparent = Self::is_sol_transparent_item(&self.tokens[i]);
                let is_nl = matches!(self.tokens[i], Item::Tok(ParsoidToken::Nl(_)));
                let is_transclusion_end = match &self.tokens[i] {
                    Item::Tok(ParsoidToken::SelfclosingTag(tk)) => tk.attribs.iter().any(|kv| {
                        kv.key.as_str() == Some("typeof")
                            && kv
                                .value
                                .as_str()
                                .map(|v| v.starts_with("mw:Transclusion/End"))
                                .unwrap_or(false)
                    }),
                    _ => false,
                };

                if !is_nl && !is_sol_transparent {
                    break;
                }
                if is_transclusion_end {
                    break;
                }
                i -= 1;
            }

            // Build the IndentPre compound token.
            let mut indent_pre_tk = IndentPreTk::new();

            // `<pre>` open tag, carrying preTSR.
            let da = self.pre_tsr.clone().map(DataParsoid::with_tsr_range);
            indent_pre_tk.add_token(Item::Tok(ParsoidToken::Tag(TagTk::new(
                "pre",
                vec![],
                da.unwrap_or_default(),
            ))));

            for j in 0..(i + 1) {
                let t = self.tokens[j].clone();
                // Convert a tokenized listItem back to text.
                if let Item::Tok(ParsoidToken::Tag(tk)) = &t
                    && tk.name == "listItem"
                {
                    // Extract the bullets text and convert to string.
                    if let Some(bullets) = tk
                        .attribs
                        .iter()
                        .find(|kv| kv.key.as_str() == Some("bullets"))
                        .and_then(|kv| kv.value.as_str())
                    {
                        indent_pre_tk.add_token(Item::Str(bullets.to_string()));
                        continue;
                    }
                }
                indent_pre_tk.add_token(t);
            }

            indent_pre_tk.add_token(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "pre",
                vec![],
                DataParsoid::default(),
            ))));

            ret.push(Item::Tok(ParsoidToken::IndentPre(indent_pre_tk)));

            // Remaining tokens after the wrapped prefix.
            for j in (i + 1)..n {
                let mut t = self.tokens[j].clone();
                if Self::is_indent_pre_ws(&t) {
                    t = Item::Str(" ".to_string());
                }
                ret.push(t);
            }

            self.tokens.clear();
        }

        ret
    }

    /// Process current line pre-tokens into the main token buffer.
    fn process_curr_line(&mut self, token: Option<Item>, meta_to_ws: bool) {
        if !self.curr_line_pre_toks.is_empty() {
            if meta_to_ws && self.ws_tk_index != -1 {
                let idx = self.ws_tk_index as usize;
                if idx < self.curr_line_pre_toks.len() {
                    self.curr_line_pre_toks[idx] = Item::Str(" ".to_string());
                }
            }
            self.tokens.append(&mut self.curr_line_pre_toks);
            self.curr_line_pre_toks.clear();
            self.ws_tk_index = -1;
        }
        if let Some(token) = token {
            self.tokens.push(token);
        }
    }

    /// Purge buffers and return tokens.
    fn purge_buffers(&mut self, token: Item) -> Vec<Item> {
        self.process_curr_line(Some(token), true);
        std::mem::take(&mut self.tokens)
    }

    /// Discard pre on this line; generate pre for previous lines.
    fn discard_curr_line_pre(&mut self, token: Item) -> Vec<Item> {
        let mut ret = self.gen_pre();
        ret.extend(self.purge_buffers(token));
        ret
    }

    /// Initialize a pre TSR from a newline token.
    fn init_pre_tsr(nltk: &ParsoidToken) -> Option<SourceRange> {
        if let ParsoidToken::Nl(t) = nltk
            && let Some(tsr) = &t.data_parsoid.tsr
        {
            return Some(SourceRange::new(tsr.end, tsr.end));
        }
        None
    }

    /// Handle a newline token.
    fn on_newline(&mut self, token: Item) -> Option<Vec<Item>> {
        let ret = match self.state {
            State::Sol | State::Pre => {
                let ret = self.purge_buffers(token.clone());
                if let Item::Tok(tok) = &token {
                    self.pre_tsr = Self::init_pre_tsr(tok);
                }
                self.state = State::Sol;
                ret
            }
            State::MultilinePre | State::PreCollect => {
                self.process_curr_line(Some(token), false);
                self.state = State::SolAfterPre;
                Vec::new()
            }
            State::SolAfterPre => {
                let ret = self.discard_curr_line_pre(token.clone());
                self.state = State::Sol;
                if let Item::Tok(tok) = &token {
                    self.pre_tsr = Self::init_pre_tsr(tok);
                }
                ret
            }
            State::Ignore => {
                // Returning null would invoke onAny; we skip by returning [token].
                let ret = vec![token.clone()];
                self.reset();
                if let Item::Tok(tok) = &token {
                    self.pre_tsr = Self::init_pre_tsr(tok);
                }
                ret
            }
        };

        Some(ret)
    }

    /// Handle an EOF token.
    fn on_eof(&mut self, token: Item) -> Option<Vec<Item>> {
        let ret = match self.state {
            State::Sol | State::Pre => self.purge_buffers(token),
            State::SolAfterPre | State::MultilinePre => self.discard_curr_line_pre(token.clone()),
            State::PreCollect => {
                self.process_curr_line(None, false);
                let mut ret = self.gen_pre();
                ret.push(token);
                ret
            }
            State::Ignore => vec![token],
        };
        Some(ret)
    }

    /// Handle a non-newline/EOF token.
    fn on_any(&mut self, token: Item) -> Option<Vec<Item>> {
        if self.state == State::Ignore {
            // Cannot get here (onAny is disabled in IGNORE).
            return Some(vec![token]);
        }

        let mut ret: Vec<Item> = Vec::new();

        match self.state {
            State::Sol => {
                if let Item::Str(s) = &token
                    && let Some(first) = s.chars().next()
                    && first == ' '
                {
                    // Move to PRE, set wsTkIndex.
                    ret = std::mem::take(&mut self.tokens);
                    self.ws_tk_index = 0;
                    self.curr_line_pre_toks = vec![Item::Tok(Self::new_indent_pre_ws())];
                    self.state = State::Pre;

                    if s.len() > 1 {
                        // Treat everything after first space as new token.
                        let rest = s[1..].to_string();
                        let is_sol_transparent = Self::is_sol_transparent_str(&rest);
                        self.curr_line_pre_toks.push(Item::Str(rest));
                        if !is_sol_transparent {
                            self.state = State::PreCollect;
                        }
                    }
                    return Some(ret);
                }

                if Self::is_sol_transparent_item(&token) {
                    // Continue watching; update preTSR (approximation).
                    self.tokens.push(token);
                } else {
                    ret = self.purge_buffers(token);
                    self.move_to_ignore_state();
                }
            }
            State::Pre | State::PreCollect | State::MultilinePre => {
                let is_wikitext_block_tag = match &token {
                    Item::Tok(ParsoidToken::Tag(t)) => {
                        consts::wikitext_block_elems().contains(&t.name)
                    }
                    Item::Tok(ParsoidToken::EndTag(t)) => {
                        consts::wikitext_block_elems().contains(&t.name)
                    }
                    _ => false,
                };

                if is_wikitext_block_tag {
                    ret = if self.state == State::Pre {
                        self.purge_buffers(token)
                    } else {
                        self.discard_curr_line_pre(token)
                    };
                    self.move_to_ignore_state();
                } else {
                    let is_sol_transparent = Self::is_sol_transparent_item(&token);
                    self.curr_line_pre_toks.push(token);
                    if !is_sol_transparent {
                        self.state = State::PreCollect;
                    }
                }
            }
            State::SolAfterPre => {
                if let Item::Str(s) = &token
                    && let Some(first) = s.chars().next()
                    && first == ' '
                {
                    self.ws_tk_index = self.curr_line_pre_toks.len() as isize;
                    self.curr_line_pre_toks
                        .push(Item::Tok(Self::new_indent_pre_ws()));
                    self.state = State::MultilinePre;

                    if s.len() > 1 {
                        let rest = s[1..].to_string();
                        let is_sol_transparent = Self::is_sol_transparent_str(&rest);
                        self.curr_line_pre_toks.push(Item::Str(rest));
                        if !is_sol_transparent {
                            self.state = State::PreCollect;
                        }
                    }
                    return Some(ret);
                }

                if Self::is_sol_transparent_item(&token) {
                    self.curr_line_pre_toks.push(token);
                } else {
                    ret = self.discard_curr_line_pre(token);
                    self.move_to_ignore_state();
                }
            }
            State::Ignore => {
                // Unreachable.
            }
        }

        Some(ret)
    }

    /// Whether a string is sol-transparent (whitespace-only).
    fn is_sol_transparent_str(s: &str) -> bool {
        !s.is_empty() && s.chars().all(|c| c == ' ' || c == '\t')
    }

    /// Whether an item is sol-transparent.
    ///
    /// Faithful port of `TokenUtils::isSolTransparent`, including the
    /// `meta` self-closing-tag case: a `<meta>` is SOL-transparent unless it
    /// carries a literal-HTML marker (`stx === 'html'`). Template/param/
    /// behavior-switch metas are therefore SOL-transparent.
    fn is_sol_transparent_item(item: &Item) -> bool {
        match item {
            Item::Str(s) => Self::is_sol_transparent_str(s),
            Item::Tok(ParsoidToken::Comment(_)) => true,
            Item::Tok(ParsoidToken::EmptyLine(_)) => true,
            Item::Tok(ParsoidToken::SelfclosingTag(tk)) if tk.name == "behavior-switch" => true,
            Item::Tok(ParsoidToken::SelfclosingTag(tk))
                if tk.name == "meta" && tk.data_parsoid.stx.as_deref() != Some("html") =>
            {
                true
            }
            _ => false,
        }
    }
}

impl Default for PreHandler {
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
            SourceRange::new(0, 1),
        )))
    }

    fn eof() -> Item {
        Item::Tok(ParsoidToken::Eof(crate::wikitext::tokens_v2::EOFTk))
    }

    #[test]
    fn test_indented_line_becomes_pre() {
        // " code" → IndentPre(<pre> ... </pre>)
        let mut handler = PreHandler::new();
        let out = handler.run(vec![text(" code"), nl(), eof()]);

        // The <pre> tag is nested inside an IndentPre compound token.
        let has_indent_pre = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::IndentPre(_))));
        assert!(has_indent_pre, "expected IndentPre token in {:?}", out);

        let has_pre_open = out.iter().any(|it| {
            if let Item::Tok(ParsoidToken::IndentPre(ip)) = it {
                ip.nested_tokens
                    .iter()
                    .any(|n| matches!(n, Item::Tok(ParsoidToken::Tag(t)) if t.name == "pre"))
            } else {
                false
            }
        });
        let has_pre_close = out.iter().any(|it| {
            if let Item::Tok(ParsoidToken::IndentPre(ip)) = it {
                ip.nested_tokens
                    .iter()
                    .any(|n| matches!(n, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "pre"))
            } else {
                false
            }
        });
        assert!(has_pre_open, "expected <pre> open in {:?}", out);
        assert!(has_pre_close, "expected </pre> close in {:?}", out);
    }

    #[test]
    fn test_plain_line_not_pre() {
        let mut handler = PreHandler::new();
        let out = handler.run(vec![text("hello"), nl(), eof()]);

        let has_pre = out.iter().any(|it| {
            matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "pre")
                || matches!(it, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "pre")
        });
        assert!(!has_pre, "did not expect <pre> in {:?}", out);
    }

    #[test]
    fn test_indented_sol_transparent_metas_not_pre() {
        // A leading-space line whose remaining content is entirely
        // SOL-transparent (transclusion metas + a comment + spaces) must NOT
        // become an indent `<pre>`: `TokenUtils::isSolTransparent` treats a
        // non-literal-HTML `<meta>` as SOL-transparent (mirrors the
        // "empty-transclusion on its own line" comment tests).
        let mut handler = PreHandler::new();

        let mut meta_start = SelfclosingTagTk::new("meta", vec![], DataParsoid::default());
        meta_start.attribs.push(crate::wikitext::tokens_v2::KV {
            key: crate::wikitext::tokens_v2::KeyValue::Str("typeof".to_string()),
            value: crate::wikitext::tokens_v2::KeyValue::Str("mw:Transclusion".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });

        let out = handler.run(vec![
            text(" "),
            Item::Tok(ParsoidToken::SelfclosingTag(meta_start)),
            text(" "),
            Item::Tok(ParsoidToken::Comment(
                crate::wikitext::tokens_v2::CommentTk::new("".to_string(), DataParsoid::default()),
            )),
            text(" "),
            nl(),
            eof(),
        ]);

        let has_pre = out.iter().any(|it| {
            matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "pre")
                || matches!(it, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "pre")
                || matches!(it, Item::Tok(ParsoidToken::IndentPre(_)))
        });
        assert!(!has_pre, "did not expect <pre> in {:?}", out);
    }
}
