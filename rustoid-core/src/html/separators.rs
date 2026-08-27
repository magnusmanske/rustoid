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
    /// STUB: the comment-splitting regexp (`$splitRe`) and the `parent-child`/
    /// `sibling` heuristics depend on PCRE; this skeleton implements the min/max
    /// newline counting without the comment-line-aware splitting.
    pub fn make_separator(
        sep: &str,
        constraints: &Constraints,
        at_start_of_output: bool,
    ) -> String {
        let sep_nl_count = sep.chars().filter(|&c| c == '\n').count();
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
            // Strip excess newlines (naive: strip trailing newlines only).
            let excess = sep_nl_count.saturating_sub(max);
            if excess > 0 {
                let mut stripped = String::with_capacity(out.len());
                let mut seen = 0;
                for c in out.chars() {
                    if c == '\n' && seen >= max {
                        continue;
                    }
                    if c == '\n' {
                        seen += 1;
                    }
                    stripped.push(c);
                }
                out = stripped;
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
}
