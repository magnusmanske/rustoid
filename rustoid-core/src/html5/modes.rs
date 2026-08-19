//! The tree-construction insertion modes, ported from RemexHtml's `InsertionMode`
//! subclasses. Each mode's handlers are free functions taking `&mut TreeBuilder`
//! and `&mut Dispatcher`, avoiding the shared-borrow cycle PHP models with
//! circular object references.
//!
//! Frameset- and raw-text-only modes (`InFrameset`, `AfterFrameset`,
//! `AfterAfterFrameset`, `InPre`, `InTextarea`) are unreachable from Parsoid
//! wikitext (raw text is consumed by the tokenizer); they defer to `InBody`.
//!
//! The mode functions intentionally re-dispatch to the top-level entry points
//! after switching the insertion mode; this bounded recursion (through the
//! finite insertion-mode state machine) trips `unconditional_recursion`, which
//! we suppress here.
#![allow(unconditional_recursion)]
#![allow(clippy::only_used_in_recursion)]

use super::dispatcher::{Dispatcher, ModeId};
use super::element::{Attributes, Element};
use super::html_data::{NS_HTML, NS_MATHML, NS_SVG, is_special};
use super::tree_builder::{QUIRKS as TB_QUIRKS, TreeBuilder};
use super::tree_handler::TreeHandler;

// Splits `text[start..start+length]` into `(ws_run, remainder)` where `ws_run`
// is the leading run of HTML whitespace. Returns `(start, len)` for each part.
fn split_ws(text: &str, start: usize, length: usize) -> ((usize, usize), (usize, usize)) {
    let slice = &text[start..start + length];
    let ws = slice
        .chars()
        .take_while(|c| matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' '))
        .count();
    ((start, ws), (start + ws, length - ws))
}

/// Character data dispatch.
#[allow(unconditional_recursion)]
pub fn characters<H: TreeHandler>(
    b: &mut TreeBuilder<H>,
    d: &mut Dispatcher,
    text: &str,
    start: usize,
    length: usize,
    ss: usize,
    sl: usize,
) {
    match d.mode {
        ModeId::Initial => initial::characters(b, d, text, start, length, ss, sl),
        ModeId::BeforeHtml => before_html::characters(b, d, text, start, length, ss, sl),
        ModeId::BeforeHead => before_head::characters(b, d, text, start, length, ss, sl),
        ModeId::InHead => in_head::characters(b, d, text, start, length, ss, sl),
        ModeId::InHeadNoscript => in_head_noscript::characters(b, d, text, start, length, ss, sl),
        ModeId::AfterHead => after_head::characters(b, d, text, start, length, ss, sl),
        ModeId::InBody => in_body::characters(b, d, text, start, length, ss, sl),
        ModeId::Text => text_mode::characters(b, d, text, start, length, ss, sl),
        ModeId::InTable => in_table::characters(b, d, text, start, length, ss, sl),
        ModeId::InTableText => in_table_text::characters(b, d, text, start, length, ss, sl),
        ModeId::InCaption => in_caption::characters(b, d, text, start, length, ss, sl),
        ModeId::InColumnGroup => in_column_group::characters(b, d, text, start, length, ss, sl),
        ModeId::InTableBody => in_table_body::characters(b, d, text, start, length, ss, sl),
        ModeId::InRow => in_row::characters(b, d, text, start, length, ss, sl),
        ModeId::InCell => in_cell::characters(b, d, text, start, length, ss, sl),
        ModeId::InSelect => in_select::characters(b, d, text, start, length, ss, sl),
        ModeId::InSelectInTable => {
            in_select_in_table::characters(b, d, text, start, length, ss, sl)
        }
        ModeId::InTemplate => in_template::characters(b, d, text, start, length, ss, sl),
        ModeId::AfterBody => after_body::characters(b, d, text, start, length, ss, sl),
        ModeId::AfterAfterBody => after_after_body::characters(b, d, text, start, length, ss, sl),
        ModeId::InForeignContent => in_foreign::characters(b, d, text, start, length, ss, sl),
        _ => in_body::characters(b, d, text, start, length, ss, sl),
    }
}

/// Start-tag dispatch.
#[allow(unconditional_recursion)]
pub fn start_tag<H: TreeHandler>(
    b: &mut TreeBuilder<H>,
    d: &mut Dispatcher,
    name: &str,
    attrs: Attributes,
    self_close: bool,
    ss: usize,
    sl: usize,
) -> Option<usize> {
    match d.mode {
        ModeId::Initial => initial::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::BeforeHtml => before_html::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::BeforeHead => before_head::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InHead => in_head::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InHeadNoscript => {
            in_head_noscript::start_tag(b, d, name, attrs, self_close, ss, sl)
        }
        ModeId::AfterHead => after_head::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InBody => in_body::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InTable => in_table::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InTableText => in_table_text::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InCaption => in_caption::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InColumnGroup => in_column_group::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InTableBody => in_table_body::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InRow => in_row::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InCell => in_cell::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InSelect => in_select::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::InSelectInTable => {
            in_select_in_table::start_tag(b, d, name, attrs, self_close, ss, sl)
        }
        ModeId::InTemplate => in_template::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::AfterBody => after_body::start_tag(b, d, name, attrs, self_close, ss, sl),
        ModeId::AfterAfterBody => {
            after_after_body::start_tag(b, d, name, attrs, self_close, ss, sl)
        }
        ModeId::InForeignContent => in_foreign::start_tag(b, d, name, attrs, self_close, ss, sl),
        _ => in_body::start_tag(b, d, name, attrs, self_close, ss, sl),
    }
}

/// End-tag dispatch.
#[allow(unconditional_recursion)]
pub fn end_tag<H: TreeHandler>(
    b: &mut TreeBuilder<H>,
    d: &mut Dispatcher,
    name: &str,
    ss: usize,
    sl: usize,
) -> Option<usize> {
    match d.mode {
        ModeId::Initial => initial::end_tag(b, d, name, ss, sl),
        ModeId::BeforeHtml => before_html::end_tag(b, d, name, ss, sl),
        ModeId::BeforeHead => before_head::end_tag(b, d, name, ss, sl),
        ModeId::InHead => in_head::end_tag(b, d, name, ss, sl),
        ModeId::InHeadNoscript => in_head_noscript::end_tag(b, d, name, ss, sl),
        ModeId::AfterHead => after_head::end_tag(b, d, name, ss, sl),
        ModeId::InBody => in_body::end_tag(b, d, name, ss, sl),
        ModeId::Text => text_mode::end_tag(b, d, name, ss, sl),
        ModeId::InTable => in_table::end_tag(b, d, name, ss, sl),
        ModeId::InTableText => in_table_text::end_tag(b, d, name, ss, sl),
        ModeId::InCaption => in_caption::end_tag(b, d, name, ss, sl),
        ModeId::InColumnGroup => in_column_group::end_tag(b, d, name, ss, sl),
        ModeId::InTableBody => in_table_body::end_tag(b, d, name, ss, sl),
        ModeId::InRow => in_row::end_tag(b, d, name, ss, sl),
        ModeId::InCell => in_cell::end_tag(b, d, name, ss, sl),
        ModeId::InSelect => in_select::end_tag(b, d, name, ss, sl),
        ModeId::InSelectInTable => in_select_in_table::end_tag(b, d, name, ss, sl),
        ModeId::InTemplate => in_template::end_tag(b, d, name, ss, sl),
        ModeId::AfterBody => after_body::end_tag(b, d, name, ss, sl),
        ModeId::AfterAfterBody => after_after_body::end_tag(b, d, name, ss, sl),
        ModeId::InForeignContent => in_foreign::end_tag(b, d, name, ss, sl),
        _ => in_body::end_tag(b, d, name, ss, sl),
    }
}

/// End-document dispatch.
#[allow(unconditional_recursion)]
pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
    match d.mode {
        ModeId::Initial => initial::end_document(b, d, pos),
        ModeId::BeforeHtml => before_html::end_document(b, d, pos),
        ModeId::BeforeHead => before_head::end_document(b, d, pos),
        ModeId::InHead => in_head::end_document(b, d, pos),
        ModeId::InHeadNoscript => in_head_noscript::end_document(b, d, pos),
        ModeId::AfterHead => after_head::end_document(b, d, pos),
        ModeId::InBody => in_body::end_document(b, d, pos),
        ModeId::Text => text_mode::end_document(b, d, pos),
        ModeId::InTable => in_table::end_document(b, d, pos),
        ModeId::InTableText => in_table_text::end_document(b, d, pos),
        ModeId::InCaption => in_caption::end_document(b, d, pos),
        ModeId::InColumnGroup => in_column_group::end_document(b, d, pos),
        ModeId::InTableBody => in_table_body::end_document(b, d, pos),
        ModeId::InRow => in_row::end_document(b, d, pos),
        ModeId::InCell => in_cell::end_document(b, d, pos),
        ModeId::InSelect => in_select::end_document(b, d, pos),
        ModeId::InSelectInTable => in_select_in_table::end_document(b, d, pos),
        ModeId::InTemplate => in_template::end_document(b, d, pos),
        ModeId::AfterBody => after_body::end_document(b, d, pos),
        ModeId::AfterAfterBody => after_after_body::end_document(b, d, pos),
        ModeId::InForeignContent => in_foreign::end_document(b, d, pos),
        _ => in_body::end_document(b, d, pos),
    }
}

// ---------------------------------------------------------------------------
// Mode implementations.
// ---------------------------------------------------------------------------

mod initial {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (_ws, non_ws) = split_ws(text, start, length);
        if non_ws.1 == 0 {
            return;
        }
        b.error("missing doctype", ss);
        b.quirks = super::super::tree_builder::QUIRKS;
        d.switch_mode(ModeId::BeforeHtml);
        super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        b.error("missing doctype", ss);
        b.quirks = super::super::tree_builder::QUIRKS;
        d.switch_mode(ModeId::BeforeHtml);
        super::start_tag(b, d, name, attrs, sc, ss, sl)
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        b.error("missing doctype", ss);
        b.quirks = super::super::tree_builder::QUIRKS;
        d.switch_mode(ModeId::BeforeHtml);
        super::end_tag(b, d, name, ss, sl)
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        b.error("missing doctype", pos);
        b.quirks = super::super::tree_builder::QUIRKS;
        d.switch_mode(ModeId::BeforeHtml);
        super::end_document(b, d, pos);
    }
}

mod before_html {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (_ws, non_ws) = split_ws(text, start, length);
        if non_ws.1 == 0 {
            return;
        }
        b.insert_element("html", Attributes::new(), false, ss, 0);
        d.switch_mode(ModeId::BeforeHead);
        super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        if name == "html" {
            let uid = b.insert_element(name, attrs, false, ss, sl);
            d.switch_mode(ModeId::BeforeHead);
            return Some(uid);
        }
        b.insert_element("html", Attributes::new(), false, ss, 0);
        d.switch_mode(ModeId::BeforeHead);
        super::start_tag(b, d, name, attrs, sc, ss, sl)
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        if !matches!(name, "head" | "body" | "html" | "br") {
            b.error("end tag not allowed before html", ss);
            return None;
        }
        b.insert_element("html", Attributes::new(), false, ss, 0);
        d.switch_mode(ModeId::BeforeHead);
        super::end_tag(b, d, name, ss, sl)
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        b.insert_element("html", Attributes::new(), false, pos, 0);
        d.switch_mode(ModeId::BeforeHead);
        super::end_document(b, d, pos);
    }
}

mod before_head {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (_ws, non_ws) = split_ws(text, start, length);
        if non_ws.1 == 0 {
            return;
        }
        let head = b.insert_element("head", Attributes::new(), false, ss, 0);
        b.head_element = b.stack.item_by_uid(head).cloned();
        d.switch_mode(ModeId::InHead);
        super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        if name == "html" {
            in_body::start_tag(b, d, name, attrs, sc, ss, sl)
        } else if name == "head" {
            let head = b.insert_element(name, attrs, false, ss, sl);
            b.head_element = b.stack.item_by_uid(head).cloned();
            d.switch_mode(ModeId::InHead);
            Some(head)
        } else {
            let head = b.insert_element("head", Attributes::new(), false, ss, 0);
            b.head_element = b.stack.item_by_uid(head).cloned();
            d.switch_mode(ModeId::InHead);
            super::start_tag(b, d, name, attrs, sc, ss, sl)
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        if !matches!(name, "head" | "body" | "html" | "br") {
            b.error("end tag not allowed before head", ss);
            return None;
        }
        let head = b.insert_element("head", Attributes::new(), false, ss, 0);
        b.head_element = b.stack.item_by_uid(head).cloned();
        d.switch_mode(ModeId::InHead);
        super::end_tag(b, d, name, ss, sl)
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        let head = b.insert_element("head", Attributes::new(), false, pos, 0);
        b.head_element = b.stack.item_by_uid(head).cloned();
        d.switch_mode(ModeId::InHead);
        super::end_document(b, d, pos);
    }
}

mod in_head {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (ws, non_ws) = split_ws(text, start, length);
        if ws.1 > 0 {
            b.insert_characters(text, ws.0, ws.1, ss, sl);
        }
        if non_ws.1 == 0 {
            return;
        }
        b.pop(ss, 0);
        d.switch_mode(ModeId::AfterHead);
        super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "html" => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
            "base" | "basefont" | "bgsound" | "link" | "meta" | "title" | "noframes" | "style"
            | "script" => Some(b.insert_element(name, attrs, true, ss, sl)),
            "noscript" if !b.scripting_flag => {
                let uid = b.insert_element(name, attrs, false, ss, sl);
                d.switch_mode(ModeId::InHeadNoscript);
                Some(uid)
            }
            "template" => {
                b.afe.insert_marker();
                b.frameset_ok = false;
                d.template_mode_stack_push(ModeId::InTemplate);
                d.switch_mode(ModeId::InTemplate);
                Some(b.insert_element(name, attrs, false, ss, sl))
            }
            "head" => {
                b.error("unexpected head tag in head, ignoring", ss);
                None
            }
            _ => {
                b.pop(ss, 0);
                d.switch_mode(ModeId::AfterHead);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "head" => {
                let popped = b.pop(ss, sl);
                d.switch_mode(ModeId::AfterHead);
                popped
            }
            "body" | "html" | "br" => {
                b.pop(ss, 0);
                d.switch_mode(ModeId::AfterHead);
                super::end_tag(b, d, name, ss, sl)
            }
            "template" => {
                if b.stack.has_template() {
                    b.generate_implied_end_tags_thoroughly(ss);
                    if b.stack.current().map(|e| e.html_name.as_str()) != Some("template") {
                        b.error("found </template> when other tags are still open", ss);
                    }
                    let result = b.pop_all_up_to_name("template", ss, sl);
                    b.afe.clear_to_marker();
                    d.template_mode_stack_pop();
                    d.reset(b);
                    result
                } else {
                    b.error(
                        "found </template> but there is no open template, ignoring",
                        ss,
                    );
                    None
                }
            }
            _ => {
                b.error(&format!("ignoring </{name}> in head"), ss);
                None
            }
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        b.pop(pos, 0);
        d.switch_mode(ModeId::AfterHead);
        super::end_document(b, d, pos);
    }
}

mod in_head_noscript {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (ws, non_ws) = split_ws(text, start, length);
        if ws.1 > 0 {
            in_head::characters(b, d, text, ws.0, ws.1, ss, sl);
        }
        if non_ws.1 == 0 {
            return;
        }
        b.error(
            "unexpected non-whitespace character in head in noscript, closing noscript",
            ss,
        );
        b.pop(ss, 0);
        d.switch_mode(ModeId::InHead);
        super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "html" => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
            "basefont" | "bgsound" | "link" | "meta" | "noframes" | "style" => {
                in_head::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            "head" | "noscript" => {
                b.error(
                    &format!("unexpected <{name}> in head in noscript, ignoring"),
                    ss,
                );
                None
            }
            _ => {
                b.error(
                    &format!("unexpected <{name}> in head in noscript, closing noscript"),
                    ss,
                );
                b.pop(ss, 0);
                d.switch_mode(ModeId::InHead);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "noscript" => {
                let popped = b.pop(ss, sl);
                d.switch_mode(ModeId::InHead);
                popped
            }
            "br" => {
                b.error("unexpected </br> in head in noscript, closing noscript", ss);
                b.pop(ss, 0);
                d.switch_mode(ModeId::InHead);
                super::end_tag(b, d, name, ss, sl)
            }
            _ => {
                b.error(
                    &format!("unexpected </{name}> in head in noscript, ignoring"),
                    ss,
                );
                None
            }
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        b.error("unexpected end-of-file in head in noscript", pos);
        b.pop(pos, 0);
        d.switch_mode(ModeId::InHead);
        super::end_document(b, d, pos);
    }
}

mod after_head {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (ws, non_ws) = split_ws(text, start, length);
        if ws.1 > 0 {
            b.insert_characters(text, ws.0, ws.1, ss, sl);
        }
        if non_ws.1 == 0 {
            return;
        }
        b.insert_element("body", Attributes::new(), false, ss, 0);
        d.switch_mode(ModeId::InBody);
        super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "html" => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
            "body" => {
                let uid = b.insert_element(name, attrs, false, ss, sl);
                b.frameset_ok = false;
                d.switch_mode(ModeId::InBody);
                Some(uid)
            }
            "frameset" => {
                let uid = b.insert_element(name, attrs, false, ss, sl);
                d.switch_mode(ModeId::InFrameset);
                Some(uid)
            }
            "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style"
            | "template" | "title" => {
                b.error(&format!("unexpected <{name}> after </head>, accepting"), ss);
                if let Some(head) = b.head_element.clone() {
                    b.stack.push(head);
                }
                let result = in_head::start_tag(b, d, name, attrs, sc, ss, sl);
                if let Some(head) = b.head_element.clone() {
                    b.stack.remove_by_uid(head.uid);
                }
                result
            }
            "head" => {
                b.error("unexpected <head> after </head>, ignoring", ss);
                None
            }
            _ => {
                b.insert_element("body", Attributes::new(), false, ss, 0);
                d.switch_mode(ModeId::InBody);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "template" => in_head::end_tag(b, d, name, ss, sl),
            "body" | "html" | "br" => {
                b.insert_element("body", Attributes::new(), false, ss, 0);
                d.switch_mode(ModeId::InBody);
                super::end_tag(b, d, name, ss, sl)
            }
            _ => {
                b.error(&format!("unexpected </{name}> after head, ignoring"), ss);
                None
            }
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        b.insert_element("body", Attributes::new(), false, pos, 0);
        d.switch_mode(ModeId::InBody);
        super::end_document(b, d, pos);
    }
}

mod in_body {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        _d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        if !is_html_ws(text, start, length) {
            b.frameset_ok = false;
        }
        b.reconstruct_afe(ss);
        b.insert_characters(text, start, length, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        // The full "in body" start-tag switch. Ported faithfully below.
        let is_new_afe: bool;
        match name {
            "html" => {
                b.error("merging unexpected html tag", ss);
                if b.stack.has_template() || b.stack.length() < 1 {
                    return None;
                }
                let elt0 = b.stack.item(0).clone();
                b.merge_attributes(elt0.uid, &attrs, ss);
                return None;
            }
            "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style"
            | "template" | "title" => {
                return in_head::start_tag(b, d, name, attrs, sc, ss, sl);
            }
            "body" => {
                b.error("ignored unexpected body tag", ss);
                return None;
            }
            "frameset" => {
                b.error("ignored unexpected frameset tag", ss);
                return None;
            }
            "address" | "article" | "aside" | "blockquote" | "center" | "details" | "dir"
            | "div" | "dl" | "fieldset" | "figcaption" | "figure" | "footer" | "header"
            | "main" | "menu" | "nav" | "ol" | "p" | "section" | "summary" | "ul" => {
                b.close_p_in_button_scope(ss);
                is_new_afe = false;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                b.close_p_in_button_scope(ss);
                if is_heading(b.stack.current().map(|e| e.html_name.as_str())) {
                    b.pop(ss, 0);
                }
                is_new_afe = false;
            }
            "pre" | "listing" => {
                b.close_p_in_button_scope(ss);
                b.frameset_ok = false;
                is_new_afe = false;
            }
            "form" => {
                if b.form_element.is_some() && !b.stack.has_template() {
                    b.error("ignoring nested form tag", ss);
                    return None;
                }
                b.close_p_in_button_scope(ss);
                let uid = b.insert_element("form", attrs, false, ss, sl);
                if !b.stack.has_template() {
                    b.form_element = b.stack.item_by_uid(uid).cloned();
                }
                return Some(uid);
            }
            "li" | "dd" | "dt" => {
                b.frameset_ok = false;
                // Close previous li/dd/dt in scope.
                for i in (0..b.stack.length()).rev() {
                    let elt = b.stack.item(i);
                    let hn = elt.html_name.clone();
                    if hn == name || (name != "li" && (hn == "dd" || hn == "dt")) {
                        b.generate_implied_end_tags_and_pop(&hn, ss, 0);
                        break;
                    }
                    if is_special(&elt.namespace, &elt.name)
                        && !matches!(hn.as_str(), "address" | "div" | "p")
                    {
                        break;
                    }
                }
                b.close_p_in_button_scope(ss);
                is_new_afe = false;
            }
            "plaintext" => {
                b.close_p_in_button_scope(ss);
                is_new_afe = false;
            }
            "button" => {
                if b.stack.is_in_scope("button") {
                    b.generate_implied_end_tags(None, ss);
                    b.pop_all_up_to_name("button", ss, 0);
                }
                b.reconstruct_afe(ss);
                b.frameset_ok = false;
                is_new_afe = false;
            }
            "a" => {
                b.afe.find_element_by_name("a");
                b.reconstruct_afe(ss);
                is_new_afe = true;
            }
            "b" | "big" | "code" | "em" | "font" | "i" | "s" | "small" | "strike" | "strong"
            | "tt" | "u" => {
                b.reconstruct_afe(ss);
                is_new_afe = true;
            }
            "nobr" => {
                b.reconstruct_afe(ss);
                if b.stack.is_in_scope("nobr") {
                    b.adoption_agency("nobr", ss, 0);
                    b.reconstruct_afe(ss);
                }
                is_new_afe = true;
            }
            "applet" | "marquee" | "object" => {
                b.reconstruct_afe(ss);
                b.afe.insert_marker();
                b.frameset_ok = false;
                is_new_afe = false;
            }
            "table" => {
                if b.quirks != TB_QUIRKS {
                    b.close_p_in_button_scope(ss);
                }
                b.frameset_ok = false;
                d.switch_mode(ModeId::InTable);
                is_new_afe = false;
            }
            "area" | "br" | "embed" | "img" | "keygen" | "wbr" => {
                b.reconstruct_afe(ss);
                b.frameset_ok = false;
                return Some(b.insert_element(name, attrs, true, ss, sl));
            }
            "input" => {
                b.reconstruct_afe(ss);
                b.frameset_ok = false;
                return Some(b.insert_element(name, attrs, true, ss, sl));
            }
            "menuitem" | "param" | "source" | "track" => {
                return Some(b.insert_element(name, attrs, true, ss, sl));
            }
            "hr" => {
                b.close_p_in_button_scope(ss);
                b.frameset_ok = false;
                return Some(b.insert_element(name, attrs, true, ss, sl));
            }
            "image" => {
                return super::start_tag(b, d, "img", attrs, sc, ss, sl);
            }
            "textarea" => {
                b.frameset_ok = false;
                is_new_afe = false;
            }
            "xmp" => {
                b.close_p_in_button_scope(ss);
                b.reconstruct_afe(ss);
                b.frameset_ok = false;
                is_new_afe = false;
            }
            "iframe" => {
                b.frameset_ok = false;
                is_new_afe = false;
            }
            "noscript" => {
                b.reconstruct_afe(ss);
                is_new_afe = false;
            }
            "noembed" => {
                is_new_afe = false;
            }
            "select" => {
                b.reconstruct_afe(ss);
                b.frameset_ok = false;
                if d.is_in_table_mode() {
                    d.switch_mode(ModeId::InSelectInTable);
                } else {
                    d.switch_mode(ModeId::InSelect);
                }
                is_new_afe = false;
            }
            "optgroup" | "option" => {
                if b.stack.current().map(|e| e.html_name.as_str()) == Some("option") {
                    b.pop(ss, 0);
                }
                b.reconstruct_afe(ss);
                is_new_afe = false;
            }
            "rb" | "rtc" => {
                if b.stack.is_in_scope("ruby") {
                    b.generate_implied_end_tags(None, ss);
                }
                is_new_afe = false;
            }
            "rp" | "rt" => {
                if b.stack.is_in_scope("ruby") {
                    b.generate_implied_end_tags(Some("rtc"), ss);
                }
                is_new_afe = false;
            }
            "math" => {
                b.reconstruct_afe(ss);
                return Some(b.insert_foreign(NS_MATHML, "math", attrs, sc, ss, sl));
            }
            "svg" => {
                b.reconstruct_afe(ss);
                return Some(b.insert_foreign(NS_SVG, "svg", attrs, sc, ss, sl));
            }
            "caption" | "col" | "colgroup" | "frame" | "head" | "tbody" | "td" | "tfoot" | "th"
            | "thead" | "tr" => {
                b.error(&format!("{name} is invalid in body mode"), ss);
                return None;
            }
            _ => {
                b.reconstruct_afe(ss);
                is_new_afe = false;
            }
        }

        let uid = b.insert_element(name, attrs, false, ss, sl);
        if is_new_afe && let Some(elt) = b.stack.item_by_uid(uid) {
            b.afe.push(elt);
        }
        Some(uid)
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "template" => in_head::end_tag(b, d, name, ss, sl),
            "body" => {
                if b.stack.is_in_scope("body") {
                    b.check_unclosed(
                        &[
                            "dd", "dt", "li", "optgroup", "option", "p", "rb", "rp", "rt", "rtc",
                            "tbody", "td", "tfoot", "th", "thead", "tr", "body", "html",
                        ],
                        ss,
                    );
                    d.switch_mode(ModeId::AfterBody);
                }
                None
            }
            "html" => {
                if b.stack.is_in_scope("body") {
                    b.check_unclosed(
                        &[
                            "dd", "dt", "li", "optgroup", "option", "p", "rb", "rp", "rt", "rtc",
                            "tbody", "td", "tfoot", "th", "thead", "tr", "body", "html",
                        ],
                        ss,
                    );
                    d.switch_mode(ModeId::AfterBody);
                    return super::end_tag(b, d, name, ss, sl);
                }
                None
            }
            "address" | "article" | "aside" | "blockquote" | "button" | "center" | "details"
            | "dir" | "div" | "dl" | "fieldset" | "figcaption" | "figure" | "footer" | "header"
            | "listing" | "main" | "menu" | "nav" | "ol" | "pre" | "section" | "summary" | "ul" => {
                if b.stack.is_in_scope(name) {
                    return b.generate_implied_end_tags_and_pop(name, ss, sl);
                }
                None
            }
            "form" => {
                if b.stack.has_template() {
                    if b.stack.is_in_scope("form") {
                        return b.generate_implied_end_tags_and_pop("form", ss, sl);
                    }
                    None
                } else if b.form_element.is_some() {
                    b.generate_implied_end_tags(None, ss);
                    b.pop(ss, sl)
                } else {
                    None
                }
            }
            "p" => {
                if !b.stack.is_in_button_scope("p") {
                    b.insert_element("p", Attributes::new(), false, ss, 0);
                    b.pop(ss, sl);
                }
                b.generate_implied_end_tags_and_pop("p", ss, sl)
            }
            "li" | "dd" | "dt" => {
                if b.stack.is_in_list_scope(name)
                    || (name == "dd" && b.stack.is_in_scope("dt"))
                    || (name == "dt" && b.stack.is_in_scope("dd"))
                {
                    return b.generate_implied_end_tags_and_pop(name, ss, sl);
                }
                None
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if ["h1", "h2", "h3", "h4", "h5", "h6"]
                    .iter()
                    .any(|h| b.stack.is_in_scope(h))
                {
                    b.generate_implied_end_tags(None, ss);
                    return b.pop_all_up_to_names(&["h1", "h2", "h3", "h4", "h5", "h6"], ss, sl);
                }
                None
            }
            "a" | "b" | "big" | "code" | "em" | "font" | "i" | "nobr" | "s" | "small"
            | "strike" | "strong" | "tt" | "u" => b.adoption_agency(name, ss, sl),
            "applet" | "marquee" | "object" => {
                if b.stack.is_in_scope(name) {
                    b.generate_implied_end_tags(None, ss);
                    let result = b.pop_all_up_to_name(name, ss, sl);
                    b.afe.clear_to_marker();
                    return result;
                }
                None
            }
            "br" => {
                b.error("end tag </br> is invalid, assuming start tag", ss);
                super::start_tag(b, d, name, Attributes::new(), false, ss, sl)
            }
            _ => b.any_other_end_tag(name, ss, sl),
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        b.check_unclosed(
            &[
                "dd", "dt", "li", "p", "tbody", "td", "tfoot", "th", "thead", "tr", "body", "html",
            ],
            pos,
        );
        if !d.template_mode_stack_is_empty() {
            in_template::end_document(b, d, pos);
        } else {
            b.stop_parsing(pos);
        }
    }
}

// The remaining modes (Text, InTable, InTableText, InCaption, InColumnGroup,
// InTableBody, InRow, InCell, InSelect, InSelectInTable, InTemplate, AfterBody,
// AfterAfterBody, InForeignContent) are defined in the following statements.

mod text_mode {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        _d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        b.insert_characters(text, start, length, ss, sl);
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        _name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        let popped = b.pop(ss, sl);
        d.restore_mode();
        popped
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        b.error("unexpected end of input in text mode", pos);
        b.pop(pos, 0);
        d.restore_mode();
        super::end_document(b, d, pos);
    }
}

mod in_table {
    use super::*;
    const TABLE_CONTEXT: &[&str] = &["table", "template", "html"];
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let allowed = ["table", "tbody", "tfoot", "thead", "tr"];
        if b.stack
            .current()
            .map(|e| allowed.contains(&e.html_name.as_str()))
            == Some(true)
        {
            d.switch_and_save(ModeId::InTableText);
            super::characters(b, d, text, start, length, ss, sl);
        } else {
            b.error("unexpected text in table, fostering", ss);
            b.foster_parenting = true;
            in_body::characters(b, d, text, start, length, ss, sl);
            b.foster_parenting = false;
        }
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "caption" => {
                b.clear_stack_back(TABLE_CONTEXT, ss);
                b.afe.insert_marker();
                d.switch_mode(ModeId::InCaption);
                Some(b.insert_element(name, attrs, false, ss, sl))
            }
            "colgroup" => {
                b.clear_stack_back(TABLE_CONTEXT, ss);
                d.switch_mode(ModeId::InColumnGroup);
                Some(b.insert_element(name, attrs, false, ss, sl))
            }
            "col" => {
                b.clear_stack_back(TABLE_CONTEXT, ss);
                b.insert_element("colgroup", Attributes::new(), false, ss, 0);
                d.switch_mode(ModeId::InColumnGroup);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            "tbody" | "tfoot" | "thead" => {
                b.clear_stack_back(TABLE_CONTEXT, ss);
                let uid = b.insert_element(name, attrs, false, ss, sl);
                d.switch_mode(ModeId::InTableBody);
                Some(uid)
            }
            "td" | "th" | "tr" => {
                b.clear_stack_back(TABLE_CONTEXT, ss);
                b.insert_element("tbody", Attributes::new(), false, ss, 0);
                d.switch_mode(ModeId::InTableBody);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            "table" => {
                b.error("unexpected <table> in table", ss);
                if !b.stack.is_in_table_scope("table") {
                    return None;
                }
                b.pop_all_up_to_name("table", ss, 0);
                d.reset(b);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            "style" | "script" | "template" => in_head::start_tag(b, d, name, attrs, sc, ss, sl),
            "form" => {
                if b.stack.has_template() || b.form_element.is_some() {
                    b.error("invalid form in table, ignoring", ss);
                    return None;
                }
                b.error("invalid form in table, inserting void element", ss);
                let uid = b.insert_element("form", attrs, true, ss, sl);
                b.form_element = b.stack.item_by_uid(uid).cloned();
                Some(uid)
            }
            "input" => {
                let is_hidden =
                    attrs.get("type").map(|t| t.eq_ignore_ascii_case("hidden")) == Some(true);
                if is_hidden {
                    b.error("begrudgingly accepting a hidden input in table mode", ss);
                    return Some(b.insert_element(name, attrs, true, ss, sl));
                }
                b.error("invalid start tag in table, fostering", ss);
                b.foster_parenting = true;
                let result = in_body::start_tag(b, d, name, attrs, sc, ss, sl);
                b.foster_parenting = false;
                result
            }
            _ => {
                b.error("invalid start tag in table, fostering", ss);
                b.foster_parenting = true;
                let result = in_body::start_tag(b, d, name, attrs, sc, ss, sl);
                b.foster_parenting = false;
                result
            }
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "table" => {
                if !b.stack.is_in_table_scope("table") {
                    b.error("</table> found but no table element in scope, ignoring", ss);
                    return None;
                }
                let result = b.pop_all_up_to_name("table", ss, sl);
                d.reset(b);
                result
            }
            "body" | "caption" | "col" | "colgroup" | "html" | "tbody" | "td" | "tfoot" | "th"
            | "thead" | "tr" => {
                b.error("ignoring invalid end tag inside table", ss);
                None
            }
            "template" => in_head::end_tag(b, d, name, ss, sl),
            _ => {
                b.error("unexpected end tag in table, fostering", ss);
                b.foster_parenting = true;
                let result = in_body::end_tag(b, d, name, ss, sl);
                b.foster_parenting = false;
                result
            }
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        in_body::end_document(b, d, pos);
    }
}

mod in_table_text {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        _d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        _ss: usize,
        _sl: usize,
    ) {
        b.pending_table_characters
            .push(text[start..start + length].to_string());
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        d.flush_table_text(b);
        d.restore_mode();
        super::start_tag(b, d, name, attrs, sc, ss, sl)
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        d.flush_table_text(b);
        d.restore_mode();
        super::end_tag(b, d, name, ss, sl)
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        d.flush_table_text(b);
        d.restore_mode();
        super::end_document(b, d, pos);
    }
}

mod in_caption {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        in_body::characters(b, d, text, start, length, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr" => {
                b.error(&format!("start tag <{name}> not allowed in caption"), ss);
                if !b.stack.is_in_table_scope("caption") {
                    return None;
                }
                b.pop_all_up_to_name("caption", ss, 0);
                b.afe.clear_to_marker();
                d.switch_mode(ModeId::InTable);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            _ => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "caption" => {
                if !b.stack.is_in_table_scope("caption") {
                    b.error(
                        "</caption> matches a start tag which is not in scope, ignoring",
                        ss,
                    );
                    return None;
                }
                b.generate_implied_end_tags(None, ss);
                let result = b.pop_all_up_to_name("caption", ss, sl);
                b.afe.clear_to_marker();
                d.switch_mode(ModeId::InTable);
                result
            }
            "table" => {
                if !b.stack.is_in_table_scope("caption") {
                    b.error(
                        "</table> found in caption, but there is no caption in scope, ignoring",
                        ss,
                    );
                    return None;
                }
                b.generate_implied_end_tags(None, ss);
                b.pop_all_up_to_name("caption", ss, 0);
                b.afe.clear_to_marker();
                d.switch_mode(ModeId::InTable);
                super::end_tag(b, d, name, ss, sl)
            }
            "body" | "col" | "colgroup" | "html" | "tbody" | "td" | "tfoot" | "th" | "thead"
            | "tr" => {
                b.error(&format!("end tag </{name}> ignored in caption mode"), ss);
                None
            }
            _ => in_body::end_tag(b, d, name, ss, sl),
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        in_body::end_document(b, d, pos);
    }
}

mod in_column_group {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (ws, non_ws) = split_ws(text, start, length);
        if ws.1 > 0 {
            b.insert_characters(text, ws.0, ws.1, ss, sl);
        }
        if non_ws.1 == 0 {
            return;
        }
        if b.stack.current().map(|e| e.html_name.as_str()) != Some("colgroup") {
            b.error(
                "text should close the colgroup but another element is open",
                ss,
            );
            return;
        }
        b.pop(ss, 0);
        d.switch_mode(ModeId::InTable);
        super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "html" => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
            "col" => Some(b.insert_element(name, attrs, true, ss, sl)),
            "template" => in_head::start_tag(b, d, name, attrs, sc, ss, sl),
            _ => {
                if b.stack.current().map(|e| e.html_name.as_str()) != Some("colgroup") {
                    b.error(
                        "start tag should close the colgroup but another element is open",
                        ss,
                    );
                    return None;
                }
                b.pop(ss, 0);
                d.switch_mode(ModeId::InTable);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "colgroup" => {
                if b.stack.current().map(|e| e.html_name.as_str()) != Some("colgroup") {
                    b.error(
                        "</colgroup> found but another element is open, ignoring",
                        ss,
                    );
                    return None;
                }
                let popped = b.pop(ss, sl);
                d.switch_mode(ModeId::InTable);
                popped
            }
            "col" => {
                b.error("</col> found in column group mode, ignoring", ss);
                None
            }
            "template" => in_head::end_tag(b, d, name, ss, sl),
            _ => {
                if b.stack.current().map(|e| e.html_name.as_str()) != Some("colgroup") {
                    b.error("non-matching end tag should close the colgroup but another element is open", ss);
                    return None;
                }
                b.pop(ss, 0);
                d.switch_mode(ModeId::InTable);
                super::end_tag(b, d, name, ss, sl)
            }
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        in_body::end_document(b, d, pos);
    }
}

mod in_table_body {
    use super::*;
    const TABLE_BODY_CONTEXT: &[&str] = &["tbody", "tfoot", "thead", "template", "html"];
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        in_table::characters(b, d, text, start, length, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "tr" => {
                b.clear_stack_back(TABLE_BODY_CONTEXT, ss);
                let uid = b.insert_element(name, attrs, false, ss, sl);
                d.switch_mode(ModeId::InRow);
                Some(uid)
            }
            "th" | "td" => {
                b.clear_stack_back(TABLE_BODY_CONTEXT, ss);
                b.insert_element("tr", Attributes::new(), false, ss, 0);
                d.switch_mode(ModeId::InRow);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead" => {
                let in_scope = b.stack.is_in_table_scope("tbody")
                    || b.stack.is_in_table_scope("thead")
                    || b.stack.is_in_table_scope("tfoot");
                if !in_scope {
                    b.error(&format!("<{name}> encountered in table body mode when there is no tbody/thead/tfoot in scope"), ss);
                    return None;
                }
                b.clear_stack_back(TABLE_BODY_CONTEXT, ss);
                b.pop(ss, 0);
                d.switch_mode(ModeId::InTable);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            _ => in_table::start_tag(b, d, name, attrs, sc, ss, sl),
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "tbody" | "tfoot" | "thead" => {
                if !b.stack.is_in_table_scope(name) {
                    b.error(&format!("</{name}> found but no {name} in scope"), ss);
                    return None;
                }
                b.clear_stack_back(TABLE_BODY_CONTEXT, ss);
                let popped = b.pop(ss, sl);
                d.switch_mode(ModeId::InTable);
                popped
            }
            "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th" | "tr" => {
                b.error(&format!("</{name}> found in table body mode, ignoring"), ss);
                None
            }
            _ => in_table::end_tag(b, d, name, ss, sl),
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        in_table::end_document(b, d, pos);
    }
}

mod in_row {
    use super::*;
    const TABLE_ROW_CONTEXT: &[&str] = &["tr", "template", "html"];
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        in_table::characters(b, d, text, start, length, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "th" | "td" => {
                b.clear_stack_back(TABLE_ROW_CONTEXT, ss);
                let uid = b.insert_element(name, attrs, false, ss, sl);
                d.switch_mode(ModeId::InCell);
                b.afe.insert_marker();
                Some(uid)
            }
            "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead" | "tr" => {
                if !b.stack.is_in_table_scope("tr") {
                    b.error(
                        &format!("<{name}> should close the tr but it is not in scope"),
                        ss,
                    );
                    return None;
                }
                b.clear_stack_back(TABLE_ROW_CONTEXT, ss);
                b.pop(ss, 0);
                d.switch_mode(ModeId::InTableBody);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            _ => in_table::start_tag(b, d, name, attrs, sc, ss, sl),
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "tr" => {
                if !b.stack.is_in_table_scope("tr") {
                    b.error("</tr> found but no tr element in scope", ss);
                    return None;
                }
                b.clear_stack_back(TABLE_ROW_CONTEXT, ss);
                let popped = b.pop(ss, sl);
                d.switch_mode(ModeId::InTableBody);
                popped
            }
            "table" => {
                if !b.stack.is_in_table_scope("tr") {
                    b.error("</table> should close the tr but it is not in scope", ss);
                    return None;
                }
                b.clear_stack_back(TABLE_ROW_CONTEXT, ss);
                b.pop(ss, 0);
                d.switch_mode(ModeId::InTableBody);
                super::end_tag(b, d, name, ss, sl)
            }
            "tbody" | "tfoot" | "thead" => {
                if !b.stack.is_in_table_scope(name) || !b.stack.is_in_table_scope("tr") {
                    return None;
                }
                b.clear_stack_back(TABLE_ROW_CONTEXT, ss);
                b.pop(ss, 0);
                d.switch_mode(ModeId::InTableBody);
                super::end_tag(b, d, name, ss, sl)
            }
            "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th" => {
                b.error(&format!("</{name}> encountered in row mode, ignoring"), ss);
                None
            }
            _ => in_table::end_tag(b, d, name, ss, sl),
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        in_table::end_document(b, d, pos);
    }
}

mod in_cell {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        in_body::characters(b, d, text, start, length, ss, sl);
    }
    fn close_the_cell<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, ss: usize) {
        b.generate_implied_end_tags(None, ss);
        let cur = b.stack.current().map(|e| e.html_name.clone());
        if !matches!(cur.as_deref(), Some("td") | Some("th")) {
            b.error(
                "closing the cell but there are tags open which can't be closed automatically",
                ss,
            );
        }
        b.afe.clear_to_marker();
        d.switch_mode(ModeId::InRow);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr" => {
                if !b.stack.is_in_table_scope("td") && !b.stack.is_in_table_scope("th") {
                    b.error(
                        &format!("<{name}> tag should close the cell but none is in scope"),
                        ss,
                    );
                    return None;
                }
                close_the_cell(b, d, ss);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            _ => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "td" | "th" => {
                if !b.stack.is_in_table_scope(name) {
                    b.error(
                        &format!("</{name}> encountered but there is no {name} in scope, ignoring"),
                        ss,
                    );
                    return None;
                }
                b.generate_implied_end_tags(None, ss);
                let result = b.pop_all_up_to_name(name, ss, sl);
                b.afe.clear_to_marker();
                d.switch_mode(ModeId::InRow);
                result
            }
            "body" | "caption" | "col" | "colgroup" | "html" => {
                b.error(&format!("unexpected </{name}> in cell, ignoring"), ss);
                None
            }
            "table" | "tbody" | "tfoot" | "thead" | "tr" => {
                if !b.stack.is_in_table_scope(name) {
                    b.error(
                        &format!("</{name}> encountered but there is no {name} in scope, ignoring"),
                        ss,
                    );
                    return None;
                }
                close_the_cell(b, d, ss);
                super::end_tag(b, d, name, ss, sl)
            }
            _ => in_body::end_tag(b, d, name, ss, sl),
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        in_body::end_document(b, d, pos);
    }
}

mod in_select {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        _d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        // Strip nulls (mirror `InsertionMode::stripNulls`): skip NUL and
        // insert the remainder as a single run.
        let slice = &text[start..start + length];
        let filtered: String = slice.chars().filter(|&c| c != '\0').collect();
        if !filtered.is_empty() {
            b.insert_characters(&filtered, 0, filtered.len(), ss, sl);
        }
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "html" => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
            "option" => {
                if b.stack.current().map(|e| e.html_name.as_str()) == Some("option") {
                    b.pop(ss, 0);
                }
                Some(b.insert_element("option", attrs, false, ss, sl))
            }
            "optgroup" => {
                if b.stack.current().map(|e| e.html_name.as_str()) == Some("option") {
                    b.pop(ss, 0);
                }
                if b.stack.current().map(|e| e.html_name.as_str()) == Some("optgroup") {
                    b.pop(ss, 0);
                }
                Some(b.insert_element("optgroup", attrs, false, ss, sl))
            }
            "select" => {
                if !b.stack.is_in_select_scope("select") {
                    b.error(
                        "<select> found in select mode but no select element is in scope, ignoring",
                        ss,
                    );
                    return None;
                }
                b.error("<select> found inside a select element", ss);
                b.pop_all_up_to_name("select", ss, sl);
                d.reset(b);
                None
            }
            "input" | "keygen" | "textarea" => {
                b.error(&format!("<{name}> found inside a select element"), ss);
                if !b.stack.is_in_select_scope("select") {
                    return None;
                }
                b.pop_all_up_to_name("select", ss, 0);
                d.reset(b);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            "script" | "template" => in_head::start_tag(b, d, name, attrs, sc, ss, sl),
            _ => {
                b.error(
                    &format!("<{name}> found inside a select element, ignoring"),
                    ss,
                );
                None
            }
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "optgroup" => {
                if b.stack.current().map(|e| e.html_name.as_str()) == Some("option")
                    && b.stack.length() >= 2
                {
                    let penultimate = b.stack.item(b.stack.length() - 2);
                    if penultimate.html_name == "optgroup" {
                        b.pop(ss, 0);
                    }
                }
                if b.stack.current().map(|e| e.html_name.as_str()) != Some("optgroup") {
                    b.error("unexpected </optgroup>, ignoring", ss);
                    return None;
                }
                b.pop(ss, sl)
            }
            "option" => {
                if b.stack.current().map(|e| e.html_name.as_str()) != Some("option") {
                    b.error("unexpected </option>, ignoring", ss);
                    return None;
                }
                b.pop(ss, sl)
            }
            "select" => {
                if !b.stack.is_in_select_scope("select") {
                    b.error("</select> found but the select element is not in scope", ss);
                    return None;
                }
                let result = b.pop_all_up_to_name("select", ss, sl);
                d.reset(b);
                result
            }
            "template" => in_head::end_tag(b, d, name, ss, sl),
            _ => {
                b.error(&format!("unexpected </{name}> in select, ignoring"), ss);
                None
            }
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        in_body::end_document(b, d, pos);
    }
}

mod in_select_in_table {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        in_select::characters(b, d, text, start, length, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th" => {
                b.error(
                    &format!("unexpected <{name}> in select in table, closing select"),
                    ss,
                );
                b.pop_all_up_to_name("select", ss, 0);
                d.reset(b);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
            _ => in_select::start_tag(b, d, name, attrs, sc, ss, sl),
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th" => {
                if !b.stack.is_in_table_scope(name) {
                    b.error(
                        &format!("unexpected </{name}> in select in table, ignoring"),
                        ss,
                    );
                    return None;
                }
                b.error(
                    &format!("unexpected </{name}> in select in table, closing select"),
                    ss,
                );
                b.pop_all_up_to_name("select", ss, 0);
                d.reset(b);
                super::end_tag(b, d, name, ss, sl)
            }
            _ => in_select::end_tag(b, d, name, ss, sl),
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        in_select::end_document(b, d, pos);
    }
}

mod in_template {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        in_body::characters(b, d, text, start, length, ss, sl);
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style"
            | "template" | "title" => {
                return in_head::start_tag(b, d, name, attrs, sc, ss, sl);
            }
            "caption" | "colgroup" | "tbody" | "tfoot" | "thead" => {
                d.template_mode_stack_pop();
                d.template_mode_stack_push(ModeId::InTable);
                d.switch_mode(ModeId::InTable);
            }
            "col" => {
                d.template_mode_stack_pop();
                d.template_mode_stack_push(ModeId::InColumnGroup);
                d.switch_mode(ModeId::InColumnGroup);
            }
            "tr" => {
                d.template_mode_stack_pop();
                d.template_mode_stack_push(ModeId::InTableBody);
                d.switch_mode(ModeId::InTableBody);
            }
            "td" | "th" => {
                d.template_mode_stack_pop();
                d.template_mode_stack_push(ModeId::InRow);
                d.switch_mode(ModeId::InRow);
            }
            _ => {
                d.template_mode_stack_pop();
                d.template_mode_stack_push(ModeId::InBody);
                d.switch_mode(ModeId::InBody);
            }
        }
        super::start_tag(b, d, name, attrs, sc, ss, sl)
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "template" => in_head::end_tag(b, d, name, ss, sl),
            _ => {
                b.error(&format!("unexpected </{name}> in template, ignoring"), ss);
                None
            }
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, d: &mut Dispatcher, pos: usize) {
        if !b.stack.has_template() {
            b.stop_parsing(pos);
            return;
        }
        b.error("unexpected end of file in template", pos);
        b.pop_all_up_to_name("template", pos, 0);
        b.afe.clear_to_marker();
        d.template_mode_stack_pop();
        d.reset(b);
        super::end_document(b, d, pos);
    }
}

mod after_body {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (ws, non_ws) = split_ws(text, start, length);
        if ws.1 > 0 {
            in_body::characters(b, d, text, ws.0, ws.1, ss, sl);
        }
        if non_ws.1 > 0 {
            b.error("unexpected non-whitespace character after body", ss);
            d.switch_mode(ModeId::InBody);
            super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
        }
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "html" => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
            _ => {
                b.error("unexpected start tag after body", ss);
                d.switch_mode(ModeId::InBody);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "html" => {
                if b.is_fragment {
                    b.error("unexpected </html> in fragment", ss);
                    return None;
                }
                d.switch_mode(ModeId::AfterAfterBody);
                None
            }
            _ => {
                b.error("unexpected end tag after body", ss);
                d.switch_mode(ModeId::InBody);
                super::end_tag(b, d, name, ss, sl)
            }
        }
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, _d: &mut Dispatcher, pos: usize) {
        b.stop_parsing(pos);
    }
}

mod after_after_body {
    use super::*;
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        let (ws, non_ws) = split_ws(text, start, length);
        if ws.1 > 0 {
            b.insert_characters(text, ws.0, ws.1, ss, sl);
        }
        if non_ws.1 > 0 {
            b.error("unexpected non-whitespace characters after after body", ss);
            d.switch_mode(ModeId::InBody);
            super::characters(b, d, text, non_ws.0, non_ws.1, ss, sl);
        }
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        match name {
            "html" => in_body::start_tag(b, d, name, attrs, sc, ss, sl),
            _ => {
                b.error("unexpected start tag after after body", ss);
                d.switch_mode(ModeId::InBody);
                super::start_tag(b, d, name, attrs, sc, ss, sl)
            }
        }
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        b.error("unexpected end tag after after body", ss);
        d.switch_mode(ModeId::InBody);
        super::end_tag(b, d, name, ss, sl)
    }
    pub fn end_document<H: TreeHandler>(b: &mut TreeBuilder<H>, _d: &mut Dispatcher, pos: usize) {
        b.stop_parsing(pos);
    }
}

mod in_foreign {
    use super::*;
    const NOT_ALLOWED: &[&str] = &[
        "b",
        "big",
        "blockquote",
        "body",
        "br",
        "center",
        "code",
        "dd",
        "div",
        "dl",
        "dt",
        "em",
        "embed",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "head",
        "hr",
        "i",
        "img",
        "li",
        "listing",
        "menu",
        "meta",
        "nobr",
        "ol",
        "p",
        "pre",
        "ruby",
        "s",
        "small",
        "span",
        "strong",
        "strike",
        "sub",
        "sup",
        "table",
        "tt",
        "u",
        "ul",
        "var",
    ];
    pub fn characters<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        _d: &mut Dispatcher,
        text: &str,
        start: usize,
        length: usize,
        ss: usize,
        sl: usize,
    ) {
        b.frameset_ok = false;
        b.insert_characters(text, start, length, ss, sl);
    }
    fn is_integration_point(e: &Element) -> bool {
        e.namespace == NS_HTML || e.is_mathml_text_integration() || e.is_html_integration()
    }
    pub fn start_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        attrs: Attributes,
        sc: bool,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        let allowed = !NOT_ALLOWED.contains(&name);
        if !allowed {
            b.error(&format!("unexpected <{name}> tag in foreign content"), ss);
            if !b.is_fragment {
                while let Some(cur) = b.stack.current() {
                    if is_integration_point(cur) {
                        break;
                    }
                    b.pop(ss, 0);
                }
                return super::start_tag(b, d, name, attrs, sc, ss, sl);
            }
        }
        let acn_ns = b
            .adjusted_current_node()
            .and_then(|uid| b.stack.item_by_uid(uid).map(|e| e.namespace.clone()))
            .unwrap_or(NS_HTML.to_string());
        Some(b.insert_foreign(&acn_ns, name, attrs, sc, ss, sl))
    }
    pub fn end_tag<H: TreeHandler>(
        b: &mut TreeBuilder<H>,
        d: &mut Dispatcher,
        name: &str,
        ss: usize,
        sl: usize,
    ) -> Option<usize> {
        let node = b.stack.current().map(|e| e.name.clone());
        if node.as_deref().map(|n| !n.eq_ignore_ascii_case(name)) == Some(true) {
            b.error("mismatched end tag in foreign content", ss);
        }
        for idx in (1..b.stack.length()).rev() {
            let elt = b.stack.item(idx);
            if elt.name.eq_ignore_ascii_case(name) {
                let uid = elt.uid;
                b.pop_all_up_to_element(uid, ss, sl);
                return Some(uid);
            }
            if elt.namespace == NS_HTML {
                // Re-dispatch as the current handler's end tag.
                return super::end_tag(b, d, name, ss, sl);
            }
        }
        None
    }
    pub fn end_document<H: TreeHandler>(_b: &mut TreeBuilder<H>, _d: &mut Dispatcher, _pos: usize) {
    }
}

// Helpers.

fn is_heading(name: Option<&str>) -> bool {
    matches!(name, Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
}

fn is_html_ws(text: &str, start: usize, length: usize) -> bool {
    text[start..start + length]
        .chars()
        .all(|c| matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' '))
}
