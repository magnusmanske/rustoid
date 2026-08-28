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
use crate::html::dsr::{DomSourceRange, SelectiveUpdateData, SourceRange, is_valid_dsr};
use crate::html::env::SerializerEnv;
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

/// The single-flag context a [`SerializerState::serialize_children_to_string`]
/// sub-serialization sets on `$inState`. Mirrors PHP's `$states = ['inLink',
/// 'inCaption', 'inIndentPre', 'inPHPBlock', 'inAttribute']`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InState {
    Link,
    Caption,
    IndentPre,
    PhpBlock,
    Attribute,
}

/// The mutable serializer state (port of `SerializerState`).
///
/// `'a` is the lifetime of the borrowed [`SerializerEnv`] (carrying the
/// `SiteConfig`/context `Title`); test-only states use `'static` with `env: None`.
pub struct SerializerState<'a> {
    /// The serializer environment (`None` in test-only states without a config).
    pub env: Option<SerializerEnv<'a>>,

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

    /// (selser) The selective-update data (carrying the revision wikitext,
    /// `revText`). `None` outside selser mode. Faithful to PHP's private
    /// `?SelectiveUpdateData $selserData`.
    pub selser_data: Option<SelectiveUpdateData>,

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

impl<'a> SerializerState<'a> {
    /// Construct a fresh serializer state with no environment (for tests).
    pub fn new() -> Self {
        Self {
            env: None,
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
            selser_data: None,
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

    /// Construct a serializer state carrying the given environment (for real
    /// html2wt serialization with a `SiteConfig`/context `Title`).
    pub fn with_env(env: SerializerEnv<'a>) -> Self {
        let mut state = SerializerState::new();
        state.env = Some(env);
        state
    }

    /// Initialize a few boolean flags based on serialization mode. Faithful to
    /// `SerializerState::initMode(bool $selserMode)` (for use by
    /// `WikitextSerializer` once the selser path is wired).
    pub fn init_mode(&mut self, selser_mode: bool) {
        self.selser_mode = selser_mode;
    }

    /// Extracts a subset of the page source bound by the supplied source range.
    /// Faithful to `SerializerState::getOrigSrc(SourceRange $sr): ?string`.
    ///
    /// Requires selser mode (PHP `Assert::invariant`); returns `None` when not
    /// in selser mode, when no `selser_data` is present, or when the range is
    /// out of bounds.
    pub fn get_orig_src(&self, sr: &SourceRange) -> Option<String> {
        if !self.selser_mode {
            return None;
        }
        let rev_text = self.selser_data.as_ref()?.rev_text.as_str();
        // `$sr->start <= $sr->end` treats null offsets as 0 (PHP null
        // comparison); replicate that only for the bounds check used here,
        // then delegate the actual substring to `SourceRange::substr` (which
        // returns "" for ranges with null offsets, matching PHP's
        // `safeSubstr` behavior for empty ranges).
        let start = sr.start.unwrap_or(0);
        let end = sr.end.unwrap_or(0);
        if start <= end && start <= rev_text.len() {
            // Prefer the range's own source, else the revision text.
            let src = sr.source.as_deref().unwrap_or(rev_text);
            Some(sr.substr(src).to_string())
        } else {
            None
        }
    }

    /// Check the validity of a DSR in the context of the page source. Faithful
    /// to `SerializerState::isValidDSR(?DomSourceRange $dsr, bool $all)`:
    /// returns `false` when `is_valid_dsr` would, or when the offsets are out
    /// of bounds or would slice in the middle of a UTF-8 sequence.
    pub fn is_valid_dsr(&self, dsr: Option<&DomSourceRange>, all: bool) -> bool {
        if !is_valid_dsr(dsr, all) {
            return false;
        }
        let dsr = dsr.unwrap();
        let rev_text = match self.selser_data.as_ref() {
            Some(sd) => sd.rev_text.as_str(),
            None => return false,
        };
        let start = dsr.start.unwrap_or(0);
        let end = dsr.end.unwrap_or(0);
        if !(start <= end && end <= rev_text.len()) {
            return false;
        }
        // UTF-8 boundary checks (faithful to PHP's `$check` closure).
        let check = |s: usize, e: usize| -> bool { utf8_boundary_ok(rev_text.as_bytes(), s, e) };
        if !all {
            return check(start, end);
        }
        // Check each inner range.
        let open_end = start + dsr.open_width.unwrap_or(0);
        if open_end > end {
            return false;
        }
        if !check(start, open_end) {
            return false;
        }
        let close_start = end.saturating_sub(dsr.close_width.unwrap_or(0));
        if start > close_start {
            return false;
        }
        if !check(close_start, end) {
            return false;
        }
        if open_end > close_start {
            return false;
        }
        check(open_end, close_start)
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

    /// Serialize the children of `node` to an owned string in a specific
    /// single-flag context (`inLink`/`inCaption`/`inIndentPre`/`inPHPBlock`/
    /// `inAttribute`). Faithful to PHP's `SerializerState::serializeChildrenToString`:
    /// save the surrounding state, run a sub-serialization with the flag set and
    /// `onSOL = false`, then restore and return the captured output.
    fn serialize_children_to_string(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        wt_escaper: Option<WtEscapeHandler>,
        in_state: InState,
    ) -> String {
        // Save the portions of state the sub-serialization will mutate.
        let old_sep = std::mem::take(&mut self.separator);
        let old_sol = self.on_sol;
        let old_out = std::mem::take(&mut self.out);
        let old_start = self.at_start_of_output;
        let old_curr_line = std::mem::take(&mut self.curr_line);
        let old_in_link = self.in_link;
        let old_in_caption = self.in_caption;
        let old_in_indent_pre = self.in_indent_pre;
        let old_in_php_block = self.in_php_block;
        let old_in_attribute = self.in_attribute;
        let old_slc = std::mem::take(&mut self.single_line_context);
        let old_prev_unmod = self.prev_node_unmodified;
        let old_curr_unmod = self.curr_node_unmodified;
        let old_prev_node = self.prev_node;

        self.out = String::new();
        self.reset_sep();
        self.on_sol = false;
        self.at_start_of_output = false;
        self.in_link = false;
        self.in_caption = false;
        self.in_indent_pre = false;
        self.in_php_block = false;
        self.in_attribute = false;
        self.set_in_state(in_state, true);
        self.single_line_context.disable();
        self.reset_curr_line(None);

        // Serialize the children (with the optional escaping handler) and flush
        // the buffered line into `out`.
        self.update_sep(node);
        if let Some(escaper) = wt_escaper {
            self.serialize_children_with_escaper(tree, node, escaper);
        } else {
            self.serialize_children(tree, node);
        }
        self.flush_line();

        let bits = std::mem::take(&mut self.out);

        // Restore the surrounding state.
        self.out = old_out;
        self.separator = old_sep;
        self.on_sol = old_sol;
        self.at_start_of_output = old_start;
        self.curr_line = old_curr_line;
        self.in_link = old_in_link;
        self.in_caption = old_in_caption;
        self.in_indent_pre = old_in_indent_pre;
        self.in_php_block = old_in_php_block;
        self.in_attribute = old_in_attribute;
        self.single_line_context = old_slc;
        self.prev_node_unmodified = old_prev_unmod;
        self.curr_node_unmodified = old_curr_unmod;
        self.prev_node = old_prev_node;

        bits
    }

    /// Set the `in_state` flag (and clear the others).
    fn set_in_state(&mut self, in_state: InState, value: bool) {
        match in_state {
            InState::Link => self.in_link = value,
            InState::Caption => self.in_caption = value,
            InState::IndentPre => self.in_indent_pre = value,
            InState::PhpBlock => self.in_php_block = value,
            InState::Attribute => self.in_attribute = value,
        }
    }

    /// Serialize the children of `node` to an owned string in indent-pre
    /// context. Faithful to `SerializerState::serializeIndentPreChildrenToString`.
    pub fn serialize_indent_pre_children_to_string(
        &mut self,
        tree: &DomTree,
        node: NodeId,
    ) -> String {
        self.serialize_children_to_string(tree, node, None, InState::IndentPre)
    }

    /// Serialize the children of a link to an owned string. Faithful to
    /// `SerializerState::serializeLinkChildrenToString`.
    pub fn serialize_link_children_to_string(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        wt_escaper: Option<WtEscapeHandler>,
    ) -> String {
        self.serialize_children_to_string(tree, node, wt_escaper, InState::Link)
    }

    /// Serialize the children of a caption to an owned string. Faithful to
    /// `SerializerState::serializeCaptionChildrenToString`.
    pub fn serialize_caption_children_to_string(
        &mut self,
        tree: &DomTree,
        node: NodeId,
        wt_escaper: Option<WtEscapeHandler>,
    ) -> String {
        self.serialize_children_to_string(tree, node, wt_escaper, InState::Caption)
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

impl<'a> Default for SerializerState<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// Faithful port of the `$check` closure in PHP's `SerializerState::isValidDSR`:
/// verify that the byte range `[start, end)` of `src` starts on a non-continuation
/// byte and ends on a complete UTF-8 character boundary.
fn utf8_boundary_ok(src: &[u8], start: usize, end: usize) -> bool {
    if start == end {
        // Zero-length string is always ok.
        return true;
    }
    let first_char = src[start];
    if (first_char & 0xC0) == 0x80 {
        return false; // Bad UTF-8 at start of string.
    }
    let mut i = 0isize;
    // This loop won't pass `start` because we've already asserted the first
    // character isn't 10xx xxxx.
    loop {
        i -= 1;
        if i <= -5 {
            return false; // Bad UTF-8 at end (>4 byte sequence).
        }
        let last_char = src[(end as isize + i) as usize];
        if (last_char & 0xC0) != 0x80 {
            break;
        }
    }
    let last_char = src[(end as isize + i) as usize];
    if (last_char & 0x80) == 0 {
        i == -1
    } else if (last_char & 0xE0) == 0xC0 {
        i == -2
    } else if (last_char & 0xF0) == 0xE0 {
        i == -3
    } else if (last_char & 0xF8) == 0xF0 {
        i == -4
    } else {
        false
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

    fn selser_state(rev_text: &str) -> SerializerState<'static> {
        let mut st = SerializerState::new();
        st.init_mode(true);
        st.selser_data = Some(SelectiveUpdateData::new(rev_text.to_string()));
        st
    }

    #[test]
    fn test_get_orig_src_requires_selser_mode() {
        let st = SerializerState::new();
        let sr = SourceRange::new(Some(0), Some(3));
        assert_eq!(st.get_orig_src(&sr), None);
    }

    #[test]
    fn test_get_orig_src_in_bounds() {
        let st = selser_state("hello world");
        let sr = SourceRange::new(Some(0), Some(5));
        assert_eq!(st.get_orig_src(&sr).as_deref(), Some("hello"));
    }

    #[test]
    fn test_get_orig_src_out_of_bounds() {
        let st = selser_state("hi");
        // end past the revision text length is clamped by substr
        let sr = SourceRange::new(Some(0), Some(10));
        assert_eq!(st.get_orig_src(&sr).as_deref(), Some("hi"));
        // start > end returns None
        let sr2 = SourceRange::new(Some(5), Some(1));
        assert_eq!(st.get_orig_src(&sr2), None);
    }

    #[test]
    fn test_get_orig_src_prefers_range_source() {
        let st = selser_state("rev text");
        let sr = SourceRange::with_source(Some(0), Some(3), Some("xyz".to_string()));
        assert_eq!(st.get_orig_src(&sr).as_deref(), Some("xyz"));
    }

    #[test]
    fn test_is_valid_dsr_utf8_boundaries() {
        let st = selser_state("éé"); // two 2-byte chars
        // [0,2) slices the first é correctly
        assert!(st.is_valid_dsr(
            Some(&DomSourceRange::new(Some(0), Some(2), None, None, 0, 0)),
            false
        ));
        // [1,2) starts mid-é (continuation byte) => invalid
        assert!(!st.is_valid_dsr(
            Some(&DomSourceRange::new(Some(1), Some(2), None, None, 0, 0)),
            false
        ));
    }

    #[test]
    fn test_is_valid_dsr_null_or_out_of_bounds() {
        let st = selser_state("abc");
        assert!(!st.is_valid_dsr(None, false));
        // end beyond revision text
        assert!(!st.is_valid_dsr(
            Some(&DomSourceRange::new(Some(0), Some(99), None, None, 0, 0)),
            false
        ));
        // start > end
        assert!(!st.is_valid_dsr(
            Some(&DomSourceRange::new(Some(3), Some(1), None, None, 0, 0)),
            false
        ));
    }

    #[test]
    fn test_is_valid_dsr_all_requires_widths() {
        let st = selser_state("<b>x</b>");
        let no_widths = DomSourceRange::new(Some(0), Some(8), None, None, 0, 0);
        assert!(!st.is_valid_dsr(Some(&no_widths), true));
        let with_widths = DomSourceRange::new(Some(0), Some(8), Some(3), Some(4), 0, 0);
        assert!(st.is_valid_dsr(Some(&with_widths), true));
    }
}
