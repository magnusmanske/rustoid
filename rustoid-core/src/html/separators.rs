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

/// Newline constraints for a separator. Mirrors the `min`/`max` pair.
/// The `constraintInfo` side-band (`sepType`, `nodeA`, `nodeB`, `onSOL`,
/// `forceSOL`) from PHP `Separators::updateSeparatorConstraints` is kept
/// separately on [`SeparatorData`] (it is per-pair, not merged, and is set
/// afresh on every `updateSeparatorConstraints` call).
#[derive(Debug, Clone, Default)]
pub struct Constraints {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

/// The topological relationship between the two nodes a separator sits between
/// (mirrors PHP `$sepType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SepType {
    /// `nodeA` is the parent of `nodeB`.
    ParentChild,
    /// `nodeB` is the parent of `nodeA`.
    ChildParent,
    /// `nodeA` and `nodeB` are siblings.
    Sibling,
}

/// The `constraintInfo` side-band collected by
/// `Separators::updateSeparatorConstraints` (PHP keeps this inside the
/// constraints array; it is stored as a separate field here because it is
/// overwritten on each call rather than merged).
#[derive(Debug, Clone, Default)]
pub struct ConstraintInfo {
    pub sep_type: Option<SepType>,
    pub node_a: Option<NodeId>,
    pub node_b: Option<NodeId>,
    pub on_sol: bool,
    pub force_sol: bool,
}

/// The separator information `SerializerState` accumulates (`$state->sep`).
#[derive(Debug, Clone, Default)]
pub struct SeparatorData {
    /// Newline constraints (`null` when none).
    pub constraints: Option<Constraints>,
    /// The `constraintInfo` for the most recent node pair (unmerged side-band).
    pub constraint_info: ConstraintInfo,
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
        tree: &DomTree,
        sep: &str,
        constraints: &Constraints,
        at_start_of_output: bool,
        constraint_info: &ConstraintInfo,
    ) -> String {
        let sep_nl_count = sep_nl_count(sep);
        let mut min_nls = constraints.min.unwrap_or(0);

        if at_start_of_output && min_nls > 0 {
            min_nls = min_nls.saturating_sub(1);
        }

        let mut out = sep.to_string();
        if min_nls > 0 && sep_nl_count < min_nls {
            let extra_nls = min_nls - sep_nl_count;
            let nl_buf = "\n".repeat(extra_nls);

            // Two best-guess heuristics for where to add the newlines relative
            // to nodeA/nodeB (faithful to `Separators::makeSeparator`):
            //
            // 1. parent-child: when nodeA's first child is not a content node,
            //    the separator was plucked from the child's constraints, so the
            //    newlines should be prepended.
            // 2. sibling: when nodeB is a literal-HTML element, nodeA forced the
            //    newline, so it should be emitted right after nodeA (i.e.
            //    prepended to nodeB's separator).
            let prepend = match constraint_info.sep_type {
                Some(SepType::ParentChild) => {
                    let first_child_is_content = constraint_info
                        .node_a
                        .and_then(|na| crate::html::dom_tree::first_non_deleted_child(tree, na))
                        .is_some_and(|c| crate::html::dom_tree::is_content_node(tree, c));
                    let node_b_is_child_table = constraint_info.node_b.is_some_and(|nb| {
                        crate::wikitext::consts::child_table_tags()
                            .contains(&crate::html::dom_utils::node_name(tree.node(nb)))
                            && !crate::html::wts_utils::is_literal_html_node(tree.node(nb))
                    });
                    !first_child_is_content && !node_b_is_child_table
                }
                Some(SepType::Sibling) => constraint_info
                    .node_b
                    .is_some_and(|nb| crate::html::wts_utils::is_literal_html_node(tree.node(nb))),
                _ => false,
            };

            if prepend {
                out = format!("{nl_buf}{out}");
            } else {
                out.push_str(&nl_buf);
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
    /// constraints and source. Faithful to `Separators::buildSep`: in selser
    /// mode it first attempts to recover the exact separator from original
    /// source via DSR offsets, falling back to trimmed-whitespace recovery and
    /// then to constraint-based construction.
    pub fn build_sep(state: &mut SerializerState, tree: &DomTree, node: NodeId) -> Option<String> {
        let constraints = state.separator.constraints.clone();
        let constraint_info = state.separator.constraint_info.clone();

        // In selser mode, first attempt to recover the exact separator from
        // original source; this also mutates `state.separator.src` (recovered
        // text is stashed there for the trimmed-whitespace fallback below).
        let recovered = if state.selser_mode {
            Self::build_sep_selser(state, tree, node)
        } else {
            None
        };

        let src = state.separator.src.clone().unwrap_or_default();
        state.separator.src = None;

        let mut sep = recovered;

        // If the selser recovery didn't produce a separator but left buffered
        // source, reconstruct via constraints (mirrors the `makeSeparator`
        // fallback at the end of PHP's `buildSep`).
        if sep.is_none() && (constraints.is_some() || !src.is_empty()) {
            if let Some(c) = constraints {
                sep = Some(Self::make_separator(
                    tree,
                    &src,
                    &c,
                    state.at_start_of_output,
                    &constraint_info,
                ));
            } else {
                sep = Some(src);
            }
        }

        // Wrap leading whitespace that would otherwise trigger indent-pre.
        sep.map(|s| Self::make_sep_indent_pre_safe(state, tree, &s, &constraint_info))
    }

    /// The selser DSR-recovery branch of `Separators::buildSep`. Recovers the
    /// exact separator between `prev_node` and `node` from the revision source
    /// using their DSR offsets, or via trimmed-whitespace heuristics. Returns
    /// `Some(sep)` when recovered, or `None` to fall through to the constraint
    /// path (leaving any recovered text in `state.separator.src`).
    fn build_sep_selser(
        state: &mut SerializerState,
        tree: &DomTree,
        node: NodeId,
    ) -> Option<String> {
        let prev_node = state.separator.last_source_node?;
        if node == prev_node {
            return None;
        }

        // `$origSepNeededAndUsable` — only recover from source when the edited
        // context has valid DSR on both sides and neither node is adjacent to a
        // deleted block node.
        let orig_sep_usable = !state.in_inserted_content
            && !crate::html::wts_utils::next_to_deleted_block_node_in_wt(tree, prev_node, true)
            && !crate::html::wts_utils::next_to_deleted_block_node_in_wt(tree, node, false)
            && crate::html::wts_utils::orig_src_valid_in_edited_context(state, tree, prev_node)
            && crate::html::wts_utils::orig_src_valid_in_edited_context(state, tree, node);

        let mut recovered: Option<String> = None;

        if orig_sep_usable {
            let dsr_a = Self::dsr_for_source_node(tree, prev_node, node, false);
            let dsr_b = Self::dsr_for_source_node(tree, node, prev_node, true);

            if let (Some(a), Some(b)) = (dsr_a, dsr_b)
                && state.is_valid_dsr(Some(&a), false) && state.is_valid_dsr(Some(&b), false) {
                    recovered = Self::sep_from_dsr(state, &a, &b);
                }
        }

        // Trimmed-whitespace fallback (mirrors the `$sep === null` block at the
        // end of PHP `buildSep`), stashing recovered text in `state.separator.src`.
        let sep_type = state.separator.constraint_info.sep_type;
        match sep_type {
            Some(SepType::ParentChild) => {
                let ws = state.recover_trimmed_whitespace(tree, node, true);
                if let Some(ws) = ws {
                    state.separator.src = Some(format!(
                        "{ws}{}",
                        state.separator.src.as_deref().unwrap_or("")
                    ));
                }
            }
            Some(SepType::ChildParent) => {
                let ws = state.recover_trimmed_whitespace(tree, node, false);
                if let Some(ws) = ws {
                    match &mut state.separator.src {
                        Some(s) => s.push_str(&ws),
                        None => state.separator.src = Some(ws),
                    }
                }
            }
            _ => {}
        }

        recovered
    }

    /// Resolve the DSR (with auto-inserted tag widths nulled) of the node on one
    /// side of a separator. `is_node_b` selects the `$node` (B) side vs the
    /// `$prevNode` (A) side. Faithful to the `$dsrA`/`$dsrB` computation in
    /// PHP `buildSep`.
    fn dsr_for_source_node(
        tree: &DomTree,
        id: NodeId,
        other: NodeId,
        is_node_b: bool,
    ) -> Option<crate::html::dsr::DomSourceRange> {
        let n = tree.node(id);
        match &n.kind {
            crate::dom::node::NodeKind::Element(_) => {
                // A vs B: the parent/child relationship matters for the walking.
                if is_node_b && tree.parent(other) == Some(id) {
                    // `$node` is parent of `$prevNode`: walk up while it has no
                    // usable DSR and is a last child.
                    let mut cur = id;
                    loop {
                        if tree.next_sibling(cur).is_some()
                            || crate::html::dom_utils::at_the_top(tree, cur)
                        {
                            break;
                        }
                        let dsr = crate::html::wts_utils::get_dsr(tree.node(cur));
                        if dsr.is_some()
                            && dsr
                                .as_ref()
                                .is_some_and(|d| d.start.is_some() && d.end.is_some())
                        {
                            break;
                        }
                        let Some(parent) = tree.parent(cur) else {
                            break;
                        };
                        cur = parent;
                    }
                    Self::handle_auto_inserted(tree.node(cur))
                } else {
                    Self::handle_auto_inserted(n)
                }
            }
            crate::dom::node::NodeKind::Text(_) | crate::dom::node::NodeKind::Comment(_) => {
                // Text/comment: extrapolate DSR from the previous element sibling
                // (or the parent, for the last-child case).
                Self::extrapolate_dsr_for_text(tree, id, n, is_node_b, other)
            }
            crate::dom::node::NodeKind::Document => None,
        }
    }

    /// `Separators::handleAutoInserted` — clone the node's DSR, nulling the
    /// open/close width when the corresponding tag was auto-inserted.
    fn handle_auto_inserted(
        node: &crate::dom::node::Node,
    ) -> Option<crate::html::dsr::DomSourceRange> {
        let dp = node.dp.as_ref()?;
        let mut dsr = crate::html::wts_utils::get_dsr(node)?;
        // Note: `auto_inserted_start`/`auto_inserted_end` live on the
        // serializer-side DSR model via `tokens_v2`; null the widths to match.
        if dp.auto_inserted_start {
            dsr.open_width = None;
        }
        if dp.auto_inserted_end {
            dsr.close_width = None;
        }
        Some(dsr)
    }

    /// Extrapolate a DSR for a text/comment node from its previous element
    /// sibling (or parent), faithful to the `$dsrA` text/comment branch of PHP
    /// `buildSep`.
    fn extrapolate_dsr_for_text(
        tree: &DomTree,
        id: NodeId,
        n: &crate::dom::node::Node,
        _is_node_b: bool,
        _other: NodeId,
    ) -> Option<crate::html::dsr::DomSourceRange> {
        // Check if `id` is the last child of a zero-width element and use that
        // parent's DSR instead (typical case: text in p).
        if tree.next_sibling(id).is_none()
            && let Some(parent) = tree.parent(id) {
                let parent_node = tree.node(parent);
                if matches!(parent_node.kind, crate::dom::node::NodeKind::Element(_))
                    && crate::html::wts_utils::get_dsr(parent_node)
                        .as_ref()
                        .and_then(|d| d.close_width)
                        == Some(0)
                {
                    return Self::handle_auto_inserted(parent_node);
                }
            }

        // Can we extrapolate DSR from the previous element sibling? Yes, if the
        // parent didn't have its children edited.
        if let Some(prev) = tree.prev_sibling(id)
            && matches!(tree.node(prev).kind, crate::dom::node::NodeKind::Element(_))
            && let Some(parent) = tree.parent(id)
            && !crate::html::diff_utils::DiffUtils::direct_children_changed(tree.node(parent))
        {
            let end_dsr = crate::html::wts_utils::get_dsr(tree.node(prev)).and_then(|d| d.end);
            if let Some(end) = end_dsr {
                let correction = match &n.kind {
                    crate::dom::node::NodeKind::Comment(c) => {
                        let unclosed = Self::has_unclosed_comment_prev(tree, id);
                        crate::html::wts_utils::decoded_comment_length(c, unclosed)
                    }
                    crate::dom::node::NodeKind::Text(t) => t.len(),
                    _ => 0,
                };
                return Some(crate::html::dsr::DomSourceRange {
                    start: Some(end),
                    end: Some(end + correction),
                    source: None,
                    open_width: Some(0),
                    close_width: Some(0),
                    leading_ws: 0,
                    trailing_ws: 0,
                });
            }
        }
        None
    }

    /// Whether the comment at `id` is preceded by an `mw:Placeholder/UnclosedComment`
    /// meta (which shortens the comment delimiter length). Reused from
    /// `selser.rs`.
    fn has_unclosed_comment_prev(tree: &DomTree, id: NodeId) -> bool {
        let Some(prev) = tree.prev_sibling(id) else {
            return false;
        };
        crate::html::dom_utils::has_type_of(tree.node(prev), "mw:Placeholder/UnclosedComment")
    }

    /// Extract the separator between two DSR ranges, faithful to the
    /// containment-relationship switch in PHP `buildSep` + `isValidSep`.
    fn sep_from_dsr(
        state: &SerializerState,
        dsr_a: &crate::html::dsr::DomSourceRange,
        dsr_b: &crate::html::dsr::DomSourceRange,
    ) -> Option<String> {
        use crate::html::dsr::SourceRange;
        let a_start = dsr_a.start.unwrap_or(0);
        let a_end = dsr_a.end.unwrap_or(0);
        let b_start = dsr_b.start.unwrap_or(0);
        let b_end = dsr_b.end.unwrap_or(0);
        // The plain source-range views (start/end) that PHP treats DomSourceRange
        // as via `SourceRange::to`/`openRange`/`closeRange`.
        let a = SourceRange::with_source(dsr_a.start, dsr_a.end, dsr_a.source.clone());
        let b = SourceRange::with_source(dsr_b.start, dsr_b.end, dsr_b.source.clone());

        let sep = if a_start <= b_start {
            if b_end <= a_end {
                if a_start == b_start && a_end == b_end {
                    // Same range: no separator between them.
                    Some(String::new())
                } else if dsr_a.open_width.is_some() && state.is_valid_dsr(Some(dsr_a), true) {
                    // B in A, parent→child.
                    state.get_orig_src(&dsr_a.open_range().to(&b))
                } else {
                    None
                }
            } else if a_end <= b_start {
                // B following A (siblings).
                state.get_orig_src(&a.to(&b))
            } else if dsr_b.close_width.is_some() && state.is_valid_dsr(Some(dsr_b), true) {
                // A in B, child→parent.
                state.get_orig_src(&a.to(&dsr_b.close_range()))
            } else {
                None
            }
        } else if a_end <= b_end {
            if dsr_b.close_width.is_some() && state.is_valid_dsr(Some(dsr_b), true) {
                // A in B, child→parent.
                state.get_orig_src(&a.to(&dsr_b.close_range()))
            } else {
                None
            }
        } else {
            None
        };

        // Reset if the recovered separator is not valid wikitext separator text.
        match sep {
            Some(s) if crate::html::wts_utils::is_valid_sep(&s) => Some(s),
            _ => None,
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
    /// Faithful to `Separators::makeSepIndentPreSafe` (non-selser-relevant
    /// branches; `inPHPBlock`/`inIndentPre` mirror `$state->inPHPBlock`/
    /// `$state->inIndentPre`).
    pub fn make_sep_indent_pre_safe(
        state: &mut SerializerState,
        tree: &DomTree,
        sep: &str,
        constraint_info: &ConstraintInfo,
    ) -> String {
        let sep_type = constraint_info.sep_type;
        let node_a = constraint_info.node_a;
        let node_b = constraint_info.node_b;
        let force_sol = constraint_info.force_sol && sep_type != Some(SepType::ChildParent);

        // Ex: "<div>foo</div>\n <span>bar</span>". We also test onSOL state to
        // deal with // <ul> <li>foo</li></ul> and strip the leading space before
        // non-indent-pre-safe tags.
        if !state.in_php_block
            && !state.in_indent_pre
            && indent_pre_ws_in_sep_matches(sep)
            && (sep.contains('\n') || constraint_info.on_sol || force_sol)
        {
            let mut is_indent_pre_safe = false;

            if node_b.is_some_and(|nb| {
                crate::html::wts_utils::preceding_space_suppresses_indent_pre(tree, nb, node_b)
            }) {
                is_indent_pre_safe = true;
            } else if sep_type == Some(SepType::Sibling)
                || node_a.is_some_and(|na| crate::html::dom_utils::at_the_top(tree, na))
            {
                // Walk past sol-transparent nodes in the right-sibling chain of
                // nodeB till we establish indent-pre safety.
                let mut nb = node_b;
                while let Some(n) = nb {
                    if crate::html::dom_tree::is_diff_marker(tree, n)
                        || crate::html::wts_utils::emits_sol_transparent_single_line_wt(
                            tree.node(n),
                        )
                    {
                        nb = tree.next_sibling(n);
                    } else {
                        break;
                    }
                }
                is_indent_pre_safe = nb.is_none_or(|n| {
                    crate::html::wts_utils::preceding_space_suppresses_indent_pre(tree, n, node_b)
                });
            }

            // Check whether nodeB is nested inside an element that suppresses
            // indent-pres.
            if let Some(nb) = node_b
                && !is_indent_pre_safe
                && !crate::html::dom_utils::at_the_top(tree, nb)
            {
                let mut parent_b = tree.parent(nb);
                while parent_b.is_some_and(|p| {
                    crate::html::wts_utils::is_zero_width_wikitext_elt(tree.node(p))
                }) {
                    parent_b = parent_b.and_then(|p| tree.parent(p));
                }

                // The token stream paragraph wrapper tracks this with
                // `$inBlockquote`.
                is_indent_pre_safe = parent_b.is_some_and(|p| {
                    crate::html::dom_utils::has_name_or_has_ancestor_of_name(tree, p, "blockquote")
                });

                // First scope wins.
                while let Some(p) = parent_b {
                    if is_indent_pre_safe || crate::html::dom_utils::at_the_top(tree, p) {
                        break;
                    }
                    let name = crate::html::dom_utils::node_name(tree.node(p));
                    if crate::wikitext::token_utils::tag_opens_block_scope(&name)
                        && (name != "p"
                            || crate::html::wts_utils::is_literal_html_node(tree.node(p)))
                    {
                        is_indent_pre_safe = true;
                        break;
                    } else if crate::wikitext::token_utils::tag_closes_block_scope(&name) {
                        break;
                    }
                    parent_b = tree.parent(p);
                }
            }

            let strip_leading_space = (constraint_info.on_sol || force_sol)
                && node_b.is_some_and(|nb| {
                    !crate::html::wts_utils::is_literal_html_node(tree.node(nb))
                        && crate::wikitext::consts::html_tags_requiring_sol_context()
                            .contains(&crate::html::dom_utils::node_name(tree.node(nb)))
                });
            if !is_indent_pre_safe || strip_leading_space {
                // Wrap non-nl ws from last line, but preserve comments. This
                // avoids triggering indent-pres.
                return wrap_indent_pre_ws(sep, strip_leading_space, state);
            }
        }

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
        let (sep_type, a_cons, b_cons) = if tree.parent(node_b) == Some(node_a) {
            // parent-child: nodeA is parent of nodeB.
            let a = handler_a.first_child(tree, node_a, node_b, state);
            let b = handler_b.before(tree, node_b, node_a, state);
            (SepType::ParentChild, a, b)
        } else if tree.parent(node_a) == Some(node_b) {
            // child-parent: nodeB is parent of nodeA.
            let a = handler_a.after(tree, node_a, node_b, state);
            let b = handler_b.last_child(tree, node_b, node_a, state);
            (SepType::ChildParent, a, b)
        } else {
            // sibling.
            let a = handler_a.after(tree, node_a, node_b, state);
            let b = handler_b.before(tree, node_b, node_a, state);
            (SepType::Sibling, a, b)
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

        // The per-pair side-band (mirrors setting `$state->sep->constraints
        // ['constraintInfo']` at the end of `updateSeparatorConstraints`).
        state.separator.constraint_info = ConstraintInfo {
            sep_type: Some(sep_type),
            node_a: Some(node_a),
            node_b: Some(node_b),
            on_sol: state.on_sol,
            force_sol: handler_b.force_sol(),
        };
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

/// Does `sep` match `INDENT_PRE_WS_IN_SEP_REGEXP =
/// /^((?: *\n|COMMENT)*)( +)([^\n]*)$/D`? Returns `(spaces_start, tail_start)`
/// when it matches, else `None`.
///
/// The first group consumes runs of ` *\n` or comments greedily from the start;
/// the second group is one-or-more spaces; the third is the remaining
/// non-newline tail up to end-of-string.
fn indent_pre_ws_in_sep_match(sep: &str) -> Option<(usize, usize)> {
    let bytes = sep.as_bytes();
    let mut i = 0;
    // Greedy first group: `( *\n | comment)*`. A comment always consumes past
    // its `-->`; a ` *\n` consumes spaces then a newline.
    loop {
        if let Some(end) = match_comment_at(sep, i) {
            i = end;
            continue;
        }
        // Match ` *\n`.
        let mut j = i;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'\n' {
            i = j + 1;
            continue;
        }
        break;
    }
    // Second group: ` +` (one-or-more spaces).
    let spaces_start = i;
    let mut j = i;
    while j < bytes.len() && bytes[j] == b' ' {
        j += 1;
    }
    if j == spaces_start {
        return None;
    }
    // Third group: `[^\n]*` to end (no newline allowed in the tail).
    if sep[j..].contains('\n') {
        return None;
    }
    Some((spaces_start, j))
}

/// Whether `sep` matches the indent-pre whitespace regex (i.e. it has leading
/// spaces on its last line).
fn indent_pre_ws_in_sep_matches(sep: &str) -> bool {
    indent_pre_ws_in_sep_match(sep).is_some()
}

/// Wrap the leading spaces captured by `INDENT_PRE_WS_IN_SEP_REGEXP` in
/// `<nowiki>…</nowiki>` (or strip them when `strip_leading_space`), preserving
/// the prefix (newlines/comments) and the trailing tail. Faithful to the
/// `preg_replace_callback` in `Separators::makeSepIndentPreSafe`.
fn wrap_indent_pre_ws(sep: &str, strip_leading_space: bool, state: &mut SerializerState) -> String {
    let Some((spaces_start, tail_start)) = indent_pre_ws_in_sep_match(sep) else {
        return sep.to_string();
    };
    let prefix = &sep[..spaces_start];
    let spaces = &sep[spaces_start..tail_start];
    let tail = &sep[tail_start..];

    let middle = if strip_leading_space {
        String::new()
    } else {
        // Since we nowiki-ed, we are no longer in SOL state.
        state.on_sol = false;
        state.has_indent_pre_nowikis = true;
        format!("<nowiki>{spaces}</nowiki>")
    };
    format!("{prefix}{middle}{tail}")
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
    use crate::dom::node::Node;

    fn mk_tree() -> DomTree {
        DomTree::new(Node::document())
    }

    fn msep(tree: &DomTree, sep: &str, c: &Constraints, at_start: bool) -> String {
        Separators::make_separator(tree, sep, c, at_start, &ConstraintInfo::default())
    }

    #[test]
    fn test_make_separator_min_newlines() {
        let tree = mk_tree();
        let c = Constraints {
            min: Some(2),
            max: Some(2),
        };
        assert_eq!(msep(&tree, "", &c, false), "\n\n");
        assert_eq!(msep(&tree, "\n", &c, false), "\n\n");
    }

    #[test]
    fn test_make_separator_at_start_of_output() {
        let tree = mk_tree();
        let c = Constraints {
            min: Some(2),
            max: Some(2),
        };
        // The first newline is skipped at start-of-output.
        assert_eq!(msep(&tree, "", &c, true), "\n");
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
        let tree = mk_tree();
        // A comment whose internal newline would exceed the max must not be
        // stripped (only *real* separator newlines are counted/stripped).
        let c = Constraints {
            min: Some(0),
            max: Some(0),
        };
        assert_eq!(msep(&tree, "<!-- cmt\n -->", &c, false), "<!-- cmt\n -->");
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

    #[test]
    fn test_indent_pre_ws_match() {
        // Leading spaces on the last line.
        assert_eq!(indent_pre_ws_in_sep_match("\n "), Some((1, 2)));
        // Comment followed by spaces on the last line.
        assert_eq!(indent_pre_ws_in_sep_match("<!--c--> "), Some((8, 9)));
        // No trailing space -> no match.
        assert_eq!(indent_pre_ws_in_sep_match("\nx"), None);
        assert_eq!(indent_pre_ws_in_sep_match("x"), None);
        // Spaces with a newline in the tail -> no match.
        assert_eq!(indent_pre_ws_in_sep_match(" \n"), None);
    }

    #[test]
    fn test_wrap_indent_pre_ws() {
        let mut st = SerializerState::new();
        // Wraps the leading spaces in nowiki, preserving the newline prefix.
        assert_eq!(
            wrap_indent_pre_ws("\n ", false, &mut st),
            "\n<nowiki> </nowiki>"
        );
        assert!(st.has_indent_pre_nowikis);
        assert!(!st.on_sol);

        // With strip_leading_space, the space is removed.
        let mut st2 = SerializerState::new();
        assert_eq!(wrap_indent_pre_ws("\n ", true, &mut st2), "\n");
        assert!(!st2.has_indent_pre_nowikis);
    }

    #[test]
    fn test_make_separator_sibling_prepends_for_html_node() {
        // sibling + literal-HTML nodeB => newlines prepended.
        let tree = mk_tree();
        let c = Constraints {
            min: Some(1),
            max: Some(1),
        };
        let info = ConstraintInfo {
            sep_type: Some(SepType::Sibling),
            node_a: None,
            node_b: None,
            on_sol: false,
            force_sol: false,
        };
        // With no node_b in the info (no literal-HTML check possible), newlines
        // are appended, matching the else branch.
        assert_eq!(
            Separators::make_separator(&tree, "", &c, false, &info),
            "\n"
        );
    }
}
