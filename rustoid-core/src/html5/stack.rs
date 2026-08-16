//! Faithful port of RemexHtml's `SimpleStack` — the "stack of open elements"
//! with the HTML5 scope-checking predicates.
//!
//! Mirrors `Wikimedia\RemexHtml\TreeBuilder\SimpleStack` (including its
//! "remove leaves a hole" semantics: `remove` clears `stackIndex` but does not
//! shrink the element array). Indices used by `ActiveFormattingElements` are
//! stable because the array only grows.

use super::element::Element;
use super::html_data::{NS_HTML, NS_MATHML, NS_SVG};

/// The stack of open elements.
#[derive(Default)]
pub struct Stack {
    /// Elements, in insertion order. Removal does not shrink this; `remove`
    /// only clears the removed element's `stack_index` (see `SimpleStack`).
    elements: Vec<Element>,
}

/// The default-scope boundary set (breaks a scope region).
fn default_scope_boundary(ns: &str, name: &str) -> bool {
    match ns {
        NS_HTML => matches!(
            name,
            "applet"
                | "caption"
                | "html"
                | "table"
                | "td"
                | "th"
                | "marquee"
                | "object"
                | "template"
        ),
        NS_MATHML => matches!(name, "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml"),
        NS_SVG => matches!(name, "foreignObject" | "desc" | "title"),
        _ => false,
    }
}

/// The table-scope boundary set.
fn table_scope_boundary(ns: &str, name: &str) -> bool {
    ns == NS_HTML && matches!(name, "html" | "table" | "template")
}

impl Stack {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current (most recently inserted) element, if any.
    pub fn current(&self) -> Option<&Element> {
        // The current element is the last element with a live `stack_index`.
        let last_live = self
            .elements
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| e.stack_index.is_some());
        last_live.map(|(_, e)| e)
    }

    /// A mutable reference to the current element.
    pub fn current_mut(&mut self) -> Option<&mut Element> {
        let mut last: Option<&mut Element> = None;
        for e in self.elements.iter_mut().rev() {
            if e.stack_index.is_some() {
                last = Some(e);
                break;
            }
        }
        last
    }

    /// Push an element and return its index.
    pub fn push(&mut self, elt: Element) -> usize {
        let idx = self.elements.len();
        self.elements.push(elt);
        self.elements[idx].stack_index = Some(idx);
        idx
    }

    /// Pop the current element from the stack, returning its index.
    pub fn pop(&mut self) -> Option<usize> {
        let idx = {
            let top_live = self
                .elements
                .iter()
                .enumerate()
                .rev()
                .find(|(_, e)| e.stack_index.is_some())
                .map(|(i, _)| i);
            top_live?
        };
        self.elements[idx].stack_index = None;
        Some(idx)
    }

    /// Replace `old_idx` with `new` (same position in the stack).
    pub fn replace(&mut self, old_idx: usize, new: Element) -> usize {
        self.elements[old_idx].stack_index = None;
        self.elements[old_idx] = new;
        self.elements[old_idx].stack_index = Some(old_idx);
        old_idx
    }

    /// Remove an element from the middle of the stack (clears its index and
    /// shifts subsequent indices down, without shrinking the array).
    pub fn remove(&mut self, idx: usize) {
        self.elements[idx].stack_index = None;
        for i in (idx + 1)..self.elements.len() {
            if let Some(si) = self.elements[i].stack_index {
                self.elements[i].stack_index = Some(si - 1);
            }
        }
    }

    /// Is there an HTML element `name` in default scope?
    pub fn is_in_scope(&self, name: &str) -> bool {
        self.is_in_specific_scope(name, &default_scope_boundary)
    }

    /// Is the given element (by index) in default scope?
    pub fn is_element_in_scope(&self, idx: usize) -> bool {
        for i in (0..self.elements.len()).rev() {
            let node = &self.elements[i];
            if node.stack_index.is_none() {
                continue;
            }
            if i == idx {
                return true;
            }
            if default_scope_boundary(&node.namespace, &node.name) {
                return false;
            }
        }
        false
    }

    /// Is any of `names` (an HTML tag set) in default scope?
    pub fn is_one_of_set_in_scope(&self, names: &[&str]) -> bool {
        for i in (0..self.elements.len()).rev() {
            let node = &self.elements[i];
            if node.stack_index.is_none() {
                continue;
            }
            if node.namespace == NS_HTML && names.contains(&node.name.as_str()) {
                return true;
            }
            if default_scope_boundary(&node.namespace, &node.name) {
                return false;
            }
        }
        false
    }

    pub fn is_in_list_scope(&self, name: &str) -> bool {
        self.is_in_specific_scope(name, &|ns, n| {
            default_scope_boundary(ns, n) || (ns == NS_HTML && matches!(n, "ol" | "li"))
        })
    }

    pub fn is_in_button_scope(&self, name: &str) -> bool {
        self.is_in_specific_scope(name, &|ns, n| {
            default_scope_boundary(ns, n) || (ns == NS_HTML && n == "button")
        })
    }

    pub fn is_in_table_scope(&self, name: &str) -> bool {
        self.is_in_specific_scope(name, &table_scope_boundary)
    }

    pub fn is_in_select_scope(&self, name: &str) -> bool {
        for i in (0..self.elements.len()).rev() {
            let node = &self.elements[i];
            if node.stack_index.is_none() {
                continue;
            }
            if node.namespace == NS_HTML && node.name == name {
                return true;
            }
            if node.namespace != NS_HTML {
                return false;
            }
            if node.name != "optgroup" && node.name != "option" {
                return false;
            }
        }
        false
    }

    fn is_in_specific_scope(&self, name: &str, boundary: &dyn Fn(&str, &str) -> bool) -> bool {
        for i in (0..self.elements.len()).rev() {
            let node = &self.elements[i];
            if node.stack_index.is_none() {
                continue;
            }
            if node.namespace == NS_HTML && node.name == name {
                return true;
            }
            if boundary(&node.namespace, &node.name) {
                return false;
            }
        }
        false
    }

    /// Get an element by index.
    pub fn item(&self, idx: usize) -> &Element {
        &self.elements[idx]
    }

    /// A mutable element by index.
    pub fn item_mut(&mut self, idx: usize) -> &mut Element {
        &mut self.elements[idx]
    }

    /// The number of *live* elements (excluding removal holes).
    pub fn length(&self) -> usize {
        self.elements
            .iter()
            .filter(|e| e.stack_index.is_some())
            .count()
    }

    /// Is there an HTML `<template>` element in the stack?
    pub fn has_template(&self) -> bool {
        self.elements
            .iter()
            .any(|e| e.stack_index.is_some() && e.namespace == NS_HTML && e.name == "template")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html5::element::Attributes;

    fn el(name: &str, uid: usize) -> Element {
        Element::new(NS_HTML, name, Attributes::new(), uid)
    }

    #[test]
    fn test_scope() {
        let mut s = Stack::new();
        s.push(el("html", 1));
        s.push(el("body", 2));
        s.push(el("div", 3));
        assert!(s.is_in_scope("div"));
        assert!(s.is_in_scope("body"));
        assert!(s.is_in_scope("html"));
        assert!(!s.is_in_scope("table"));
    }

    #[test]
    fn test_scope_breaks_at_table() {
        let mut s = Stack::new();
        s.push(el("html", 1));
        s.push(el("body", 2));
        s.push(el("table", 3));
        s.push(el("tbody", 4));
        s.push(el("tr", 5));
        // Inside a table, a `p` is not in scope (the table breaks the default
        // scope before we reach the body level).
        assert!(!s.is_in_scope("p"));
        assert!(s.is_in_scope("table"));
        assert!(s.is_in_table_scope("table"));
    }

    #[test]
    fn test_pop_and_current() {
        let mut s = Stack::new();
        let _html = s.push(el("html", 1));
        let div = s.push(el("div", 2));
        assert_eq!(s.current().unwrap().uid, 2);
        let popped = s.pop().unwrap();
        assert_eq!(popped, div);
        assert_eq!(s.current().unwrap().uid, 1);
    }
}
