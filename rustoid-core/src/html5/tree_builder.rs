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
    /// The fragment context element (a virtual element not on the stack).
    pub fragment_context: Option<Element>,
    pub head_element: Option<Element>,
    pub form_element: Option<Element>,
    pub frameset_ok: bool,
    pub quirks: u8,
    pub foster_parenting: bool,
    /// Pending table characters (raw substrings, whitespace determined at flush).
    pub pending_table_characters: Vec<String>,
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
            // The virtual fragment context element is not pushed; we only keep
            // a reference (the handler does not create a DOM node for it).
            let ctx = Element::new(ns, nm, Attributes::new(), self.next_uid());
            self.fragment_context = Some(ctx);

            // The synthetic <html> root.
            let mut html = Element::new(NS_HTML, "html", Attributes::new(), self.next_uid());
            html.is_virtual = true;
            self.handler
                .insert_element(Preposition::Root, None, &mut html, false, 0, 0);
            self.stack.push(html);
        }
    }

    /// The adjusted current node's uid, if any.
    pub fn adjusted_current_node(&self) -> Option<usize> {
        if self.stack.length() == 1 && self.is_fragment {
            self.fragment_context.as_ref().map(|e| e.uid)
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

    /// Insert a pre-built element at an explicit position, assigning a uid,
    /// and return that uid. Used by the adoption agency algorithm.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_element_at(
        &mut self,
        ns: &str,
        name: &str,
        attrs: Attributes,
        void: bool,
        prep: Preposition,
        reference: Option<usize>,
        source_start: usize,
        source_length: usize,
    ) -> usize {
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

    /// Reparent the children of `from_uid` onto `to_uid` (mirrors
    /// `TreeHandler::reparentChildren`).
    pub fn reparent_children(&mut self, from_uid: usize, to_uid: usize, source_start: usize) {
        let (Some(from), Some(to)) = (
            self.stack.item_by_uid(from_uid),
            self.stack.item_by_uid(to_uid),
        ) else {
            return;
        };
        let (from, to) = (from.clone(), to.clone());
        self.handler.reparent_children(&from, &to, source_start);
    }

    /// Pop the current node from the stack and notify the handler.
    pub fn pop(&mut self, source_start: usize, source_length: usize) -> Option<usize> {
        let element = self.stack.pop()?;
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

    /// Reconstruct the active formatting elements (mirrors
    /// `TreeBuilder::reconstructAFE`).
    pub fn reconstruct_afe(&mut self, source_start: usize) {
        let Some(tail_node) = self.afe.tail_node() else {
            return;
        };
        let mut node = tail_node;

        // If the tail is a marker/bookmark, or an open element, do nothing.
        match self.afe.entry(node).clone() {
            super::active_formatting_elements::AfeEntry::Marker
            | super::active_formatting_elements::AfeEntry::Bookmark => return,
            super::active_formatting_elements::AfeEntry::Element(elt) => {
                if self.stack.item_by_uid(elt.uid).is_some() {
                    return;
                }
            }
        }

        // Walk backward to the last marker or an open element.
        let mut found = false;
        while let Some(prev) = self.afe.prev_node(node) {
            node = prev;
            match self.afe.entry(node).clone() {
                super::active_formatting_elements::AfeEntry::Marker
                | super::active_formatting_elements::AfeEntry::Bookmark => {
                    found = true;
                    break;
                }
                super::active_formatting_elements::AfeEntry::Element(elt) => {
                    if self.stack.item_by_uid(elt.uid).is_some() {
                        found = true;
                        break;
                    }
                }
            }
        }

        // If we stopped on a marker or open element, advance to the entry after
        // it; otherwise start from the head.
        let mut node = if found {
            self.afe.next_node(node)
        } else {
            self.afe.head_node()
        };

        // Re-create dangling formatting elements and advance forward.
        while let Some(idx) = node {
            let entry = self.afe.entry(idx).clone();
            let super::active_formatting_elements::AfeEntry::Element(elt) = entry else {
                break;
            };
            let name = elt.name.clone();
            let attrs = elt.attrs.clone();
            let new_uid = self.insert_foreign(NS_HTML, &name, attrs, false, source_start, 0);
            if let Some(new_elt) = self.stack.item_by_uid(new_uid).cloned() {
                self.afe.replace(elt.uid, &new_elt);
            }
            node = self.afe.next_node(idx);
        }
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
    /// Returns the uid of the popped element named `name`, or `None` if absent.
    pub fn generate_implied_end_tags_and_pop(
        &mut self,
        name: &str,
        source_start: usize,
        source_length: usize,
    ) -> Option<usize> {
        self.generate_implied_end_tags(Some(name), source_start);
        if self.stack.current().map(|e| e.html_name.as_str()) != Some(name) {
            self.handler.error(
                &format!("found </{name}> but elements are open that cannot have implied end tags, closing them"),
                source_start,
            );
        }
        self.pop_all_up_to_name(name, source_start, source_length)
    }

    /// Pop elements until an element with `uid` is popped.
    /// Returns `Some(uid)` if that element was popped, `None` otherwise.
    pub fn pop_all_up_to_element(
        &mut self,
        uid: usize,
        source_start: usize,
        source_length: usize,
    ) -> Option<usize> {
        let mut popped_uid = None;
        while let Some(popped) = self.pop(source_start, 0) {
            if popped == uid {
                popped_uid = Some(uid);
                break;
            }
        }
        let _ = source_length;
        popped_uid
    }

    /// Pop elements until an element named `name` is popped.
    /// Returns the uid of the popped element named `name`, or `None`.
    pub fn pop_all_up_to_name(
        &mut self,
        name: &str,
        source_start: usize,
        source_length: usize,
    ) -> Option<usize> {
        // Peek at the current element's name *before* popping, since `pop`
        // removes the element from the dense stack and only returns its uid.
        while let Some(cur) = self.stack.current() {
            let cur_name = cur.html_name.clone();
            let uid = cur.uid;
            self.pop(source_start, 0);
            if cur_name == name {
                return Some(uid);
            }
        }
        let _ = source_length;
        None
    }

    /// Pop elements until an element with one of `names` is popped.
    /// Returns the uid of the popped matched element, or `None`.
    pub fn pop_all_up_to_names(
        &mut self,
        names: &[&str],
        source_start: usize,
        source_length: usize,
    ) -> Option<usize> {
        // Peek at the current element's name *before* popping, since `pop`
        // removes the element from the dense stack and only returns its uid.
        while let Some(cur) = self.stack.current() {
            let name = cur.html_name.clone();
            let uid = cur.uid;
            self.pop(source_start, 0);
            if names.contains(&name.as_str()) {
                return Some(uid);
            }
        }
        let _ = source_length;
        None
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
    /// Returns the uid of the popped matching element, or `None` if not found.
    pub fn any_other_end_tag(
        &mut self,
        name: &str,
        source_start: usize,
        source_length: usize,
    ) -> Option<usize> {
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
                return None;
            }
        }

        let (Some(idx), Some(uid)) = (found_idx, found_uid) else {
            return None;
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
        self.pop(source_start, source_length)
    }

    /// The adoption agency algorithm.
    /// Returns the uid of the removed formatting element, or `None`.
    pub fn adoption_agency(
        &mut self,
        subject: &str,
        source_start: usize,
        source_length: usize,
    ) -> Option<usize> {
        // Step 1: current node is `subject` and not in the AFE → pop and abort.
        if let Some(cur) = self.stack.current()
            && cur.html_name == subject
            && !self.afe.is_in_list(cur.uid)
        {
            return self.pop(source_start, source_length);
        }

        // Steps 2-4: outer loop bounded by 8 iterations.
        for _outer in 0..8 {
            // Step 5: last AFE element with the subject name.
            let fmt_elt = self.afe.find_element_by_name(subject).cloned();
            let Some(fmt_elt) = fmt_elt else {
                return self.any_other_end_tag(subject, source_start, source_length);
            };
            let fmt_uid = fmt_elt.uid;

            // Step 6: not in the stack → remove from AFE and abort.
            let fmt_idx = self.stack.index_of_uid(fmt_uid);
            let Some(fmt_idx) = fmt_idx else {
                self.afe.remove(fmt_uid);
                return None;
            };

            // Step 7: not in scope → ignore and abort.
            if !self.stack.is_uid_in_scope(fmt_uid) {
                return None;
            }

            // Step 8: not the current node is a parse error (do not abort).
            if self.stack.current().map(|e| e.uid) != Some(fmt_uid) {
                self.handler.error(
                    "end tag matched a formatting element which was not the current node",
                    source_start,
                );
            }

            // Step 9: furthest block above the formatting element.
            let mut furthest_block: Option<Element> = None;
            let mut furthest_block_index = 0usize;
            let stack_length = self.stack.length();
            for i in (fmt_idx + 1)..stack_length {
                let item = self.stack.item(i);
                if is_special(&item.namespace, &item.name) {
                    furthest_block = Some(item.clone());
                    furthest_block_index = i;
                    break;
                }
            }

            // Step 10: no furthest block → pop to fmt and remove from AFE.
            let Some(furthest_block) = furthest_block else {
                let result = self.pop_all_up_to_element(fmt_uid, source_start, source_length);
                self.afe.remove(fmt_uid);
                return result;
            };
            let furthest_block_uid = furthest_block.uid;

            // Step 11: common ancestor is the element immediately above fmt.
            if fmt_idx == 0 {
                // Unreachable in practice (there is always an element below a
                // formatting element), but avoid a subtraction underflow.
                self.afe.remove(fmt_uid);
                return None;
            }
            let ancestor = self.stack.item(fmt_idx - 1).clone();
            let ancestor_uid = ancestor.uid;

            // Step 12: bookmark after fmt.
            let mut bookmark_node = self.afe.insert_bookmark_after(fmt_uid)?;

            // Step 13: inner loop.
            let mut last_node = furthest_block.clone();
            let mut last_node_uid = furthest_block_uid;
            let mut node_index = furthest_block_index;
            let mut stack_removals: Vec<usize> = Vec::new();
            // Queued (prep, reference, element) insertions.
            let mut insertions: Vec<(Preposition, Option<usize>, Element)> = Vec::new();

            let mut inner = 1usize;
            loop {
                // Step 13.3: node = element immediately above.
                node_index -= 1;
                let node_elt = self.stack.item(node_index).clone();
                let node_uid = node_elt.uid;

                // Step 13.4: reached the formatting element.
                if node_uid == fmt_uid {
                    break;
                }

                // Step 13.5: inner > 3 and node in AFE → remove from AFE.
                let mut is_afe = self.afe.is_in_list(node_uid);
                if inner > 3 && is_afe {
                    self.afe.remove(node_uid);
                    is_afe = false;
                }

                // Step 13.6: node not in AFE → mark for removal and continue.
                if !is_afe {
                    stack_removals.push(node_index);
                    inner += 1;
                    continue;
                }

                // Step 13.7: clone node, replace in AFE and stack, node = clone.
                let new_elt = Element::new(
                    &node_elt.namespace,
                    &node_elt.name,
                    node_elt.attrs.clone(),
                    self.next_uid(),
                );
                let new_uid = new_elt.uid;
                self.afe.replace(node_uid, &new_elt);
                self.stack.replace_by_uid(node_uid, new_elt.clone());

                // Step 13.8: if last node is furthest block, move bookmark after
                // new node.
                if last_node_uid == furthest_block_uid {
                    self.afe.remove_index(bookmark_node);
                    if let Some(n) = self.afe.insert_bookmark_after(new_uid) {
                        bookmark_node = n;
                    }
                }

                // Step 13.9: queue insertion of last node into node.
                insertions.push((Preposition::Under, Some(new_uid), last_node.clone()));

                // Step 13.10: last node = node.
                last_node = new_elt;
                last_node_uid = new_uid;
                inner += 1;
            }

            // Step 14: insert last node at the appropriate place for `ancestor`.
            let (prep, reference) = self.appropriate_place(Some(ancestor_uid));
            insertions.push((prep, reference, last_node));

            // Execute queued insertions in reverse order.
            for (prep, reference, mut elt) in insertions.into_iter().rev() {
                self.handler
                    .insert_element(prep, reference, &mut elt, false, source_start, 0);
            }

            // Steps 15-17: new formatting element, move furthest block children.
            let new_fmt = Element::new(
                &fmt_elt.namespace,
                &fmt_elt.name,
                fmt_elt.attrs.clone(),
                self.next_uid(),
            );
            let new_fmt_uid = new_fmt.uid;
            self.reparent_children(furthest_block_uid, new_fmt_uid, source_start);

            // Step 18: remove fmt from AFE, replace bookmark with new fmt.
            self.afe.remove(fmt_uid);
            self.afe.replace_index(bookmark_node, &new_fmt);

            // Step 19: rebuild the stack.
            let mut temp_stack: Vec<Element> = Vec::new();
            let mut index = self.stack.length();
            while index > furthest_block_index {
                index -= 1;
                if let Some(elt) = self.stack.pop() {
                    temp_stack.push(elt);
                }
            }
            temp_stack.push(new_fmt.clone());
            while index > fmt_idx {
                index -= 1;
                let elt = self.stack.pop();
                let Some(elt) = elt else {
                    break;
                };
                if stack_removals.contains(&index) {
                    self.handler.end_tag(&elt, source_start, 0);
                } else {
                    temp_stack.push(elt);
                }
            }
            let elt = self.stack.pop();
            if let Some(elt) = elt {
                self.handler.end_tag(&elt, source_start, 0);
            }
            for elt in temp_stack.into_iter().rev() {
                self.stack.push(elt);
            }
        }

        None
    }
}
