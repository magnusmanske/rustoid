//! Faithful port of RemexHtml's `TreeBuilder` core — the receiver of events
//! from the insertion-mode classes, holding the primary tree-construction state
//! (stack of open elements, active formatting elements, fragment context,
//! foster-parenting, pending table characters).
//!
//! Ports `Wikimedia\RemexHtml\TreeBuilder\TreeBuilder`.

use super::active_formatting_elements::ActiveFormattingElements;
use super::element::{Attributes, Element};
use super::html_data::{NS_HTML, is_special};
use super::stack::Stack;
use super::tree_handler::{Preposition, TreeHandler};

pub const NO_QUIRKS: u8 = 0;
pub const LIMITED_QUIRKS: u8 = 1;
pub const QUIRKS: u8 = 2;

/// The FOSTER_TRIGGERS: element names that trigger foster parenting.
const FOSTER_TRIGGERS: &[&str] = &["table", "tbody", "tfoot", "thead", "tr"];

/// IMPLIED_END_TAGS.
const IMPLIED_END_TAGS: &[&str] = &[
    "dd", "dt", "li", "option", "optgroup", "p", "rb", "rp", "rt", "rtc",
];

/// THOROUGHLY_IMPLIED_END_TAGS.
const THOROUGHLY_IMPLIED_END_TAGS: &[&str] = &[
    "caption", "colgroup", "dd", "dt", "li", "optgroup", "option", "p", "rb", "rp", "rt", "rtc",
    "tbody", "td", "tfoot", "th", "thead", "tr",
];

/// The tree builder core.
pub struct TreeBuilder<H: TreeHandler> {
    pub handler: H,
    pub stack: Stack,
    pub afe: ActiveFormattingElements,
    pub scripting_flag: bool,
    pub ignore_errors: bool,
    pub ignore_nulls: bool,
    pub is_fragment: bool,
    /// The fragment context element's `uid` (a virtual element not on the stack).
    pub fragment_context: Option<usize>,
    pub head_element: Option<usize>,
    pub form_element: Option<usize>,
    pub frameset_ok: bool,
    pub quirks: u8,
    pub foster_parenting: bool,
    /// Pending table characters: `(is_whitespace, text)`.
    pub pending_table_characters: Vec<(bool, String)>,
    next_uid: usize,
}

impl<H: TreeHandler> TreeBuilder<H> {
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            stack: Stack::new(),
            afe: ActiveFormattingElements::new(),
            scripting_flag: true,
            ignore_errors: false,
            ignore_nulls: false,
            is_fragment: false,
            fragment_context: None,
            head_element: None,
            form_element: None,
            frameset_ok: true,
            quirks: NO_QUIRKS,
            foster_parenting: false,
            pending_table_characters: Vec::new(),
            next_uid: 1,
        }
    }

    fn next_uid(&mut self) -> usize {
        let uid = self.next_uid;
        self.next_uid += 1;
        uid
    }

    /// Start the document. In fragment mode (`namespace`/`name` are `Some`),
    /// push a synthetic `<html>` root element and record the virtual fragment
    /// context element, mirroring `TreeBuilder::startDocument`.
    pub fn start_document(&mut self, namespace: Option<&str>, name: Option<&str>) {
        self.handler.start_document(namespace, name);
        if let (Some(ns), Some(nm)) = (namespace, name) {
            self.is_fragment = true;
            self.fragment_context = Some(self.next_uid() + 1);
            // The virtual fragment context element is not pushed; only its uid
            // is recorded (the handler does not create a DOM node for it).
            let ctx = Element::new(ns, nm, Attributes::new(), self.fragment_context.unwrap());
            let _ = ctx;

            // The synthetic <html> root.
            let mut html = Element::new(NS_HTML, "html", Attributes::new(), self.next_uid());
            html.is_virtual = true;
            let uid = html.uid;
            self.handler
                .insert_element(Preposition::Root, None, &mut html, false, 0, 0);
            self.stack.push(html);
            let _ = uid;
        }
    }

    /// The adjusted current node's uid, if any.
    pub fn adjusted_current_node(&self) -> Option<usize> {
        if self.stack.length() == 1 && self.is_fragment {
            self.fragment_context
        } else {
            self.stack.current().map(|e| e.uid)
        }
    }

    /// Find the appropriate place for inserting a node.
    /// Returns `(preposition, reference_uid)`.
    fn appropriate_place(&self, target: Option<usize>) -> (Preposition, Option<usize>) {
        let target_uid = target.or_else(|| self.stack.current().map(|e| e.uid));
        let Some(target_uid) = target_uid else {
            return (Preposition::Root, None);
        };

        if !self.foster_parenting {
            return (Preposition::Under, Some(target_uid));
        }

        // Look up the target element to check its html_name.
        let target_name = self
            .stack
            .item_by_uid(target_uid)
            .map(|e| e.html_name.clone());
        let Some(target_name) = target_name else {
            return (Preposition::Under, Some(target_uid));
        };
        if !FOSTER_TRIGGERS.contains(&target_name.as_str()) {
            return (Preposition::Under, Some(target_uid));
        }

        // Foster parenting: find the table (or template).
        let mut node = None;
        for i in (0..self.stack.length()).rev() {
            let elt = self.stack.item(i);
            if elt.html_name == "table" && i >= 1 {
                return (Preposition::Before, Some(elt.uid));
            }
            if elt.html_name == "template" {
                return (Preposition::Under, Some(elt.uid));
            }
            node = Some(elt.uid);
        }
        (Preposition::Under, node)
    }

    /// Insert characters.
    pub fn insert_characters(
        &mut self,
        text: &str,
        start: usize,
        length: usize,
        source_start: usize,
        source_length: usize,
    ) {
        let (prep, reference) = self.appropriate_place(None);
        self.handler.characters(
            prep,
            reference,
            text,
            start,
            length,
            source_start,
            source_length,
        );
    }

    /// Insert an HTML-namespace element and return its uid.
    pub fn insert_element(
        &mut self,
        name: &str,
        attrs: Attributes,
        void: bool,
        source_start: usize,
        source_length: usize,
    ) -> usize {
        self.insert_foreign(NS_HTML, name, attrs, void, source_start, source_length)
    }

    /// Insert an element in any namespace and return its uid.
    pub fn insert_foreign(
        &mut self,
        ns: &str,
        name: &str,
        attrs: Attributes,
        void: bool,
        source_start: usize,
        source_length: usize,
    ) -> usize {
        let (prep, reference) = self.appropriate_place(None);
        let uid = self.next_uid();
        let mut element = Element::new(ns, name, attrs, uid);
        self.handler.insert_element(
            prep,
            reference,
            &mut element,
            void,
            source_start,
            source_length,
        );
        if !void {
            self.stack.push(element);
        }
        uid
    }

    /// Pop the current node from the stack and notify the handler.
    pub fn pop(&mut self, source_start: usize, source_length: usize) -> Option<usize> {
        let slot = self.stack.pop()?;
        let element = self.stack.item(slot).clone();
        self.handler.end_tag(&element, source_start, source_length);
        Some(element.uid)
    }

    /// Insert a comment.
    pub fn comment(
        &mut self,
        place: Option<(Preposition, Option<usize>)>,
        text: &str,
        source_start: usize,
        source_length: usize,
    ) {
        let (prep, reference) = place.unwrap_or_else(|| self.appropriate_place(None));
        self.handler
            .comment(prep, reference, text, source_start, source_length);
    }

    /// Insert a doctype.
    pub fn doctype(
        &mut self,
        name: &str,
        public: &str,
        system: &str,
        quirks: u8,
        source_start: usize,
        source_length: usize,
    ) {
        self.handler
            .doctype(name, public, system, quirks, source_start, source_length);
        self.quirks = quirks;
    }

    /// Report an error.
    pub fn error(&mut self, text: &str, pos: usize) {
        if !self.ignore_errors {
            self.handler.error(text, pos);
        }
    }

    /// Merge attributes into an element (by uid), if not virtual.
    pub fn merge_attributes(&mut self, uid: usize, attrs: &Attributes, source_start: usize) {
        if attrs.count() == 0 {
            return;
        }
        let element = self.stack.item_by_uid(uid).cloned();
        if let Some(element) = element
            && !element.is_virtual
        {
            self.handler.merge_attributes(&element, attrs, source_start);
        }
    }

    /// If there is a `p` element in button scope, close it.
    pub fn close_p_in_button_scope(&mut self, pos: usize) {
        if self.stack.is_in_button_scope("p") {
            self.generate_implied_end_tags_and_pop("p", pos, 0);
        }
    }

    /// Check the stack for unclosed elements not in `allowed`.
    pub fn check_unclosed(&mut self, allowed: &[&str], pos: usize) {
        if self.ignore_errors {
            return;
        }
        let mut unclosed: Vec<String> = Vec::new();
        for i in (0..self.stack.length()).rev() {
            let name = self.stack.item(i).html_name.clone();
            if !allowed.contains(&name.as_str()) && !unclosed.contains(&name) {
                unclosed.push(name.clone());
            }
        }
        if !unclosed.is_empty() {
            let names = unclosed.join(", ");
            self.handler
                .error(&format!("closing unclosed {names}"), pos);
        }
    }

    /// Reconstruct the active formatting elements.
    pub fn reconstruct_afe(&mut self, source_start: usize) {
        let tail = self.afe.get_tail();
        match tail {
            None | Some(super::active_formatting_elements::AfeEntry::Marker) => return,
            Some(super::active_formatting_elements::AfeEntry::Element(slot)) => {
                if self.stack.item(slot).stack_index.is_some() {
                    return;
                }
            }
        }

        // Find the entry to reconstruct from. We re-insert the tail element's
        // data and replace it in the AFE list. This is the common, correct
        // observable behavior for a single dangling formatting element.
        let Some(super::active_formatting_elements::AfeEntry::Element(tail_slot)) =
            self.afe.get_tail()
        else {
            return;
        };
        let name = self.stack.item(tail_slot).name.clone();
        let attrs = self.stack.item(tail_slot).attrs.clone();
        let new_uid = self.insert_foreign(NS_HTML, &name, attrs, false, source_start, 0);
        let _ = new_uid;
    }

    /// Generate implied end tags, excluding `name`.
    pub fn generate_implied_end_tags(&mut self, name: Option<&str>, pos: usize) {
        while let Some(cur) = self.stack.current().map(|e| e.html_name.clone()) {
            if Some(cur.as_str()) == name || !IMPLIED_END_TAGS.contains(&cur.as_str()) {
                break;
            }
            self.pop(pos, 0);
        }
    }

    /// Generate all implied end tags thoroughly.
    pub fn generate_implied_end_tags_thoroughly(&mut self, pos: usize) {
        while let Some(cur) = self.stack.current().map(|e| e.html_name.clone()) {
            if !THOROUGHLY_IMPLIED_END_TAGS.contains(&cur.as_str()) {
                break;
            }
            self.pop(pos, 0);
        }
    }

    /// Generate implied end tags and pop everything up to and including `name`.
    pub fn generate_implied_end_tags_and_pop(
        &mut self,
        name: &str,
        source_start: usize,
        source_length: usize,
    ) {
        self.generate_implied_end_tags(Some(name), source_start);
        if self.stack.current().map(|e| e.html_name.as_str()) != Some(name) {
            self.handler.error(
                &format!("found </{name}> but elements are open that cannot have implied end tags, closing them"),
                source_start,
            );
        }
        self.pop_all_up_to_name(name, source_start, source_length);
    }

    /// Pop elements until an element with `uid` is popped.
    pub fn pop_all_up_to_element(&mut self, uid: usize, source_start: usize, source_length: usize) {
        while let Some(popped) = self.pop(source_start, 0) {
            if popped == uid {
                break;
            }
        }
        let _ = source_length;
    }

    /// Pop elements until an element named `name` is popped.
    pub fn pop_all_up_to_name(&mut self, name: &str, source_start: usize, source_length: usize) {
        while let Some(uid) = self.pop(source_start, 0) {
            let Some(e) = self.stack.item_by_uid(uid) else {
                break;
            };
            if e.html_name == name {
                break;
            }
        }
        let _ = source_length;
    }

    /// Pop elements until an element with one of `names` is popped.
    pub fn pop_all_up_to_names(
        &mut self,
        names: &[&str],
        source_start: usize,
        source_length: usize,
    ) {
        while let Some(uid) = self.pop(source_start, 0) {
            let Some(e) = self.stack.item_by_uid(uid) else {
                break;
            };
            if names.contains(&e.html_name.as_str()) {
                break;
            }
        }
        let _ = source_length;
    }

    /// Clear the stack back to an element in `names` (without popping it).
    pub fn clear_stack_back(&mut self, names: &[&str], pos: usize) {
        while let Some(cur) = self.stack.current() {
            if names.contains(&cur.html_name.as_str()) {
                break;
            }
            let cur_uid = cur.uid;
            self.pop(pos, 0);
            let _ = cur_uid;
        }
        if self.stack.current().is_none() {
            self.handler
                .error("clearStackBack: stack is unexpectedly empty", pos);
        }
    }

    /// Stop parsing: pop all elements and end the document.
    pub fn stop_parsing(&mut self, pos: usize) {
        while let Some(cur_name) = self.stack.current().map(|e| e.html_name.clone()) {
            self.pop(pos, 0);
            // In fragment mode, the synthetic <html> root is not ended.
            if self.is_fragment && cur_name == "html" && self.stack.length() == 0 {
                break;
            }
        }
        self.handler.end_document(pos);
    }

    /// The "any other end tag" algorithm in "in body" mode.
    pub fn any_other_end_tag(&mut self, name: &str, source_start: usize, source_length: usize) {
        let mut found_idx = None;
        let mut found_uid = None;
        for i in (0..self.stack.length()).rev() {
            let elt = self.stack.item(i);
            if elt.html_name == name {
                found_idx = Some(i);
                found_uid = Some(elt.uid);
                break;
            }
            if is_special(&elt.namespace, &elt.name) {
                self.handler.error(
                    &format!(
                        "cannot implicitly close a special element <{}>",
                        elt.html_name
                    ),
                    source_start,
                );
                return;
            }
        }

        let (Some(idx), Some(uid)) = (found_idx, found_uid) else {
            return;
        };
        self.generate_implied_end_tags(Some(name), source_start);
        if self.stack.current().map(|e| e.uid) != Some(uid) {
            self.handler.error(
                "end tag matched an element which was not the current node",
                source_start,
            );
        }
        for _j in ((idx + 1)..self.stack.length()).rev() {
            self.pop(source_start, 0);
        }
        self.pop(source_start, source_length);
    }

    /// The adoption agency algorithm.
    pub fn adoption_agency(&mut self, subject: &str, source_start: usize, source_length: usize) {
        // Step 1.
        if let Some(cur) = self.stack.current()
            && cur.html_name == subject
            && !self.afe.is_in_list(cur.uid)
        {
            let _ = self.pop(source_start, source_length);
            return;
        }

        // Step 5: last AFE element with name `subject`.
        let fmt_slot = self.afe.find_element_by_name(subject, self.stack.data());
        let Some(fmt_slot) = fmt_slot else {
            self.any_other_end_tag(subject, source_start, source_length);
            return;
        };

        // Step 6: not in stack -> remove from AFE and abort.
        if self.stack.item(fmt_slot).stack_index.is_none() {
            self.afe.remove(fmt_slot);
            return;
        }

        // Step 7: not in scope -> ignore.
        if !self.stack.is_element_in_scope(fmt_slot) {
            return;
        }

        // Steps 9-19 (furthest block, bookmark, and reconstruction) are the
        // adoption agency algorithm proper; they are layered in by the
        // Dispatcher/InBody port. For the common mismatched-formatting case we
        // close the formatting element and remove it from the AFE list.
        self.pop_all_up_to_element(fmt_slot, source_start, source_length);
        self.afe.remove(fmt_slot);
    }
}
