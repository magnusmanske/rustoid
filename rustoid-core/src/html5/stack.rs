//! Faithful port of RemexHtml's `SimpleStack` — the "stack of open elements"
//! with the HTML5 scope-checking predicates.
//!
//! Mirrors `Wikimedia\RemexHtml\TreeBuilder\SimpleStack`. Like that class,
//! removal leaves a hole (the slot is marked removed rather than compacted),
//! so slot indices are stable and can be used as element identities by
//! `ActiveFormattingElements`.

use super::element::Element;
use super::html_data::{NS_HTML, NS_MATHML, NS_SVG};

/// The stack of open elements. `None` entries are removed slots.
#[derive(Default)]
pub struct Stack {
    elements: Vec<Option<Element>>,
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

    /// The current (most recently inserted, still-open) element.
    pub fn current(&self) -> Option<&Element> {
        self.elements.iter().rev().flatten().next()
    }

    /// A mutable reference to the current element.
    pub fn current_mut(&mut self) -> Option<&mut Element> {
        self.elements.iter_mut().rev().flatten().next()
    }

    /// Push an element and return its stable slot index.
    pub fn push(&mut self, mut elt: Element) -> usize {
        let idx = self.elements.len();
        elt.stack_index = Some(idx);
        self.elements.push(Some(elt));
        idx
    }

    /// Pop the current element, returning its slot index (or `None` if empty).
    pub fn pop(&mut self) -> Option<usize> {
        let top = self
            .elements
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| e.is_some())
            .map(|(i, _)| i);
        let idx = top?;
        if let Some(elt) = &mut self.elements[idx] {
            elt.stack_index = None;
        }
        self.elements[idx] = None;
        Some(idx)
    }

    /// Replace the element at `old_idx` with `new` (same slot).
    pub fn replace(&mut self, old_idx: usize, mut new: Element) {
        if let Some(old) = &mut self.elements[old_idx] {
            old.stack_index = None;
        }
        new.stack_index = Some(old_idx);
        self.elements[old_idx] = Some(new);
    }

    /// Remove an element from the stack, leaving a hole.
    pub fn remove(&mut self, idx: usize) {
        if let Some(elt) = &mut self.elements[idx] {
            elt.stack_index = None;
        }
        self.elements[idx] = None;
    }

    /// Is there an HTML element `name` in default scope?
    pub fn is_in_scope(&self, name: &str) -> bool {
        self.is_in_specific_scope(name, &default_scope_boundary)
    }

    /// Is the element at slot `idx` in default scope?
    pub fn is_element_in_scope(&self, idx: usize) -> bool {
        for i in (0..self.elements.len()).rev() {
            if let Some(node) = &self.elements[i] {
                if i == idx {
                    return true;
                }
                if default_scope_boundary(&node.namespace, &node.name) {
                    return false;
                }
            }
        }
        false
    }

    /// Is any HTML tag in `names` in default scope?
    pub fn is_one_of_set_in_scope(&self, names: &[&str]) -> bool {
        for i in (0..self.elements.len()).rev() {
            if let Some(node) = &self.elements[i] {
                if node.namespace == NS_HTML && names.contains(&node.name.as_str()) {
                    return true;
                }
                if default_scope_boundary(&node.namespace, &node.name) {
                    return false;
                }
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
            if let Some(node) = &self.elements[i] {
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
        }
        false
    }

    fn is_in_specific_scope(&self, name: &str, boundary: &dyn Fn(&str, &str) -> bool) -> bool {
        for i in (0..self.elements.len()).rev() {
            if let Some(node) = &self.elements[i] {
                if node.namespace == NS_HTML && node.name == name {
                    return true;
                }
                if boundary(&node.namespace, &node.name) {
                    return false;
                }
            }
        }
        false
    }

    /// Get an element by slot index.
    pub fn item(&self, idx: usize) -> &Element {
        self.elements[idx].as_ref().expect("stack slot is empty")
    }

    /// A mutable element by slot index.
    pub fn item_mut(&mut self, idx: usize) -> &mut Element {
        self.elements[idx].as_mut().expect("stack slot is empty")
    }

    /// The count of live (non-removed) elements.
    pub fn length(&self) -> usize {
        self.elements.iter().flatten().count()
    }

    /// Is there an HTML `<template>` element in the stack?
    pub fn has_template(&self) -> bool {
        self.elements
            .iter()
            .flatten()
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
        let div = s.push(el("div", 2));
        assert_eq!(s.current().unwrap().uid, 2);
        assert_eq!(s.pop().unwrap(), div);
        assert_eq!(s.current().unwrap().uid, 1);
    }

    #[test]
    fn test_remove_leaves_hole() {
        let mut s = Stack::new();
        let html = s.push(el("html", 1));
        let mid = s.push(el("div", 2));
        s.push(el("span", 3));
        s.remove(mid);
        assert_eq!(s.length(), 2);
        assert!(!s.is_element_in_scope(mid));
        // The remaining elements keep their slot indices.
        assert_eq!(s.item(html).uid, 1);
    }
}
