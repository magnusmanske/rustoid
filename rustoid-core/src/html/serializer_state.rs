//! SerializerState — faithful port of PHP Parsoid's
//! `src/Html2Wt/SerializerState.php`.
//!
//! The mutable state object shared across the wikitext-serialization walk. It
//! accumulates the output string, tracks start-of-line / link / caption /
//! indent-pre context flags, buffers the current line of `ConstrainedText`
//! chunks, and owns the separator data that [`Separators`](super::separators)
//! reads and writes.
//!
//! Cross-cutting methods (escaping, `getOrigSrc`, selser mode) are stubbed where
//! they depend on the not-yet-ported `WikitextSerializer`/`DOMHandler` graph;
//! the flags they manage are present and faithfully named.

use crate::html::constrained_text::ConstrainedText;
use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::separators::SeparatorData;
use crate::html::single_line_context::SingleLineContext;

/// The `wikitext escaping handler` stack element: a context-specific escaping
/// *predicate* (returns `true` when `text` must be escaped). Faithful to PHP's
/// `SerializerState::$wteHandlerStack` of `?callable($state, $text, $opts): bool`.
///
/// The predicate captures the enclosing node (a `NodeId`) and receives, at call
/// time, the `tree` and the text-node context (`opts.node`) so it can navigate
/// exactly as the PHP closures do (via their bound `$liNode`/`$tdNode`/`$thNode`).
pub type WtEscapeHandler = Box<
    dyn Fn(
        &SerializerState,
        &str,
        &crate::html::wikitext_escape_handlers::EscapeOpts,
        &DomTree,
    ) -> bool,
>;

/// The mutable serializer state (port of `SerializerState`).
pub struct SerializerState {
    /// Separator info (constraints / collected source / last source node).
    pub separator: SeparatorData,

    /// Are we at the start of a new wikitext line?
    pub on_sol: bool,
    /// True until the first character has been output.
    pub at_start_of_output: bool,

    /// Serializing link content (children of `<a>`)?
    pub in_link: bool,
    /// Serializing caption content?
    pub in_caption: bool,
    /// Serializing an indent-pre tag?
    pub in_indent_pre: bool,
    /// Serializing a PHP-block tag?
    pub in_php_block: bool,
    /// Recursively serializing a template-generated attribute?
    pub in_attribute: bool,
    /// Serializing a subtree marked inserted (VE/CX)?
    pub in_inserted_content: bool,

    /// Did we introduce nowikis for indent-pre protection?
    pub has_indent_pre_nowikis: bool,
    /// Did we introduce nowikis for quote preservation?
    pub has_quote_nowikis: bool,
    /// Did we introduce `<nowiki />`s?
    pub has_self_closing_nowikis: bool,
    /// Did we introduce nowikis around `=.*=` text?
    pub has_heading_escapes: bool,

    /// Nesting level of wikitext tables.
    pub wiki_table_nesting: usize,

    /// Stack of wikitext escaping handlers.
    pub wte_handler_stack: Vec<WtEscapeHandler>,

    /// The current output line: concatenated text + chunks + first node.
    pub curr_line: CurrentLine,

    /// Single-line context stack.
    pub single_line_context: SingleLineContext,

    /// Redirect text to emit at the start of the file (`null` when none).
    pub redirect_text: Option<String>,

    /// The serialized output.
    pub out: String,

    /// Are we in selective-serialization (selser) mode?
    pub selser_mode: bool,

    /// (selser) Was the previous node unmodified by an edit?
    pub prev_node_unmodified: bool,
    /// (selser) Is the current node unmodified by an edit?
    pub curr_node_unmodified: bool,

    /// Should the wikitext escaping code run on the next emitted chunk?
    pub needs_escaping: bool,

    /// Whether the node whose chunk is about to be emitted is the last child of
    /// its parent (used by the trailing-`=` heading-escape heuristic). Set by
    /// `serialize_node` alongside `needs_escaping`; mirrors PHP computing
    /// `nextNonDeletedSibling($node) === null` inside `emitChunk`.
    pub is_last_child: bool,

    /// Fast path for special protected characters (from LanguageVariantHandler).
    pub protect: Option<String>,

    /// The current node being serialized.
    pub curr_node: Option<NodeId>,
    /// The previously serialized node.
    pub prev_node: Option<NodeId>,

    /// Open annotation ranges (`name → extended`).
    pub open_annotations: std::collections::HashMap<String, bool>,

    /// Trace-output log prefix.
    pub log_prefix: String,

    /// Does the input content version have trimmed-ws DSR (≥2.1.1)?
    pub have_trimmed_ws_dsr: bool,
}

/// The current output line (`$state->currLine`).
#[derive(Debug, Clone, Default)]
pub struct CurrentLine {
    /// Concatenated output text of the current line.
    pub text: String,
    /// The chunks comprising the current line.
    pub chunks: Vec<ConstrainedText>,
    /// The first DOM node processed on this line.
    pub first_node: Option<NodeId>,
}

impl SerializerState {
    /// Construct a fresh serializer state. `options` mirror the PHP constructor
    /// options (`onSOL`, `inPHPBlock`, `inAttribute`, `protect`, `selserMode`).
    pub fn new() -> Self {
        Self {
            separator: SeparatorData::default(),
            on_sol: true,
            at_start_of_output: true,
            in_link: false,
            in_caption: false,
            in_indent_pre: false,
            in_php_block: false,
            in_attribute: false,
            in_inserted_content: false,
            has_indent_pre_nowikis: false,
            has_quote_nowikis: false,
            has_self_closing_nowikis: false,
            has_heading_escapes: false,
            wiki_table_nesting: 0,
            wte_handler_stack: Vec::new(),
            curr_line: CurrentLine::default(),
            single_line_context: SingleLineContext::default(),
            redirect_text: None,
            out: String::new(),
            selser_mode: false,
            prev_node_unmodified: false,
            curr_node_unmodified: false,
            needs_escaping: false,
            is_last_child: false,
            protect: None,
            curr_node: None,
            prev_node: None,
            open_annotations: std::collections::HashMap::new(),
            log_prefix: "OUT:".to_string(),
            have_trimmed_ws_dsr: false,
        }
    }

    /// Append to the buffered separator source without changing `on_sol`.
    /// Faithful to `SerializerState::appendSep`.
    pub fn append_sep(&mut self, src: &str) {
        match &mut self.separator.src {
            Some(existing) => existing.push_str(src),
            None => self.separator.src = Some(src.to_string()),
        }
    }

    /// Cycle the "last source node" after processing a node. Faithful to
    /// `SerializerState::updateSep`.
    pub fn update_sep(&mut self, node: NodeId) {
        self.separator.last_source_node = Some(node);
    }

    /// Reset the separator data (`SerializerState::resetSep`).
    pub fn reset_sep(&mut self) {
        self.separator = SeparatorData::default();
    }

    /// Reset the current line (`SerializerState::resetCurrLine`).
    pub fn reset_curr_line(&mut self, node: Option<NodeId>) {
        self.curr_line = CurrentLine {
            text: String::new(),
            chunks: Vec::new(),
            first_node: node,
        };
    }

    /// Flush the buffered line, escaping its chunks, into `out`. Faithful to
    /// `SerializerState::flushLine`.
    pub fn flush_line(&mut self) {
        let chunks = std::mem::take(&mut self.curr_line.chunks);
        self.out.push_str(&ConstrainedText::escape_line(&chunks));
    }

    /// Reset the current line's text/chunks (keeping `first_node`). Faithful to
    /// the `chunks = []` half of `flushLine` (used in tests).
    pub fn clear_chunks(&mut self) {
        self.curr_line.chunks.clear();
    }

    /// Push a chunk onto the current line. Faithful to `SerializerState::pushToCurrLine`.
    pub fn push_to_curr_line(&mut self, chunk: ConstrainedText) {
        self.curr_line.text.push_str(&chunk.text);
        self.curr_line.chunks.push(chunk);
    }

    /// Detect a separator that introduces SOL state and, if it contains a
    /// newline, flush/reset the current line. Faithful to
    /// `SerializerState::sepIntroducedSOL`.
    pub fn sep_introduced_sol(&mut self, sep: &str, node: NodeId) {
        // Strip newlines in comments (a no-op in this skeleton; comment
        // handling is layered on with the escape handlers).
        let non_comment = sep
            .split("<!--")
            .next()
            .unwrap_or(sep)
            .split("-->")
            .next()
            .unwrap_or(sep);
        if non_comment.ends_with('\n') {
            self.on_sol = true;
        }
        if non_comment.contains('\n') {
            self.flush_line();
            self.reset_curr_line(Some(node));
        }
    }

    /// Emit a separator into the current line, handling single-line context and
    /// SOL detection. Faithful to `SerializerState::emitSep`.
    pub fn emit_sep(&mut self, sep: &str, node: NodeId) {
        let mut chunk = ConstrainedText::cast(sep, node);
        if self.single_line_context.enforced() {
            chunk.text = chunk.text.replace('\n', " ");
        }
        let text = chunk.text.clone();
        self.push_to_curr_line(chunk);
        self.sep_introduced_sol(&text, node);
        self.reset_sep();
        self.update_sep(node);
    }

    /// Emit a chunk of output for `node`, applying separator and (optionally)
    /// single-line-context handling. Faithful to the non-selser skeleton of
    /// `SerializerState::emitChunk` (escaping is layered on by
    /// `WikitextSerializer::emitWikitext`/`escapeWikitext`).
    pub fn emit_chunk(&mut self, text: impl Into<String>, node: NodeId, tree: &DomTree) {
        let mut text = text.into();
        // Emit the pending separator first, gated on node identity (mirrors
        // `$origSepNeeded = $node !== $sep->lastSourceNode`).
        self.emit_sep_for_node(node);
        if self.single_line_context.enforced() {
            text = text.replace('\n', " ");
        }
        // Escape the chunk if requested (only text nodes set `needs_escaping`).
        // Faithful to `SerializerState::emitChunk` calling
        // `$this->serializer->escapeWikitext(..., 'isLastChild' => ...)`.
        if self.needs_escaping {
            let opts = crate::html::wikitext_escape_handlers::EscapeOpts {
                node: Some(node),
                is_last_child: self.is_last_child,
                in_multiline_mode: false,
            };
            text = crate::html::wikitext_escape_handlers::escape_wikitext(self, tree, &text, opts);
            self.needs_escaping = false;
        }
        self.push_to_curr_line(ConstrainedText::cast(text, node));
        // After emitting content, we are no longer at start-of-line.
        self.on_sol = false;
        self.at_start_of_output = false;
    }

    /// Build and emit the pending separator for `node`, but only when `node`
    /// differs from the last node a separator was emitted for. Faithful to
    /// `SerializerState::emitSepForNode` (non-selser, no DSR recovery).
    pub fn emit_sep_for_node(&mut self, node: NodeId) {
        // A separator is only needed when this node hasn't already had one.
        if self.separator.last_source_node == Some(node) {
            return;
        }
        let sep = crate::html::separators::Separators::build_sep(self, node);
        // `emit_sep` resets the separator and records `last_source_node`.
        self.emit_sep(sep.as_deref().unwrap_or(""), node);
    }

    /// Walk the children of `node`, delegating each to the serializer. Faithful
    /// to `SerializerState::serializeChildren`.
    pub fn serialize_children(&mut self, tree: &DomTree, node: NodeId) {
        crate::html::serializer::walk_children(tree, node, self);
    }

    /// Walk the children of `node` with a context-specific escaping handler
    /// pushed onto the `wteHandlerStack` (popped afterwards). Faithful to
    /// `SerializerState::serializeChildren($node, $wtEscaper)`.
    pub fn serialize_children_with_escaper(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        wt_escaper: WtEscapeHandler,
    ) {
        self.wte_handler_stack.push(wt_escaper);
        crate::html::serializer::walk_children(tree, node, self);
        self.wte_handler_stack.pop();
    }

    /// Serialize the children of `node` to an owned string in indent-pre
    /// context. Faithful to `SerializerState::serializeIndentPreChildrenToString`:
    /// a sub-serialization that captures the child output, runs with
    /// `inIndentPre = true` and `onSOL = false`, then restores the surrounding
    /// state. (The full `serializeChildrenToString` save/restore is factored
    /// here only for the indent-pre field; the other `in*` contexts are added
    /// as their handlers land.)
    pub fn serialize_indent_pre_children_to_string(
        &mut self,
        tree: &DomTree,
        node: NodeId,
    ) -> String {
        // Save the portions of state the sub-serialization will mutate.
        let old_sep = std::mem::take(&mut self.separator);
        let old_sol = self.on_sol;
        let old_out = std::mem::take(&mut self.out);
        let old_start = self.at_start_of_output;
        let old_curr_line = std::mem::take(&mut self.curr_line);
        let old_indent_pre = self.in_indent_pre;
        let old_slc = std::mem::take(&mut self.single_line_context);
        let old_prev_unmod = self.prev_node_unmodified;
        let old_curr_unmod = self.curr_node_unmodified;
        let old_prev_node = self.prev_node;

        self.out = String::new();
        self.reset_sep();
        self.on_sol = false;
        self.at_start_of_output = false;
        self.in_indent_pre = true;
        self.single_line_context.disable();
        self.reset_curr_line(None);

        // Serialize the children and flush the buffered line into `out`.
        self.update_sep(node);
        self.serialize_children(tree, node);
        self.flush_line();

        let bits = std::mem::take(&mut self.out);

        // Restore the surrounding state.
        self.out = old_out;
        self.separator = old_sep;
        self.on_sol = old_sol;
        self.at_start_of_output = old_start;
        self.curr_line = old_curr_line;
        self.in_indent_pre = old_indent_pre;
        self.single_line_context = old_slc;
        self.prev_node_unmodified = old_prev_unmod;
        self.curr_node_unmodified = old_curr_unmod;
        self.prev_node = old_prev_node;

        bits
    }

    /// Set the current/previous node tracking (`SerializerState::updateModificationFlags`).
    pub fn update_modification_flags(&mut self, node: NodeId) {
        self.prev_node_unmodified = self.curr_node_unmodified;
        self.curr_node_unmodified = false;
        self.prev_node = Some(node);
    }

    /// Recover trimmed whitespace for `node` (leading/trailing). Faithful to
    /// `SerializerState::recoverTrimmedWhitespace` → `Separators::recoverTrimmedWhitespace`:
    /// returns `None` outside selser mode.
    pub fn recover_trimmed_whitespace(&self, _node: NodeId, _leading: bool) -> Option<String> {
        None
    }
}

impl Default for SerializerState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_append_sep() {
        let mut st = SerializerState::new();
        st.append_sep(" ");
        st.append_sep("\n");
        assert_eq!(st.separator.src.as_deref(), Some(" \n"));
    }

    #[test]
    fn test_reset_sep() {
        let mut st = SerializerState::new();
        st.append_sep("x");
        st.update_sep(3);
        st.reset_sep();
        assert_eq!(st.separator.src, None);
        assert_eq!(st.separator.last_source_node, None);
    }

    #[test]
    fn test_flush_line() {
        let mut st = SerializerState::new();
        st.push_to_curr_line(ConstrainedText::cast("abc", 1));
        st.push_to_curr_line(ConstrainedText::cast("def", 2));
        st.flush_line();
        assert_eq!(st.out, "abcdef");
        assert!(st.curr_line.chunks.is_empty());
    }

    #[test]
    fn test_emit_sep_sets_sol() {
        let mut st = SerializerState::new();
        st.on_sol = false;
        st.emit_sep("\n", 1);
        assert!(st.on_sol);
    }
}
