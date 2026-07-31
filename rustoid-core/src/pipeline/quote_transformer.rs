//! Quote transformer — resolves raw quote tokens to Bold/Italic open/close tags.
//!
//! This implements the same state machine as Parsoid's QuoteTransformer
//! and the MediaWiki PHP parser for balancing and converting quote markers.

use crate::error::Result;
use crate::wikitext::tokens::WikitextToken;

/// Transforms a token stream by replacing Quote tokens with appropriate
/// Bold/Italic element tokens.
pub struct QuoteTransformer;

/// Resolved inline formatting element types.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedToken {
    /// Plain text.
    Text(String),
    /// Start of a bold span.
    BoldOpen,
    /// End of a bold span.
    BoldClose,
    /// Start of an italic span.
    ItalicOpen,
    /// End of an italic span.
    ItalicClose,
    /// Any other token (pass-through).
    Other(WikitextToken),
}

impl QuoteTransformer {
    /// Process a token stream, resolving quote tokens per line.
    ///
    /// Quote tokens within a line are buffered and resolved when a newline or
    /// block-level token is encountered.
    pub fn transform(tokens: Vec<WikitextToken>) -> Result<Vec<WikitextToken>> {
        // Pre-pass: collect tokens per line, resolve quotes per line
        let mut result: Vec<WikitextToken> = Vec::new();
        let mut line_tokens: Vec<WikitextToken> = Vec::new();

        for token in tokens {
            match &token {
                WikitextToken::Newline | WikitextToken::ParagraphBreak => {
                    // Resolve quotes in the current line
                    let resolved = Self::resolve_line_quotes(&line_tokens);
                    result.extend(resolved);
                    result.push(token);
                    line_tokens.clear();
                }
                WikitextToken::EOF => {
                    let resolved = Self::resolve_line_quotes(&line_tokens);
                    result.extend(resolved);
                    result.push(WikitextToken::EOF);
                }
                _ => {
                    line_tokens.push(token);
                }
            }
        }

        Ok(result)
    }

    /// Resolve Quote tokens within a single line to open/close tags.
    fn resolve_line_quotes(tokens: &[WikitextToken]) -> Vec<WikitextToken> {
        let mut quote_positions: Vec<usize> = Vec::new();
        let mut quote_values: Vec<String> = Vec::new();
        let mut original_lengths: Vec<usize> = Vec::new();

        for (i, token) in tokens.iter().enumerate() {
            if let WikitextToken::Quote(val) = token {
                quote_positions.push(i);
                quote_values.push(val.clone());
                original_lengths.push(val.len());
            }
        }

        if quote_positions.is_empty() {
            return tokens.to_vec();
        }

        // Count bold and italic quotes
        let mut num_bold: usize = 0;
        let mut num_italic: usize = 0;
        for val in &quote_values {
            let len = val.len();
            if len == 2 || len == 5 {
                num_italic += 1;
            }
            if len == 3 || len == 5 {
                num_bold += 1;
            }
        }

        // Balance: if both are odd, convert a bold to italic+apostrophe
        if num_italic % 2 == 1 && num_bold % 2 == 1 {
            Self::balance_quotes(&mut quote_values, &tokens, &quote_positions);
        }

        // Convert quotes to tags using the state machine
        Self::convert_quotes_to_tokens(tokens, &quote_positions, &quote_values, &original_lengths)
    }

    /// Balance odd counts of both bold and italic by converting a bold
    /// to an italic plus an apostrophe.
    fn balance_quotes(
        quote_values: &mut [String],
        tokens: &[WikitextToken],
        quote_positions: &[usize],
    ) {
        let mut first_single_letter = None;
        let mut first_multi_letter = None;
        let mut first_space = None;

        for (qi, val) in quote_values.iter().enumerate() {
            if val.len() != 3 {
                continue; // only look at bold (3-quote) tokens
            }
            let pos = quote_positions[qi];

            // Look backwards through tokens to find the last text content
            let last_char_is_space = Self::last_char_before_is_space(tokens, pos);
            let second_last_char_is_space = Self::second_last_char_before_is_space(tokens, pos);

            if last_char_is_space && first_space.is_none() {
                first_space = Some(qi);
            } else if !last_char_is_space {
                if second_last_char_is_space && first_single_letter.is_none() {
                    first_single_letter = Some(qi);
                } else if first_multi_letter.is_none() {
                    first_multi_letter = Some(qi);
                }
            }
        }

        let convert_idx = first_single_letter.or(first_multi_letter).or(first_space);

        if let Some(idx) = convert_idx {
            quote_values[idx] = "''".to_string();
        }
    }

    /// Look backwards to determine if the last non-whitespace character before
    /// a quote token is a space.
    fn last_char_before_is_space(tokens: &[WikitextToken], pos: usize) -> bool {
        for i in (0..pos).rev() {
            match &tokens[i] {
                WikitextToken::Text(t) => {
                    return t.chars().last().map_or(true, |c| c == ' ');
                }
                WikitextToken::WikilinkClose | WikitextToken::ExtLinkClose => {
                    // Links end with ]] or ] — look inside the link text
                    // For now, treat as non-space (it's typically a word)
                    return false;
                }
                WikitextToken::ItalicClose | WikitextToken::BoldClose => {
                    // These are formatting closes — continue looking
                    continue;
                }
                _ => {
                    // Other tokens (like tags, etc.) — continue looking
                    continue;
                }
            }
        }
        true // at start of line, treat as space
    }

    fn second_last_char_before_is_space(tokens: &[WikitextToken], pos: usize) -> bool {
        for i in (0..pos).rev() {
            match &tokens[i] {
                WikitextToken::Text(t) => {
                    let chars: Vec<char> = t.chars().rev().collect();
                    return chars.len() < 2 || chars[1] == ' ';
                }
                WikitextToken::WikilinkClose | WikitextToken::ExtLinkClose => {
                    return false;
                }
                WikitextToken::ItalicClose | WikitextToken::BoldClose => {
                    continue;
                }
                _ => {
                    continue;
                }
            }
        }
        true
    }

    /// Convert quote tokens to BoldOpen/BoldClose/ItalicOpen/ItalicClose using
    /// the MediaWiki state machine.
    fn convert_quotes_to_tokens(
        tokens: &[WikitextToken],
        quote_positions: &[usize],
        quote_values: &[String],
        original_lengths: &[usize],
    ) -> Vec<WikitextToken> {
        let mut result: Vec<WikitextToken> = Vec::new();
        let mut token_idx: usize = 0;
        let mut qi: usize = 0;

        // State: "" | "i" | "b" | "bi" | "ib" | "both"
        #[derive(Clone, Copy)]
        enum State {
            Empty,
            I,
            B,
            BI,
            IB,
            Both(usize), // Both stores the index of the lastboth
        }

        let mut state = State::Empty;

        while token_idx < tokens.len() {
            if qi < quote_positions.len() && token_idx == quote_positions[qi] {
                let qlen = quote_values[qi].len();
                // If this was a 3-quote converted to 2-quote, prepend apostrophe
                let was_converted = qlen == 2 && original_lengths[qi] == 3;
                if was_converted {
                    result.push(WikitextToken::Text("'".to_string()));
                }
                match (qlen, state) {
                    (2, State::Empty | State::B) => {
                        result.push(WikitextToken::ItalicOpen);
                        state = match &state {
                            State::B => State::IB,
                            _ => State::I,
                        };
                    }
                    (2, State::I) => {
                        result.push(WikitextToken::ItalicClose);
                        state = State::Empty;
                    }
                    (2, State::BI) => {
                        result.push(WikitextToken::ItalicClose);
                        state = State::B;
                    }
                    (2, State::IB) => {
                        // annoying case
                        result.extend(vec![
                            WikitextToken::BoldClose,
                            WikitextToken::ItalicClose,
                            WikitextToken::BoldOpen,
                        ]);
                        state = State::B;
                    }
                    (2, State::Both(last)) => {
                        result[last] = WikitextToken::BoldOpen;
                        result.insert(last + 1, WikitextToken::ItalicOpen);
                        result.push(WikitextToken::ItalicClose);
                        state = State::B;
                    }

                    (3, State::Empty | State::I) => {
                        result.push(WikitextToken::BoldOpen);
                        state = match &state {
                            State::I => State::BI,
                            _ => State::B,
                        };
                    }
                    (3, State::B) => {
                        result.push(WikitextToken::BoldClose);
                        state = State::Empty;
                    }
                    (3, State::IB) => {
                        result.push(WikitextToken::BoldClose);
                        state = State::I;
                    }
                    (3, State::BI) => {
                        result.extend(vec![
                            WikitextToken::ItalicClose,
                            WikitextToken::BoldClose,
                            WikitextToken::ItalicOpen,
                        ]);
                        state = State::I;
                    }
                    (3, State::Both(last)) => {
                        result[last] = WikitextToken::ItalicOpen;
                        result.insert(last + 1, WikitextToken::BoldOpen);
                        result.push(WikitextToken::BoldClose);
                        state = State::I;
                    }

                    (5, State::Empty) => {
                        let last = result.len();
                        result.push(WikitextToken::BoldOpen);
                        state = State::Both(last);
                    }
                    (5, State::B) => {
                        result.extend(vec![WikitextToken::BoldClose, WikitextToken::ItalicOpen]);
                        state = State::I;
                    }
                    (5, State::I) => {
                        result.extend(vec![WikitextToken::ItalicClose, WikitextToken::BoldOpen]);
                        state = State::B;
                    }
                    (5, State::BI) => {
                        result.extend(vec![WikitextToken::ItalicClose, WikitextToken::BoldClose]);
                        state = State::Empty;
                    }
                    (5, State::IB) => {
                        result.extend(vec![WikitextToken::BoldClose, WikitextToken::ItalicClose]);
                        state = State::Empty;
                    }
                    (5, State::Both(last)) => {
                        // First 5-quote opens Italic + Bold
                        result[last] = WikitextToken::ItalicOpen;
                        result.insert(last + 1, WikitextToken::BoldOpen);
                        // Second 5-quote closes Bold + Italic
                        result.extend(vec![WikitextToken::BoldClose, WikitextToken::ItalicClose]);
                        state = State::Empty;
                    }

                    _ => {
                        // Unknown quote length — pass through as text
                        result.push(WikitextToken::Text(quote_values[qi].clone()));
                    }
                }
                qi += 1;
                token_idx += 1;
            } else {
                result.push(tokens[token_idx].clone());
                token_idx += 1;
            }
        }

        // Close any remaining open tags (auto-inserted ends)
        match state {
            State::Both(last) => {
                result[last] = WikitextToken::ItalicOpen;
                result.insert(last + 1, WikitextToken::BoldOpen);
                result.push(WikitextToken::BoldClose);
                result.push(WikitextToken::ItalicClose);
            }
            State::B | State::IB => {
                result.push(WikitextToken::BoldClose);
            }
            State::I | State::BI => {
                result.push(WikitextToken::ItalicClose);
            }
            State::Empty => {}
        }
        if matches!(state, State::BI) {
            result.push(WikitextToken::BoldClose);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_bold() {
        let tokens = vec![
            WikitextToken::Quote("'''".to_string()),
            WikitextToken::Text("bold".to_string()),
            WikitextToken::Quote("'''".to_string()),
        ];
        let result = QuoteTransformer::resolve_line_quotes(&tokens);
        assert!(matches!(result[0], WikitextToken::BoldOpen));
        assert!(matches!(result[1], WikitextToken::Text(_)));
        assert!(matches!(result[2], WikitextToken::BoldClose));
    }

    #[test]
    fn test_simple_italic() {
        let tokens = vec![
            WikitextToken::Quote("''".to_string()),
            WikitextToken::Text("italic".to_string()),
            WikitextToken::Quote("''".to_string()),
        ];
        let result = QuoteTransformer::resolve_line_quotes(&tokens);
        assert!(matches!(result[0], WikitextToken::ItalicOpen));
        assert!(matches!(result[1], WikitextToken::Text(_)));
        assert!(matches!(result[2], WikitextToken::ItalicClose));
    }

    #[test]
    fn test_bold_italic_5quotes() {
        let tokens = vec![
            WikitextToken::Quote("'''''".to_string()),
            WikitextToken::Text("both".to_string()),
            WikitextToken::Quote("'''''".to_string()),
        ];
        let result = QuoteTransformer::resolve_line_quotes(&tokens);
        eprintln!("result: {:?}", result);
        // Should produce ItalicOpen, BoldOpen, text, BoldClose, ItalicClose
        assert!(matches!(result[0], WikitextToken::ItalicOpen));
        assert!(matches!(result[1], WikitextToken::BoldOpen));
        assert!(matches!(result[2], WikitextToken::Text(_)));
        assert!(matches!(result[3], WikitextToken::BoldClose));
        assert!(matches!(result[4], WikitextToken::ItalicClose));
    }
}
