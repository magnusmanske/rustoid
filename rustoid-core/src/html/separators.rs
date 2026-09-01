//! Separators — faithful port of PHP Parsoid's `src/Html2Wt/Separators.php`.
//!
//! Computes the separator strings (whitespace/newlines/comments) that are
//! emitted *between* serialized nodes, based on newline constraints gathered
//! from the surrounding DOM handlers and on DSR-based source recovery.
//!
//! PHP's `Separators` is a class that holds a `$state` reference and reads/writes
//! the separator buffer (`$state->sep`). In Rust the separator *data* lives on
//! `SerializerState` (as `separator`), and `Separators` is a unit struct of
//! associated functions taking `&mut SerializerState`. This flattens the PHP
//! class/state split while preserving the exact data flow and logic.

use crate::html::dom_handler::DomHandler;
use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::serializer_state::SerializerState;

/// Newline constraints for a separator. Mirrors the `min`/`max` pair plus the
/// `constraintInfo` side-band (`sepType`, `nodeA`, `nodeB`, `onSOL`, `forceSOL`)
/// from `Separators::updateSeparatorConstraints`.
#[derive(Debug, Clone, Default)]
pub struct Constraints {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

/// The separator information `SerializerState` accumulates (`$state->sep`).
#[derive(Debug, Clone, Default)]
pub struct SeparatorData {
    /// Newline constraints (`null` when none).
    pub constraints: Option<Constraints>,
    /// Collected separator source text (whitespace/comments).
    pub src: Option<String>,
    /// The last DOM node that emitted a chunk (so separators aren't reused on
    /// consecutive `emitChunk` calls for the same node).
    pub last_source_node: Option<NodeId>,
}

/// The `Separators` algorithm, as associated functions over `&mut SerializerState`.
pub struct Separators;

impl Separators {
    /// Create a separator given a (possibly empty) source and newline
    /// constraints. Faithful to `Separators::getSepNlConstraints` +
    /// `mergeConstraints` + `makeSeparator` for the non-selser path.
    ///
    /// The newline count is computed *after* splitting the separator on
    /// wikitext comments and comment-only lines (`$splitRe` in PHP), so a
    /// newline that lives inside a comment does not count toward the separator's
    /// line count and is never stripped.
    pub fn make_separator(
        sep: &str,
        constraints: &Constraints,
        at_start_of_output: bool,
    ) -> String {
        let sep_nl_count = sep_nl_count(sep);
        let mut min_nls = constraints.min.unwrap_or(0);

        if at_start_of_output && min_nls > 0 {
            min_nls = min_nls.saturating_sub(1);
        }

        let mut out = sep.to_string();
        if min_nls > 0 && sep_nl_count < min_nls {
            for _ in 0..(min_nls - sep_nl_count) {
                out.push('\n');
            }
        } else if let Some(max) = constraints.max {
            // Strip excess newlines *outside* comments only (comment-only lines
            // and the newlines inside comments are preserved verbatim).
            let excess = sep_nl_count.saturating_sub(max);
            if excess > 0 {
                out = strip_nls_outside_comments(&out, excess);
            }
        }

        out
    }

    /// Build the separator to emit before `node`, based on the buffered
    /// constraints and source. Faithful to the *skeleton* of `Separators::buildSep`
    /// (DSR-based recovery is layered on once `getOrigSrc` is available).
    pub fn build_sep(state: &mut SerializerState, _node: NodeId) -> Option<String> {
        // In selser mode, recover the separator from source via DSR; for now,
        // fall back to the constraint-based construction.
        let constraints = state.separator.constraints.clone();
        let src = state.separator.src.clone().unwrap_or_default();
        state.separator.src = None;

        match constraints {
            Some(c) => Some(Self::make_separator(&src, &c, state.at_start_of_output)),
            None => {
                if src.is_empty() {
                    None
                } else {
                    Some(src)
                }
            }
        }
    }

    /// Merges two constraint sets (`Separators::mergeConstraints`).
    pub fn merge_constraints(old: Option<&Constraints>, new: &Constraints) -> Constraints {
        let (old_min, old_max) = match old {
            Some(c) => (c.min.unwrap_or(0), c.max.unwrap_or(2)),
            None => (0, 2),
        };
        let new_min = new.min.unwrap_or(0);
        let new_max = new.max.unwrap_or(2);
        let min = old_min.max(new_min);
        let mut max = old_max.min(new_max);
        if min > max {
            max = min;
        }
        Constraints {
            min: Some(min),
            max: Some(max),
        }
    }

    /// Safe separator handling for indent-pre: wraps leading whitespace on a
    /// newline in `<nowiki>` when it would otherwise trigger indent-pre.
    /// STUB: faithful wrapping is deferred; returns the separator unchanged.
    pub fn make_sep_indent_pre_safe(
        _state: &mut SerializerState,
        sep: &str,
        _constraints: &Constraints,
    ) -> String {
        sep.to_string()
    }

    /// Merge two newline-constraint sets (`Separators::getSepNlConstraints`).
    /// Resolves min/max conflicts (nodeB wins on conflict), defaulting max to 2.
    pub fn get_sep_nl_constraints(a: Option<&Constraints>, b: Option<&Constraints>) -> Constraints {
        let mut nl = Constraints {
            min: a.and_then(|c| c.min),
            max: a.and_then(|c| c.max),
        };

        if let Some(b) = b {
            if let Some(b_min) = b.min {
                if let Some(cur_max) = nl.max {
                    if cur_max < b_min {
                        // Conflict: nodeB wins.
                        nl.min = Some(b_min);
                        nl.max = Some(b_min);
                    } else {
                        nl.min = Some(nl.min.unwrap_or(0).max(b_min));
                    }
                } else {
                    nl.min = Some(nl.min.unwrap_or(0).max(b_min));
                }
            }
            if let Some(b_max) = b.max {
                if nl.min.unwrap_or(0) > b_max {
                    // Conflict: nodeB wins.
                    nl.min = Some(b_max);
                    nl.max = Some(b_max);
                } else {
                    nl.max = Some(nl.max.unwrap_or(b_max).min(b_max));
                }
            }
        }

        if nl.max.is_none() {
            nl.max = Some(2);
        }
        if nl.min.unwrap_or(0) > nl.max.unwrap() {
            nl.max = nl.min;
        }
        nl
    }

    /// Figure out the separator constraints between `node_a` and `node_b` and
    /// merge them into `state.separator.constraints`. Faithful to
    /// `Separators::updateSeparatorConstraints`.
    pub fn update_separator_constraints(
        state: &mut SerializerState,
        tree: &DomTree,
        node_a: NodeId,
        handler_a: &mut dyn DomHandler,
        node_b: NodeId,
        handler_b: &mut dyn DomHandler,
    ) {
        let (a_cons, b_cons) = if tree.parent(node_b) == Some(node_a) {
            // parent-child: nodeA is parent of nodeB.
            let a = handler_a.first_child(tree, node_a, node_b, state);
            let b = handler_b.before(tree, node_b, node_a, state);
            (a, b)
        } else if tree.parent(node_a) == Some(node_b) {
            // child-parent: nodeB is parent of nodeA.
            let a = handler_a.after(tree, node_a, node_b, state);
            let b = handler_b.last_child(tree, node_b, node_a, state);
            (a, b)
        } else {
            // sibling.
            let a = handler_a.after(tree, node_a, node_b, state);
            let b = handler_b.before(tree, node_b, node_a, state);
            (a, b)
        };

        let nl = Self::get_sep_nl_constraints(a_cons.as_ref(), b_cons.as_ref());
        match &mut state.separator.constraints {
            Some(existing) => {
                let merged = Self::merge_constraints(Some(existing), &nl);
                *existing = merged;
            }
            None => {
                state.separator.constraints = Some(nl);
            }
        }
    }
}

/// Match a wikitext comment `<!-- ... -->` starting at `s[i]`. Returns the index
/// just past the closing `-->` (the first `-->`), or `None`. Mirrors PHP's
/// `COMMENT_REGEXP_FRAGMENT = '<!--(?>[\s\S]*?-->)'` (non-greedy).
fn match_comment_at(s: &str, i: usize) -> Option<usize> {
    if !s[i..].starts_with("<!--") {
        return None;
    }
    let end = s[i + 4..].find("-->")?;
    Some(i + 4 + end + 3)
}

/// Match a run of one or more comment-only lines starting at `s[i]` (where
/// `s[i] == '\n'`). Mirrors PHP's `$splitRe` first alternative
/// `(?:\n(?:[ \t]*?COMMENT[ \t]*?)+(?=\n))+`. Returns the index of the final
/// lookahead newline (which is *not* consumed), or `None`.
fn match_comment_only_line_run(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if i >= bytes.len() || bytes[i] != b'\n' {
        return None;
    }
    let mut pos = i;
    let mut any = false;
    loop {
        if pos >= bytes.len() || bytes[pos] != b'\n' {
            break;
        }
        let mut p = pos + 1;
        let mut matched_comment_in_line = false;
        loop {
            while p < bytes.len() && matches!(bytes[p], b' ' | b'\t') {
                p += 1;
            }
            if s[p..].starts_with("<!--")
                && let Some(end) = match_comment_at(s, p)
            {
                p = end;
                matched_comment_in_line = true;
                continue;
            }
            break;
        }
        if !matched_comment_in_line {
            break;
        }
        while p < bytes.len() && matches!(bytes[p], b' ' | b'\t') {
            p += 1;
        }
        // Lookahead `\n` (not consumed): the run can continue only if the next
        // char is a newline.
        if p < bytes.len() && bytes[p] == b'\n' {
            any = true;
            pos = p;
        } else {
            break;
        }
    }
    if any { Some(pos) } else { None }
}

/// Split `sep` into alternating text / comment segments, mirroring
/// `preg_split( $splitRe, $sep )`. A "comment" segment is a standalone comment
/// or a run of comment-only lines (which the PHP parser ignores); a "text"
/// segment is everything else.
fn split_sep(sep: &str) -> Vec<(String, bool)> {
    let bytes = sep.as_bytes();
    let mut i = 0;
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut text_buf = String::new();
    let flush_text = |buf: &mut String, out: &mut Vec<(String, bool)>| {
        if !buf.is_empty() {
            out.push((std::mem::take(buf), false));
        }
    };
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            if let Some(end) = match_comment_only_line_run(sep, i) {
                flush_text(&mut text_buf, &mut out);
                out.push((sep[i..end].to_string(), true));
                i = end;
                continue;
            }
        } else if sep[i..].starts_with("<!--")
            && let Some(end) = match_comment_at(sep, i)
        {
            flush_text(&mut text_buf, &mut out);
            out.push((sep[i..end].to_string(), true));
            i = end;
            continue;
        }
        let c = sep[i..].chars().next().unwrap();
        text_buf.push(c);
        i += c.len_utf8();
    }
    flush_text(&mut text_buf, &mut out);
    out
}

/// Count the newlines in `sep` after splitting out comments and comment-only
/// lines (mirrors PHP's `substr_count( implode( preg_split( $splitRe, $sep ) ), "\n" )`).
fn sep_nl_count(sep: &str) -> usize {
    split_sep(sep)
        .into_iter()
        .filter(|(_, is_comment)| !is_comment)
        .map(|(text, _)| text.chars().filter(|&c| c == '\n').count())
        .sum()
}

/// Strip `n` newlines from the *text* (non-comment) segments of `sep`, leaving
/// comment segments (and comment-only lines) intact. Mirrors PHP's
/// `preg_split( capturing $splitRe )` + `preg_replace('/\n([^\n]*)/', '$1')` loop
/// that removes newlines only from the non-comment bits (dirty-diff heuristic).
fn strip_nls_outside_comments(sep: &str, n: usize) -> String {
    if n == 0 {
        return sep.to_string();
    }
    let segments = split_sep(sep);
    let mut remaining = n;
    let mut out = String::with_capacity(sep.len());
    for (text, is_comment) in segments {
        if is_comment {
            out.push_str(&text);
            continue;
        }
        if remaining == 0 {
            out.push_str(&text);
            continue;
        }
        // Remove up to `remaining` newlines from this text segment.
        let mut kept = String::with_capacity(text.len());
        for c in text.chars() {
            if c == '\n' && remaining > 0 {
                remaining -= 1;
            } else {
                kept.push(c);
            }
        }
        out.push_str(&kept);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_separator_min_newlines() {
        let c = Constraints {
            min: Some(2),
            max: Some(2),
        };
        assert_eq!(Separators::make_separator("", &c, false), "\n\n");
        assert_eq!(Separators::make_separator("\n", &c, false), "\n\n");
    }

    #[test]
    fn test_make_separator_at_start_of_output() {
        let c = Constraints {
            min: Some(2),
            max: Some(2),
        };
        // The first newline is skipped at start-of-output.
        assert_eq!(Separators::make_separator("", &c, true), "\n");
    }

    #[test]
    fn test_merge_constraints() {
        let a = Constraints {
            min: Some(1),
            max: Some(2),
        };
        let b = Constraints {
            min: Some(2),
            max: Some(2),
        };
        let m = Separators::merge_constraints(Some(&a), &b);
        assert_eq!(m.min, Some(2));
        assert_eq!(m.max, Some(2));
    }

    #[test]
    fn test_sep_nl_count_ignores_comments() {
        // Newlines inside a comment don't count as separator newlines.
        assert_eq!(sep_nl_count("<!-- cmt\n -->"), 0);
        assert_eq!(sep_nl_count("<!--cmt-->"), 0);
        // A real newline outside a comment counts.
        assert_eq!(sep_nl_count("\n"), 1);
        assert_eq!(sep_nl_count("\n\n"), 2);
        // A comment-only line consumes its leading newline (the trailing `\n` is
        // a lookahead, so it survives): `a\n<!--c-->\nb` → `a\nb`.
        assert_eq!(sep_nl_count("a\n<!--c-->\nb"), 1);
        // A comment-only line (surrounded by newlines) contributes nothing.
        assert_eq!(sep_nl_count("\n<!--c-->\n"), 1);
    }

    #[test]
    fn test_make_separator_preserves_comment_newline() {
        // A comment whose internal newline would exceed the max must not be
        // stripped (only *real* separator newlines are counted/stripped).
        let c = Constraints {
            min: Some(0),
            max: Some(0),
        };
        assert_eq!(
            Separators::make_separator("<!-- cmt\n -->", &c, false),
            "<!-- cmt\n -->"
        );
    }

    #[test]
    fn test_strip_nls_outside_comments() {
        // Two real newlines, strip one: the comment stays intact.
        assert_eq!(strip_nls_outside_comments("<!--c-->\n\n", 1), "<!--c-->\n");
        // The newline inside the comment is never stripped.
        assert_eq!(
            strip_nls_outside_comments("<!-- c\n -->", 1),
            "<!-- c\n -->"
        );
    }
}
