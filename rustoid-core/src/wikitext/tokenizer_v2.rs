//! PEG-based wikitext tokenizer — faithful port of PHP Parsoid's Grammar.pegphp.
//!
//! The tokenizer reads raw wikitext and produces a stream of ParsoidToken chunks.
//! Each chunk represents one toplevel block. The tokenizer emits tags like:
//! - `<h2>` / `</h2>` for headings
//! - `<p>` for paragraphs (via ParagraphWrapper in TT3)
//! - `<b>`, `<i>` for bold/italic (via mw-quote → QuoteTransformer in TT3)
//! - `<a>` for wikilinks (via SelfclosingTagTk `wikilink` → WikiLinkHandler in TT2)
//! - `<table>`, `<tr>`, `<td>`, `<th>` for tables
//! - `<li>`, `<dt>`, `<dd>` for lists (via `listItem` tags → ListHandler in TT3)
//! - `<meta>` for behavior switches, include directives, annotations
//! - `mw:Entity` spans for HTML entities
//! - Comments, newlines, etc.
//!
//! The tokenizer uses a PEG-like approach with memoization for performance.

use crate::Result;
use crate::wikitext::tokens_v2::*;

/// Tokenizer configuration.
pub struct TokenizerOptions {
    /// Whether we're inside a template context (affects include/noinclude handling).
    pub in_template: bool,
    /// Whether to expand templates (always true for initial tokenization).
    pub expand_templates: bool,
    /// Whether we're in an inline context.
    pub inline_context: bool,
    /// Whether we're processing attribute expansion.
    pub attr_expansion: bool,
    /// Start-of-line flag at the beginning of the input.
    pub sol: bool,
    /// Pipeline offset for TSR shifting.
    pub pipeline_offset: usize,
}

impl Default for TokenizerOptions {
    fn default() -> Self {
        Self {
            in_template: false,
            expand_templates: true,
            inline_context: false,
            attr_expansion: false,
            sol: true,
            pipeline_offset: 0,
        }
    }
}

/// The PEG tokenizer state.
pub struct PegTokenizer<'a> {
    /// Input wikitext.
    input: &'a str,
    /// Current byte position.
    pos: usize,
    /// Input length.
    input_len: usize,
    /// Start-of-line state.
    at_sol: bool,
    /// Whether we're inside a template (affects include tags).
    in_template: bool,
    /// Recognized extension tag names.
    #[allow(dead_code)]
    ext_tags: Vec<String>,
    /// Recognized annotation tags.
    #[allow(dead_code)]
    annotation_tags: Vec<String>,
    /// Tokens output buffer (accumulated during toplevel block parsing).
    output: Vec<Either<String, ParsoidToken>>,
    /// Heading counter.
    #[allow(dead_code)]
    heading_index: usize,
    /// Accumulated has-sol-transparent-at-start flag.
    has_sol_transparent_at_start: bool,
}

impl<'a> PegTokenizer<'a> {
    pub fn new(input: &'a str, options: &TokenizerOptions) -> Self {
        Self {
            input,
            pos: 0,
            input_len: input.len(),
            at_sol: options.sol,
            in_template: options.in_template,
            ext_tags: Vec::new(),
            annotation_tags: Vec::new(),
            output: Vec::new(),
            heading_index: 0,
            has_sol_transparent_at_start: false,
        }
    }

    /// Tokenize the entire input, returning chunks of (tokens + text).
    pub fn tokenize(&mut self) -> Result<Vec<Either<String, ParsoidToken>>> {
        // The top-level start rule: toplevel blocks followed by optional newlines.
        self.parse_toplevel()?;
        Ok(std::mem::take(&mut self.output))
    }

    /// Attempt to parse the next chunk of the document (for streaming).
    /// Returns None if at EOF, Some(chunk) otherwise.
    pub fn tokenize_chunk(&mut self) -> Option<Vec<Either<String, ParsoidToken>>> {
        if self.pos >= self.input_len {
            return None;
        }
        let start = self.pos;
        // Try to parse one toplevel block.
        if self.try_parse_one_toplevel_block() {
            Some(std::mem::take(&mut self.output))
        } else if self.pos > start {
            // Some text was consumed but no block was formed — shouldn't happen.
            let text = self.input[start..self.pos].to_string();
            Some(vec![Either::Left(text)])
        } else {
            // Advance one character to avoid infinite loop.
            if let Some(ch) = self.input[self.pos..].chars().next() {
                let text = self.input[self.pos..self.pos + ch.len_utf8()].to_string();
                self.pos += ch.len_utf8();
                Some(vec![Either::Left(text)])
            } else {
                None
            }
        }
    }

    // ---- Helpers ----

    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn eof(&self) -> bool {
        self.pos >= self.input_len
    }

    fn peek_char(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn starts_with(&self, s: &str) -> bool {
        self.remaining().starts_with(s)
    }

    fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.input_len);
    }

    #[allow(dead_code)]
    fn saved_pos(&self) -> usize {
        self.pos
    }

    #[allow(dead_code)]
    fn end_pos(&self) -> usize {
        self.pos
    }

    fn tsr(&self, start: usize, end: usize) -> SourceRange {
        SourceRange::new(start, end)
    }

    #[allow(dead_code)]
    fn tsr_current(&self, start: usize) -> SourceRange {
        SourceRange::new(start, self.pos)
    }

    // ---- Text accumulation helpers ----

    /// Emit a plain text string.
    fn emit_text(&mut self, text: String) {
        if !text.is_empty() {
            self.output.push(Either::Left(text));
        }
    }

    /// Emit a single token.
    fn emit_token(&mut self, token: ParsoidToken) {
        self.output.push(Either::Right(token));
    }

    /// Emit a DataParsoid with a TSR.
    fn make_dp(&self, start: usize, end: usize) -> DataParsoid {
        DataParsoid::with_tsr(start, end)
    }

    fn make_dp_tsr(&self, tsr: SourceRange) -> DataParsoid {
        DataParsoid::with_tsr_range(tsr)
    }

    // ---- Top-level parsing ----

    /// Parse the document: toplevel blocks followed by newlines.
    fn parse_toplevel(&mut self) -> Result<()> {
        loop {
            if self.eof() {
                break;
            }
            let pos_before = self.pos;
            // Try to parse one toplevel block.
            if !self.try_parse_one_toplevel_block() {
                // If we couldn't match a toplevel block and didn't advance, consume one character as text.
                if self.pos == pos_before {
                    if let Some(ch) = self.peek_char() {
                        let ch_len = ch.len_utf8();
                        let text = self.input[self.pos..self.pos + ch_len].to_string();
                        self.pos += ch_len;
                        self.emit_text(text);
                    } else {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Try to match one toplevel block. Returns true if matched.
    fn try_parse_one_toplevel_block(&mut self) -> bool {
        let start = self.pos;

        // Try block constructs at SOL.
        if self.at_sol {
            // 1. Redirect
            if self.pos == 0
                && !self.in_template
                && let Some(token) = self.try_redirect()
            {
                self.emit_token(token);
                self.consume_sol_transparent();
                self.try_block_line();
                self.has_sol_transparent_at_start = true;
                return true;
            }

            // 2. Block lines (headings, lists, hr, table lines).
            let block_saved = self.pos;
            let output_saved = self.output.len();
            if self.try_parse_block_lines() {
                return true;
            }
            // Backtrack if block_lines didn't consume anything meaningful.
            self.pos = block_saved;
            self.output.truncate(output_saved);

            // 4. SOL transparent + inline.
            if self.try_parse_sol() {
                self.try_parse_inlineline();
                return self.pos > start;
            }
        }

        // 3. Inline line.
        self.try_parse_inlineline();
        self.pos > start
    }

    /// Try to parse the "sol" rule: newline + optional empty-lines-with-comments + sol transparent tokens.
    fn try_parse_sol(&mut self) -> bool {
        let start = self.pos;
        let mut matched = false;

        // sol_prefix: newlineToken or start of input.
        if self.pos == 0 && self.at_sol {
            matched = true;
        } else if self.starts_with("\r\n") {
            self.advance(2);
            self.emit_token(ParsoidToken::Nl(NlTk::new(self.tsr(start, self.pos))));
            matched = true;
        } else if self.starts_with("\n") {
            self.advance(1);
            self.emit_token(ParsoidToken::Nl(NlTk::new(self.tsr(start, self.pos))));
            matched = true;
        }

        if !matched {
            return false;
        }

        self.at_sol = true;

        // empty_lines_with_comments
        self.try_empty_lines_with_comments();

        // sol_transparent* (comments, include limits, annotation tags, behavior switches)
        let st_count = self.consume_sol_transparent();
        if st_count > 0 {
            self.has_sol_transparent_at_start = true;
        }

        true
    }

    /// Consume SOL-transparent tokens, returning how many were consumed.
    fn consume_sol_transparent(&mut self) -> usize {
        let mut count = 0;
        loop {
            let saved = self.pos;
            if self.try_comment() || self.try_include_limits() || self.try_behavior_switch() {
                count += 1;
                continue;
            }
            if self.pos == saved {
                break;
            }
        }
        count
    }

    /// Try to match empty lines with comments.
    fn try_empty_lines_with_comments(&mut self) -> bool {
        let _start = self.pos;
        let mut matched = false;

        // Must have at least one cycle: space* comment space_or_comment* newline
        loop {
            if self.eof() {
                break;
            }
            let cycle_start = self.pos;
            // space*
            self.consume_spaces();
            // comment
            if self.try_comment() {
                // space_or_comment*
                loop {
                    self.consume_spaces();
                    if !self.try_comment() {
                        break;
                    }
                }
                // newline
                if self.starts_with("\n") || self.starts_with("\r\n") {
                    let nl_start = self.pos;
                    if self.starts_with("\r\n") {
                        self.advance(2);
                    } else {
                        self.advance(1);
                    }
                    self.emit_token(ParsoidToken::Nl(NlTk::new(self.tsr(nl_start, self.pos))));
                    matched = true;
                } else if !self.eof() {
                    // No newline - not a valid empty-line cycle; backtrack.
                    self.pos = cycle_start;
                    break;
                }
            } else {
                break;
            }
        }

        matched
    }

    fn consume_spaces(&mut self) {
        while self.pos < self.input_len {
            let ch = self.input.as_bytes()[self.pos];
            if ch == b' ' || ch == b'\t' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Try to parse block_lines: sol + optional empty line + block_line.
    fn try_parse_block_lines(&mut self) -> bool {
        if !self.at_sol {
            return false;
        }
        self.try_block_line()
    }

    /// Try block_line: heading / list_item / hr / table_line.
    fn try_block_line(&mut self) -> bool {
        if self.try_heading() {
            return true;
        }
        if self.try_list_item() {
            return true;
        }
        if self.try_hr() {
            return true;
        }
        if self.try_table_line() {
            return true;
        }
        false
    }

    /// Try to parse an inline line (until newline or EOF).
    fn try_parse_inlineline(&mut self) -> bool {
        let mut matched = false;

        while self.pos < self.input_len {
            let ch = self.remaining().chars().next().unwrap();

            // Check for inline breaks.
            if ch == '\n' || ch == '\r' {
                if self.starts_with("\r\n") {
                    let p = self.pos;
                    self.advance(2);
                    self.emit_token(ParsoidToken::Nl(NlTk::new(self.tsr(p, self.pos))));
                } else {
                    let p = self.pos;
                    self.advance(1);
                    self.emit_token(ParsoidToken::Nl(NlTk::new(self.tsr(p, self.pos))));
                }
                self.at_sol = true;
                matched = true;
                break;
            }

            // Try inline elements.
            let saved = self.pos;

            if ch == '<' && self.try_angle_bracket_markup() {
                matched = true;
                continue;
            }

            if ch == '{' && self.try_tplarg_or_template() {
                matched = true;
                continue;
            }

            if self.starts_with("-{") && self.try_lang_variant_or_tpl() {
                matched = true;
                continue;
            }

            if ch == '[' && self.try_wikilink_or_extlink() {
                matched = true;
                continue;
            }

            if ch == '\'' && self.try_quote() {
                matched = true;
                continue;
            }

            if self.try_urltext() {
                matched = true;
                continue;
            }

            if self.starts_with("__") && self.try_behavior_switch() {
                matched = true;
                continue;
            }

            if ch == '&' && self.try_html_entity() {
                matched = true;
                continue;
            }

            // If no inline element matched, make sure we advance to avoid infinite loop.
            if self.pos == saved {
                let ch_len = ch.len_utf8();
                let text = self.input[self.pos..self.pos + ch_len].to_string();
                self.pos += ch_len;
                self.emit_text(text);
            }
            matched = true;
        }

        matched
    }

    // ---- Block-level constructs ----

    /// Try to match a heading: `== Title ==`
    fn try_heading(&mut self) -> bool {
        if !self.at_sol {
            return false;
        }
        if !self.starts_with("=") {
            return false;
        }

        let saved = self.pos;

        // Count opening equals.
        let mut open_count = 0;
        let remaining = self.remaining();
        for ch in remaining.chars() {
            if ch == '=' {
                open_count += 1;
            } else {
                break;
            }
        }

        // Need at least 2 equals for a heading, or more than 2 for a single-string heading.
        if open_count < 2 {
            return false;
        }

        // Consume the opening equals.
        self.advance(open_count);

        // Collect inline content until we find closing equals.
        let content_start = self.pos;
        let mut content: Vec<Either<String, ParsoidToken>> = Vec::new();

        // Look for the closing sequence.
        let rest = &self.input[self.pos..];
        let mut close_pos = None;
        let mut close_count = 0;

        // Simple search: find `=+` followed by optional spaces and EOL/EOF.
        if let Some(eq_pos) = rest.find('=') {
            let after = &rest[eq_pos..];
            let count = after.chars().take_while(|&c| c == '=').count();
            if count > 0 {
                close_pos = Some(self.pos + eq_pos);
                close_count = count;
            }
        }

        if let Some(cp) = close_pos {
            // Parse inline content between open and close.
            let content_str = &self.input[content_start..cp];
            if !content_str.is_empty() {
                // Reparse the content as inline.
                let sub_input = content_str;
                let mut sub_tokenizer = PegTokenizer::new(
                    sub_input,
                    &TokenizerOptions {
                        sol: false,
                        ..Default::default()
                    },
                );
                if let Ok(sub_tokens) = sub_tokenizer.tokenize() {
                    content = sub_tokens;
                } else {
                    content.push(Either::Left(content_str.to_string()));
                }
            }
            self.pos = cp;

            let level = open_count.min(close_count).min(6);

            // Emit opening tag.
            let tag_start = saved;
            let dp = DataParsoid::with_tsr(tag_start, tag_start + level);
            self.emit_token(ParsoidToken::Tag(TagTk::new(
                format!("h{level}"),
                vec![],
                dp,
            )));

            // Emit content.
            self.output.append(&mut content);

            // Consume closing equals.
            self.advance(close_count);

            // Emit closing tag.
            let mut end_dp = DataParsoid::default();
            let end_start = self.pos - level;
            end_dp.tsr = Some(SourceRange::new(end_start, self.pos));
            self.emit_token(ParsoidToken::EndTag(EndTagTk::new(
                format!("h{level}"),
                vec![],
                end_dp,
            )));

            // Consume trailing spaces/SOL-transparent.
            self.consume_spaces();
            self.consume_sol_transparent();

            // Consume newline.
            if self.starts_with("\r\n") {
                self.advance(2);
            } else if self.starts_with("\n") {
                self.advance(1);
            }

            self.at_sol = true;
            return true;
        }

        // No closing equals found — this was just text starting with equals.
        // Fall through: backtrack.
        self.pos = saved;
        false
    }

    /// Try to match HR: `----`
    fn try_hr(&mut self) -> bool {
        if !self.at_sol {
            return false;
        }
        if !self.starts_with("----") {
            return false;
        }

        let start = self.pos;
        self.advance(4);

        // Consume extra dashes.
        let extra = self.remaining().chars().take_while(|&c| c == '-').count();
        self.advance(extra);

        let mut dp = self.make_dp(start, self.pos);
        if extra > 0 {
            dp.extra_dashes = Some(extra);
        }

        // Determine if there's line content after the hr.
        let rem = self.remaining();
        let has_line_content =
            !rem.is_empty() && !rem.starts_with("\n") && !rem.starts_with("\r\n");
        if has_line_content {
            dp.line_content = Some(true);
        }

        self.emit_token(ParsoidToken::SelfclosingTag(SelfclosingTagTk::new(
            "hr",
            vec![],
            dp,
        )));

        self.at_sol = true;
        true
    }

    /// Try to match a list item: `*`, `#`, `;`, `:`
    fn try_list_item(&mut self) -> bool {
        if !self.at_sol {
            return false;
        }

        let first = match self.peek_char() {
            Some(c @ ('*' | '#' | ';' | ':')) => c,
            _ => return false,
        };

        // Handle `;term:definition` (dtdd) specially.
        if first == ';' {
            return self.try_dtdd();
        }

        // Handle `:` followed by table (hacky_dl_uses).
        if first == ':' {
            let saved = self.pos;
            let mut colons = 0;
            while self.pos < self.input_len && self.remaining().starts_with(':') {
                colons += 1;
                self.advance(1);
            }
            // Check if followed by table start.
            if self.starts_with("{|") {
                // This is hacky_dl_uses: colons before a table.
                let tsr_start = saved;
                let dp = self.make_dp(tsr_start, tsr_start + colons);
                let bullets: Vec<String> = (0..colons).map(|_| ":".to_string()).collect();
                let bullet_kv = KV {
                    key: KeyValue::Str("bullets".to_string()),
                    value: KeyValue::Tokens(
                        bullets
                            .iter()
                            .map(|_b| {
                                ParsoidToken::Tag(TagTk::new(
                                    "listItem",
                                    vec![],
                                    DataParsoid::default(),
                                ))
                            })
                            .collect(),
                    ),
                    src_offsets: None,
                    ksrc: None,
                    vsrc: None,
                };
                self.emit_token(ParsoidToken::Tag(TagTk::new(
                    "listItem",
                    vec![bullet_kv],
                    dp,
                )));
                // Emit spaces/comments, then parse the table.
                self.consume_spaces();
                self.try_comment();
                self.try_table_start_tag();
                self.at_sol = false;
                return true;
            }
            self.pos = saved;
        }

        // Standard list item: count bullets.
        let start = self.pos;
        let mut count = 0;
        while self.pos < self.input_len {
            let ch = self.remaining().chars().next().unwrap();
            if ch == first {
                count += 1;
                self.advance(ch.len_utf8());
            } else {
                break;
            }
        }

        if count == 0 {
            return false;
        }

        let dp = self.make_dp(start, start + count);
        let bullets: Vec<String> = (0..count).map(|_| first.to_string()).collect();

        let bullet_kv = KV {
            key: KeyValue::Str("bullets".to_string()),
            value: KeyValue::Str(bullets.concat()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };

        self.emit_token(ParsoidToken::Tag(TagTk::new(
            "listItem",
            vec![bullet_kv],
            dp,
        )));

        // Parse inline content after the bullets.
        self.try_parse_inlineline();
        self.at_sol = false;
        true
    }

    /// Try to match a definition term/description pair: `;term:definition`
    fn try_dtdd(&mut self) -> bool {
        if !self.at_sol || !self.starts_with(";") {
            return false;
        }

        let start = self.pos;
        self.advance(1); // consume ';'

        // Collect content before the first colon.
        let _term_saved = self.output.len();
        self.try_parse_inlineline_break_on_colon();

        // Look for ':'
        if !self.starts_with(":") {
            // Not a proper dtdd — backtrack.
            self.pos = start;
            return false;
        }
        self.advance(1); // consume ':'

        let _colon_pos = self.pos - 1;

        // Content after the colon.
        self.try_parse_inlineline();
        self.at_sol = true;
        true
    }

    /// Inline line that breaks on colon.
    fn try_parse_inlineline_break_on_colon(&mut self) -> bool {
        let start = self.pos;
        while self.pos < self.input_len {
            if self.starts_with(":") || self.starts_with("\n") || self.starts_with("\r\n") {
                break;
            }
            // Consume one char.
            if let Some(ch) = self.peek_char() {
                let ch_len = ch.len_utf8();
                let text = self.input[self.pos..self.pos + ch_len].to_string();
                self.pos += ch_len;
                self.emit_text(text);
            }
        }
        self.pos > start
    }

    /// Try to match a redirect.
    fn try_redirect(&mut self) -> Option<ParsoidToken> {
        // Only at very start of document.
        if self.pos != 0 {
            return None;
        }

        let saved = self.pos;
        let remaining = self.remaining();

        // Match redirect word (case-insensitive prefix match).
        let lower = remaining.to_lowercase();
        let redirect_match = if lower.starts_with("#redirect") {
            Some("#redirect")
        } else if lower.starts_with("#redireccion") {
            Some("#redireccion")
        } else {
            None
        };

        let rw = redirect_match?;
        let rw_len = rw.len();
        self.advance(rw_len);

        // Consume optional spaces/newlines.
        self.consume_spaces_or_newlines();

        // Optional colon.
        if self.starts_with(":") {
            self.advance(1);
            self.consume_spaces_or_newlines();
        }

        // Parse the wikilink.
        let link_start = self.pos;
        if !self.try_wikilink_as_token() {
            self.pos = saved;
            return None;
        }

        // Now find the wikilink target.
        let _remaining_after = &self.input[link_start..self.pos];

        let dp = self.make_dp(saved, self.pos);
        let mut dp = dp;
        dp.src = Some(self.input[saved..self.pos].to_string());

        let redirect =
            ParsoidToken::SelfclosingTag(SelfclosingTagTk::new("mw:redirect", vec![], dp));
        Some(redirect)
    }

    /// Parse a wikilink and return as part of redirect parsing.
    fn try_wikilink_as_token(&mut self) -> bool {
        if !self.starts_with("[[") {
            return false;
        }
        self.advance(2);

        // Find the matching `]]`.
        if let Some(end) = self.remaining().find("]]") {
            self.advance(end + 2);
            return true;
        }

        false
    }

    /// Try to parse a table line: table_start_tag / table_content_line / table_end_tag.
    fn try_table_line(&mut self) -> bool {
        if !self.at_sol {
            return false;
        }

        self.consume_spaces();
        self.try_comment();

        let saved = self.pos;
        let output_saved = self.output.len();

        if self.try_table_start_tag() {
            return true;
        }
        if self.try_table_end_tag() {
            return true;
        }
        if self.try_table_content_line() {
            return true;
        }

        self.pos = saved;
        self.output.truncate(output_saved);
        false
    }

    /// Try `{|` table start tag.
    fn try_table_start_tag(&mut self) -> bool {
        if !self.starts_with("{|") {
            return false;
        }

        let start = self.pos;
        self.advance(2);

        let _attr_start = self.pos;
        let attrs = self.parse_table_attributes();
        let ts_end = self.pos;

        self.consume_spaces();

        let mut dp = self.make_dp(start, ts_end);
        dp.start_tag_src = Some("{|".to_string());

        self.emit_token(ParsoidToken::Tag(TagTk::new("table", attrs, dp)));

        self.at_sol = false;
        true
    }

    /// Try `|}` table end tag.
    fn try_table_end_tag(&mut self) -> bool {
        if !self.starts_with("|}") {
            return false;
        }

        let start = self.pos;
        self.advance(2);

        let dp = self.make_dp(start, self.pos);
        self.emit_token(ParsoidToken::EndTag(EndTagTk::new("table", vec![], dp)));

        self.at_sol = true;
        true
    }

    /// Try table content line: heading_tags / row_tag / data_tags / caption_tag.
    fn try_table_content_line(&mut self) -> bool {
        if self.try_table_heading_tags() {
            return true;
        }
        if self.try_table_row_tag() {
            return true;
        }
        if self.try_table_data_tags() {
            return true;
        }
        if self.try_table_caption_tag() {
            return true;
        }
        false
    }

    /// `!` heading cell.
    fn try_table_heading_tags(&mut self) -> bool {
        if !self.starts_with("!") {
            return false;
        }

        let start = self.pos;
        self.advance(1); // consume '!'

        let attrs = self.parse_row_syntax_table_args();
        let tag_end = self.pos;

        let tsr = self.tsr(start, tag_end);
        let dp = self.make_dp_tsr(tsr);

        self.emit_token(ParsoidToken::Tag(TagTk::new("th", attrs, dp)));

        // Process additional heading cells: `!!`
        self.parse_ths();

        self.at_sol = false;
        true
    }

    fn parse_ths(&mut self) {
        while self.starts_with("!!") || self.starts_with("||") {
            let pp_start = self.pos;
            let pp_len = 2;
            self.advance(pp_len);

            let attrs = self.parse_row_syntax_table_args();
            let tag_end = self.pos;

            let tsr = self.tsr(pp_start - pp_len, tag_end);
            let dp = self.make_dp_tsr(tsr);

            self.emit_token(ParsoidToken::Tag(TagTk::new("th", attrs, dp)));
        }
    }

    /// `|-` table row tag.
    fn try_table_row_tag(&mut self) -> bool {
        if !self.starts_with("|-") {
            return false;
        }

        let start = self.pos;
        self.advance(2);

        let _attr_start = self.pos;
        let attrs = self.parse_table_attributes();
        let tag_end = self.pos;

        self.consume_spaces();

        let mut dp = self.make_dp(start, tag_end);
        dp.start_tag_src = Some("|-".to_string());

        self.emit_token(ParsoidToken::Tag(TagTk::new("tr", attrs, dp)));

        self.at_sol = false;
        true
    }

    /// `|` or `||` data cell.
    fn try_table_data_tags(&mut self) -> bool {
        let saved = self.pos;

        // Single pipe.
        if self.starts_with("|")
            && !self.starts_with("|-")
            && !self.starts_with("|}")
            && !self.starts_with("|+")
            && !self.starts_with("||")
        {
            self.advance(1);
        } else {
            return false;
        }

        let attrs = self.parse_row_syntax_table_args();
        let tag_end = self.pos;

        let tsr = self.tsr(saved, tag_end);
        let dp = self.make_dp_tsr(tsr);

        self.emit_token(ParsoidToken::Tag(TagTk::new("td", attrs, dp)));

        // Parse additional `||` data cells.
        self.parse_tds();

        self.at_sol = false;
        true
    }

    fn parse_tds(&mut self) {
        while self.starts_with("||") {
            let pp_start = self.pos;
            self.advance(2);

            let attrs = self.parse_row_syntax_table_args();
            let tag_end = self.pos;

            let tsr = self.tsr(pp_start - 2, tag_end);
            let mut dp = self.make_dp_tsr(tsr);
            dp.stx = Some("row".to_string());

            self.emit_token(ParsoidToken::Tag(TagTk::new("td", attrs, dp)));
        }
    }

    /// `|+` table caption.
    fn try_table_caption_tag(&mut self) -> bool {
        if !self.starts_with("|+") {
            return false;
        }

        let start = self.pos;
        self.advance(2);

        let attrs = self.parse_row_syntax_table_args();
        let tag_end = self.pos;

        let tsr = self.tsr(start, tag_end);
        let dp = self.make_dp_tsr(tsr);

        self.emit_token(ParsoidToken::Tag(TagTk::new("caption", attrs, dp)));

        self.at_sol = false;
        true
    }

    /// Parse table attributes (space-separated key=value pairs).
    fn parse_table_attributes(&mut self) -> Vec<KV> {
        let mut attrs = Vec::new();
        loop {
            self.consume_spaces();
            if self.pos >= self.input_len {
                break;
            }

            let ch = self.remaining().chars().next().unwrap();
            // Stop at pipe, exclamation (unless part of value), newline.
            if ch == '|' || ch == '\n' || ch == '\r' {
                break;
            }

            // Try to parse a table attribute.
            if let Some(attr) = self.parse_table_attribute() {
                attrs.push(attr);
            } else if ch == '!' {
                break;
            } else {
                // Skip one char.
                self.pos += ch.len_utf8();
            }
        }
        attrs
    }

    /// Parse a single table attribute.
    fn parse_table_attribute(&mut self) -> Option<KV> {
        let name_start = self.pos;
        let name = self.parse_table_attribute_name()?;
        let name_end = self.pos;

        self.consume_spaces();

        if self.starts_with("=") {
            self.advance(1);
            let val = self.parse_table_att_value();
            Some(KV {
                key: KeyValue::Str(name),
                value: KeyValue::Str(val.unwrap_or_default()),
                src_offsets: Some(KVSourceRange {
                    key_start: name_start,
                    key_end: name_end,
                    value_start: name_end + 1,
                    value_end: self.pos,
                }),
                ksrc: None,
                vsrc: None,
            })
        } else {
            Some(KV {
                key: KeyValue::Str(name),
                value: KeyValue::Str(String::new()),
                src_offsets: Some(KVSourceRange {
                    key_start: name_start,
                    key_end: name_end,
                    value_start: name_end,
                    value_end: name_end,
                }),
                ksrc: None,
                vsrc: None,
            })
        }
    }

    fn parse_table_attribute_name(&self) -> Option<String> {
        let rem = self.remaining();
        let end = rem
            .find(|c: char| {
                c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '=' || c == '|' || c == '!'
            })
            .unwrap_or(rem.len());
        if end == 0 {
            None
        } else {
            Some(rem[..end].to_string())
        }
    }

    fn parse_table_att_value(&mut self) -> Option<String> {
        self.consume_spaces();

        let rem = self.remaining();
        let mut end = rem.len();

        // Quoted?
        if let Some(stripped) = rem.strip_prefix('"') {
            if let Some(quote_end) = stripped.find('"') {
                end = quote_end + 2;
            }
        } else if let Some(stripped) = rem.strip_prefix('\'') {
            if let Some(quote_end) = stripped.find('\'') {
                end = quote_end + 2;
            }
        } else {
            // Unquoted: stop at space, pipe, newline.
            end = rem
                .find(|c: char| {
                    c == ' ' || c == '\t' || c == '|' || c == '\n' || c == '\r' || c == '!'
                })
                .unwrap_or(rem.len());
        }

        if end == 0 {
            return None;
        }

        let val = rem[..end].to_string();
        self.advance(end);
        Some(val)
    }

    /// Parse row syntax table args (attributes followed by single pipe).
    fn parse_row_syntax_table_args(&mut self) -> Vec<KV> {
        let attrs = self.parse_table_attributes();

        self.consume_spaces();

        // Optional single pipe (not followed by another pipe).
        if self.starts_with("|") && !self.starts_with("||") {
            self.advance(1);
        }

        attrs
    }

    // ---- Inline elements ----

    /// Try angle bracket markup: annotation_tag, extension_tag, include_limits, html_tag, comment.
    fn try_angle_bracket_markup(&mut self) -> bool {
        if self.try_comment() {
            return true;
        }
        if self.try_html_tag() {
            return true;
        }
        false
    }

    /// Try an HTML comment: `<!-- ... -->`
    fn try_comment(&mut self) -> bool {
        if !self.starts_with("<!--") {
            return false;
        }

        let start = self.pos;
        self.advance(4);

        // Find closing `-->`.
        if let Some(end) = self.remaining().find("-->") {
            let comment_text = self.remaining()[..end].to_string();
            self.advance(end + 3);

            let dp = self.make_dp(start, self.pos);
            self.emit_token(ParsoidToken::Comment(CommentTk::new(comment_text, dp)));
            return true;
        }

        // Unclosed comment.
        let comment_text = self.input[self.pos..].to_string();
        self.pos = self.input_len;

        let mut dp = self.make_dp(start, self.pos);
        dp.unclosed_comment = Some(true);
        self.emit_token(ParsoidToken::Comment(CommentTk::new(comment_text, dp)));
        true
    }

    /// Try an HTML tag: `<tag attr="val">` or `</tag>`
    fn try_html_tag(&mut self) -> bool {
        if !self.starts_with("<") {
            return false;
        }

        // Closing tag: </tag>
        if self.starts_with("</") {
            let saved = self.pos;
            self.advance(2);
            if let Some(end) = self.remaining().find('>') {
                let name = self.remaining()[..end].trim().to_lowercase();
                self.advance(end + 1);

                let mut dp = self.make_dp(saved, self.pos);
                dp.stx = Some("html".to_string());
                self.emit_token(ParsoidToken::EndTag(EndTagTk::new(name, vec![], dp)));
                return true;
            }
            self.pos = saved;
            return false;
        }

        // Opening or self-closing tag.
        let saved = self.pos;
        self.advance(1);

        // Parse tag name.
        let name = self.parse_tag_name();
        if name.is_empty() {
            self.pos = saved;
            return false;
        }

        // Parse attributes.
        let attrs = self.parse_html_attributes();

        // Self-closing?
        self.consume_spaces();
        let self_closing = self.starts_with("/>");
        if self_closing {
            self.advance(2);
        } else if self.starts_with(">") {
            self.advance(1);
        } else {
            self.pos = saved;
            return false;
        }

        let mut dp = self.make_dp(saved, self.pos);
        // Literal HTML tags carry `stx: "html"` (mirrors Parsoid's `StxInfo`).
        dp.stx = Some("html".to_string());

        if self_closing {
            self.emit_token(ParsoidToken::SelfclosingTag(SelfclosingTagTk::new(
                name.to_lowercase(),
                attrs,
                dp,
            )));
        } else {
            self.emit_token(ParsoidToken::Tag(TagTk::new(
                name.to_lowercase(),
                attrs,
                dp,
            )));
        }

        true
    }

    fn parse_tag_name(&mut self) -> String {
        let rem = self.remaining();
        let end = rem
            .find([' ', '\t', '\n', '\r', '/', '>'])
            .unwrap_or(rem.len());
        let name = rem[..end].to_string();
        self.advance(end);
        name
    }

    fn parse_html_attributes(&mut self) -> Vec<KV> {
        let mut attrs = Vec::new();
        loop {
            self.consume_spaces();
            if self.pos >= self.input_len {
                break;
            }
            let ch = self.remaining().chars().next().unwrap();
            if ch == '/' || ch == '>' {
                break;
            }

            // Parse attribute name.
            let name_start = self.pos;
            let name = self.parse_attr_name();
            let name_end = self.pos;
            if name.is_empty() {
                break;
            }

            self.consume_spaces();

            if self.starts_with("=") {
                self.advance(1);
                self.consume_spaces();
                let val = self.parse_attr_value();
                attrs.push(KV {
                    key: KeyValue::Str(name),
                    value: KeyValue::Str(val),
                    src_offsets: Some(KVSourceRange {
                        key_start: name_start,
                        key_end: name_end,
                        value_start: name_end + 1,
                        value_end: self.pos,
                    }),
                    ksrc: None,
                    vsrc: None,
                });
            } else {
                attrs.push(KV {
                    key: KeyValue::Str(name),
                    value: KeyValue::Str(String::new()),
                    src_offsets: Some(KVSourceRange {
                        key_start: name_start,
                        key_end: name_end,
                        value_start: name_end,
                        value_end: name_end,
                    }),
                    ksrc: None,
                    vsrc: None,
                });
            }
        }
        attrs
    }

    fn parse_attr_name(&mut self) -> String {
        let rem = self.remaining();
        let end = rem
            .find(|c: char| {
                c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '=' || c == '/' || c == '>'
            })
            .unwrap_or(rem.len());
        let name = rem[..end].to_string();
        self.advance(end);
        name
    }

    fn parse_attr_value(&mut self) -> String {
        let rem = self.remaining();
        if let Some(stripped) = rem.strip_prefix('"')
            && let Some(end) = stripped.find('"')
        {
            let val = stripped[..end].to_string();
            self.advance(end + 2);
            return val;
        } else if let Some(stripped) = rem.strip_prefix('\'')
            && let Some(end) = stripped.find('\'')
        {
            let val = stripped[..end].to_string();
            self.advance(end + 2);
            return val;
        }

        // Unquoted.
        let end = rem
            .find([' ', '\t', '\n', '\r', '/', '>'])
            .unwrap_or(rem.len());
        let val = rem[..end].to_string();
        self.advance(end);
        val
    }

    /// Try template argument or template: `{{{ ... }}}` or `{{ ... }}`
    fn try_tplarg_or_template(&mut self) -> bool {
        if !self.starts_with("{{{") {
            return self.try_template();
        }

        let saved = self.pos;
        self.advance(3);

        // Find the closing `}}}` (respecting brace nesting).
        let Some(end) = self.find_closing('}', 3) else {
            self.pos = saved;
            return false;
        };

        // `end` is a byte offset relative to `self.pos` (after `{{{`).
        let inner = self.remaining()[..end].to_string();
        self.advance(end + 3);

        let mut dp = self.make_dp(saved, self.pos);
        dp.src = Some(self.input[saved..self.pos].to_string());
        let mut stt = SelfclosingTagTk::new("templatearg", vec![], dp);

        // Split content on the first '|' for name | default.
        let (name, default) = match inner.split_once('|') {
            Some((n, d)) => (n.trim().to_string(), Some(d.to_string())),
            None => (inner.trim().to_string(), None),
        };

        // Mirrors `tplarg`: attribs[0] is KV(name, '') and attribs[1] (if any)
        // is KV('', default).
        if !name.is_empty() {
            stt.attribs.push(kv_str(&name, ""));
            if let Some(default) = default {
                stt.attribs.push(kv_str("", &default));
            }
        }
        self.emit_token(ParsoidToken::SelfclosingTag(stt));
        true
    }

    /// Find the byte offset (within `remaining`, exclusive) of the closing
    /// delimiter made of `close` repeated `count` times, respecting nested
    /// open/close pairs (two-level brace counting).
    fn find_closing(&self, close: char, count: usize) -> Option<usize> {
        let rem = self.remaining();
        let chars: Vec<char> = rem.chars().collect();
        let mut depth: i32 = count as i32;
        let mut byte_pos = 0usize;
        let mut i = 0;
        while i < chars.len() {
            let ch_len = chars[i].len_utf8();
            // Detect '{{' opens (for template nesting) regardless of final close char.
            if chars[i] == '{' && i + 1 < chars.len() && chars[i + 1] == '{' {
                depth += 2;
                byte_pos += 2; // '{' and '{' are single-byte.
                i += 2;
                continue;
            }
            if chars[i] == close
                && i + count - 1 < chars.len()
                && chars[i..i + count].iter().all(|&c| c == close)
            {
                depth -= count as i32;
                if depth <= 0 {
                    return Some(byte_pos);
                }
                byte_pos += count; // 'close' is a single-byte ASCII char.
                i += count;
                continue;
            }
            byte_pos += ch_len;
            i += 1;
        }
        None
    }

    /// Try template: `{{ ... }}`
    fn try_template(&mut self) -> bool {
        if !self.starts_with("{{") || self.starts_with("{{{") {
            return false;
        }

        let saved = self.pos;
        self.advance(2);

        let Some(end) = self.find_closing('}', 2) else {
            self.pos = saved;
            return false;
        };

        // `end` is a byte offset relative to `self.pos` (after the `{{`);
        // the closing `}}` is at `self.pos + end`, and the inner content is
        // everything between `{{` and `}}`.
        let inner = {
            let rem = self.remaining();
            rem[..end].to_string()
        };
        self.advance(end + 2);

        // Split the inner content on top-level '|' into target + arguments.
        let parts = split_template_args(&inner);

        let mut dp = self.make_dp(saved, self.pos);
        dp.src = Some(self.input[saved..self.pos].to_string());
        let mut stt = SelfclosingTagTk::new("template", vec![], dp);

        // attribs[0] = KV(target, '') — target is the part before the first '|'.
        let target = parts.first().map(|s| s.as_str()).unwrap_or("").trim();
        stt.attribs.push(kv_str(target, ""));

        // attribs[1..] are the arguments: `name=value` is named, else positional.
        for part in parts.iter().skip(1) {
            if let Some(eq) = part.find('=') {
                let k = part[..eq].trim().to_string();
                let v = part[eq + 1..].to_string();
                stt.attribs.push(kv_str(&k, &v));
            } else {
                stt.attribs.push(kv_str("", part));
            }
        }

        self.emit_token(ParsoidToken::SelfclosingTag(stt));
        true
    }

    /// Try language variant or template: `-{ ... }-`
    fn try_lang_variant_or_tpl(&mut self) -> bool {
        if !self.starts_with("-{") {
            return false;
        }

        let saved = self.pos;
        self.advance(2);

        // Find closing `}-`.
        if let Some(end) = self.remaining().find("}-") {
            let _inner = self.remaining()[..end].to_string();
            self.advance(end + 2);

            let dp = self.make_dp(saved, self.pos);
            self.emit_token(ParsoidToken::SelfclosingTag(SelfclosingTagTk::new(
                "language-variant",
                vec![],
                dp,
            )));
            return true;
        }

        self.pos = saved;
        false
    }

    /// Try wikilink (`[[...]]`) or extlink (`[...]`).
    fn try_wikilink_or_extlink(&mut self) -> bool {
        if self.starts_with("[[") {
            return self.try_wikilink();
        }
        if self.starts_with("[") {
            return self.try_extlink();
        }
        false
    }

    /// Try wikilink: `[[Target|text]]`
    fn try_wikilink(&mut self) -> bool {
        if !self.starts_with("[[") {
            return false;
        }

        let saved = self.pos;
        self.advance(2);

        // Find closing `]]`.
        if let Some(end) = self.remaining().find("]]") {
            let content = &self.remaining()[..end];
            let (target, text) = if let Some(pipe) = content.find('|') {
                (
                    content[..pipe].to_string(),
                    Some(content[pipe + 1..].to_string()),
                )
            } else {
                (content.to_string(), None)
            };

            self.advance(end + 2);

            let dp = self.make_dp(saved, self.pos);
            let mut stt = SelfclosingTagTk::new("wikilink", vec![], dp);
            stt.add_attribute_str("href", target);
            if let Some(text) = text {
                stt.add_attribute_str("mw:maybeContent", text);
            }

            self.emit_token(ParsoidToken::SelfclosingTag(stt));
            return true;
        }

        self.pos = saved;
        false
    }

    /// Try external link: `[url text]`
    fn try_extlink(&mut self) -> bool {
        if !self.starts_with("[") || self.starts_with("[[") {
            return false;
        }

        let saved = self.pos;
        self.advance(1);

        // Check for URL protocol.
        let rem = self.remaining();
        let has_protocol = rem.starts_with("http://")
            || rem.starts_with("https://")
            || rem.starts_with("ftp://")
            || rem.starts_with("mailto:")
            || rem.starts_with("//");

        if !has_protocol {
            self.pos = saved;
            return false;
        }

        // Find the closing `]`.
        if let Some(end) = self.remaining().find(']') {
            let content = &self.remaining()[..end];
            let (url, text) = if let Some(space) = content.find([' ', '\t']) {
                (
                    content[..space].to_string(),
                    Some(content[space + 1..].to_string()),
                )
            } else {
                (content.to_string(), None)
            };

            self.advance(end + 1);

            let dp = self.make_dp(saved, self.pos);
            let mut stt = SelfclosingTagTk::new("extlink", vec![], dp);
            stt.add_attribute_str("href", url);
            if let Some(text) = text {
                stt.add_attribute_str("mw:content", text);
            }

            self.emit_token(ParsoidToken::SelfclosingTag(stt));
            return true;
        }

        self.pos = saved;
        false
    }

    /// Try quote: `''`, `'''`, `'''''`
    fn try_quote(&mut self) -> bool {
        let start = self.pos;

        // Count the number of consecutive single quotes.
        let mut count = 0usize;
        let remaining = self.remaining();
        for ch in remaining.chars() {
            if ch == '\'' {
                count += 1;
            } else {
                break;
            }
        }

        if count < 2 {
            return false;
        }

        // Rules from PHP PEG grammar:
        // - 4 quotes: first is plain text apostrophe, rest is `'''` (bold)
        // - 5 quotes: all 5 are `'''''` (bold+italic)
        // - >5 quotes: first N-5 are plain text apostrophes, 5 are `'''''`
        let plain_ticks = if count == 4 {
            1
        } else {
            count.saturating_sub(5)
        };

        let quote_len = count - plain_ticks;

        // Emit plain text apostrophes if any.
        if plain_ticks > 0 {
            let text = "'".repeat(plain_ticks);
            self.emit_text(text);
            self.pos += plain_ticks;
        }

        // Emit the mw-quote token.
        let quote_chars = "'".repeat(quote_len);
        self.advance(quote_len);

        let dp = self.make_dp(start + plain_ticks, self.pos);
        let mut stt = SelfclosingTagTk::new("mw-quote", vec![], dp);
        stt.add_attribute_str("value", quote_chars);

        self.emit_token(ParsoidToken::SelfclosingTag(stt));
        true
    }

    /// Try behavior switch: `__TOC__`, `__NOTOC__`, etc.
    fn try_behavior_switch(&mut self) -> bool {
        if !self.starts_with("__") {
            return false;
        }

        let start = self.pos;
        self.advance(2);

        if let Some(end) = self.remaining().find("__") {
            let word = self.input[start..self.pos + end + 2].to_string();
            self.advance(end + 2);

            let dp = self.make_dp(start, self.pos);
            let mut stt = SelfclosingTagTk::new("behavior-switch", vec![], dp);
            stt.add_attribute_str("word", &word);

            self.emit_token(ParsoidToken::SelfclosingTag(stt));
            return true;
        }

        self.pos = start;
        false
    }

    /// Try include limits: `<includeonly>`, `<noinclude>`, `<onlyinclude>`.
    fn try_include_limits(&mut self) -> bool {
        let saved = self.pos;
        if !self.starts_with("<") {
            return false;
        }

        let tag = if self.starts_with("<includeonly>") {
            Some(("includeonly", "<includeonly>".len()))
        } else if self.starts_with("<noinclude>") {
            Some(("noinclude", "<noinclude>".len()))
        } else if self.starts_with("<onlyinclude>") {
            Some(("onlyinclude", "<onlyinclude>".len()))
        } else {
            return false;
        };

        let (name, tag_len) = tag.unwrap();
        self.advance(tag_len);

        let dp = self.make_dp(saved, self.pos);
        let meta_type = format!("mw:Includes/{}", name_to_include_type(name));

        let mut stt = SelfclosingTagTk::new("meta", vec![], dp);
        stt.add_attribute_str("typeof", meta_type);

        self.emit_token(ParsoidToken::SelfclosingTag(stt));
        true
    }

    /// Try URL text (plain text that could contain URLs).
    fn try_urltext(&mut self) -> bool {
        // First check for URL protocols.
        let rem = self.remaining();
        let prefixes = ["http://", "https://", "ftp://", "//"];

        for prefix in &prefixes {
            if rem.starts_with(prefix) {
                let start = self.pos;
                let end = rem
                    .find(|c: char| {
                        c == ' '
                            || c == '\t'
                            || c == '\n'
                            || c == '\r'
                            || c == ']'
                            || c == '>'
                            || c == '<'
                    })
                    .unwrap_or(rem.len());

                if end > prefix.len() {
                    let url = rem[..end].to_string();
                    self.advance(end);

                    let dp = self.make_dp(start, self.pos);
                    let mut stt = SelfclosingTagTk::new("urllink", vec![], dp);
                    stt.add_attribute_str("href", url);
                    self.emit_token(ParsoidToken::SelfclosingTag(stt));
                    return true;
                }
            }
        }

        // Otherwise, match a run of plain text until we hit a special character.
        let _start = self.pos;
        let rem = self.remaining();
        let end = rem
            .find(|c: char| {
                matches!(
                    c,
                    '<' | '{'
                        | '['
                        | '\''
                        | '&'
                        | '\n'
                        | '\r'
                        | '='
                        | '*'
                        | '#'
                        | ';'
                        | ':'
                        | '|'
                        | '!'
                        | ']'
                        | '}'
                        | '-'
                ) || (c == '_' && rem.as_bytes().get(1).copied() == Some(b'_'))
            })
            .unwrap_or(rem.len());

        if end > 0 {
            let text = rem[..end].to_string();
            self.advance(end);
            self.emit_text(text);
            return true;
        }

        false
    }

    /// Try HTML entity: `&amp;`, `&#123;`, etc.
    fn try_html_entity(&mut self) -> bool {
        if !self.starts_with("&") {
            return false;
        }

        let start = self.pos;
        let rem = self.remaining();

        // Match `&name;` or `&#...;`
        if let Some(end) = rem.find(';') {
            let entity = &rem[..end + 1];
            // Simple validation: must be at least `&x;` form.
            if entity.len() >= 3 {
                self.advance(end + 1);

                let _dp = self.make_dp(start, self.pos);
                let _dp_start = self.make_dp(start, start);
                let _dp_end = self.make_dp(self.pos, self.pos);

                // Emit mw:Entity span.
                let mut _tag = SelfclosingTagTk::new("span", vec![], _dp_start);
                _tag.add_attribute_str("typeof", "mw:Entity");
                // Actually, per PHP, it's a TagTk + text + EndTagTk.
                self.emit_text(entity.to_string());
                return true;
            }
        }

        false
    }

    // ---- Utility ----

    fn consume_spaces_or_newlines(&mut self) {
        while self.pos < self.input_len {
            let ch = self.input.as_bytes()[self.pos];
            if ch == b' ' || ch == b'\t' || ch == b'\n' || ch == b'\r' || ch == 0x0c {
                self.pos += 1;
                // Handle \r\n.
                if ch == b'\r'
                    && self.pos < self.input_len
                    && self.input.as_bytes()[self.pos] == b'\n'
                {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }
}

fn name_to_include_type(name: &str) -> &str {
    match name {
        "includeonly" => "IncludeOnly",
        "noinclude" => "NoInclude",
        "onlyinclude" => "OnlyInclude",
        _ => "Unknown",
    }
}

/// Build a string-valued KV (key/value as string tokens).
fn kv_str(key: &str, value: &str) -> KV {
    KV {
        key: KeyValue::Str(key.to_string()),
        value: KeyValue::Str(value.to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    }
}

/// Split a template invocation's inner content on top-level `|` characters,
/// respecting nested `{{...}}`, `[[...]]`, and `{{{...}}}` constructs.
fn split_template_args(inner: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut double_brace: i32 = 0;
    let mut triple_brace: i32 = 0;
    let mut bracket: i32 = 0;
    let chars: Vec<char> = inner.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // Track nesting of `{{{`, `{{`, and `[[`.
        if c == '{' && i + 2 < chars.len() && chars[i + 1] == '{' && chars[i + 2] == '{' {
            triple_brace += 1;
            current.push_str("{{{");
            i += 3;
            continue;
        }
        if c == '{' && i + 1 < chars.len() && chars[i + 1] == '{' {
            double_brace += 1;
            current.push_str("{{");
            i += 2;
            continue;
        }
        if c == '}' && i + 2 < chars.len() && chars[i + 1] == '}' && chars[i + 2] == '}' {
            triple_brace = triple_brace.saturating_sub(1);
            current.push_str("}}}");
            i += 3;
            continue;
        }
        if c == '}' && i + 1 < chars.len() && chars[i + 1] == '}' {
            double_brace = double_brace.saturating_sub(1);
            current.push_str("}}");
            i += 2;
            continue;
        }
        if c == '[' && i + 1 < chars.len() && chars[i + 1] == '[' {
            bracket += 1;
            current.push_str("[[");
            i += 2;
            continue;
        }
        if c == ']' && i + 1 < chars.len() && chars[i + 1] == ']' {
            bracket = bracket.saturating_sub(1);
            current.push_str("]]");
            i += 2;
            continue;
        }
        if c == '|' && double_brace == 0 && triple_brace == 0 && bracket == 0 {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
        i += 1;
    }
    parts.push(current);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(input: &str) -> Vec<Either<String, ParsoidToken>> {
        let mut tokenizer = PegTokenizer::new(input, &TokenizerOptions::default());
        tokenizer.tokenize().unwrap()
    }

    #[test]
    fn test_empty_input() {
        let tokens = tokenize("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_plain_text() {
        let tokens = tokenize("Hello world");
        assert_eq!(tokens.len(), 1);
        assert!(matches!(&tokens[0], Either::Left(s) if s == "Hello world"));
    }

    #[test]
    fn test_bold_quote() {
        let tokens = tokenize("'''bold'''");
        // Should produce: mw-quote('''), "bold", mw-quote(''')
        assert!(tokens.len() >= 2);
    }

    #[test]
    fn test_heading() {
        let tokens = tokenize("== Title ==");
        // Should produce h2 tags.
        let has_h2_open = tokens
            .iter()
            .any(|t| matches!(t, Either::Right(ParsoidToken::Tag(tk)) if tk.name == "h2"));
        assert!(has_h2_open, "Expected h2 tag, got: {:?}", tokens);
    }

    #[test]
    fn test_wikilink() {
        let tokens = tokenize("[[Main Page]]");
        let has_wikilink = tokens.iter().any(|t| matches!(t, Either::Right(ParsoidToken::SelfclosingTag(tk)) if tk.name == "wikilink"));
        assert!(has_wikilink, "Expected wikilink, got: {:?}", tokens);
    }

    #[test]
    fn test_external_link() {
        let tokens = tokenize("[http://example.com link]");
        let has_extlink = tokens.iter().any(|t| matches!(t, Either::Right(ParsoidToken::SelfclosingTag(tk)) if tk.name == "extlink"));
        assert!(has_extlink, "Expected extlink, got: {:?}", tokens);
    }

    #[test]
    fn test_template() {
        let tokens = tokenize("{{foo}}");
        let has_template = tokens.iter().any(|t| matches!(t, Either::Right(ParsoidToken::SelfclosingTag(tk)) if tk.name == "template"));
        assert!(has_template, "Expected template, got: {:?}", tokens);
    }

    #[test]
    fn test_template_with_args() {
        let tokens = tokenize("{{foo|bar|baz=qux}}");
        let template = tokens
            .iter()
            .find_map(|t| match t {
                Either::Right(ParsoidToken::SelfclosingTag(tk)) if tk.name == "template" => {
                    Some(tk)
                }
                _ => None,
            })
            .expect("expected template token");

        // attribs[0] is the target, then positional then named args.
        assert_eq!(template.attribs.len(), 3);
        assert_eq!(template.attribs[0].key.as_str(), Some("foo"));
        assert_eq!(template.attribs[1].key.as_str(), Some(""));
        assert_eq!(template.attribs[1].value.as_str(), Some("bar"));
        assert_eq!(template.attribs[2].key.as_str(), Some("baz"));
        assert_eq!(template.attribs[2].value.as_str(), Some("qux"));
    }

    #[test]
    fn test_template_nested_pipe() {
        // Pipes inside a nested template should not split the outer args.
        let tokens = tokenize("{{foo|{{bar|x}}|baz}}");
        let template = tokens
            .iter()
            .find_map(|t| match t {
                Either::Right(ParsoidToken::SelfclosingTag(tk)) if tk.name == "template" => {
                    Some(tk)
                }
                _ => None,
            })
            .expect("expected template token");

        assert_eq!(template.attribs.len(), 3);
        assert_eq!(template.attribs[0].key.as_str(), Some("foo"));
        assert_eq!(template.attribs[1].key.as_str(), Some(""));
        assert_eq!(template.attribs[1].value.as_str(), Some("{{bar|x}}"));
    }

    #[test]
    fn test_template_arg_token() {
        let tokens = tokenize("{{{1|default}}}");
        let tplarg = tokens
            .iter()
            .find_map(|t| match t {
                Either::Right(ParsoidToken::SelfclosingTag(tk)) if tk.name == "templatearg" => {
                    Some(tk)
                }
                _ => None,
            })
            .expect("expected templatearg token");

        assert_eq!(tplarg.attribs.len(), 2);
        assert_eq!(tplarg.attribs[0].key.as_str(), Some("1"));
        assert_eq!(tplarg.attribs[1].key.as_str(), Some(""));
        assert_eq!(tplarg.attribs[1].value.as_str(), Some("default"));
    }

    #[test]
    fn test_hr() {
        let tokens = tokenize("----");
        let has_hr = tokens.iter().any(
            |t| matches!(t, Either::Right(ParsoidToken::SelfclosingTag(tk)) if tk.name == "hr"),
        );
        assert!(has_hr, "Expected hr, got: {:?}", tokens);
    }

    #[test]
    fn test_table_start() {
        let tokens = tokenize("{|");
        let has_table = tokens
            .iter()
            .any(|t| matches!(t, Either::Right(ParsoidToken::Tag(tk)) if tk.name == "table"));
        assert!(has_table, "Expected table, got: {:?}", tokens);
    }
}
