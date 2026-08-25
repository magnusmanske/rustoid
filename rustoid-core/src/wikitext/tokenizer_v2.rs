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

/// Which magic links (RFC/PMID/ISBN) are enabled for tokenization.
/// Mirrors the three `SiteConfig::magicLinkEnabled` lookups.
#[derive(Debug, Clone, Copy, Default)]
pub struct MagicLinkConfig {
    pub rfc: bool,
    pub pmid: bool,
    pub isbn: bool,
}

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
    /// Enabled magic-link types (RFC/PMID/ISBN).
    pub magic_links: MagicLinkConfig,
    /// Localized synonyms for the `redirect` magic word (each including the
    /// leading `#`), e.g. `#REDIRECT`, `#TILVÍSUN`. Matched case-insensitively,
    /// mirroring PHP's `getMagicWordMatcher( 'redirect' )`.
    pub redirect_words: Vec<String>,
    /// Recognized extension tag names (lowercased), e.g. `nowiki`, `pre`, `ref`.
    pub ext_tags: Vec<String>,
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
            magic_links: MagicLinkConfig::default(),
            redirect_words: vec!["#redirect".to_string()],
            ext_tags: Vec::new(),
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
    /// Heading counter (start-of-line headings get an incrementing section
    /// number, mirroring PHP's `headingIndex`).
    heading_index: usize,
    /// Accumulated has-sol-transparent-at-start flag.
    has_sol_transparent_at_start: bool,
    /// Enabled magic-link types (RFC/PMID/ISBN).
    magic_links: MagicLinkConfig,
    /// Localized synonyms for the `redirect` magic word (lowercased), each
    /// including the leading `#`.
    redirect_words: Vec<String>,
}

impl<'a> PegTokenizer<'a> {
    pub fn new(input: &'a str, options: &TokenizerOptions) -> Self {
        Self {
            input,
            pos: 0,
            input_len: input.len(),
            at_sol: options.sol,
            in_template: options.in_template,
            annotation_tags: Vec::new(),
            output: Vec::new(),
            heading_index: 0,
            has_sol_transparent_at_start: false,
            magic_links: options.magic_links,
            redirect_words: options
                .redirect_words
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            ext_tags: options.ext_tags.iter().map(|s| s.to_lowercase()).collect(),
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

            // 2a. At SOL with no preceding newline, a run of comment-only lines
            // (each `space* comment space_or_comment* newline`) is consumed as a
            // single `EmptyLineTk`, mirroring PHP's `empty_lines_with_comments`.
            // This is reachable because `inlineline` already consumed the
            // preceding newline that put us at SOL.
            if self.try_empty_lines_with_comments() {
                return true;
            }

            // 4. SOL (newline + empty-lines + sol-transparent) then a block line
            // or inline line. Mirrors PHP `block_lines = sol block_line` — after
            // sol-transparent tokens (comments/behavior-switches) are consumed,
            // a list/heading/hr/table may still follow on the same line.
            if self.try_parse_sol() {
                let block_saved = self.pos;
                let block_output = self.output.len();
                if self.try_block_line() {
                    self.has_sol_transparent_at_start = true;
                    return true;
                }
                // `try_block_line` may have consumed SOL whitespace via
                // `try_table_line` and then backtracked internally, but the
                // leading space must be preserved for indent-pre detection.
                self.pos = block_saved;
                self.output.truncate(block_output);
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
    ///
    /// Mirrors PHP's `empty_lines_with_comments`: each `space* comment
    /// space_or_comment* newline` cycle is wrapped into a single `EmptyLineTk`
    /// (with the trailing newline as an `NlTk` nested inside), instead of
    /// emitting a top-level `NlTk`. This matters because a top-level newline
    /// would increment the ParagraphWrapper's `newLineCount` and spuriously
    /// break a paragraph.
    fn try_empty_lines_with_comments(&mut self) -> bool {
        let start = self.pos;
        let mut inner: Vec<ParsoidToken> = Vec::new();
        let mut matched = false;

        loop {
            if self.eof() {
                break;
            }
            let cycle_start = self.pos;
            let out_len = self.output.len();
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
                    // Collect the comment tokens emitted during this cycle (in
                    // the order they were produced), then the newline token.
                    let emitted = self.output.drain(out_len..).filter_map(|e| match e {
                        Either::Right(tok) => Some(tok),
                        Either::Left(_) => None,
                    });
                    inner.extend(emitted);
                    inner.push(ParsoidToken::Nl(NlTk::new(self.tsr(nl_start, self.pos))));
                    matched = true;
                } else if !self.eof() {
                    // No newline - not a valid empty-line cycle; backtrack.
                    self.pos = cycle_start;
                    // Drop any comment tokens emitted for the aborted cycle.
                    self.output.truncate(out_len);
                    break;
                }
            } else {
                break;
            }
        }

        if matched {
            let dp = self.make_dp(start, self.pos);
            self.emit_token(ParsoidToken::EmptyLine(EmptyLineTk::new(inner, dp)));
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

            if self.try_magic_link() {
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
            // Parse inline content between open and close, trimming the
            // surrounding whitespace so the heading text matches MediaWiki's
            // output (e.g. `== Heading ==` → `Heading`, not ` Heading `).
            let content_str = self.input[content_start..cp].trim();
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
            let mut dp = DataParsoid::with_tsr(tag_start, tag_start + level);
            // Assign a heading index (used later for section wrapping). The
            // PHP grammar increments the index only for top-level, non-
            // SOL-transparent headings outside templates.
            if !self.in_template {
                self.heading_index += 1;
                dp.tmp.heading_index = Some(self.heading_index);
            }
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
                                Item::Tok(ParsoidToken::Tag(TagTk::new(
                                    "listItem",
                                    vec![],
                                    DataParsoid::default(),
                                )))
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

        // Parse inline content after the bullets. The single space separator
        // between the bullets and the content is not part of the item text.
        self.consume_spaces();
        self.try_parse_inlineline();
        true
    }

    /// Try to match a definition term/description pair: `;term:definition`.
    /// Emits two `listItem` tokens (a `dt` for the term and a `dd` for the
    /// definition), so the ListHandler produces `<dl><dt>…</dt><dd>…</dd></dl>`.
    fn try_dtdd(&mut self) -> bool {
        if !self.at_sol || !self.starts_with(";") {
            return false;
        }

        let start = self.pos;
        self.advance(1); // consume ';'

        // First list item: the definition term.
        let bullets_1 = KV {
            key: KeyValue::Str("bullets".to_string()),
            value: KeyValue::Str(";".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        let dp_1 = self.make_dp(start, start + 1);
        self.emit_token(ParsoidToken::Tag(TagTk::new(
            "listItem",
            vec![bullets_1],
            dp_1,
        )));

        // The term content runs up to the colon (or end of line).
        self.try_parse_inlineline_break_on_colon();

        if !self.starts_with(":") {
            // No colon on this line: a `;term` definition term followed by a
            // newline. Consume the newline so a following `:definition` is
            // recognized as its own list item on the next line.
            if self.starts_with("\r\n") {
                let nl_start = self.pos;
                self.advance(2);
                self.emit_token(ParsoidToken::Nl(NlTk::new(self.tsr(nl_start, self.pos))));
            } else if self.starts_with("\n") {
                let nl_start = self.pos;
                self.advance(1);
                self.emit_token(ParsoidToken::Nl(NlTk::new(self.tsr(nl_start, self.pos))));
            }
            self.at_sol = true;
            return true;
        }
        self.advance(1); // consume ':'

        // Second list item: the definition description.
        let colon_pos = self.pos - 1;
        let mut dp_2 = self.make_dp(colon_pos, colon_pos + 1);
        dp_2.stx = Some("row".to_string());
        let bullets_2 = KV {
            key: KeyValue::Str("bullets".to_string()),
            value: KeyValue::Str(":".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        self.emit_token(ParsoidToken::Tag(TagTk::new(
            "listItem",
            vec![bullets_2],
            dp_2,
        )));

        // The definition content runs to the end of the line.
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
    /// Try redirect.
    fn try_redirect(&mut self) -> Option<ParsoidToken> {
        // Only at very start of document.
        if self.pos != 0 {
            return None;
        }

        let saved = self.pos;

        // Match redirect word (case-insensitive). Mirrors PHP's `redirect_word`,
        // which matches `[ \t\n\r\0\x0b]*` then a run of
        // `[^ \t\n\r\x0c:\[]+` and checks it against the localized `redirect`
        // magic word synonyms (each including the leading `#`).
        let remaining = self.remaining();
        let lower = remaining.to_lowercase();
        let rw = self
            .redirect_words
            .iter()
            .find(|w| lower.starts_with(w.as_str()))
            .cloned()?;
        // The redirect word must be followed by a terminator (whitespace,
        // colon, or the wikilink opener), mirroring PHP's `[^ \t\n\r\x0c:\[]+`
        // (the word cannot contain `:`, `[`, or whitespace).
        let after = self.input[self.pos + rw.len()..].chars().next();
        if let Some(c) = after
            && !c.is_whitespace()
            && c != ':'
            && c != '['
        {
            return None;
        }
        self.advance(rw.len());

        // Consume optional spaces/newlines.
        self.consume_spaces_or_newlines();

        // Optional colon.
        if self.starts_with(":") {
            self.advance(1);
            self.consume_spaces_or_newlines();
        }

        // The `src` of the redirect token is just the redirect word plus any
        // trailing spaces/colon (mirrors PHP, which sets `$dp->src = $rw`); the
        // wikilink itself is not part of `src`.
        let link_start = self.pos;

        // Parse the wikilink target.
        if !self.starts_with("[[") {
            self.pos = saved;
            return None;
        }
        self.advance(2);
        let target_start = self.pos;
        let Some(end) = find_wikilink_close(self.remaining()) else {
            self.pos = saved;
            return None;
        };
        let link_text = &self.input[target_start..self.pos + end];
        // The redirect target is the part before the first `|` (link label).
        let target = link_text.split('|').next().unwrap_or(link_text).trim();
        self.advance(end + 2);

        let mut dp = self.make_dp(saved, self.pos);
        dp.src = Some(self.input[saved..link_start].to_string());

        let mut redirect = SelfclosingTagTk::new("mw:redirect", vec![], dp);
        redirect.add_attribute_str("href", target);
        Some(ParsoidToken::SelfclosingTag(redirect))
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

        // The cell content precedes any `||` separator on the same line.
        self.parse_table_cell_text();

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
            self.parse_table_cell_text();
        }
    }

    /// Consume a text run that is a table-cell's content, stopping before a
    /// `||`, `|`, `!`, or end of line.
    fn parse_table_cell_text(&mut self) {
        if self.eof()
            || self.starts_with("||")
            || self.starts_with("|")
            || self.starts_with("!")
            || self.starts_with("\n")
            || self.starts_with("\r\n")
        {
            return;
        }
        let rem = self.remaining();
        let end = rem.find(['|', '!', '\n', '\r']).unwrap_or(rem.len());
        if end > 0 {
            let text = rem[..end].to_string();
            self.advance(end);
            self.emit_text(text);
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

            // Try to parse a table attribute. A bare word is not an attribute
            // (it is content), so stop short and let the caller parse it.
            if let Some(attr) = self.parse_table_attribute() {
                attrs.push(attr);
            } else {
                break;
            }
        }
        attrs
    }

    /// Parse a single table attribute (`name=value`). Bare words are not table
    /// attributes; they are cell/table content and are left unconsumed.
    fn parse_table_attribute(&mut self) -> Option<KV> {
        let name_start = self.pos;
        let name = self.parse_table_attribute_name()?;
        let name_end = self.pos;

        self.consume_spaces();

        if self.starts_with("=") {
            self.advance(1);
            let val = self.parse_table_att_value();
            Some(KV {
                key: name,
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
            // Not a `name=value` attribute — backtrack so the caller can treat
            // the word as content.
            self.pos = name_start;
            None
        }
    }

    fn parse_table_attribute_name(&mut self) -> Option<KeyValue> {
        let name = self.parse_attr_name_impl(true);
        if name.is_empty() { None } else { Some(name) }
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
        if self.try_extension_tag() {
            return true;
        }
        if self.try_html_tag() {
            return true;
        }
        false
    }

    /// Try an extension tag (`<nowiki>`, `<pre>`, `<ref>`, ...) — a tag whose
    /// name appears in the extension tag map. Faithful port of the
    /// `maybe_extension_tag` grammar rule: the tag (including its content and
    /// end tag, if matched) is collapsed into a single `SelfclosingTagTk` named
    /// `extension`, carrying `typeof=...`, `name`, `source`, and `options`.
    ///
    /// Only `nowiki` and `pre` are currently expanded end-to-end (in
    /// `extension_handler`); other extension tags fall through to the HTML path
    /// until their handlers are implemented.
    fn try_extension_tag(&mut self) -> bool {
        if self.starts_with("</") || !self.starts_with("<") {
            return false;
        }

        let saved = self.pos;
        self.advance(1);
        let name = self.parse_tag_name();
        if name.is_empty() {
            self.pos = saved;
            return false;
        }
        let lc_name = name.to_lowercase();
        if !matches!(lc_name.as_str(), "nowiki" | "pre") || !self.ext_tags.contains(&lc_name) {
            self.pos = saved;
            return false;
        }

        // Parse the start-tag attributes (the `pre` extension sanitizes them).
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

        // Locate the matching end tag (`</name ...>`), if any, in the remaining
        // input. The whole `... </name>` region becomes the extension's `source`.
        let end_tag_re = format!("</{}", lc_name);
        let mut end_offset: Option<(usize, usize)> = None;
        if !self_closing
            && let Some(rel_end) = self.remaining().find(&end_tag_re)
            && let Some(gt) = self.remaining()[rel_end..].find('>')
        {
            // Absolute end offset, and the width of the closing tag (`</name>`).
            end_offset = Some((self.pos + rel_end + gt + 1, gt + 1));
        }

        let mut dp = self.make_dp(saved, self.pos);

        if self_closing {
            // `<name .../>` — no content.
            dp.src = Some(self.input[saved..self.pos].to_string());
        } else if let Some((end, close_width)) = end_offset {
            // Tag with a matched end tag: content spans the whole region.
            let ext_src = self.input[saved..end].to_string();
            dp.src = Some(ext_src);
            // extTagOffsets covers start..end with the end-tag width.
            dp.ext_tag_offsets = Some(DomSourceRange {
                start: saved,
                end,
                open_width: self.pos - saved,
                close_width,
            });
            self.pos = end;
        } else {
            // Unmatched start tag (no end tag) or self-closed: the sanitizer falls
            // back to the HTML equivalent. Emit the opening tag's source only.
            dp.src = Some(self.input[saved..self.pos].to_string());
        }

        let mut stt = SelfclosingTagTk::new("extension", vec![], dp.clone());
        stt.add_attribute_str("typeof", "mw:Extension");
        stt.add_attribute_str("name", &lc_name);
        let source = dp.src.clone().unwrap_or_default();
        stt.add_attribute_str("source", &source);
        // Store the parsed start-tag attributes as rich `data-mw` attribs so the
        // `pre` extension handler can sanitize them faithfully (mirrors PHP's
        // `maybe_extension_tag`, which stores `$t->attribs` as the `options` KV).
        stt.data_mw = Some(extension_data_mw(&attrs));
        self.emit_token(ParsoidToken::SelfclosingTag(stt));
        true
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
            // The tokenizer grammar applies `WTUtils::encodeComment` to the raw
            // comment body before emitting the token (see the `comment` rule).
            let encoded = encode_comment(&comment_text);
            self.emit_token(ParsoidToken::Comment(CommentTk::new(encoded, dp)));
            return true;
        }

        // Unclosed comment.
        let comment_text = self.input[self.pos..].to_string();
        self.pos = self.input_len;

        let mut dp = self.make_dp(start, self.pos);
        dp.unclosed_comment = Some(true);
        let encoded = encode_comment(&comment_text);
        self.emit_token(ParsoidToken::Comment(CommentTk::new(encoded, dp)));
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
        while let Some(attr) = self.parse_one_attribute() {
            attrs.push(attr);
        }
        attrs
    }

    /// Parse `generic_newline_attributes` (used by AttributeExpander's
    /// reparse-KV-string path to retokenize an expanded `k=v` string).
    fn parse_generic_newline_attributes(&mut self) -> Vec<KV> {
        let mut attrs = Vec::new();
        while let Some(attr) = self.parse_one_attribute() {
            attrs.push(attr);
        }
        attrs
    }

    /// Parse a single `name[=value]` attribute. Returns `None` at end-of-input
    /// or when the next char terminates the attribute list (`/` or `>`).
    fn parse_one_attribute(&mut self) -> Option<KV> {
        self.consume_spaces();
        if self.pos >= self.input_len {
            return None;
        }
        let ch = self.remaining().chars().next().unwrap();
        if ch == '/' || ch == '>' {
            return None;
        }

        // Parse attribute name.
        let name_start = self.pos;
        let name = self.parse_attr_name();
        let name_end = self.pos;
        if name.is_empty() {
            return None;
        }

        self.consume_spaces();

        if self.starts_with("=") {
            self.advance(1);
            self.consume_spaces();
            let val = self.parse_attr_value();
            Some(KV {
                key: name,
                value: KeyValue::Str(val),
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
                key: name,
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

    /// Parse a generic HTML tag attribute name, recognizing template
    /// directives. Mirrors `generic_attribute_name`.
    fn parse_attr_name(&mut self) -> KeyValue {
        self.parse_attr_name_impl(false)
    }

    /// Parse an attribute name into a `KeyValue`. A name that contains a
    /// template directive (`{{...}}` / `{{{...}}}`) becomes a token array;
    /// otherwise it is a plain string. Mirrors `generic_attribute_name` and
    /// (for `table`) the directive portion of `table_attribute_name`.
    fn parse_attr_name_impl(&mut self, table: bool) -> KeyValue {
        let mut tokens: Vec<Item> = Vec::new();
        let mut buf = String::new();

        loop {
            if self.starts_with("{{")
                && let Some(tok) = self.parse_directive()
            {
                if !buf.is_empty() {
                    tokens.push(Item::Str(std::mem::take(&mut buf)));
                }
                tokens.push(Item::Tok(ParsoidToken::SelfclosingTag(tok)));
                continue;
            }
            // Unparseable `{{` falls through to single-char handling.

            let Some(ch) = self.peek_char() else {
                break;
            };
            let is_stop = ch == ' '
                || ch == '\t'
                || ch == '\r'
                || ch == '\n'
                || ch == '\0'
                || ch == '/'
                || ch == '='
                || ch == '>'
                || (table && (ch == '|' || ch == '!'));
            if is_stop {
                break;
            }
            self.advance(ch.len_utf8());
            buf.push(ch);
        }

        if tokens.is_empty() {
            KeyValue::Str(buf)
        } else {
            if !buf.is_empty() {
                tokens.push(Item::Str(buf));
            }
            KeyValue::Tokens(tokens)
        }
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

    /// Try template argument or template: `{{{ ... }}}` or `{{ ... }}`.
    fn try_tplarg_or_template(&mut self) -> bool {
        let Some(tok) = self.parse_directive() else {
            return false;
        };
        self.emit_token(ParsoidToken::SelfclosingTag(tok));
        true
    }

    /// Parse a `directive` (a template argument or template) without emitting
    /// it, returning the token. Mirrors the `directive` grammar rule, which is
    /// shared by inline text and attribute-name positions.
    fn parse_directive(&mut self) -> Option<SelfclosingTagTk> {
        if self.starts_with("{{{") {
            self.parse_templatearg_token()
        } else {
            self.parse_template_token()
        }
    }

    /// Parse a `templatearg` token (`{{{ ... }}}`) without emitting it.
    fn parse_templatearg_token(&mut self) -> Option<SelfclosingTagTk> {
        if !self.starts_with("{{{") {
            return None;
        }

        let saved = self.pos;
        self.advance(3);

        // Find the closing `}}}` (respecting brace nesting).
        let Some(end) = self.find_closing('}', 3) else {
            self.pos = saved;
            return None;
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
        Some(stt)
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

    /// Parse a `template` token (`{{ ... }}`) without emitting it.
    fn parse_template_token(&mut self) -> Option<SelfclosingTagTk> {
        if !self.starts_with("{{") || self.starts_with("{{{") {
            return None;
        }

        let saved = self.pos;
        self.advance(2);

        let Some(end) = self.find_closing('}', 2) else {
            self.pos = saved;
            return None;
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

        Some(stt)
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

        // Find closing `]]` (skipping `<nowiki>` blocks).
        if let Some(end) = find_wikilink_close(self.remaining()) {
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
            // Canonical behavior-switch form is the lowercase inner text.
            let word = self.input[start + 2..start + 2 + end].to_lowercase();
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

        // If we are at the start of a URL, emit a `urllink` token.
        for prefix in prefixes {
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

        // Otherwise, match a run of plain text until a special character or the
        // start of a URL protocol (so the URL can be tokenized on the next
        // iteration rather than being split at its `:`).
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

        // Stop before the earliest URL protocol, if it starts before `end`.
        let url_start = prefixes.iter().filter_map(|p| rem.find(p)).min();
        let end = url_start.map_or(end, |u| end.min(u));

        // Stop before an enabled magic-link prefix (RFC/PMID/ISBN) so the
        // inline loop can attempt `try_magic_link` at that position.
        let magic_prefixes: [Option<&str>; 3] = [
            if self.magic_links.rfc {
                Some("RFC")
            } else {
                None
            },
            if self.magic_links.pmid {
                Some("PMID")
            } else {
                None
            },
            if self.magic_links.isbn {
                Some("ISBN")
            } else {
                None
            },
        ];
        let magic_start = magic_prefixes
            .iter()
            .filter_map(|p| p.and_then(|p| rem.find(p)))
            .min();
        let end = magic_start.map_or(end, |m| end.min(m));

        if end > 0 {
            let text = rem[..end].to_string();
            self.advance(end);
            self.emit_text(text);
            return true;
        }

        false
    }

    /// Try a magic link (`RFC`, `PMID`, or `ISBN`) at the current position.
    /// Faithful port of Parsoid's `autolink` / `autoref` / `isbn` grammar
    /// productions. Returns an `extlink` self-closing token carrying a
    /// `typeof` attribute that `ExternalLinkHandler::onExtLink` recognizes.
    fn try_magic_link(&mut self) -> bool {
        // The `!isUniWord(lastUniChar)` guard: don't autolink when the
        // preceding character is a word character.
        if self.pos > 0
            && let Some(prev) = self.input[..self.pos].chars().next_back()
            && is_word_char(prev)
        {
            return false;
        }

        if self.magic_links.rfc && self.try_autoref("RFC") {
            return true;
        }
        if self.magic_links.pmid && self.try_autoref("PMID") {
            return true;
        }
        if self.magic_links.isbn && self.try_isbn() {
            return true;
        }
        false
    }

    /// Try `RFC 1234` / `PMID 5678` auto-ref magic links.
    fn try_autoref(&mut self, ref_name: &str) -> bool {
        let rem = self.remaining();
        if !rem.starts_with(ref_name) {
            return false;
        }
        let start = self.pos;
        let after_ref = ref_name.len();

        // space_or_nbsp+
        let mut pos = after_ref;
        while let Some(ch) = rem[pos..].chars().next() {
            if !is_space_or_nbsp(ch) {
                break;
            }
            pos += ch.len_utf8();
        }
        if pos == after_ref {
            return false; // need at least one space
        }
        let sp = &rem[after_ref..pos];

        // [0-9]+
        let digits_start = pos;
        while let Some(ch) = rem[pos..].chars().next() {
            if !ch.is_ascii_digit() {
                break;
            }
            pos += ch.len_utf8();
        }
        if pos == digits_start {
            return false;
        }
        let identifier = &rem[digits_start..pos];

        // end_of_word: eof or the next char is not [A-Za-z0-9_].
        if let Some(ch) = rem[pos..].chars().next()
            && (ch.is_ascii_alphanumeric() || ch == '_')
        {
            return false;
        }

        let base_url = if ref_name == "RFC" {
            "https://datatracker.ietf.org/doc/html/rfc"
        } else {
            "//www.ncbi.nlm.nih.gov/pubmed/"
        };
        let suffix = if ref_name == "RFC" {
            String::new()
        } else {
            "?dopt=Abstract".to_string()
        };
        let href = format!("{base_url}{identifier}{suffix}");
        let content = format!("{ref_name}{sp}{identifier}");
        let tsr_end = self.pos + pos;

        let mut dp = self.make_dp(start, tsr_end);
        dp.stx = Some("magiclink".to_string());

        let mut stt = SelfclosingTagTk::new("extlink", vec![], dp);
        stt.add_attribute_str("href", &href);
        stt.add_attribute_str("mw:content", &content);
        stt.add_attribute_str("typeof", format!("mw:ExtLink/{ref_name}"));

        self.advance(pos);
        self.emit_token(ParsoidToken::SelfclosingTag(stt));
        true
    }

    /// Try an `ISBN 978-...` magic link. The ISBN is validated to be 10
    /// digits (with an optional X check digit) or 13 digits beginning with
    /// 978/979, per the PHP `isbn` production.
    fn try_isbn(&mut self) -> bool {
        let rem = self.remaining();
        if !rem.starts_with("ISBN") {
            return false;
        }
        let start = self.pos;
        let after_prefix = "ISBN".len();

        // space_or_nbsp+
        let mut pos = after_prefix;
        while let Some(ch) = rem[pos..].chars().next() {
            if !is_space_or_nbsp(ch) {
                break;
            }
            pos += ch.len_utf8();
        }
        if pos == after_prefix {
            return false;
        }
        let sp = &rem[after_prefix..pos];

        // The ISBN body: a leading digit...
        let body_start = pos;
        let mut code = String::new();
        let mut cursor = pos;

        // First digit.
        let Some(first_ch) = rem[cursor..].chars().next() else {
            return false;
        };
        if !first_ch.is_ascii_digit() {
            return false;
        }
        code.push(first_ch);
        cursor += first_ch.len_utf8();

        // `(separator? [0-9])+`
        loop {
            // Consume optional separators (`-`, space, nbsp), but a digit is
            // required to continue; roll back separators if no digit follows.
            let sep_start = cursor;
            while let Some(ch) = rem[cursor..].chars().next() {
                if ch == '-' || is_space_or_nbsp(ch) {
                    cursor += ch.len_utf8();
                } else {
                    break;
                }
            }
            match rem[cursor..].chars().next() {
                Some(ch) if ch.is_ascii_digit() => {
                    code.push(ch);
                    cursor += ch.len_utf8();
                }
                _ => {
                    cursor = sep_start;
                    break;
                }
            }
        }

        // Optional trailing `(separator? [xX])`.
        {
            let sep_start = cursor;
            while let Some(ch) = rem[cursor..].chars().next() {
                if ch == '-' || is_space_or_nbsp(ch) {
                    cursor += ch.len_utf8();
                } else {
                    break;
                }
            }
            match rem[cursor..].chars().next() {
                Some(ch) if ch == 'x' || ch == 'X' => {
                    code.push('X');
                    cursor += ch.len_utf8();
                }
                _ => {
                    cursor = sep_start;
                }
            }
        }

        // end_of_word guard.
        if let Some(ch) = rem[cursor..].chars().next()
            && (ch.is_ascii_alphanumeric() || ch == '_')
        {
            return false;
        }

        // The raw ISBN source (including separators and x/X).
        let raw = &rem[body_start..cursor];

        // Validate: 10 chars, or 13 chars beginning with 978/979.
        let valid = code.len() == 10
            || (code.len() == 13 && (code.starts_with("978") || code.starts_with("979")));
        if !valid {
            return false;
        }

        let end = self.pos + cursor;
        let mut dp = self.make_dp(start, end);
        dp.stx = Some("magiclink".to_string());

        let content = format!("ISBN{sp}{raw}");
        let href = format!("Special:BookSources/{code}");

        let mut stt = SelfclosingTagTk::new("extlink", vec![], dp);
        stt.add_attribute_str("href", &href);
        stt.add_attribute_str("mw:content", &content);
        stt.add_attribute_str("typeof", "mw:WikiLink/ISBN");

        self.advance(cursor);
        self.emit_token(ParsoidToken::SelfclosingTag(stt));
        true
    }

    /// Try HTML entity: `&amp;`, `&#123;`, etc.
    ///
    /// Faithful port of Parsoid's `raw_htmlentity` and `htmlentity` grammar
    /// productions. A valid entity (one that decodes to one or two codepoints)
    /// is wrapped in an `mw:Entity` span carrying the raw source and decoded
    /// content; anything else (e.g. an unknown named entity) is emitted as
    /// plain text and left unwrapped.
    fn try_html_entity(&mut self) -> bool {
        if !self.starts_with("&") {
            return false;
        }

        let start = self.pos;
        let rem = self.remaining();

        // raw_htmlentity matches `&` + charset + `;`.
        // The charset is `[#0-9a-zA-Zרלמרלמ]` (alphanumerics plus the
        // Hebrew/Arabic legacy alias characters).
        let body_len = match rem[1..].find(';') {
            Some(n) if n >= 1 => n,
            _ => return false,
        };
        let body = &rem[1..1 + body_len];
        if !body.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '#' || matches!(c, 'ר' | 'ל' | 'מ' | 'ر' | 'ل' | 'م')
        }) {
            return false;
        }

        // `entity` includes the leading `&` and trailing `;`. The body starts
        // at byte 1, so the terminator sits at byte `1 + body_len`.
        let semi_idx = 1 + body_len;
        let entity = &rem[..=semi_idx];
        let total_len = semi_idx + 1;
        let decoded = decode_wt_entities(entity);

        // A successful decode yields one or two codepoints. Anything longer
        // means the reference was not a valid entity, in which case the PHP
        // grammar returns the raw text unchanged (and without wrapping).
        if decoded.chars().count() > 2 {
            self.advance(total_len);
            self.emit_text(decoded);
            return true;
        }

        let end = self.pos + total_len;
        self.advance(total_len);

        // Start tag: tsr is the zero-width position at `start`; `src` is the
        // raw entity and `srcContent` is the decoded character(s).
        let mut start_dp = self.make_dp(start, start);
        start_dp.src = Some(entity.to_string());
        start_dp.src_content = Some(decoded.clone());
        let mut span = TagTk::new("span", vec![], start_dp);
        span.add_attribute_str("typeof", "mw:Entity");

        // End tag: tsr is the zero-width position at `end`.
        let end_dp = self.make_dp(end, end);

        self.emit_token(ParsoidToken::Tag(span));
        self.emit_text(decoded);
        self.emit_token(ParsoidToken::EndTag(EndTagTk::new("span", vec![], end_dp)));
        true
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

/// Returns true if the codepoint is valid in HTML5 and XML (mirrors PHP's
/// `Sanitizer::validateCodepoint`).
fn validate_codepoint(cp: u32) -> bool {
    cp == 0x09
        || cp == 0x0a
        || (0x20..=0x7e).contains(&cp)
        || (0xa0..=0xd7ff).contains(&cp)
        || (0xe000..=0xfffd).contains(&cp)
        || (0x10000..=0x10ffff).contains(&cp)
}

/// Whether `c` is a word character for the auto-link boundary check (mirrors
/// PHP `Utils::isUniWord`'s `\w` test).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `c` is a non-newline space or non-breaking space (mirrors Parsoid's
/// `space_or_nbsp` grammar rule, minus entity-encoded `&nbsp;`).
fn is_space_or_nbsp(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\u{00a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Find the byte offset of the first `]]` that is not inside a `<nowiki>` element.
///
/// The PHP wikilink/redirect grammar parses `<nowiki>` as a directive, so a `]]`
/// inside `<nowiki>…</nowiki>` does *not* terminate the enclosing `[[…]]`.
fn find_wikilink_close(input: &str) -> Option<usize> {
    let lower = input.to_ascii_lowercase();
    let mut i = 0;
    while i + 1 < input.len() {
        if lower[i..].starts_with("<nowiki") {
            let rest = &input[i + 7..];
            // A real `<nowiki>` tag is followed by `>`, `/`, or whitespace.
            if rest.starts_with('>')
                || rest.starts_with('/')
                || rest.starts_with(|c: char| c.is_whitespace())
            {
                if let Some(close_rel) = lower[i + 7..].find("</nowiki") {
                    let close_start = i + 7 + close_rel;
                    let after_close = &input[close_start + 8..];
                    let gt = after_close
                        .find('>')
                        .map(|g| g + 1)
                        .unwrap_or(after_close.len());
                    i = close_start + 8 + gt;
                    continue;
                }
                // Unclosed `<nowiki>`: the rest is literal, no `]]` terminator.
                return None;
            }
        }
        if input[i..].starts_with("]]") {
            return Some(i);
        }
        let ch_len = input[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        i += ch_len;
    }
    None
}

/// Decode a single wikitext HTML entity reference (mirrors PHP's
/// `Utils::decodeWtEntities`, which is `decodeCharReferences` after
/// `normalizeCharReferences`).
///
/// The input includes the leading `&` and trailing `;`. A reference that does
/// not denote a valid character is returned unchanged, matching the PHP
/// semantic of leaving invalid/unknown references as literal text.
pub(crate) fn decode_wt_entities(entity: &str) -> String {
    // Numeric char references: `&#123;`, `&#x1F;`, `&#X1F;`.
    if let Some(rest) = entity.strip_prefix("&#") {
        let digits = match rest.strip_suffix(';') {
            Some(d) => d,
            None => return entity.to_string(),
        };
        let (radix, digits) = if let Some(hex) = digits
            .strip_prefix('x')
            .or_else(|| digits.strip_prefix('X'))
        {
            (16, hex)
        } else {
            (10, digits)
        };
        // An empty digit string is not a valid reference.
        if digits.is_empty() {
            return entity.to_string();
        }
        let cp = match u32::from_str_radix(digits, radix) {
            Ok(cp) => cp,
            Err(_) => return entity.to_string(),
        };
        if validate_codepoint(cp)
            && let Some(c) = char::from_u32(cp)
        {
            return c.to_string();
        }
        return entity.to_string();
    }

    // Named entity references. MediaWiki accepts two non-standard aliases
    // (Hebrew/Arabic forms of the right-to-left mark).
    let name = match entity.strip_prefix('&').and_then(|s| s.strip_suffix(';')) {
        Some(name) => name,
        None => return entity.to_string(),
    };
    let canonical = match name {
        "רלמ" | "رلم" => "rlm",
        other => other,
    };

    // The HTML5 table is keyed by the semicolon-terminated name.
    let key = format!("{canonical};");
    match crate::html5::html_data::named_entity_translation(&key) {
        Some(decoded) => decoded.to_string(),
        None => entity.to_string(),
    }
}

/// Map a wikitext-escaped comment to an HTML DOM-escaped comment. Faithful port
/// of `WTUtils::encodeComment`: undo the `--&(amp;)*gt;` wikitext escaping to
/// obtain the "true value", then entity-encode every `-`, `>` and `&` so the
/// result is safe to embed in an HTML comment.
fn encode_comment(comment: &str) -> String {
    let true_value = unescape_comment(comment);
    let mut out = String::with_capacity(true_value.len());
    for c in true_value.chars() {
        match c {
            '-' => out.push_str("&#x2D;"),
            '>' => out.push_str("&#x3E;"),
            '&' => out.push_str("&#x26;"),
            c => out.push(c),
        }
    }
    out
}

/// Undo the wikitext `--&(amp;)*gt;` escaping (the `preg_replace_callback` of
/// `encodeComment`), a single left-to-right pass without re-scanning output.
fn unescape_comment(comment: &str) -> String {
    let mut out = String::with_capacity(comment.len());
    let mut rest = comment;
    while let Some(idx) = rest.find("--&") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 3..];
        // Greedily consume `amp;` repetitions.
        let mut n = 0;
        let mut rem = after;
        while let Some(stripped) = rem.strip_prefix("amp;") {
            n += 1;
            rem = stripped;
        }
        if let Some(rem) = rem.strip_prefix("gt;") {
            // `--&(amp;)*gt;` decodes to `--` + `&` + `amp;`*(n-1) + `gt;` for
            // n >= 1, or `-->` for n == 0.
            if n == 0 {
                out.push_str("-->");
            } else {
                out.push_str("--&");
                for _ in 0..(n - 1) {
                    out.push_str("amp;");
                }
                out.push_str("gt;");
            }
            rest = rem;
        } else {
            out.push_str("--&");
            rest = after;
        }
    }
    out.push_str(rest);
    out
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

/// Retokenize an expanded `k=v` string as `generic_newline_attributes`,
/// returning the parsed KVs. Used by the AttributeExpander's reparse-KV-string
/// path. Mirrors `PegTokenizer::tokenizeAs( ..., 'generic_newline_attributes' )`.
pub fn tokenize_as_attributes(wikitext: &str) -> Vec<KV> {
    let options = TokenizerOptions::default();
    let mut tokenizer = PegTokenizer::new(wikitext, &options);
    tokenizer.parse_generic_newline_attributes()
}

/// Build a `DataMw` carrying the parsed start-tag attributes of an extension,
/// so the `pre` handler can sanitize them (mirrors PHP's `options` KV, which
/// holds the extension's `$t->attribs` array).
fn extension_data_mw(attrs: &[KV]) -> DataMw {
    let attribs = attrs
        .iter()
        .map(|kv| DataMwAttrib {
            key: DataMwValue::Str(kv.key.to_string()),
            value: DataMwValue::Str(kv.value.to_string()),
        })
        .collect();
    DataMw {
        parts: Vec::new(),
        attribs,
        src: None,
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
    fn test_html_tag_with_attrs() {
        // Regression: parse_tag_name/parse_attr_name must advance position.
        let tokens = tokenize("<div class=\"x\" style='y'>foo</div>");
        let div_opts: Vec<_> = tokens
            .iter()
            .filter(|t| matches!(t, Either::Right(ParsoidToken::Tag(tk)) if tk.name == "div"))
            .collect();
        assert_eq!(div_opts.len(), 1, "expected one <div>, got: {:?}", tokens);
        if let Either::Right(ParsoidToken::Tag(tk)) = div_opts[0] {
            assert_eq!(tk.attribs.len(), 2);
            assert_eq!(tk.attribs[0].key.as_str(), Some("class"));
            assert_eq!(tk.attribs[0].value.as_str(), Some("x"));
            assert_eq!(tk.attribs[1].key.as_str(), Some("style"));
            assert_eq!(tk.attribs[1].value.as_str(), Some("y"));
        }
    }

    #[test]
    fn test_html_tag_templated_attribute() {
        // A directive in attribute-name position is parsed as a token array
        // (not a string that truncates at the first `=`).
        let tokens = tokenize("<div {{1x|1=style=\"color:red\"}}>hmm</div>");
        let div = tokens
            .iter()
            .find_map(|t| match t {
                Either::Right(ParsoidToken::Tag(tk)) if tk.name == "div" => Some(tk),
                _ => None,
            })
            .expect("expected <div> tag");

        assert_eq!(div.attribs.len(), 1, "one templated attribute expected");
        let kv = &div.attribs[0];
        assert!(
            matches!(kv.key, KeyValue::Tokens(_)),
            "key is a token array"
        );
        // The value is empty (no `=` follows the directive).
        assert_eq!(kv.value.as_str(), Some(""));
        if let KeyValue::Tokens(parts) = &kv.key {
            assert_eq!(parts.len(), 1);
            assert!(matches!(
                &parts[0],
                Item::Tok(ParsoidToken::SelfclosingTag(tk)) if tk.name == "template"
            ));
        }
    }

    #[test]
    fn test_table_start() {
        let tokens = tokenize("{|");
        let has_table = tokens
            .iter()
            .any(|t| matches!(t, Either::Right(ParsoidToken::Tag(tk)) if tk.name == "table"));
        assert!(has_table, "Expected table, got: {:?}", tokens);
    }

    #[test]
    fn test_localized_redirect_word() {
        // A redirect word must be matched against the configured localized
        // `redirect` magic word synonyms (here, Icelandic `#TILVÍSUN`).
        let options = TokenizerOptions {
            redirect_words: vec!["#redirect".to_string(), "#tilvísun".to_string()],
            ..TokenizerOptions::default()
        };
        let mut tokenizer = PegTokenizer::new("#TILVÍSUN [[Main Page]]", &options);
        let tokens = tokenizer.tokenize().unwrap();
        let has_redirect = tokens.iter().any(|t| {
            matches!(t, Either::Right(ParsoidToken::SelfclosingTag(tk)) if tk.name == "mw:redirect")
        });
        assert!(
            has_redirect,
            "expected mw:redirect for #TILVÍSUN, got {:?}",
            tokens
        );
    }

    #[test]
    fn test_comment_on_own_line_becomes_empty_line_tk() {
        // "asdf\n<!-- c -->\njkl" — the comment+newline must be wrapped in an
        // EmptyLineTk (not a top-level NlTk), so the ParagraphWrapper keeps
        // the two text lines in a single paragraph.
        let options = TokenizerOptions::default();
        let mut tokenizer = PegTokenizer::new("asdf\n<!-- c -->\njkl", &options);
        let tokens = tokenizer.tokenize().unwrap();

        let has_empty_line = tokens
            .iter()
            .any(|t| matches!(t, Either::Right(ParsoidToken::EmptyLine(_))));
        assert!(has_empty_line, "expected an EmptyLineTk, got {:?}", tokens);

        // There must be no additional top-level Nl token after the comment line.
        // (The first text line's newline IS a top-level Nl; the comment's is nested.)
        let top_level_nl_count = tokens
            .iter()
            .filter(|t| matches!(t, Either::Right(ParsoidToken::Nl(_))))
            .count();
        assert_eq!(top_level_nl_count, 1, "got {:?}", tokens);
    }

    #[test]
    fn test_encode_comment() {
        // Plain comment passes through unchanged.
        assert_eq!(encode_comment(" foo "), " foo ");
        // `-`, `>`, `&` are entity-encoded.
        assert_eq!(encode_comment("a-b>c&d"), "a&#x2D;b&#x3E;c&#x26;d");
        // The wikitext `--&gt;` escape round-trips to `-->` before encoding.
        assert_eq!(encode_comment("--&gt;"), "&#x2D;&#x2D;&#x3E;");
        assert_eq!(encode_comment("--&amp;gt;"), "&#x2D;&#x2D;&#x26;gt;");
    }

    #[test]
    fn test_unescape_comment() {
        assert_eq!(unescape_comment("--&gt;"), "-->");
        assert_eq!(unescape_comment("--&amp;gt;"), "--&gt;");
        assert_eq!(unescape_comment("--&amp;amp;gt;"), "--&amp;gt;");
        assert_eq!(unescape_comment("no escapes"), "no escapes");
    }

    #[test]
    fn test_find_wikilink_close() {
        // Plain target: the first `]]` terminates.
        assert_eq!(find_wikilink_close("Foo]]"), Some(3));
        // A `]]` inside `<nowiki>` is skipped.
        assert_eq!(find_wikilink_close("<nowiki>[[Bar]]</nowiki>]]"), Some(24));
        // No closing brackets: None.
        assert_eq!(find_wikilink_close("Foo"), None);
    }
}
