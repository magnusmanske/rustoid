//! Wikitext tokenizer.
//!
//! Scans raw wikitext and produces a stream of [`WikitextToken`]s.
//!
//! This is a single-pass tokenizer driven by pattern matching at the current
//! position. Each `try_*` method looks at the remaining input and, if it
//! matches, returns the token AND a byte-length to advance. Text between tokens
//! is accumulated and emitted as `Text`.

use crate::error::Result;
use crate::wikitext::tokens::WikitextToken;

/// Configuration for the tokenizer.
#[derive(Debug, Clone)]
pub struct TokenizerOptions {
    /// Whether to process annotation tags (`<dummyanno>`, etc.).
    pub annotations: bool,
    /// Whether to process extension tags (e.g. `<ref>`, `<gallery>`).
    pub extensions: bool,
}

impl Default for TokenizerOptions {
    fn default() -> Self {
        Self {
            annotations: false,
            extensions: true,
        }
    }
}

/// The wikitext tokenizer.
pub struct Tokenizer<'a> {
    input: &'a str,
    pos: usize,
    _options: TokenizerOptions,
    /// Byte position of the start of the current plain text run (or None).
    text_start: usize,
    /// Accumulated tokens.
    tokens: Vec<WikitextToken>,
    /// Whether we're at the start of a line.
    at_line_start: bool,
    /// Whether we're inside a `<pre>` block (verbatim mode).
    in_pre: bool,
}

impl<'a> Tokenizer<'a> {
    /// Create a new tokenizer for the given wikitext input.
    pub fn new(input: &'a str, options: TokenizerOptions) -> Self {
        Self {
            input,
            pos: 0,
            _options: options,
            tokens: Vec::new(),
            text_start: 0,
            at_line_start: true,
            in_pre: false,
        }
    }

    /// Tokenize the entire input and return the token stream.
    pub fn tokenize(&mut self) -> Result<Vec<WikitextToken>> {
        while self.pos < self.input.len() {
            let remaining = &self.input[self.pos..];

            // Pre mode: everything is literal text until </pre>
            // This is checked FIRST, before at_line_start, since pre can start mid-line
            if self.in_pre {
                if let Some(end) = remaining.find("</pre>") {
                    // Emit text up to the closing tag
                    if end > 0 {
                        let text = &remaining[..end];
                        let p = self.pos;
                        self.pos += end;
                        self.emit_at(WikitextToken::Text(text.to_string()), p);
                    }
                    // Emit the closing tag
                    let p = self.pos;
                    self.pos += 6; // "</pre>"
                    self.emit_at(WikitextToken::HtmlTagClose("pre".to_string()), p);
                    self.in_pre = false;
                    continue;
                } else {
                    // No closing tag — emit rest as text and end
                    if !remaining.is_empty() {
                        let p = self.pos;
                        self.pos = self.input.len();
                        self.emit_at(WikitextToken::Text(remaining.to_string()), p);
                    }
                    continue;
                }
            }

            if self.at_line_start {
                // Block-level constructs
                let p = self.pos;
                if let Some(token) = self.try_heading(remaining) {
                    self.emit_at(token, p);
                    continue;
                }
                let p = self.pos;
                if let Some(token) = self.try_hr(remaining) {
                    self.emit_at(token, p);
                    continue;
                }
                let p = self.pos;
                if let Some(token) = self.try_redirect(remaining) {
                    self.emit_at(token, p);
                    continue;
                }
                let p = self.pos;
                if let Some(token) = self.try_list(remaining) {
                    self.emit_at(token, p);
                    continue;
                }
                let p = self.pos;
                if let Some(token) = self.try_table(remaining) {
                    self.emit_at(token, p);
                    continue;
                }
            }

            // Inline constructs
            let p = self.pos;
            if let Some(token) = self.try_nowiki(remaining) {
                self.emit_at(token, p);
                continue;
            }
            let p = self.pos;
            if let Some(token) = self.try_comment(remaining) {
                self.emit_at(token, p);
                continue;
            }
            let p = self.pos;
            if let Some(token) = self.try_template_or_arg(remaining) {
                self.emit_at(token, p);
                continue;
            }
            let p = self.pos;
            if let Some(token) = self.try_wikilink_open(remaining) {
                self.emit_at(token, p);
                continue;
            }
            if remaining.starts_with("]]") {
                let p = self.pos;
                self.advance(2);
                self.emit_at(WikitextToken::WikilinkClose, p);
                continue;
            }
            if remaining.starts_with('|') {
                let p = self.pos;
                self.advance(1);
                self.emit_at(WikitextToken::WikilinkPipe, p);
                continue;
            }
            if remaining.starts_with(']') && !remaining.starts_with("]]") {
                let p = self.pos;
                self.advance(1);
                self.emit_at(WikitextToken::ExtLinkClose, p);
                continue;
            }
            let p = self.pos;
            if let Some(token) = self.try_extlink(remaining) {
                self.emit_at(token, p);
                continue;
            }
            let p = self.pos;
            if let Some(token) = self.try_bold_italic(remaining) {
                self.emit_at(token, p);
                continue;
            }
            let p = self.pos;
            if let Some(token) = self.try_html_tag(remaining) {
                // Enter pre mode when we see a <pre> tag (not self-closing)
                if matches!(&token, WikitextToken::HtmlTagOpen(name, _) if name == "pre") {
                    self.in_pre = true;
                }
                self.emit_at(token, p);
                continue;
            }
            let p = self.pos;
            if let Some(token) = self.try_magic_word(remaining) {
                self.emit_at(token, p);
                continue;
            }
            if let Some(stripped) = remaining.strip_prefix("\n\n") {
                let p = self.pos;
                let extra = stripped.chars().take_while(|&c| c == '\n').count();
                self.advance(2 + extra);
                self.emit_at(WikitextToken::ParagraphBreak, p);
                self.at_line_start = true;
                continue;
            }
            if remaining.starts_with('\n') {
                let p = self.pos;
                self.advance(1);
                self.emit_at(WikitextToken::Newline, p);
                self.at_line_start = true;
                continue;
            }

            // Not a token — accumulate as plain text if we haven't started
            if self.text_start == self.pos && !remaining.is_empty() {
                self.text_start = self.pos;
            }
            // Advance by one char (not byte) to handle multi-byte UTF-8
            if let Some(ch) = remaining.chars().next() {
                self.pos += ch.len_utf8();
            } else {
                break;
            }
        }

        // Flush remaining text
        self.flush_text();
        self.tokens.push(WikitextToken::EOF);

        Ok(std::mem::take(&mut self.tokens))
    }

    // ---- Internal helpers ----

    fn emit_at(&mut self, token: WikitextToken, token_start: usize) {
        // Flush text up to token_start
        if self.text_start < token_start {
            let text = self.input[self.text_start..token_start].to_string();
            self.tokens.push(WikitextToken::Text(text));
        }
        // Track pre-mode state
        if let WikitextToken::HtmlTagOpen(name, _) = &token {
            if name == "pre" {
                self.in_pre = true;
            }
        }
        self.tokens.push(token);
        self.text_start = self.pos;
    }

    fn flush_text(&mut self) {
        if self.text_start < self.pos && self.pos <= self.input.len() {
            let text = self.input[self.text_start..self.pos].to_string();
            self.tokens.push(WikitextToken::Text(text));
        }
        self.text_start = self.pos;
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    // ---- Token recognizers ----

    fn try_nowiki(&mut self, remaining: &str) -> Option<WikitextToken> {
        // In pre-mode, everything is literal; <nowiki> is just text
        if self.in_pre {
            return None;
        }
        if !remaining.starts_with("<nowiki") {
            return None;
        }
        if let Some(stripped) = remaining.strip_prefix("<nowiki") {
            if let Some(rest) = stripped
                .strip_prefix("/>")
                .or_else(|| stripped.strip_prefix(" />"))
            {
                self.advance("<nowiki".len() + stripped.len() - rest.len());
                return Some(WikitextToken::NowikiContent(String::new()));
            }
            if let Some(stripped2) = stripped.strip_prefix(">")
                && let Some(end) = stripped2.find("</nowiki>")
            {
                let content = stripped2[..end].to_string();
                self.advance("<nowiki>".len() + end + "</nowiki>".len());
                return Some(WikitextToken::NowikiContent(content));
            }
        }
        None
    }

    fn try_comment(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("<!--") {
            if let Some(end) = remaining.find("-->") {
                if end >= 4 {
                    let comment = remaining[4..end].to_string();
                    self.advance(end + 3);
                    return Some(WikitextToken::Comment(comment));
                }
                // "-->") found overlapping with "<!--" — unusual but handle
                // Just consume the whole thing as unclosed
            }
            // Unclosed comment — consume until end of input
            let comment = remaining[4..].to_string();
            self.advance(remaining.len());
            return Some(WikitextToken::Comment(comment));
        }
        None
    }

    fn try_template_or_arg(&mut self, remaining: &str) -> Option<WikitextToken> {
        if let Some(stripped) = remaining.strip_prefix("{{{") {
            let start = self.pos;
            let open_len = 3;
            let mut depth: usize = open_len;
            let mut end_pos = 0usize;
            for (i, ch) in stripped.char_indices() {
                match ch {
                    '{' => depth = depth.saturating_add(1),
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            end_pos = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if depth == 0 {
                let content_end = end_pos.saturating_sub(open_len);
                let inner = &stripped[..=content_end];
                let name = inner.split('|').next().unwrap_or("").trim().to_string();
                self.advance(open_len + content_end + 1 + open_len);
                return Some(WikitextToken::TplArgOpen(name));
            }
            self.pos = start;
            self.text_start = start;
        } else if let Some(stripped) = remaining.strip_prefix("{{") {
            let start = self.pos;
            let open_len = 2;
            let mut depth: usize = open_len;
            let mut end_pos = 0usize;
            for (i, ch) in stripped.char_indices() {
                match ch {
                    '{' => depth = depth.saturating_add(1),
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            end_pos = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if depth == 0 {
                let content_end = end_pos.saturating_sub(open_len);
                let inner = &stripped[..=content_end];
                let full_name = inner.split('|').next().unwrap_or("").trim().to_string();
                self.advance(open_len + content_end + 1 + open_len);
                if let Some(rest) = full_name.strip_prefix('#') {
                    // Parser function: name is up to the first colon
                    let func_name = rest.split(':').next().unwrap_or(rest);
                    return Some(WikitextToken::ParserFnOpen(format!("#{func_name}")));
                }
                return Some(WikitextToken::TemplateOpen(full_name));
            }
            self.pos = start;
            self.text_start = start;
        }
        None
    }

    fn try_heading(&mut self, remaining: &str) -> Option<WikitextToken> {
        if !remaining.starts_with('=') {
            return None;
        }
        let level = remaining.chars().take_while(|&c| c == '=').count();
        if (2..=6).contains(&level) {
            let after_eq = &remaining[level..];
            if after_eq.starts_with(' ') || after_eq.starts_with('\n') || after_eq.starts_with("==")
            {
                self.advance(level);
                return Some(WikitextToken::HeadingOpen(level as u8));
            }
        }
        None
    }

    fn try_hr(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("----") {
            self.advance(4);
            let rem = &self.input[self.pos..];
            let extra = rem.chars().take_while(|&c| c == '-').count();
            self.advance(extra);
            return Some(WikitextToken::Hr);
        }
        None
    }

    fn try_list(&mut self, remaining: &str) -> Option<WikitextToken> {
        let first = remaining.chars().next()?;
        if matches!(first, '*' | '#' | ';' | ':') {
            let depth = remaining.chars().take_while(|&c| c == first).count();
            self.advance(depth);
            return Some(WikitextToken::ListItem(first, depth as u8));
        }
        None
    }

    fn try_table(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("{|") {
            self.advance(2);
            let attrs = self.parse_html_attributes_until_newline();
            return Some(WikitextToken::TableOpen(attrs));
        }
        if remaining.starts_with("|}") {
            self.advance(2);
            return Some(WikitextToken::TableClose);
        }
        if remaining.starts_with("|-") {
            self.advance(2);
            return Some(WikitextToken::TableRow);
        }
        if remaining.starts_with("|+") {
            self.advance(2);
            return Some(WikitextToken::TableCaption);
        }
        if remaining.starts_with("!!") || remaining.starts_with("||") {
            self.advance(2);
            return Some(WikitextToken::TableCell);
        }
        if remaining.starts_with('|') || remaining.starts_with('!') {
            self.advance(1);
            return Some(WikitextToken::TableCell);
        }
        None
    }

    fn try_redirect(&mut self, remaining: &str) -> Option<WikitextToken> {
        let lower = remaining.to_lowercase();
        if (lower.starts_with("#redirect") || lower.starts_with("#redireccion"))
            && let Some(link_start) = remaining.find("[[")
            && let Some(link_end) = remaining[link_start..].find("]]")
        {
            let target = remaining[link_start + 2..link_start + link_end].to_string();
            self.advance(link_start + link_end + 2);
            return Some(WikitextToken::Redirect(target));
        }
        None
    }

    fn try_wikilink_open(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("[[") && !remaining.starts_with("[[[") {
            self.advance(2);
            return Some(WikitextToken::WikilinkOpen);
        }
        None
    }

    fn try_extlink(&mut self, remaining: &str) -> Option<WikitextToken> {
        if !remaining.starts_with('[') || remaining.starts_with("[[") {
            return None;
        }
        let rest = &remaining[1..];
        let has_protocol = rest.starts_with("http://")
            || rest.starts_with("https://")
            || rest.starts_with("ftp://")
            || rest.starts_with("mailto:")
            || rest.starts_with("//");
        if !has_protocol {
            return None;
        }
        self.advance(1); // skip [
        let rem = &self.input[self.pos..];
        if let Some(space_or_close) = rem.find([' ', ']']) {
            let url = rem[..space_or_close].to_string();
            let ch = rem.as_bytes()[space_or_close];
            self.advance(space_or_close);
            if ch == b']' {
                // No display text: `[url]` — just advance and let the ] handler close it
                // Don't advance past ] here — the main loop's ] handler will consume it
                return Some(WikitextToken::ExtLinkOpen(url));
            }
            // Has display text: skip space, return open
            self.advance(1); // skip space
            return Some(WikitextToken::ExtLinkOpen(url));
        }
        self.advance(rem.len());
        Some(WikitextToken::ExtLinkOpen(rem.to_string()))
    }

    fn try_bold_italic(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("'''''") && !remaining.starts_with("''''''") {
            self.advance(5);
            return Some(WikitextToken::Quote("'''''".to_string()));
        }
        // 6 quotes (''''''): return None so first ' becomes text, then ''''' matches.
        // This puts the apostrophe BEFORE the 5-quote tag, matching PHP parsoid behavior.
        if remaining.starts_with("''''''") {
            return None;
        }
        // 4 quotes (''''): return None so first ' becomes text, then ''' matches
        if remaining.starts_with("''''") {
            return None;
        }
        if remaining.starts_with("'''") {
            self.advance(3);
            return Some(WikitextToken::Quote("'''".to_string()));
        }
        if remaining.starts_with("''") {
            self.advance(2);
            return Some(WikitextToken::Quote("''".to_string()));
        }
        None
    }

    fn try_html_tag(&mut self, remaining: &str) -> Option<WikitextToken> {
        if !remaining.starts_with('<') {
            return None;
        }
        if remaining.starts_with("</") {
            if let Some(end) = remaining.find('>') {
                let name = remaining[2..end].trim().to_lowercase();
                self.advance(end + 1);
                return Some(WikitextToken::HtmlTagClose(name));
            }
            return None;
        }
        // Find tag end, skipping > inside quoted attribute values
        let tag_end = Self::find_tag_end(remaining);
        if let Some(end) = tag_end {
            let tag_content = &remaining[1..end];
            if tag_content.is_empty() || tag_content.starts_with('{') {
                return None;
            }
            let self_closing = tag_content.ends_with('/');
            let name = tag_content
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_lowercase();
            if name.is_empty() || !name.chars().next().unwrap().is_ascii_alphabetic() {
                return None;
            }
            self.advance(end + 1);
            // Parse attributes from the tag content
            let attrs_str = tag_content[name.len()..].trim();
            let attrs = if attrs_str.is_empty() {
                Vec::new()
            } else {
                self.parse_attributes_from_str(attrs_str)
            };
            if self_closing {
                return Some(WikitextToken::SelfClosingTag(name, attrs));
            }
            return Some(WikitextToken::HtmlTagOpen(name, attrs));
        }
        None
    }

    fn try_magic_word(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("__")
            && let Some(end) = remaining[2..].find("__")
        {
            let word = remaining[..end + 4].to_string();
            self.advance(end + 4);
            return Some(WikitextToken::MagicWord(word));
        }
        None
    }

    /// Find the closing `>` of an HTML tag, skipping `>` inside quoted attribute values.
    fn find_tag_end(s: &str) -> Option<usize> {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' | b'\'' => {
                    let quote = bytes[i];
                    i += 1;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                b'>' => return Some(i),
                _ => i += 1,
            }
        }
        None
    }

    fn parse_html_attributes_until_newline(&mut self) -> Vec<(String, String)> {
        let rem = &self.input[self.pos..];
        let end = rem.find('\n').unwrap_or(rem.len());
        self.parse_attributes_from_str(&rem[..end])
    }

    fn parse_attributes_from_str(&self, s: &str) -> Vec<(String, String)> {
        let mut attrs = Vec::new();
        let s = s.trim();
        let mut i = 0;
        let bytes = s.as_bytes();
        while i < s.len() {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let key_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
                i += 1;
            }
            let key = s[key_start..i].to_string();
            if i < bytes.len() && bytes[i] == b'=' {
                i += 1;
                if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                    let quote = bytes[i];
                    i += 1;
                    let val_start = i;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    let val = s[val_start..i].to_string();
                    if i < bytes.len() {
                        i += 1;
                    }
                    attrs.push((key, val));
                } else {
                    let val_start = i;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    let val = s[val_start..i].to_string();
                    attrs.push((key, val));
                }
            } else {
                attrs.push((key, String::new()));
            }
        }
        attrs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(s: &str) -> Vec<WikitextToken> {
        let mut tok = Tokenizer::new(s, TokenizerOptions::default());
        tok.tokenize().unwrap()
    }

    // -- Basic text --

    #[test]
    fn test_empty_input() {
        assert_eq!(tokenize(""), vec![WikitextToken::EOF]);
    }

    #[test]
    fn test_plain_text() {
        let tokens = tokenize("hello world");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], WikitextToken::Text("hello world".into()));
    }

    #[test]
    fn test_text_accumulation() {
        let tokens = tokenize("abcdef");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], WikitextToken::Text("abcdef".into()));
    }

    #[test]
    fn test_leading_text_before_token() {
        let tokens = tokenize("text {{template}}");
        assert_eq!(tokens[0], WikitextToken::Text("text ".into()));
        assert!(matches!(tokens[1], WikitextToken::TemplateOpen(_)));
    }

    // -- Bold / italic --

    #[test]
    fn test_bold() {
        let tokens = tokenize("'''bold'''");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::Quote(q) if q == "'''"))
        );
    }

    #[test]
    fn test_italic() {
        let tokens = tokenize("''italic''");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::Quote(q) if q == "''"))
        );
    }

    #[test]
    fn test_bold_italic() {
        let tokens = tokenize("'''''bold italic'''''");
        assert_eq!(
            tokens
                .iter()
                .filter(|t| matches!(t, WikitextToken::Quote(q) if q == "'''''"))
                .count(),
            2
        );
    }

    // -- Headings --

    #[test]
    fn test_heading_level_2() {
        let tokens = tokenize("== Heading ==");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::HeadingOpen(2)))
        );
    }

    // -- Lists --

    #[test]
    fn test_unordered_list() {
        let tokens = tokenize("* item");
        assert_eq!(tokens[0], WikitextToken::ListItem('*', 1));
    }

    #[test]
    fn test_nested_list() {
        let tokens = tokenize("** item");
        assert_eq!(tokens[0], WikitextToken::ListItem('*', 2));
    }

    // -- HR --

    #[test]
    fn test_hr() {
        let tokens = tokenize("----");
        assert!(tokens.iter().any(|t| matches!(t, WikitextToken::Hr)));
    }

    // -- Wikilinks --

    #[test]
    fn test_wikilink_simple() {
        let tokens = tokenize("[[Main Page]]");
        assert_eq!(tokens[0], WikitextToken::WikilinkOpen);
        assert_eq!(tokens[1], WikitextToken::Text("Main Page".into()));
        assert_eq!(tokens[2], WikitextToken::WikilinkClose);
    }

    // -- External links --

    #[test]
    fn test_extlink_with_text() {
        let tokens = tokenize("[https://example.com some text]");
        let has_extlink = tokens
            .iter()
            .any(|t| matches!(t, WikitextToken::ExtLinkOpen(u) if u == "https://example.com"));
        assert!(has_extlink);
    }

    #[test]
    fn test_extlink_no_text() {
        let tokens = tokenize("[https://example.com]");
        let has_extlink = tokens
            .iter()
            .any(|t| matches!(t, WikitextToken::ExtLinkOpen(u) if u == "https://example.com"));
        assert!(has_extlink);
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::ExtLinkClose))
        );
    }

    // -- Templates --

    #[test]
    fn test_template_simple() {
        let tokens = tokenize("{{Foo}}");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::TemplateOpen(n) if n == "Foo"))
        );
    }

    #[test]
    fn test_parser_function() {
        let tokens = tokenize("{{#if:true|yes|no}}");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::ParserFnOpen(n) if n == "#if"))
        );
    }

    #[test]
    fn test_template_arg() {
        let tokens = tokenize("{{{1}}}");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::TplArgOpen(n) if n == "1"))
        );
    }

    #[test]
    fn test_nested_templates() {
        let tokens = tokenize("{{A|{{B}}}}");
        let tpl_count = tokens
            .iter()
            .filter(|t| matches!(t, WikitextToken::TemplateOpen(_)))
            .count();
        assert_eq!(tpl_count, 1);
    }

    #[test]
    fn test_triple_brace_nested() {
        let tokens = tokenize("{{{a|{{{b}}}}}}");
        let arg_count = tokens
            .iter()
            .filter(|t| matches!(t, WikitextToken::TplArgOpen(_)))
            .count();
        assert_eq!(arg_count, 1);
    }

    // -- Comments --

    #[test]
    fn test_comment() {
        let tokens = tokenize("<!-- comment -->");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::Comment(c) if c == " comment "))
        );
    }

    #[test]
    fn test_multiple_comments() {
        let tokens = tokenize("<!-- --><!----><!--&#x2D;--><!--&#x2D;&#x2D;-->");
        let comments: Vec<&str> = tokens
            .iter()
            .filter_map(|t| {
                if let WikitextToken::Comment(c) = t {
                    Some(c.as_str())
                } else {
                    None
                }
            })
            .collect();
        println!("comments: {:?}", comments);
        assert_eq!(comments.len(), 4, "expected 4 comments");
        assert_eq!(comments[0], " ");
        assert_eq!(comments[1], "");
        assert_eq!(comments[2], "&#x2D;");
        assert_eq!(comments[3], "&#x2D;&#x2D;");
    }

    // -- Nowiki --

    #[test]
    fn test_nowiki() {
        let tokens = tokenize("<nowiki>'''not bold'''</nowiki>");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::NowikiContent(c) if c == "'''not bold'''"))
        );
    }

    // -- HTML tags --

    #[test]
    fn test_html_open_close() {
        let tokens = tokenize("<div>content</div>");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::HtmlTagOpen(n, _) if n == "div"))
        );
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::HtmlTagClose(n) if n == "div"))
        );
    }

    #[test]
    fn test_self_closing_tag() {
        let tokens = tokenize("<br/>");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::SelfClosingTag(n, _) if n == "br"))
        );
    }

    // -- Magic words --

    #[test]
    fn test_magic_word_toc() {
        let tokens = tokenize("__TOC__");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::MagicWord(w) if w == "__TOC__"))
        );
    }

    // -- Tables --

    #[test]
    fn test_table_open() {
        let tokens = tokenize("{| class=\"wikitable\"");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::TableOpen(_)))
        );
    }

    #[test]
    fn test_table_close() {
        let tokens = tokenize("|}");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::TableClose))
        );
    }

    // -- Redirects --

    #[test]
    fn test_redirect() {
        let tokens = tokenize("#REDIRECT [[Target Page]]");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::Redirect(target) if target == "Target Page"))
        );
    }

    // -- Newlines --

    #[test]
    fn test_paragraph_break() {
        let tokens = tokenize("para1\n\npara2");
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::ParagraphBreak))
        );
    }

    #[test]
    fn test_single_newline() {
        let tokens = tokenize("line1\nline2");
        assert!(tokens.iter().any(|t| matches!(t, WikitextToken::Newline)));
    }

    #[test]
    fn test_mixed_content() {
        let tokens = tokenize("'''bold''' and ''italic'' text [[link]].");
        assert!(tokens.len() > 3);
    }
}
