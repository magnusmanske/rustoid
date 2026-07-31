//! Quote transformer — resolves raw quote tokens to Bold/Italic open/close tags.
//!
//! Implements the same chunk-based state machine as Parsoid's QuoteTransformer.
//! Tokens are split into alternating non-quote and quote chunks, then quote chunks
//! are resolved to open/close tags.

use crate::error::Result;
use crate::wikitext::tokens::WikitextToken;

pub struct QuoteTransformer;

impl QuoteTransformer {
    /// Process a token stream, resolving quote tokens per line.
    pub fn transform(tokens: Vec<WikitextToken>) -> Result<Vec<WikitextToken>> {
        let mut result: Vec<WikitextToken> = Vec::new();
        let mut line_tokens: Vec<WikitextToken> = Vec::new();

        for token in tokens {
            match &token {
                WikitextToken::Newline | WikitextToken::ParagraphBreak => {
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

    /// Split tokens into alternating chunks: non-quote, quote, non-quote, quote, ...
    /// Returns (chunks, quote_chunk_indices) where quote_chunk_indices are odd-numbered.
    fn chunkify(tokens: &[WikitextToken]) -> (Vec<Vec<WikitextToken>>, Vec<usize>) {
        let mut chunks: Vec<Vec<WikitextToken>> = Vec::new();
        let mut quote_indices: Vec<usize> = Vec::new();
        let mut current: Vec<WikitextToken> = Vec::new();
        let mut is_quote_chunk = false;

        for token in tokens {
            match token {
                WikitextToken::Quote(_) => {
                    if !is_quote_chunk {
                        // Flush non-quote chunk
                        chunks.push(std::mem::take(&mut current));
                        is_quote_chunk = true;
                    }
                    current.push(token.clone());
                }
                _ => {
                    if is_quote_chunk {
                        // Flush quote chunk
                        chunks.push(std::mem::take(&mut current));
                        quote_indices.push(chunks.len() - 1);
                        is_quote_chunk = false;
                    }
                    current.push(token.clone());
                }
            }
        }
        // Flush final chunk
        if !current.is_empty() {
            if is_quote_chunk {
                quote_indices.push(chunks.len());
            }
            chunks.push(current);
        }

        (chunks, quote_indices)
    }

    /// Resolve Quote tokens within a single line to open/close tags.
    fn resolve_line_quotes(tokens: &[WikitextToken]) -> Vec<WikitextToken> {
        let (chunks, quote_indices) = Self::chunkify(tokens);

        if quote_indices.is_empty() {
            return tokens.to_vec();
        }

        // Extract quote lengths from quote chunks
        let mut quote_lengths: Vec<usize> = Vec::new();
        for &qi in &quote_indices {
            // Each quote chunk should contain exactly one Quote token
            if let Some(WikitextToken::Quote(q)) = chunks[qi].first() {
                quote_lengths.push(q.len());
            }
        }

        // Count bold and italic
        let mut num_bold = 0;
        let mut num_italic = 0;
        for &len in &quote_lengths {
            if len == 2 || len == 5 {
                num_italic += 1;
            }
            if len == 3 || len == 5 {
                num_bold += 1;
            }
        }

        // Balance: if both counts are odd, convert a bold to italic+apostrophe
        let mut quote_lengths = quote_lengths;
        if num_italic % 2 == 1 && num_bold % 2 == 1 {
            Self::balance_quote_lengths(&mut quote_lengths, &chunks, &quote_indices);
        }

        // Convert quote chunks to tags using Parsoid state machine
        Self::convert_chunks(&chunks, &quote_indices, &quote_lengths)
    }

    fn balance_quote_lengths(
        quote_lengths: &mut [usize],
        chunks: &[Vec<WikitextToken>],
        quote_indices: &[usize],
    ) {
        let mut first_single_letter = None;
        let mut first_multi_letter = None;
        let mut first_space = None;

        for (qi, &len) in quote_lengths.iter().enumerate() {
            if len != 3 {
                continue;
            }
            // Check the non-quote chunk BEFORE this quote chunk
            let prev_chunk_idx = if quote_indices[qi] > 0 {
                quote_indices[qi] - 1
            } else {
                0
            };
            let has_text = prev_chunk_idx < chunks.len();
            let last_text = if has_text {
                chunks[prev_chunk_idx].last().and_then(|t| {
                    if let WikitextToken::Text(s) = t {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            };

            let last_char_space = last_text.as_ref().map_or(true, |t| t.ends_with(' '));
            let second_last_space = last_text.as_ref().map_or(true, |t| {
                t.chars().rev().nth(1).map(|c| c == ' ').unwrap_or(true)
            });

            if last_char_space && first_space.is_none() {
                first_space = Some(qi);
            } else if !last_char_space {
                if second_last_space && first_single_letter.is_none() {
                    first_single_letter = Some(qi);
                } else if first_multi_letter.is_none() {
                    first_multi_letter = Some(qi);
                }
            }
        }

        let convert_idx = first_single_letter.or(first_multi_letter).or(first_space);
        if let Some(idx) = convert_idx {
            quote_lengths[idx] = 2;
        }
    }

    /// Convert quote chunks to BoldOpen/BoldClose/ItalicOpen/ItalicClose.
    /// Non-quote chunks pass through unchanged.
    fn convert_chunks(
        chunks: &[Vec<WikitextToken>],
        quote_indices: &[usize],
        quote_lengths: &[usize],
    ) -> Vec<WikitextToken> {
        let mut result: Vec<WikitextToken> = Vec::new();

        #[derive(Clone, Copy)]
        enum State {
            Empty,
            I,
            B,
            BI,
            IB,
            Both(usize),
        }

        let mut state = State::Empty;
        let mut qi: usize = 0;

        for (ci, chunk) in chunks.iter().enumerate() {
            if qi < quote_indices.len() && ci == quote_indices[qi] {
                // Quote chunk
                let qlen = quote_lengths[qi];
                match (qlen, state) {
                    (2, State::Empty | State::B) => {
                        result.push(WikitextToken::ItalicOpen);
                        state = if matches!(state, State::B) {
                            State::IB
                        } else {
                            State::I
                        };
                    }
                    (2, State::I) => {
                        result.push(WikitextToken::ItalicClose);
                        state = State::Empty;
                    }
                    (2, State::BI | State::IB) => {
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
                        state = if matches!(state, State::I) {
                            State::BI
                        } else {
                            State::B
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
                        result[last] = WikitextToken::ItalicOpen;
                        result.insert(last + 1, WikitextToken::BoldOpen);
                        result.extend(vec![WikitextToken::BoldClose, WikitextToken::ItalicClose]);
                        state = State::Empty;
                    }
                    _ => {}
                }
                qi += 1;
            } else {
                // Non-quote chunk — pass through
                result.extend(chunk.clone());
            }
        }

        // Close remaining open tags
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
        assert!(matches!(result[0], WikitextToken::ItalicOpen));
        assert!(matches!(result[1], WikitextToken::BoldOpen));
        assert!(matches!(result[2], WikitextToken::Text(_)));
        assert!(matches!(result[3], WikitextToken::BoldClose));
        assert!(matches!(result[4], WikitextToken::ItalicClose));
    }
}
