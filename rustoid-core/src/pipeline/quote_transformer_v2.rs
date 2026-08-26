//! QuoteTransformer — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/QuoteTransformer.php`.
//!
//! Converts `mw-quote` self-closing tokens (emitted by the PEG tokenizer for
//! `''`, `'''`, and `'''''` sequences) into `<b>`/`<i>` open/close tokens,
//! using the exact state machine and balancing heuristics as the PHP
//! implementation.
//!
//! The transformer is line-based: quote analysis is deferred until a newline
//! (or EOF) token is seen, because MediaWiki's apostrophe rules are not
//! context-free and must balance across an entire line.

use crate::wikitext::tokens_v2::{
    DataParsoid, EndTagTk, Item, ParsoidToken, SelfclosingTagTk, SourceRange, TagTk,
};

/// The QuoteTransformer. Stateless across calls; per-run state lives in the
/// `transform` method's local buffers, mirroring the PHP instance state.
pub struct QuoteTransformer;

/// Per-run state, mirroring the PHP instance fields.
struct State {
    /// Chunks alternate between quote tokens and non-quote token sequences.
    /// The first chunk is a non-quote chunk (always present).
    chunks: Vec<Vec<Item>>,
    /// Last italic / last bold open tag seen, keyed by tag name. Stores the
    /// location `(chunk_index, item_index)` of the open tag token within
    /// `chunks`, so post-hoc `autoInsertedEndToken` marks mutate the *actual*
    /// emitted token (mirroring PHP's reference semantics where
    /// `$this->last['b']` is the same object as `$this->chunks[$i]`).
    last: std::collections::HashMap<String, (usize, usize)>,
    /// Whether onAny is currently enabled (accumulating into current chunk).
    on_any_enabled: bool,
}

impl State {
    fn new() -> Self {
        Self {
            chunks: vec![Vec::new()],
            last: std::collections::HashMap::new(),
            on_any_enabled: false,
        }
    }

    /// Push an item onto the current chunk.
    fn push_current(&mut self, item: Item) {
        let last_idx = self.chunks.len() - 1;
        self.chunks[last_idx].push(item);
    }

    /// Replace the chunk at index `i` with the given chunk.
    fn set_chunk(&mut self, i: usize, chunk: Vec<Item>) {
        self.chunks[i] = chunk;
    }

    /// Get a clone of the chunk at index `i`.
    fn chunk_clone(&self, i: usize) -> Vec<Item> {
        self.chunks[i].clone()
    }

    /// Get a clone of the first item in chunk `i`, if any.
    fn first_item(&self, i: usize) -> Option<Item> {
        self.chunks.get(i).and_then(|c| c.first()).cloned()
    }

    /// Set `auto_inserted_end_token` on the open tag at the recorded location
    /// for `name`, if any.
    fn mark_auto_inserted(&mut self, name: &str) {
        if let Some(&(chunk_idx, item_idx)) = self.last.get(name)
            && let Some(item) = self
                .chunks
                .get_mut(chunk_idx)
                .and_then(|c| c.get_mut(item_idx))
            && let Item::Tok(ParsoidToken::Tag(tk)) = item
        {
            tk.data_parsoid.auto_inserted_end_token = true;
        }
    }

    fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

impl QuoteTransformer {
    /// Transform a token stream, converting `mw-quote` tokens to
    /// `<b>`/`<i>` tags. Returns the transformed token stream.
    pub fn transform(tokens: Vec<Item>) -> Vec<Item> {
        let mut state = State::new();
        let mut output: Vec<Item> = Vec::new();

        for token in tokens {
            match &token {
                Item::Tok(ParsoidToken::SelfclosingTag(tk)) if tk.name == "mw-quote" => {
                    // onTag: mw-quote → onQuote
                    state.on_any_enabled = true;
                    let qlen = Self::quote_len(tk);
                    if qlen == 2 || qlen == 3 || qlen == 5 {
                        Self::start_new_chunk(&mut state);
                        state.push_current(token.clone());
                        Self::start_new_chunk(&mut state);
                    }
                    // Other quote lengths are dropped, matching PHP's onQuote.
                }
                Item::Tok(ParsoidToken::Tag(tk))
                    if (tk.name == "td" || tk.name == "th") && Self::is_wikitext_tag(tk) =>
                {
                    // onTag: wikitext td/th → processQuotes($token). The token is
                    // appended to the current chunk by processQuotes, so it is
                    // already part of the returned (non-empty) output.
                    let flushed = Self::process_quotes(&mut state, Some(token.clone()));
                    if flushed.is_empty() {
                        output.push(token);
                    } else {
                        output.extend(flushed);
                    }
                }
                Item::Tok(ParsoidToken::Nl(_)) => {
                    // onNewline: processQuotes($token).
                    let flushed = Self::process_quotes(&mut state, Some(token.clone()));
                    if flushed.is_empty() {
                        output.push(token);
                    } else {
                        output.extend(flushed);
                    }
                }
                Item::Tok(ParsoidToken::Eof(_)) => {
                    // onEnd: processQuotes($token).
                    let flushed = Self::process_quotes(&mut state, Some(token.clone()));
                    if flushed.is_empty() {
                        output.push(token);
                    } else {
                        output.extend(flushed);
                    }
                }
                Item::Tok(ParsoidToken::EmptyLine(_)) => {
                    // onCompoundTk(EmptyLineTk): processQuotes($token).
                    let flushed = Self::process_quotes(&mut state, Some(token.clone()));
                    if flushed.is_empty() {
                        output.push(token);
                    } else {
                        output.extend(flushed);
                    }
                }
                _ => {
                    // onAny: accumulate into current chunk if enabled.
                    if state.on_any_enabled {
                        state.push_current(token.clone());
                    } else {
                        output.push(token);
                    }
                }
            }
        }

        output
    }

    /// Start a new chunk (the current chunk is the last element of `chunks`;
    /// pushing an empty chunk makes it the new accumulation buffer).
    fn start_new_chunk(state: &mut State) {
        state.chunks.push(Vec::new());
    }

    /// Process quotes on the current line. Implements PHP's `processQuotes`.
    ///
    /// `token` is the triggering token (newline, EOF, td/th, or an empty-line
    /// compound token). When present, it is appended to the current chunk after
    /// quote conversion — exactly as PHP does — so it is ordered after any
    /// auto-inserted closing tags.
    ///
    /// Returns the flattened output and resets the buffers.
    fn process_quotes(state: &mut State, token: Option<Item>) -> Vec<Item> {
        if !state.on_any_enabled {
            // Quick abort.
            return Vec::new();
        }

        // Count number of bold and italics.
        let mut num_bold = 0usize;
        let mut num_italics = 0usize;
        let chunk_count = state.chunk_count();
        let mut i = 1;
        while i < chunk_count {
            if let Some(Item::Tok(ParsoidToken::SelfclosingTag(tk))) = state.first_item(i) {
                let qlen = Self::quote_len(&tk);
                if qlen == 2 || qlen == 5 {
                    num_italics += 1;
                }
                if qlen == 3 || qlen == 5 {
                    num_bold += 1;
                }
            }
            i += 2;
        }

        // Balance out tokens, convert placeholders into tags.
        if num_italics % 2 == 1 && num_bold % 2 == 1 {
            let mut first_single_letter_word: isize = -1;
            let mut first_multi_letter_word: isize = -1;
            let mut first_space: isize = -1;
            let chunk_count2 = state.chunk_count();
            let mut i2 = 1;
            while i2 < chunk_count2 {
                let is_bold = state
                    .first_item(i2)
                    .map(|item| {
                        if let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = item {
                            Self::quote_len(&tk) == 3
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);

                if is_bold {
                    let (is_space_1, is_space_2) = state
                        .first_item(i2)
                        .map(|item| {
                            if let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = item {
                                let s1 = Self::bool_attr(&tk, "isSpace_1");
                                let s2 = Self::bool_attr(&tk, "isSpace_2");
                                (s1, s2)
                            } else {
                                (false, false)
                            }
                        })
                        .unwrap_or((false, false));

                    if is_space_1 && first_space == -1 {
                        first_space = i2 as isize;
                    } else if !is_space_1 {
                        if is_space_2 && first_single_letter_word == -1 {
                            first_single_letter_word = i2 as isize;
                            break;
                        } else if first_multi_letter_word == -1 {
                            first_multi_letter_word = i2 as isize;
                        }
                    }
                }

                i2 += 2;
            }

            // Convert a bold to italic + apostrophe, if possible.
            if first_single_letter_word > -1 {
                Self::convert_bold(state, first_single_letter_word as usize);
            } else if first_multi_letter_word > -1 {
                Self::convert_bold(state, first_multi_letter_word as usize);
            } else if first_space > -1 {
                Self::convert_bold(state, first_space as usize);
            }
        }

        // Convert the quote tokens into tags.
        Self::convert_quotes_to_tags(state);

        // Return all collected tokens including the trigger token (if any).
        if let Some(token) = token {
            state.push_current(token);
        }

        // Flatten chunks into output.
        let mut result = Vec::new();
        for idx in 0..state.chunk_count() {
            result.extend(state.chunk_clone(idx));
        }

        // Reset for next line.
        state.chunks.clear();
        state.chunks.push(Vec::new());
        state.last.clear();
        state.on_any_enabled = false;

        result
    }

    /// Get the `value` attribute length of an mw-quote token.
    fn quote_len(tk: &SelfclosingTagTk) -> usize {
        tk.attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("value"))
            .and_then(|kv| kv.value.as_str())
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Get a boolean attribute value.
    fn bool_attr(tk: &SelfclosingTagTk, name: &str) -> bool {
        tk.attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some(name))
            .and_then(|kv| kv.value.as_str())
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
    }

    /// Convert a bold token to italic to balance an uneven number of both bold
    /// and italic tags. One quote is converted back to text.
    fn convert_bold(state: &mut State, i: usize) {
        // Append a plain-text apostrophe to the previous (non-quote) chunk.
        if i >= 1
            && let Some(prev) = state.chunks.get_mut(i - 1)
        {
            prev.push(Item::Str("'".to_string()));
        }

        // Extract the old bold token's tsr.
        let old_bold = state.first_item(i);
        let old_tsr = if let Some(Item::Tok(ParsoidToken::SelfclosingTag(tk))) = &old_bold {
            tk.data_parsoid.tsr.clone()
        } else {
            None
        };

        // Build a new mw-quote (italic) token with shifted tsr.
        let new_tsr = old_tsr.map(|tsr| SourceRange::new(tsr.start + 1, tsr.end));
        let new_dp = DataParsoid {
            tsr: new_tsr,
            ..Default::default()
        };
        let mut italic = SelfclosingTagTk::new("mw-quote", vec![], new_dp);
        italic.add_attribute_str("value", "''");

        state.set_chunk(i, vec![Item::Tok(ParsoidToken::SelfclosingTag(italic))]);
    }

    /// Convert quote tokens to tags, using the same state machine as the PHP
    /// parser.
    fn convert_quotes_to_tags(state: &mut State) {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum S {
            Empty,
            B,
            I,
            Bi,
            Ib,
            Both,
        }

        let mut last_both: isize = -1;
        let mut s = S::Empty;

        let chunk_count = state.chunk_count();
        let mut i = 1;
        while i < chunk_count {
            let qlen = state
                .first_item(i)
                .map(|item| {
                    if let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = item {
                        Self::quote_len(&tk)
                    } else {
                        0
                    }
                })
                .unwrap_or(0);

            if qlen == 2 {
                match s {
                    S::I => {
                        Self::quote_to_tag(state, i, vec![Self::end("i")], false);
                        s = S::Empty;
                    }
                    S::Bi => {
                        Self::quote_to_tag(state, i, vec![Self::end("i")], false);
                        s = S::B;
                    }
                    S::Ib => {
                        Self::quote_to_tag(
                            state,
                            i,
                            vec![Self::end("b"), Self::end("i"), Self::start("b")],
                            true,
                        );
                        s = S::B;
                    }
                    S::Both => {
                        Self::quote_to_tag(
                            state,
                            last_both as usize,
                            vec![Self::start("b"), Self::start("i")],
                            false,
                        );
                        Self::quote_to_tag(state, i, vec![Self::end("i")], false);
                        s = S::B;
                    }
                    S::B => {
                        Self::quote_to_tag(state, i, vec![Self::start("i")], false);
                        s = S::Bi;
                    }
                    S::Empty => {
                        Self::quote_to_tag(state, i, vec![Self::start("i")], false);
                        s = S::I;
                    }
                }
            } else if qlen == 3 {
                match s {
                    S::B => {
                        Self::quote_to_tag(state, i, vec![Self::end("b")], false);
                        s = S::Empty;
                    }
                    S::Ib => {
                        Self::quote_to_tag(state, i, vec![Self::end("b")], false);
                        s = S::I;
                    }
                    S::Bi => {
                        Self::quote_to_tag(
                            state,
                            i,
                            vec![Self::end("i"), Self::end("b"), Self::start("i")],
                            true,
                        );
                        s = S::I;
                    }
                    S::Both => {
                        Self::quote_to_tag(
                            state,
                            last_both as usize,
                            vec![Self::start("i"), Self::start("b")],
                            false,
                        );
                        Self::quote_to_tag(state, i, vec![Self::end("b")], false);
                        s = S::I;
                    }
                    S::I => {
                        Self::quote_to_tag(state, i, vec![Self::start("b")], false);
                        s = S::Ib;
                    }
                    S::Empty => {
                        Self::quote_to_tag(state, i, vec![Self::start("b")], false);
                        s = S::B;
                    }
                }
            } else if qlen == 5 {
                match s {
                    S::B => {
                        Self::quote_to_tag(state, i, vec![Self::end("b"), Self::start("i")], false);
                        s = S::I;
                    }
                    S::I => {
                        Self::quote_to_tag(state, i, vec![Self::end("i"), Self::start("b")], false);
                        s = S::B;
                    }
                    S::Bi => {
                        Self::quote_to_tag(state, i, vec![Self::end("i"), Self::end("b")], false);
                        s = S::Empty;
                    }
                    S::Ib => {
                        Self::quote_to_tag(state, i, vec![Self::end("b"), Self::end("i")], false);
                        s = S::Empty;
                    }
                    S::Both => {
                        Self::quote_to_tag(
                            state,
                            last_both as usize,
                            vec![Self::start("i"), Self::start("b")],
                            false,
                        );
                        Self::quote_to_tag(state, i, vec![Self::end("b"), Self::end("i")], false);
                        s = S::Empty;
                    }
                    S::Empty => {
                        last_both = i as isize;
                        s = S::Both;
                    }
                }
            }

            i += 2;
        }

        // Now close all remaining tags. Order is important.
        if s == S::Both {
            Self::quote_to_tag(
                state,
                last_both as usize,
                vec![Self::start("b"), Self::start("i")],
                false,
            );
            s = S::Bi;
        }
        if s == S::B || s == S::Ib {
            let tag = Self::end("b");
            state.mark_auto_inserted("b");
            state.push_current(Item::Tok(tag));
        }
        if s == S::I || s == S::Bi || s == S::Ib {
            let tag = Self::end("i");
            state.mark_auto_inserted("i");
            state.push_current(Item::Tok(tag));
        }
        if s == S::Bi {
            let tag = Self::end("b");
            state.mark_auto_inserted("b");
            state.push_current(Item::Tok(tag));
        }
    }

    /// Convert a single quote token to tags. Updates `last` and the chunk.
    fn quote_to_tag(
        state: &mut State,
        chunk: usize,
        tags: Vec<ParsoidToken>,
        ignore_bogus_two: bool,
    ) {
        let old_tag = state.first_item(chunk);
        let Some(old_tag) = old_tag else {
            return;
        };

        let (tsr, start_pos, end_pos) =
            if let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = &old_tag {
                let tsr = tk.data_parsoid.tsr.clone();
                let start = tsr.as_ref().map(|t| t.start);
                let end = tsr.as_ref().map(|t| t.end);
                (tsr, start, end)
            } else {
                (None, None, None)
            };

        let mut result: Vec<Item> = Vec::with_capacity(tags.len());
        let mut cur_start = start_pos;

        for (i, mut tag) in tags.into_iter().enumerate() {
            if tsr.is_some() {
                if i == 0 && ignore_bogus_two {
                    // Mark the *currently open* tag of this name as having an
                    // auto-inserted end token (mirrors `$this->last[...]->autoInsertedEndToken`).
                    let name = tag.get_name().to_string();
                    state.mark_auto_inserted(&name);
                } else if i == 2 && ignore_bogus_two {
                    if let Some(dp) = tag.data_parsoid_mut() {
                        dp.auto_inserted_start_token = true;
                    }
                } else {
                    let name = tag.get_name().to_string();
                    if name == "b" {
                        if let (Some(st), Some(_en)) = (cur_start, end_pos)
                            && let Some(dp) = tag.data_parsoid_mut()
                        {
                            dp.tsr = Some(SourceRange::new(st, st + 3));
                            cur_start = dp.tsr.as_ref().map(|t| t.end);
                        }
                    } else if name == "i"
                        && let (Some(st), Some(_en)) = (cur_start, end_pos)
                        && let Some(dp) = tag.data_parsoid_mut()
                    {
                        dp.tsr = Some(SourceRange::new(st, st + 2));
                        cur_start = dp.tsr.as_ref().map(|t| t.end);
                    }
                }
            }

            // Update `last` map to record the location of the last open tag of
            // each name (mirrors `$this->last[$name] = $tag`).
            let name = tag.get_name().to_string();
            if matches!(tag, ParsoidToken::EndTag(_)) {
                state.last.remove(&name);
            } else {
                state.last.insert(name, (chunk, result.len()));
            }

            result.push(Item::Tok(tag));
        }

        state.set_chunk(chunk, result);
    }

    fn start(name: &str) -> ParsoidToken {
        ParsoidToken::Tag(TagTk::new(name.to_string(), vec![], DataParsoid::default()))
    }

    fn end(name: &str) -> ParsoidToken {
        ParsoidToken::EndTag(EndTagTk::new(
            name.to_string(),
            vec![],
            DataParsoid::default(),
        ))
    }

    /// Whether a `td`/`th` opening tag came from wikitext table syntax (`|`,
    /// `!`, `||`) rather than a literal HTML `<td>`/`<th>`. Mirrors PHP's
    /// `TokenizerUtils::isHTMLTag`, which returns true only when the token's
    /// `dataParsoid->stx` is `"html"`.
    fn is_wikitext_tag(tk: &TagTk) -> bool {
        tk.data_parsoid.stx.as_deref() != Some("html")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(value: &str) -> ParsoidToken {
        let dp = DataParsoid {
            tsr: Some(SourceRange::new(0, value.len())),
            ..DataParsoid::default()
        };
        let mut tk = SelfclosingTagTk::new("mw-quote", vec![], dp);
        tk.add_attribute_str("value", value);
        ParsoidToken::SelfclosingTag(tk)
    }

    fn nl() -> ParsoidToken {
        ParsoidToken::Nl(crate::wikitext::tokens_v2::NlTk::new(SourceRange::new(
            0, 1,
        )))
    }

    fn text(s: &str) -> Item {
        Item::Str(s.to_string())
    }

    fn tok(t: ParsoidToken) -> Item {
        Item::Tok(t)
    }

    #[test]
    fn test_simple_bold() {
        let input = vec![
            tok(quote("'''")),
            text("bold"),
            tok(quote("'''")),
            tok(nl()),
        ];
        let out = QuoteTransformer::transform(input);

        let has_bold_open = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "b"));
        let has_bold_close = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "b"));
        assert!(has_bold_open, "expected <b> open in {:?}", out);
        assert!(has_bold_close, "expected </b> close in {:?}", out);
    }

    #[test]
    fn test_simple_italic() {
        let input = vec![
            tok(quote("''")),
            text("italic"),
            tok(quote("''")),
            tok(nl()),
        ];
        let out = QuoteTransformer::transform(input);

        let has_italic_open = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "i"));
        let has_italic_close = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "i"));
        assert!(has_italic_open, "expected <i> open in {:?}", out);
        assert!(has_italic_close, "expected </i> close in {:?}", out);
    }

    #[test]
    fn test_bold_italic_5quotes() {
        let input = vec![
            tok(quote("'''''")),
            text("both"),
            tok(quote("'''''")),
            tok(nl()),
        ];
        let out = QuoteTransformer::transform(input);

        let has_bold_open = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "b"));
        let has_italic_open = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "i"));
        assert!(has_bold_open, "expected <b> open in {:?}", out);
        assert!(has_italic_open, "expected <i> open in {:?}", out);
    }

    #[test]
    fn test_no_quotes_passthrough() {
        let out = QuoteTransformer::transform(vec![text("hello"), tok(nl())]);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Item::Str(s) if s == "hello"));
        assert!(matches!(&out[1], Item::Tok(ParsoidToken::Nl(_))));
    }

    #[test]
    fn test_unbalanced_bold_only() {
        // Single '''''' should not crash and produce some output.
        let input = vec![tok(quote("'''")), tok(nl())];
        let out = QuoteTransformer::transform(input);
        // Odd number of bolds → auto-inserted end tag eventually, but only
        // after the close is attempted. For a single bold, we get <b> and
        // (on close) an auto-inserted </b>.
        assert!(!out.is_empty());
    }

    #[test]
    fn test_auto_inserted_end_tag_precedes_newline() {
        // For an unbalanced `''foo`, the auto-inserted `</i>` must be emitted
        // before the newline token (PHP's `processQuotes` appends the trigger
        // token *after* `convertQuotesToTags`).
        let input = vec![tok(quote("''")), text("foo"), tok(nl())];
        let out = QuoteTransformer::transform(input);

        assert_eq!(out.len(), 4, "expected <i>, foo, </i>, nl in {out:?}");
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "i"));
        assert!(matches!(&out[1], Item::Str(s) if s == "foo"));
        assert!(matches!(&out[2], Item::Tok(ParsoidToken::EndTag(t)) if t.name == "i"));
        assert!(matches!(&out[3], Item::Tok(ParsoidToken::Nl(_))));
    }
}
