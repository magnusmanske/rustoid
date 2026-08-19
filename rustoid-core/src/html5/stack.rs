//! Faithful port of RemexHtml's "stack of open elements".
//!
//! `SimpleStack` and `CachingStack` in RemexHtml both keep a *dense* element
//! vector with no holes: `push` appends, `pop` removes the last element,
//! `replace` swaps in place, and `remove` splices an element out (compacting,
//! like `CachingStack::remove`). Array indices are transient; the stable
//! cross-component identity is `Element::uid` (the PHP `Element` object
//! identity), which the active-formatting-elements list key on.

use super::element::Element;
use super::html_data::{NS_HTML, NS_MATHML, NS_SVG};

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

/// The dense stack of open elements.
#[derive(Default)]
pub struct Stack {
    elements: Vec<Element>,
}

impl Stack {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current (most recently inserted) element.
    pub fn current(&self) -> Option<&Element> {
        self.elements.last()
    }

    /// A mutable reference to the current element.
    pub fn current_mut(&mut self) -> Option<&mut Element> {
        self.elements.last_mut()
    }

    /// Push an element and return its (dense, transient) stack index.
    pub fn push(&mut self, mut elt: Element) -> usize {
        let idx = self.elements.len();
        elt.stack_index = Some(idx);
        self.elements.push(elt);
        idx
    }

    /// Pop the current element, returning it (like `SimpleStack::pop`).
    pub fn pop(&mut self) -> Option<Element> {
        let mut elt = self.elements.pop()?;
        elt.stack_index = None;
        Some(elt)
    }

    /// Replace the element at `old_idx` with `new` (same slot).
    ///
    /// Mirrors `Stack::replace`; the caller guarantees the same name/namespace.
    pub fn replace(&mut self, old_idx: usize, mut new: Element) {
        if let Some(old) = self.elements.get_mut(old_idx) {
            old.stack_index = None;
        }
        new.stack_index = Some(old_idx);
        self.elements[old_idx] = new;
    }

    /// Remove the element at `idx`, compacting the elements above (mirrors
    /// `CachingStack::remove`).
    pub fn remove(&mut self, idx: usize) {
        let mut elt = self.elements.remove(idx);
        elt.stack_index = None;
        for (i, e) in self.elements.iter_mut().enumerate().skip(idx) {
            e.stack_index = Some(i);
        }
    }

    /// Remove an element by `uid` (used when the caller only has the identity).
    pub fn remove_by_uid(&mut self, uid: usize) -> Option<Element> {
        let idx = self.elements.iter().position(|e| e.uid == uid)?;
        let mut elt = self.elements.remove(idx);
        elt.stack_index = None;
        for (i, e) in self.elements.iter_mut().enumerate().skip(idx) {
            e.stack_index = Some(i);
        }
        Some(elt)
    }

    /// Is there an HTML element `name` in default scope?
    pub fn is_in_scope(&self, name: &str) -> bool {
        self.is_in_specific_scope(name, &default_scope_boundary)
    }

    /// Is the element at slot `idx` in default scope?
    pub fn is_element_in_scope(&self, idx: usize) -> bool {
        let target_uid = self.elements[idx].uid;
        for node in self.elements.iter().rev() {
            if node.uid == target_uid {
                return true;
            }
            if default_scope_boundary(&node.namespace, &node.name) {
                return false;
            }
        }
        false
    }

    /// Is the element with `uid` in default scope?
    pub fn is_uid_in_scope(&self, uid: usize) -> bool {
        for node in self.elements.iter().rev() {
            if node.uid == uid {
                return true;
            }
            if default_scope_boundary(&node.namespace, &node.name) {
                return false;
            }
        }
        false
    }

    /// Is any HTML tag in `names` in default scope?
    pub fn is_one_of_set_in_scope(&self, names: &[&str]) -> bool {
        for node in self.elements.iter().rev() {
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
        for node in self.elements.iter().rev() {
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
        for node in self.elements.iter().rev() {
            if node.namespace == NS_HTML && node.name == name {
                return true;
            }
            if boundary(&node.namespace, &node.name) {
                return false;
            }
        }
        false
    }

    /// Get an element by (dense) index.
    pub fn item(&self, idx: usize) -> &Element {
        &self.elements[idx]
    }

    /// A mutable element by (dense) index.
    pub fn item_mut(&mut self, idx: usize) -> &mut Element {
        &mut self.elements[idx]
    }

    /// Get an element by its `uid`, if it is in the stack.
    pub fn item_by_uid(&self, uid: usize) -> Option<&Element> {
        self.elements.iter().find(|e| e.uid == uid)
    }

    /// The raw element slice (dense; no holes).
    pub fn data(&self) -> &[Element] {
        &self.elements
    }

    /// The number of elements in the stack.
    pub fn length(&self) -> usize {
        self.elements.len()
    }

    /// Is there an HTML `<template>` element in the stack?
    pub fn has_template(&self) -> bool {
        self.elements
            .iter()
            .any(|e| e.namespace == NS_HTML && e.name == "template")
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
        assert!(!s.is_in_scope("p"));
        assert!(s.is_in_scope("table"));
        assert!(s.is_in_table_scope("table"));
    }

    #[test]
    fn test_pop_and_current() {
        let mut s = Stack::new();
        s.push(el("html", 1));
        s.push(el("div", 2));
        assert_eq!(s.current().unwrap().uid, 2);
        assert_eq!(s.pop().unwrap().uid, 2);
        assert_eq!(s.current().unwrap().uid, 1);
    }

    #[test]
    fn test_remove_compacts() {
        let mut s = Stack::new();
        s.push(el("html", 1));
        s.push(el("div", 2));
        s.push(el("span", 3));
        s.remove(1);
        // Span shifts down; indices are transitive.
        assert_eq!(s.length(), 2);
        assert_eq!(s.item(0).uid, 1);
        assert_eq!(s.item(1).uid, 3);
        assert_eq!(s.item(1).stack_index, Some(1));
    }

    #[test]
    fn test_is_element_in_scope_and_uid() {
        let mut s = Stack::new();
        s.push(el("html", 1));
        s.push(el("body", 2));
        let div = s.push(el("div", 3));
        assert!(s.is_element_in_scope(div));
        assert!(s.is_uid_in_scope(3));
        s.pop();
        assert!(!s.is_uid_in_scope(3));
    }
}
