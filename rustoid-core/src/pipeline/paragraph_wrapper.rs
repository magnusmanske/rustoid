//! Paragraph wrapper — inserts <p> tags into the token stream.
//!
//! Ported from Parsoid's ParagraphWrapper.php / PWrap logic.
//! Inserts ParagraphOpen/ParagraphClose tokens around inline content.
//! Block-level tokens and SOL-transparent tokens are passed through
//! outside paragraph context.

use crate::wikitext::tokens::WikitextToken;

pub struct ParagraphWrapper;

impl ParagraphWrapper {
    /// Process a token stream, inserting paragraph open/close tags.
    /// Runs after quote transformation (Bold/Italic open/close tokens present).
    pub fn wrap(tokens: Vec<WikitextToken>) -> Vec<WikitextToken> {
        let mut result: Vec<WikitextToken> = Vec::new();
        let mut has_open_p: bool = false;
        let mut pending_newlines: usize = 0;
        let mut pending_nl_tokens: Vec<WikitextToken> = Vec::new();

        for token in tokens {
            match &token {
                // Skip these — they're handled in the pipeline stage
                WikitextToken::ParagraphOpen | WikitextToken::ParagraphClose => {
                    continue;
                }

                // Block-level tokens: flush paragraph if open, emit pending newlines, then emit token
                WikitextToken::HeadingOpen(_)
                | WikitextToken::Hr
                | WikitextToken::TableOpen(_)
                | WikitextToken::TableClose
                | WikitextToken::TableRow
                | WikitextToken::ListItem(_, _) => {
                    Self::close_p_if_open(&mut result, &mut has_open_p);
                    // Emit exactly one newline before block tokens, regardless of pending count
                    for _ in 0..pending_newlines.min(1) {
                        result.push(WikitextToken::Newline);
                    }
                    pending_newlines = 0;
                    pending_nl_tokens.clear();
                    result.push(token);
                }

                // HTML block OPEN elements: <div>, <blockquote>, <pre> etc.
                // These start a block context — close paragraph before them.
                WikitextToken::HtmlTagOpen(name, _) if Self::is_block_tag(name) => {
                    Self::close_p_if_open(&mut result, &mut has_open_p);
                    pending_newlines = 0;
                    pending_nl_tokens.clear();
                    result.push(token);
                }

                // HTML block CLOSE elements: </div>, </blockquote> etc.
                // These close the current paragraph (the content inside the block),
                // then emit the close tag.
                WikitextToken::HtmlTagClose(name) if Self::is_block_tag(name) => {
                    Self::close_p_if_open(&mut result, &mut has_open_p);
                    pending_newlines = 0;
                    pending_nl_tokens.clear();
                    result.push(token);
                }

                // HTML self-closing block elements: <br/>, <hr/> etc.
                WikitextToken::SelfClosingTag(name, _) if Self::is_block_tag(name) => {
                    Self::close_p_if_open(&mut result, &mut has_open_p);
                    pending_newlines = 0;
                    pending_nl_tokens.clear();
                    result.push(token);
                }

                // Newline — accumulate pending newlines
                WikitextToken::Newline => {
                    pending_newlines += 1;
                    pending_nl_tokens.push(token);
                }

                // Paragraph break — close current p, emit separator newline
                WikitextToken::ParagraphBreak => {
                    Self::close_p_if_open(&mut result, &mut has_open_p);
                    result.push(WikitextToken::Newline);
                    pending_newlines = 0;
                    pending_nl_tokens.clear();
                }

                // SOL-transparent: comments go through without changing paragraph state.
                // Comments consume one preceding newline (they're SOL-transparent).
                // The newline AFTER a comment is preserved for the next content token.
                WikitextToken::Comment(_) => {
                    // A comment following newlines consumes exactly one newline.
                    if pending_newlines > 0 {
                        pending_newlines -= 1;
                    }
                    // Emit the comment into the current context
                    if !has_open_p {
                        result.push(WikitextToken::ParagraphOpen);
                        has_open_p = true;
                    }
                    result.push(token);
                }

                // EOF — close any open paragraph
                WikitextToken::EOF => {
                    Self::close_p_if_open(&mut result, &mut has_open_p);
                    result.push(WikitextToken::EOF);
                    return result;
                }

                // All other tokens (Text, Bold/Italic open/close, Wikilink, etc.)
                _ => {
                    // Flush pending newlines: 2+ newlines close and reopen paragraph
                    if pending_newlines >= 2 {
                        Self::close_p_if_open(&mut result, &mut has_open_p);
                        result.push(WikitextToken::Newline);
                        pending_newlines = 0;
                        pending_nl_tokens.clear();
                    } else if pending_newlines == 1 {
                        // Single newline — emit as plain newline within paragraph
                        result.push(WikitextToken::Newline);
                        pending_newlines = 0;
                        pending_nl_tokens.clear();
                    }

                    // Open paragraph if needed
                    if !has_open_p {
                        result.push(WikitextToken::ParagraphOpen);
                        has_open_p = true;
                    }

                    result.push(token);
                }
            }
        }

        // Should not reach here (EOF handles cleanup)
        result
    }

    fn close_p_if_open(result: &mut Vec<WikitextToken>, has_open_p: &mut bool) {
        if *has_open_p {
            result.push(WikitextToken::ParagraphClose);
            *has_open_p = false;
        }
    }

    /// Check if the tag name is considered a block-level HTML element.
    fn is_block_tag(name: &str) -> bool {
        matches!(
            name,
            "div"
                | "blockquote"
                | "pre"
                | "table"
                | "tr"
                | "td"
                | "th"
                | "center"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "ul"
                | "ol"
                | "li"
                | "dl"
                | "dt"
                | "dd"
                | "section"
                | "article"
                | "aside"
                | "nav"
                | "header"
                | "footer"
                | "hr"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_paragraph() {
        let tokens = vec![
            WikitextToken::Text("hello".to_string()),
            WikitextToken::Text(" world".to_string()),
            WikitextToken::EOF,
        ];
        let result = ParagraphWrapper::wrap(tokens);
        // Should be: ParagraphOpen, Text("hello"), Text(" world"), ParagraphClose, EOF
        assert!(matches!(result[0], WikitextToken::ParagraphOpen));
        assert!(matches!(result[1], WikitextToken::Text(_)));
        assert!(matches!(result[2], WikitextToken::Text(_)));
        assert!(matches!(result[3], WikitextToken::ParagraphClose));
        assert!(matches!(result[4], WikitextToken::EOF));
    }

    #[test]
    fn test_two_paragraphs() {
        let tokens = vec![
            WikitextToken::Text("first".to_string()),
            WikitextToken::Newline,
            WikitextToken::Newline,
            WikitextToken::Text("second".to_string()),
            WikitextToken::EOF,
        ];
        let result = ParagraphWrapper::wrap(tokens);
        // Should be: POpen, "first", PClose, Newline, POpen, "second", PClose, EOF
        let mut found_first = false;
        let mut found_second = false;
        let mut para_count = 0;
        for t in &result {
            match t {
                WikitextToken::ParagraphOpen => para_count += 1,
                WikitextToken::Text(s) if s == "first" => found_first = true,
                WikitextToken::Text(s) if s == "second" => found_second = true,
                _ => {}
            }
        }
        assert!(found_first, "got {:?}", result);
        assert!(found_second, "got {:?}", result);
        assert_eq!(para_count, 2, "expected 2 paragraphs, got {:?}", result);
    }

    #[test]
    fn test_list_passes_through() {
        let tokens = vec![
            WikitextToken::Text("text".to_string()),
            WikitextToken::Newline,
            WikitextToken::ListItem('*', 1),
            WikitextToken::Text("item".to_string()),
            WikitextToken::EOF,
        ];
        let result = ParagraphWrapper::wrap(tokens);
        // Should not wrap list item in paragraph
        let has_list = result
            .iter()
            .any(|t| matches!(t, WikitextToken::ListItem(_, _)));
        assert!(has_list, "got {:?}", result);
    }
}
