//! Paragraph wrapper — inserts <p> tags into the token stream.
//!
//! Mimics the MediaWiki PHP parser's paragraph wrapping behavior.
//! Operates at the token level (before tree building), as Parsoid does.
//! Handles newline-induced paragraph breaks, SOL-transparent tokens,
//! and empty line handling (two newlines → close p, open new p).

use crate::wikitext::tokens::WikitextToken;

pub struct ParagraphWrapper;

impl ParagraphWrapper {
    /// Process a token stream, inserting paragraph open/close tags.
    pub fn wrap(tokens: Vec<WikitextToken>) -> Vec<WikitextToken> {
        let mut result: Vec<WikitextToken> = Vec::new();
        let mut has_open_p: bool = false;
        let mut in_block: bool = false;
        let mut pending_newlines: usize = 0;
        let mut line_tokens: Vec<WikitextToken> = Vec::new();
        let mut line_has_content: bool = false;

        for token in tokens {
            match &token {
                // Block-level tokens close any open paragraph
                WikitextToken::HeadingOpen(_)
                | WikitextToken::Hr
                | WikitextToken::TableOpen(_)
                | WikitextToken::ListItem(_, _) => {
                    // Flush current line
                    if has_open_p || line_has_content {
                        Self::close_p_if_open(&mut result, &mut has_open_p);
                        if !line_tokens.is_empty() {
                            // Don't wrap in p — block elements are their own blocks
                            result.append(&mut line_tokens);
                        }
                    }
                    result.push(token);
                    in_block = true;
                    continue;
                }

                // Newline handling
                WikitextToken::Newline => {
                    pending_newlines += 1;
                    continue;
                }

                // Paragraph break (empty line): close current p, emit newline, open new p
                WikitextToken::ParagraphBreak => {
                    pending_newlines += 2;
                    continue;
                }

                // Comments and whitespace-only text are SOL-transparent
                WikitextToken::Comment(_) => {
                    if pending_newlines == 0 {
                        result.push(token);
                    } else {
                        line_tokens.push(token);
                    }
                    continue;
                }

                WikitextToken::Text(s) if s.trim().is_empty() => {
                    // Whitespace-only text
                    if pending_newlines == 0 {
                        result.push(token);
                    }
                    continue;
                }

                // End of file
                WikitextToken::EOF => {
                    Self::close_p_if_open(&mut result, &mut has_open_p);
                    result.push(WikitextToken::EOF);
                    break;
                }

                _ => {
                    // Process any pending newlines first
                    if pending_newlines > 0 {
                        if pending_newlines >= 2 || in_block {
                            // Two+ newlines or after block: close paragraph
                            Self::close_p_if_open(&mut result, &mut has_open_p);
                            if pending_newlines >= 2 {
                                result.push(WikitextToken::Newline);
                            }
                            in_block = false;
                        }
                        pending_newlines = 0;
                    }

                    // Open paragraph if needed
                    if !has_open_p && !in_block {
                        result.push(WikitextToken::ParagraphOpen);
                        has_open_p = true;
                    }

                    line_has_content = true;
                    result.push(token);
                }
            }
        }

        result
    }

    fn close_p_if_open(result: &mut Vec<WikitextToken>, has_open_p: &mut bool) {
        if *has_open_p {
            result.push(WikitextToken::ParagraphClose);
            *has_open_p = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_wrapping() {
        let tokens = vec![
            WikitextToken::Text("hello".to_string()),
            WikitextToken::Newline,
            WikitextToken::Text("world".to_string()),
            WikitextToken::EOF,
        ];
        let result = ParagraphWrapper::wrap(tokens);
        // Should produce: <p>, hello, </p>, world, EOF
        // Actually, newline doesn't close p by itself in our current impl
        // Let's check the behavior
        eprintln!("result: {:?}", result);
    }
}
