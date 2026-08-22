//! The tree-construction dispatcher — distributes token events to the
//! appropriate insertion mode, tracks the stack of template insertion modes,
//! and exposes the shortcuts (`inHead`, `inBody`, `inTable`, `inSelect`,
//! `inTemplate`, `inForeign`) that Parsoid's `TreeBuilderStage` relies on.
//!
//! Ports `Wikimedia\RemexHtml\TreeBuilder\Dispatcher`.

use super::tree_builder::TreeBuilder;
use super::tree_handler::TreeHandler;

/// Insertion-mode ids (mirror `Dispatcher::INITIAL` etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeId {
    Initial = 1,
    BeforeHtml = 2,
    BeforeHead = 3,
    InHead = 4,
    InHeadNoscript = 5,
    AfterHead = 6,
    InBody = 7,
    Text = 8,
    InTable = 9,
    InTableText = 10,
    InCaption = 11,
    InColumnGroup = 12,
    InTableBody = 13,
    InRow = 14,
    InCell = 15,
    InSelect = 16,
    InSelectInTable = 17,
    InTemplate = 18,
    AfterBody = 19,
    InFrameset = 20,
    AfterFrameset = 21,
    AfterAfterBody = 22,
    AfterAfterFrameset = 23,
    InForeignContent = 24,
    InPre = 25,
    InTextarea = 26,
}

/// The dispatcher. Since Rust cannot hold 26 polymorphic mode objects with
/// shared borrows of `self` (like PHP's objects-with-circular-references easily
/// do), the modes are re-created per-event and the dispatcher keeps only the
/// current mode id, the original-mode id, and the template-mode stack. The
/// `inHead`/`inBody`/… shortcuts are honored by re-instantiating the target
/// mode for the duration of a single "use the rules for" call.
pub struct Dispatcher {
    pub mode: ModeId,
    original_mode: Option<ModeId>,
    template_modes: Vec<ModeId>,
    pub current_handler_mode: ModeId,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            mode: ModeId::Initial,
            original_mode: None,
            template_modes: Vec::new(),
            current_handler_mode: ModeId::Initial,
        }
    }

    pub fn switch_mode(&mut self, mode: ModeId) {
        self.mode = mode;
        self.current_handler_mode = mode;
    }

    pub fn switch_and_save(&mut self, mode: ModeId) {
        self.original_mode = Some(self.mode);
        self.mode = mode;
        self.current_handler_mode = mode;
    }

    pub fn restore_mode(&mut self) {
        if let Some(orig) = self.original_mode.take() {
            self.mode = orig;
            self.current_handler_mode = orig;
        }
    }

    pub fn is_in_table_mode(&self) -> bool {
        matches!(
            self.mode,
            ModeId::InTable
                | ModeId::InCaption
                | ModeId::InTableBody
                | ModeId::InRow
                | ModeId::InCell
        )
    }

    /// Flush pending table text (mirror `Dispatcher::flushTableText` +
    /// `InTableText::flush`): move buffered table text to the appropriate
    /// place (foster-parenting non-whitespace text, else inserted normally).
    pub fn flush_table_text<H: TreeHandler>(&mut self, builder: &mut TreeBuilder<H>) {
        if self.mode != ModeId::InTableText {
            return;
        }
        let pending = std::mem::take(&mut builder.pending_table_characters);
        let contains_nonspace = pending.iter().any(|text| !is_html_whitespace(text));
        for text in pending {
            if text.is_empty() {
                continue;
            }
            builder.foster_parenting = contains_nonspace;
            builder.insert_characters(&text, 0, text.len(), 0, 0);
            builder.foster_parenting = false;
        }
    }

    /// Compute the appropriate insertion mode (mirror `getAppropriateMode`).
    pub fn reset<H: TreeHandler>(&mut self, builder: &TreeBuilder<H>) {
        let mode = appropriate_mode(builder, self.template_modes.last().copied());
        self.switch_mode(mode);
    }

    /// Push a template insertion mode.
    pub fn template_mode_stack_push(&mut self, mode: ModeId) {
        self.template_modes.push(mode);
    }

    /// Pop the template insertion mode.
    pub fn template_mode_stack_pop(&mut self) {
        self.template_modes.pop();
    }

    /// Whether the template-mode stack is empty.
    pub fn template_mode_stack_is_empty(&self) -> bool {
        self.template_modes.is_empty()
    }

    /// The current template mode, if any.
    pub fn template_mode_stack_current(&self) -> Option<ModeId> {
        self.template_modes.last().copied()
    }
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the appropriate insertion mode (mirror `Dispatcher::getAppropriateMode`).
fn appropriate_mode<H: TreeHandler>(
    builder: &TreeBuilder<H>,
    template_mode: Option<ModeId>,
) -> ModeId {
    let stack = &builder.stack;
    for idx in (0..stack.length()).rev() {
        let last_iter = idx == 0;
        let node = stack.item(idx);

        // In fragment mode, the bottommost stack element is the synthetic
        // <html> root; the actual adjusted current node is the fragment
        // context element (e.g. <body>). Mirror PHP's `getAppropriateMode`.
        let html_name = if last_iter && builder.is_fragment {
            builder
                .fragment_context
                .as_ref()
                .map(|e| e.html_name.as_str())
                .unwrap_or_else(|| node.html_name.as_str())
        } else {
            node.html_name.as_str()
        };

        match html_name {
            "select" => {
                if last_iter {
                    return ModeId::InSelect;
                }
                for ancestor_idx in (1..idx).rev() {
                    let ancestor = stack.item(ancestor_idx);
                    if ancestor.html_name == "template" {
                        return ModeId::InSelect;
                    } else if ancestor.html_name == "table" {
                        return ModeId::InSelectInTable;
                    }
                }
                return ModeId::InSelect;
            }
            "td" | "th" => {
                if !last_iter {
                    return ModeId::InCell;
                }
            }
            "tr" => return ModeId::InRow,
            "tbody" | "thead" | "tfoot" => return ModeId::InTableBody,
            "caption" => return ModeId::InCaption,
            "colgroup" => return ModeId::InColumnGroup,
            "table" => return ModeId::InTable,
            "template" => return template_mode.unwrap_or(ModeId::InTemplate),
            "head" => {
                if last_iter {
                    return ModeId::InBody;
                } else {
                    return ModeId::InHead;
                }
            }
            "body" => return ModeId::InBody,
            "frameset" => return ModeId::InFrameset,
            "html" => {
                if builder.head_element.is_none() {
                    return ModeId::BeforeHead;
                } else {
                    return ModeId::AfterHead;
                }
            }
            _ => {}
        }
    }
    ModeId::InBody
}

/// The set of table modes (mirror `Dispatcher::TABLE_MODES`).
pub fn is_table_mode(mode: ModeId) -> bool {
    matches!(
        mode,
        ModeId::InTable | ModeId::InCaption | ModeId::InTableBody | ModeId::InRow | ModeId::InCell
    )
}

/// HTML table-text whitespace (space, tab, LF, FF, CR).
fn is_html_whitespace(s: &str) -> bool {
    s.chars()
        .all(|c| matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' '))
}
