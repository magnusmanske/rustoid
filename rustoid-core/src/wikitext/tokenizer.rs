//! Wikitext tokenizer.
//!
//! Scans raw wikitext and produces a stream of [`WikitextToken`]s.
//!
//! This is a state-machine based tokenizer that handles the full wikitext
//! syntax. It is designed to be single-pass and allocation-friendly.

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
///
/// # Usage
///
/// ```rust,ignore
/// let mut tok = Tokenizer::new("'''bold''' text", TokenizerOptions::default());
/// let tokens: Vec<WikitextToken> = tok.tokenize()?;
/// ```
#[allow(dead_code)]
pub struct Tokenizer<'a> {
    input: &'a str,
    pos: usize,
    options: TokenizerOptions,
}

impl<'a> Tokenizer<'a> {
    /// Create a new tokenizer for the given wikitext input.
    pub fn new(input: &'a str, options: TokenizerOptions) -> Self {
        Self {
            input,
            pos: 0,
            options,
        }
    }

    /// Tokenize the entire input and return the token stream.
    ///
    /// Currently a placeholder; Phase 1 implementation will fill this in.
    pub fn tokenize(&mut self) -> Result<Vec<WikitextToken>> {
        let mut tokens = Vec::new();

        while self.pos < self.input.len() {
            let remaining = &self.input[self.pos..];

            // Try each token pattern in order of specificity.
            if let Some(token) = self.try_comment(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_nowiki(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_template(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_heading(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_list(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_table(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_hr(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_wikilink(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_extlink(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_bold_italic(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_html_tag(remaining) {
                tokens.push(token);
                continue;
            }
            if let Some(token) = self.try_paragraph_break(remaining) {
                tokens.push(token);
                continue;
            }

            // Fall through: accumulate plain text
            let start = self.pos;
            self.pos += 1;
            if self.pos >= self.input.len() {
                tokens.push(WikitextToken::Text(self.input[start..].to_string()));
            } else {
                // Continue accumulating text until we can match something.
                // This is simplified — the real tokenizer will be more efficient.
                continue;
            }
        }

        // Collapse consecutive Text tokens and add EOF
        tokens = collapse_text_tokens(tokens);
        tokens.push(WikitextToken::EOF);

        Ok(tokens)
    }

    // ---- Token recognizers (placeholder implementations) ----

    fn try_comment(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("<!--")
            && let Some(end) = remaining.find("-->")
        {
            let comment = remaining[4..end].to_string();
            self.pos += end + 3;
            return Some(WikitextToken::Comment(comment));
        }
        None
    }

    fn try_nowiki(&mut self, remaining: &str) -> Option<WikitextToken> {
        if let Some(stripped) = remaining.strip_prefix("<nowiki>")
            && let Some(end) = stripped.find("</nowiki>")
        {
            let content = stripped[..end].to_string();
            self.pos += 8 + end + 9; // <nowiki> + content + </nowiki>
            return Some(WikitextToken::NowikiContent(content));
        }
        None
    }

    fn try_template(&mut self, remaining: &str) -> Option<WikitextToken> {
        if let Some(stripped) = remaining.strip_prefix("{{{") {
            // Template argument
            let start = self.pos;
            if let Some(end) = stripped.find("}}}") {
                let inner = &stripped[..end];
                let name = inner.split('|').next().unwrap_or("").trim().to_string();
                self.pos += 3 + end + 3;
                return Some(WikitextToken::TplArgOpen(name));
            }
            self.pos = start;
        } else if let Some(stripped) = remaining.strip_prefix("{{") {
            let start = self.pos;
            if let Some(end) = stripped.find("}}") {
                let inner = &stripped[..end];
                let name = inner.split('|').next().unwrap_or("").trim().to_string();
                self.pos += 2 + end + 2;
                if name.starts_with('#') {
                    return Some(WikitextToken::ParserFnOpen(name));
                }
                return Some(WikitextToken::TemplateOpen(name));
            }
            self.pos = start;
        }
        None
    }

    fn try_heading(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with('=') {
            let level = remaining.chars().take_while(|&c| c == '=').count();
            if (2..=6).contains(&level) {
                self.pos += level;
                return Some(WikitextToken::HeadingOpen(level as u8));
            }
        }
        None
    }

    fn try_list(&mut self, remaining: &str) -> Option<WikitextToken> {
        let first = remaining.chars().next()?;
        if matches!(first, '*' | '#' | ';' | ':') {
            let depth = remaining.chars().take_while(|&c| c == first).count();
            self.pos += depth;
            return Some(WikitextToken::ListItem(first, depth as u8));
        }
        None
    }

    fn try_table(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("{|") {
            self.pos += 2;
            let attrs = Vec::new();
            return Some(WikitextToken::TableOpen(attrs));
        }
        if remaining.starts_with("|}") {
            self.pos += 2;
            return Some(WikitextToken::TableClose);
        }
        if remaining.starts_with("|-") {
            self.pos += 2;
            return Some(WikitextToken::TableRow);
        }
        if remaining.starts_with("|+") {
            self.pos += 2;
            return Some(WikitextToken::TableCaption);
        }
        None
    }

    fn try_hr(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("----") {
            self.pos += 4;
            return Some(WikitextToken::Hr);
        }
        None
    }

    fn try_wikilink(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("[[") {
            self.pos += 2;
            return Some(WikitextToken::WikilinkOpen);
        }
        None
    }

    fn try_extlink(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("[http") || remaining.starts_with("[https") {
            self.pos += 1;
            // Find the URL portion
            let rest = &remaining[1..];
            if let Some(space) = rest.find(' ') {
                let url = rest[..space].to_string();
                self.pos += space + 1;
                return Some(WikitextToken::ExtLinkOpen(url));
            }
            // Revert the [ advance
            self.pos -= 1;
        }
        None
    }

    fn try_bold_italic(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("'''''") {
            self.pos += 5;
            return Some(WikitextToken::BoldOpen);
        } else if remaining.starts_with("'''") {
            self.pos += 3;
            return Some(WikitextToken::BoldOpen);
        } else if remaining.starts_with("''") {
            self.pos += 2;
            return Some(WikitextToken::ItalicOpen);
        }
        None
    }

    fn try_html_tag(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("</") {
            if let Some(end) = remaining.find('>') {
                let name = remaining[2..end].trim().to_lowercase();
                self.pos += end + 1;
                return Some(WikitextToken::HtmlTagClose(name));
            }
        } else if remaining.starts_with('<')
            && let Some(end) = remaining.find('>')
        {
            let tag_content = &remaining[1..end];
            let self_closing = tag_content.ends_with('/');
            let name = tag_content
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_lowercase();
            self.pos += end + 1;
            if self_closing {
                return Some(WikitextToken::SelfClosingTag(name, Vec::new()));
            }
            return Some(WikitextToken::HtmlTagOpen(name, Vec::new()));
        }
        None
    }

    fn try_paragraph_break(&mut self, remaining: &str) -> Option<WikitextToken> {
        if remaining.starts_with("\n\n") {
            self.pos += 2;
            return Some(WikitextToken::ParagraphBreak);
        }
        if remaining.starts_with('\n') {
            self.pos += 1;
            return Some(WikitextToken::Newline);
        }
        None
    }
}

/// Collapse consecutive Text tokens into a single Text token.
fn collapse_text_tokens(tokens: Vec<WikitextToken>) -> Vec<WikitextToken> {
    let mut result: Vec<WikitextToken> = Vec::new();
    for token in tokens {
        if let WikitextToken::Text(ref new_text) = token
            && let Some(WikitextToken::Text(last_text)) = result.last_mut()
        {
            last_text.push_str(new_text);
            continue;
        }
        result.push(token);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        let mut tok = Tokenizer::new("", TokenizerOptions::default());
        let tokens = tok.tokenize().unwrap();
        assert_eq!(tokens, vec![WikitextToken::EOF]);
    }

    #[test]
    fn test_bold_text() {
        let mut tok = Tokenizer::new("'''bold'''", TokenizerOptions::default());
        let tokens = tok.tokenize().unwrap();
        assert_eq!(tokens.len(), 3); // BoldOpen, collapsed Text+"'''", EOF
        assert!(tokens.iter().any(|t| matches!(t, WikitextToken::BoldOpen)));
    }

    #[test]
    fn test_wikilink() {
        let mut tok = Tokenizer::new("[[Main Page]]", TokenizerOptions::default());
        let tokens = tok.tokenize().unwrap();
        assert_eq!(tokens.len(), 3); // WikilinkOpen, WikilinkClose, EOF
    }

    #[test]
    fn test_template() {
        let mut tok = Tokenizer::new("{{Foo}}", TokenizerOptions::default());
        let tokens = tok.tokenize().unwrap();
        assert_eq!(tokens.len(), 2); // TemplateOpen, EOF
    }

    #[test]
    fn test_heading() {
        let mut tok = Tokenizer::new("== Heading ==", TokenizerOptions::default());
        let tokens = tok.tokenize().unwrap();
        // Opening heading, then the rest is text for now
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::HeadingOpen(2)))
        );
    }

    #[test]
    fn test_comment() {
        let mut tok = Tokenizer::new("<!-- hidden -->text", TokenizerOptions::default());
        let tokens = tok.tokenize().unwrap();
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, WikitextToken::Comment(_)))
        );
    }
}
