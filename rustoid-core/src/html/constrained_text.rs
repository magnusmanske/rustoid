//! Constrained-text chunks for wikitext serialization (base classes).
//!
//! Faithful port of PHP Parsoid's
//! `src/Html2Wt/ConstrainedText/{ConstrainedText,State,Result}.php`. A
//! `ConstrainedText` is a chunk of output wikitext plus a pointer to the DOM
//! node that generated it, plus optional `prefix`/`suffix` escape strings that
//! subclasses supply to keep wikitext boundaries safe.
//!
//! The `escapeLine` entry point threads left/right context through a line of
//! chunks so each chunk can decide whether to emit `<nowiki>`-style escapes.

use crate::html::dom_tree::NodeId;

/// Result of escaping a single chunk: the (possibly escaped) text plus optional
/// prefix/suffix strings. Mirrors PHP's `ConstrainedText\Result`.
#[derive(Debug, Clone, Default)]
pub struct Result {
    pub text: String,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    /// Whether this chunk matches greedily (protects the left context from the
    /// next chunk's prefix).
    pub greedy: bool,
}

impl Result {
    pub fn new(text: impl Into<String>, prefix: Option<String>, suffix: Option<String>) -> Self {
        Self {
            text: text.into(),
            prefix,
            suffix,
            greedy: false,
        }
    }
}

/// Per-line escape context threaded through [`ConstrainedText::escape_line`].
/// Mirrors PHP's `ConstrainedText\State`.
#[derive(Debug, Clone)]
pub struct State {
    /// The fully-escaped text of chunks to the left (fed back as `leftContext`).
    pub left_context: String,
    /// The remaining raw text of chunks to the right.
    pub right_context: String,
    /// The position of the chunk currently being escaped.
    pub pos: usize,
}

impl State {
    pub fn new(line: &[ConstrainedText]) -> Self {
        let right_context: String = line.iter().map(|c| c.text.as_str()).collect();
        Self {
            left_context: String::new(),
            right_context,
            pos: 0,
        }
    }
}

/// A chunk of wikitext output. Mirrors PHP's `ConstrainedText\ConstrainedText`.
///
/// `node` is the `NodeId` (into the navigation `DomTree`) of the DOM node that
/// produced this chunk; PHP stores a `Node` reference directly, but our tree is
/// navigated by stable id.
#[derive(Debug, Clone)]
pub struct ConstrainedText {
    pub text: String,
    pub node: NodeId,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    /// Whether this chunk came from selective serialization (selser).
    pub selser: bool,
    /// Suppress separator emission before this chunk.
    pub no_sep: bool,
}

impl ConstrainedText {
    pub fn new(
        text: impl Into<String>,
        node: NodeId,
        prefix: Option<String>,
        suffix: Option<String>,
    ) -> Self {
        Self {
            text: text.into(),
            node,
            prefix,
            suffix,
            selser: false,
            no_sep: false,
        }
    }

    /// Coerce `text` (already a `ConstrainedText`, or a plain string) into a
    /// `ConstrainedText`. Mirrors `ConstrainedText::cast`.
    pub fn cast(text: impl Into<String>, node: NodeId) -> Self {
        Self::new(text, node, None, None)
    }

    /// Determine the escape prefix/suffix for this chunk given the line context.
    /// The base implementation applies no escaping. Mirrors `escape`.
    pub fn escape(&self, _state: &State) -> Result {
        Result::new(self.text.clone(), self.prefix.clone(), self.suffix.clone())
    }

    /// Simple equality (base-class text equality). Mirrors `equals`.
    pub fn equals(&self, other: &ConstrainedText) -> bool {
        self.text == other.text
    }

    /// Escape a line of chunks, threading left/right context so each chunk can
    /// insert `prefix`/`suffix` boundary escapes. Mirrors `escapeLine`.
    pub fn escape_line(line: &[ConstrainedText]) -> String {
        let mut state = State::new(line);
        let mut safe_left = String::new();
        while state.pos < line.len() {
            let chunk = &line[state.pos];
            // Drop this chunk's raw text from the right context.
            state.right_context = state
                .right_context
                .chars()
                .skip(chunk.text.chars().count())
                .collect();
            let escaped = chunk.escape(&state);
            if let Some(prefix) = &escaped.prefix {
                state.left_context.push_str(prefix);
            }
            state.left_context.push_str(&escaped.text);
            if let Some(suffix) = &escaped.suffix {
                state.left_context.push_str(suffix);
            }
            if escaped.greedy {
                safe_left.push_str(&state.left_context);
                state.left_context.clear();
            }
            state.pos += 1;
        }
        safe_left.push_str(&state.left_context);
        safe_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cast() {
        let ct = ConstrainedText::cast("hello", 3);
        assert_eq!(ct.text, "hello");
        assert_eq!(ct.node, 3);
        assert_eq!(ct.prefix, None);
        assert_eq!(ct.suffix, None);
        assert!(!ct.selser);
        assert!(!ct.no_sep);
    }

    #[test]
    fn test_escape_line_no_subclass_escapes() {
        // With only base chunks (no prefix/suffix), escape_line is a plain
        // concatenation.
        let line = vec![
            ConstrainedText::cast("foo", 1),
            ConstrainedText::cast("bar", 2),
            ConstrainedText::cast("baz", 3),
        ];
        assert_eq!(ConstrainedText::escape_line(&line), "foobarbaz");
    }

    #[test]
    fn test_state_builds_right_context() {
        let line = vec![
            ConstrainedText::cast("ab", 1),
            ConstrainedText::cast("cde", 2),
        ];
        let state = State::new(&line);
        assert_eq!(state.right_context, "abcde");
        assert_eq!(state.left_context, "");
        assert_eq!(state.pos, 0);
    }

    #[test]
    fn test_escape_line_strips_past_chunk_from_right_context() {
        // A chunk with a prefix exercises the prefix/suffix path (base `escape`
        // returns the prefix set on the chunk). Verify the right-context
        // advancement and prefix emission.
        let mut a = ConstrainedText::cast("ab", 1);
        a.prefix = Some("<nowiki>".to_string());
        let line = vec![a, ConstrainedText::cast("cd", 2)];
        assert_eq!(ConstrainedText::escape_line(&line), "<nowiki>abcd");
    }
}
