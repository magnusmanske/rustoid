//! Quote transformer — resolves raw quote tokens to Bold/Italic open/close tags.
//!
//! Ported from Parsoid's QuoteTransformer.php.
//! Strategy: count bold/italic, balance if both odd, then convert via state machine.

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

    /// Split tokens into alternating chunks: non-quote (even), quote (odd).
    /// Quote tokens of length 2, 3, or 5 become their own chunk.
    /// Other tokens accumulate in non-quote chunks.
    fn chunkify(tokens: &[WikitextToken]) -> Vec<Vec<WikitextToken>> {
        let mut chunks: Vec<Vec<WikitextToken>> = vec![Vec::new()]; // Start with non-quote chunk
        let _unused = ();

        for token in tokens {
            if let WikitextToken::Quote(q) = token {
                let qlen = q.len();
                if qlen == 2 || qlen == 3 || qlen == 5 {
                    // Valid quote — start new quote chunk
                    chunks.push(vec![token.clone()]);
                    // Start new non-quote chunk
                    chunks.push(Vec::new());
                } else {
                    // Invalid quote length — treat as text in current chunk
                    chunks.last_mut().unwrap().push(token.clone());
                }
            } else {
                chunks.last_mut().unwrap().push(token.clone());
            }
        }

        // Remove trailing empty non-quote chunk
        if chunks.last().map(|c| c.is_empty()).unwrap_or(false) {
            chunks.pop();
        }

        chunks
    }

    /// Resolve Quote tokens within a single line.
    /// Follows Parsoid's algorithm: count, balance, convert.
    fn resolve_line_quotes(tokens: &[WikitextToken]) -> Vec<WikitextToken> {
        let chunks = Self::chunkify(tokens);

        // Check if there are any quote chunks (odd indices)
        if chunks.len() < 2 {
            return tokens.to_vec();
        }

        // Count bold and italic quote chunks
        let mut num_italic: usize = 0;
        let mut num_bold: usize = 0;
        let mut quote_lengths: Vec<usize> = Vec::new();
        let mut quote_indices: Vec<usize> = Vec::new();

        for (ci, chunk) in chunks.iter().enumerate() {
            if ci % 2 == 1 && chunk.len() == 1 {
                if let WikitextToken::Quote(q) = &chunk[0] {
                    let qlen = q.len();
                    if qlen == 2 || qlen == 5 {
                        num_italic += 1;
                    }
                    if qlen == 3 || qlen == 5 {
                        num_bold += 1;
                    }
                    quote_lengths.push(qlen);
                    quote_indices.push(ci);
                }
            }
        }

        if quote_indices.is_empty() {
            return tokens.to_vec();
        }

        let mut chunks = chunks;

        // Balance: if both counts are odd, convert a bold to italic+apostrophe
        if num_italic % 2 == 1 && num_bold % 2 == 1 {
            Self::balance_quotes(&mut chunks, &quote_indices, &quote_lengths);
        }

        // Convert quote chunks to tags using the state machine
        Self::convert_quotes_to_tags(&mut chunks)
    }

    /// Balance: convert one 3-quote into "'" + 2-quote (italic).
    /// This resolves the case where both bold and italic counts are odd.
    fn balance_quotes(
        chunks: &mut [Vec<WikitextToken>],
        _quote_indices: &[usize],
        _quote_lengths: &[usize],
    ) {
        let mut first_single_letter: Option<usize> = None;
        let mut first_multi_letter: Option<usize> = None;
        let mut first_space: Option<usize> = None;

        for (qi, chunk) in chunks.iter().enumerate() {
            if qi % 2 == 0 || chunk.len() != 1 {
                continue;
            }
            let qlen = match &chunk[0] {
                WikitextToken::Quote(q) => q.len(),
                _ => continue,
            };
            if qlen != 3 {
                continue;
            }

            // Check the preceding non-quote chunk for context
            let prev_chunk = chunks.get(qi - 1);
            let last_char_is_space = prev_chunk
                .and_then(|c| c.last())
                .and_then(|t| {
                    if let WikitextToken::Text(s) = t {
                        Some(s.ends_with(' '))
                    } else {
                        None
                    }
                })
                .unwrap_or(true); // Default: treat as space

            let second_last_is_space = prev_chunk
                .and_then(|c| {
                    if c.len() >= 2 {
                        if let WikitextToken::Text(s) = &c[c.len() - 2] {
                            Some(s.ends_with(' '))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .unwrap_or(true);

            if last_char_is_space && first_space.is_none() {
                first_space = Some(qi);
            } else if !last_char_is_space {
                if second_last_is_space && first_single_letter.is_none() {
                    first_single_letter = Some(qi);
                } else if first_multi_letter.is_none() {
                    first_multi_letter = Some(qi);
                }
            }
        }

        let convert_idx = first_single_letter.or(first_multi_letter).or(first_space);

        if let Some(idx) = convert_idx {
            // Convert 3-quote to "'" + 2-quote
            // Push "'" into the preceding non-quote chunk
            if idx > 0 {
                chunks[idx - 1].push(WikitextToken::Text("'".to_string()));
            }
            // Replace the 3-quote token with a 2-quote token
            chunks[idx] = vec![WikitextToken::Quote("''".to_string())];
        }
    }

    /// Convert quote chunks to open/close tags using the PHP parser's state machine.
    /// This is a direct port of Parsoid's convertQuotesToTags().
    fn convert_quotes_to_tags(chunks: &mut Vec<Vec<WikitextToken>>) -> Vec<WikitextToken> {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum State {
            Empty,
            B,           // Bold open
            I,           // Italic open
            BI,          // Bold open, then Italic open (B+I)
            IB,          // Italic open, then Bold open (I+B)
            Both(usize), // Both open via 5-quote; stores the chunk index
        }

        let mut state = State::Empty;
        let mut last_both: isize = -1;

        let quote_chunk_count = chunks
            .iter()
            .enumerate()
            .filter(|(i, c)| i % 2 == 1 && c.len() == 1)
            .count();

        if quote_chunk_count == 0 {
            return chunks.iter().flatten().cloned().collect();
        }

        // We'll process in-place by replacing quote chunks with tag tokens
        let mut qi = 0usize; // tracks which quote chunk we're on (0-based)
        let total_chunks = chunks.len();

        for ci in 0..total_chunks {
            if ci % 2 == 0 {
                // Non-quote chunk — skip (kept as-is)
                continue;
            }
            if chunks[ci].len() != 1 {
                continue;
            }
            let qlen = match &chunks[ci][0] {
                WikitextToken::Quote(q) => q.len(),
                _ => continue,
            };

            match qlen {
                2 => match state {
                    State::I => {
                        replace_quote_chunk(chunks, ci, vec![italic_close()]);
                        state = State::Empty;
                    }
                    State::BI => {
                        // Annoying: close italic, then close+reopen bold
                        replace_quote_chunk(chunks, ci, vec![italic_close()]);
                        state = State::B;
                    }
                    State::IB => {
                        replace_quote_chunk(
                            chunks,
                            ci,
                            vec![bold_close(), italic_close(), bold_open()],
                        );
                        state = State::B;
                    }
                    State::Both(i) => {
                        // Deferred opening: insert both opens at the 5-quote position,
                        // then close italic now
                        replace_quote_chunk(chunks, i as usize, vec![bold_open(), italic_open()]);
                        replace_quote_chunk(chunks, ci, vec![italic_close()]);
                        state = State::B;
                    }
                    State::Empty | State::B => {
                        replace_quote_chunk(chunks, ci, vec![italic_open()]);
                        state = if state == State::B {
                            State::BI
                        } else {
                            State::I
                        };
                    }
                },
                3 => match state {
                    State::B => {
                        replace_quote_chunk(chunks, ci, vec![bold_close()]);
                        state = State::Empty;
                    }
                    State::IB => {
                        replace_quote_chunk(chunks, ci, vec![bold_close()]);
                        state = State::I;
                    }
                    State::BI => {
                        replace_quote_chunk(
                            chunks,
                            ci,
                            vec![italic_close(), bold_close(), italic_open()],
                        );
                        state = State::I;
                    }
                    State::Both(i) => {
                        replace_quote_chunk(chunks, i as usize, vec![italic_open(), bold_open()]);
                        replace_quote_chunk(chunks, ci, vec![bold_close()]);
                        state = State::I;
                    }
                    State::Empty | State::I => {
                        replace_quote_chunk(chunks, ci, vec![bold_open()]);
                        state = if state == State::I {
                            State::IB
                        } else {
                            State::B
                        };
                    }
                },
                5 => match state {
                    State::B => {
                        replace_quote_chunk(chunks, ci, vec![bold_close(), italic_open()]);
                        state = State::I;
                    }
                    State::I => {
                        replace_quote_chunk(chunks, ci, vec![italic_close(), bold_open()]);
                        state = State::B;
                    }
                    State::BI => {
                        replace_quote_chunk(chunks, ci, vec![italic_close(), bold_close()]);
                        state = State::Empty;
                    }
                    State::IB => {
                        replace_quote_chunk(chunks, ci, vec![bold_close(), italic_close()]);
                        state = State::Empty;
                    }
                    State::Both(i) => {
                        replace_quote_chunk(chunks, i as usize, vec![italic_open(), bold_open()]);
                        replace_quote_chunk(chunks, ci, vec![bold_close(), italic_close()]);
                        state = State::Empty;
                    }
                    State::Empty => {
                        last_both = ci as isize;
                        state = State::Both(ci);
                        // Don't replace the chunk yet — we'll do it when we know the order
                    }
                },
                _ => {}
            }

            qi += 1;
        }

        // Close remaining open tags
        match state {
            State::Both(i) => {
                replace_quote_chunk(chunks, i, vec![bold_open(), italic_open()]);
                state = State::BI;
            }
            State::B | State::IB => {
                // Append close to the last non-quote chunk
                let last = chunks.len() - 1;
                if last % 2 == 1 {
                    chunks.push(Vec::new());
                }
                chunks.last_mut().unwrap().push(bold_close());
            }
            State::I | State::BI => {
                let last = chunks.len() - 1;
                if last % 2 == 1 {
                    chunks.push(Vec::new());
                }
                chunks.last_mut().unwrap().push(italic_close());
            }
            State::Empty => {}
        }
        if matches!(state, State::BI) {
            chunks.last_mut().unwrap().push(bold_close());
        }

        // Flatten chunks into result
        chunks.iter().flatten().cloned().collect()
    }
}

fn replace_quote_chunk(chunks: &mut [Vec<WikitextToken>], ci: usize, new: Vec<WikitextToken>) {
    chunks[ci] = new;
}

fn bold_open() -> WikitextToken {
    WikitextToken::BoldOpen
}
fn bold_close() -> WikitextToken {
    WikitextToken::BoldClose
}
fn italic_open() -> WikitextToken {
    WikitextToken::ItalicOpen
}
fn italic_close() -> WikitextToken {
    WikitextToken::ItalicClose
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
        // Parsoid: '''''foo''''' → <i><b>foo</b></i>
        // 5-quote in Empty → Both(i), matching 5-quote → bold_close, italic_close
        // End-of-line cleanup for Both → bold_open, italic_open (B+I then I then B)
        assert!(
            matches!(result[0], WikitextToken::ItalicOpen),
            "got {:?}",
            result
        );
        assert!(
            matches!(result[1], WikitextToken::BoldOpen),
            "got {:?}",
            result
        );
        assert!(matches!(result[2], WikitextToken::Text(_)));
        assert!(
            matches!(result[3], WikitextToken::BoldClose),
            "got {:?}",
            result
        );
        assert!(
            matches!(result[4], WikitextToken::ItalicClose),
            "got {:?}",
            result
        );
    }

    #[test]
    fn test_two_three_quotes() {
        // ''foo''' → <i>foo'</i>
        let tokens = vec![
            WikitextToken::Quote("''".to_string()),
            WikitextToken::Text("foo".to_string()),
            WikitextToken::Quote("'''".to_string()),
        ];
        let result = QuoteTransformer::resolve_line_quotes(&tokens);
        // 2-quote opens italic (state I), 3-quote adds bold (state IB).
        // End cleanup: IB → bold_close, then BI → italic_close, then bold_close? No.
        // Actually: State::IB cleanup → bold_close. Then State::I not matched.
        assert!(matches!(result[0], WikitextToken::ItalicOpen));
        assert!(matches!(result[1], WikitextToken::Text(_)));
        // 3-quote in I state → bold_open, state IB
        // End: IB → bold_close, then I → italic_close
        assert!(matches!(result[2], WikitextToken::BoldOpen));
        assert!(matches!(result[3], WikitextToken::BoldClose));
        assert!(matches!(result[4], WikitextToken::ItalicClose));
    }
}
