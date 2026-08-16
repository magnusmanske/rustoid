//! The list of active formatting elements (AFE), including the Noah's Ark
//! clause. Ports `Wikimedia\RemexHtml\TreeBuilder\ActiveFormattingElements`.
//!
//! Entries are either a scope marker or a reference to an element by its stack
//! slot index. The doubly-linked list uses stable indices into an arena so
//! that `prevAFE`/`nextAFE` pointers mirror the PHP object links. Noah's Ark
//! buckets are stored as ordered lists of node indices per segment (equivalent
//! to the PHP `nextNoah` chains, at most 3 members each).

use std::collections::HashMap;

/// An entry in the active-formatting-elements list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfeEntry {
    Marker,
    Element(usize),
}

/// A node in the AFE arena.
#[derive(Debug, Clone, Copy)]
struct AfeNode {
    entry: AfeEntry,
    prev: Option<usize>,
    next: Option<usize>,
}

/// The active formatting elements list.
#[derive(Default)]
pub struct ActiveFormattingElements {
    nodes: Vec<AfeNode>,
    head: Option<usize>,
    tail: Option<usize>,
    /// Noah's Ark buckets per marker segment: bucket-key → ordered node indices.
    noah: Vec<HashMap<String, Vec<usize>>>,
}

impl ActiveFormattingElements {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            head: None,
            tail: None,
            noah: vec![HashMap::new()],
        }
    }

    /// Insert a scope marker.
    pub fn insert_marker(&mut self) {
        let idx = self.nodes.len();
        self.nodes.push(AfeNode {
            entry: AfeEntry::Marker,
            prev: self.tail,
            next: None,
        });
        if let Some(tail) = self.tail {
            self.nodes[tail].next = Some(idx);
        } else {
            self.head = Some(idx);
        }
        self.tail = Some(idx);
        self.noah.push(HashMap::new());
    }

    /// Push an element onto the AFE list. `noah_key` is the precomputed Noah's
    /// Ark bucket key for the element.
    pub fn push(&mut self, slot: usize, noah_key: &str) {
        let node_idx = self.nodes.len();
        self.nodes.push(AfeNode {
            entry: AfeEntry::Element(slot),
            prev: self.tail,
            next: None,
        });

        // Noah's Ark clause: >3 identical copies before a marker → drop oldest.
        let to_remove = {
            let table = self.noah.last_mut().expect("noah stack is non-empty");
            let bucket = table.entry(noah_key.to_string()).or_default();
            if bucket.len() >= 3 {
                Some(bucket.remove(0))
            } else {
                None
            }
        };
        if let Some(oldest) = to_remove {
            self.remove_node(oldest);
        }
        let table = self.noah.last_mut().expect("noah stack is non-empty");
        table
            .entry(noah_key.to_string())
            .or_default()
            .push(node_idx);

        if let Some(tail) = self.tail {
            self.nodes[tail].next = Some(node_idx);
        } else {
            self.head = Some(node_idx);
        }
        self.tail = Some(node_idx);
    }

    /// Clear the list up to and including the last marker.
    pub fn clear_to_marker(&mut self) {
        let mut tail = self.tail;
        while let Some(ti) = tail {
            if self.nodes[ti].entry == AfeEntry::Marker {
                break;
            }
            let prev = self.nodes[ti].prev;
            self.nodes[ti].prev = None;
            if let Some(p) = prev {
                self.nodes[p].next = None;
            }
            tail = prev;
        }
        if let Some(ti) = tail {
            let prev = self.nodes[ti].prev;
            if let Some(p) = prev {
                self.nodes[p].next = None;
            }
            tail = prev;
            self.noah.pop();
        } else {
            self.noah[0].clear();
        }
        if tail.is_none() {
            self.head = None;
        }
        self.tail = tail;
    }

    /// Find the last element with the given `html_name` before the last marker.
    pub fn find_element_by_name(
        &self,
        name: &str,
        elements: &[Option<crate::html5::element::Element>],
    ) -> Option<usize> {
        let mut cur = self.tail;
        while let Some(ci) = cur {
            match self.nodes[ci].entry {
                AfeEntry::Marker => break,
                AfeEntry::Element(slot) => {
                    if let Some(e) = &elements[slot]
                        && e.html_name == name
                    {
                        return Some(slot);
                    }
                }
            }
            cur = self.nodes[ci].prev;
        }
        None
    }

    /// Is the element (by slot) in the AFE *live* list?
    pub fn is_in_list(&self, slot: usize) -> bool {
        self.node_of(slot).is_some()
    }

    /// The live node index for a given element slot, if present. Walks the
    /// live list from the head so removed nodes are skipped.
    pub fn node_of(&self, slot: usize) -> Option<usize> {
        let mut cur = self.head;
        while let Some(ci) = cur {
            if self.nodes[ci].entry == AfeEntry::Element(slot) {
                return Some(ci);
            }
            cur = self.nodes[ci].next;
        }
        None
    }

    /// Remove a node (by node index), updating the Noah buckets.
    fn remove_node(&mut self, node_idx: usize) {
        let (prev, next) = {
            let n = &self.nodes[node_idx];
            (n.prev, n.next)
        };
        if self.head == Some(node_idx) {
            self.head = next;
        }
        if self.tail == Some(node_idx) {
            self.tail = prev;
        }
        if let Some(p) = prev {
            self.nodes[p].next = next;
        }
        if let Some(nx) = next {
            self.nodes[nx].prev = prev;
        }
        self.nodes[node_idx].prev = None;
        self.nodes[node_idx].next = None;

        for table in self.noah.iter_mut() {
            for bucket in table.values_mut() {
                bucket.retain(|&n| n != node_idx);
            }
        }
    }

    /// Remove an element (by slot) from the AFE list.
    pub fn remove(&mut self, slot: usize) {
        if let Some(node_idx) = self.node_of(slot) {
            self.remove_node(node_idx);
        }
    }

    /// Replace node `a` with node `b` (entry and list position move to `b`).
    pub fn replace_node(&mut self, a: usize, b: usize, noah_key: &str) {
        let (entry_a, prev_a, next_a) = {
            let n = &self.nodes[a];
            (n.entry, n.prev, n.next)
        };
        if self.head == Some(a) {
            self.head = Some(b);
        }
        if self.tail == Some(a) {
            self.tail = Some(b);
        }
        if let Some(p) = prev_a {
            self.nodes[p].next = Some(b);
        }
        if let Some(nx) = next_a {
            self.nodes[nx].prev = Some(b);
        }
        self.nodes[b].prev = prev_a;
        self.nodes[b].next = next_a;
        self.nodes[b].entry = entry_a;
        self.nodes[a].prev = None;
        self.nodes[a].next = None;

        // Update Noah buckets: remove `a`, add `b`.
        for table in self.noah.iter_mut() {
            for bucket in table.values_mut() {
                if let Some(pos) = bucket.iter().position(|&n| n == a) {
                    bucket[pos] = b;
                }
            }
        }
        let _ = noah_key;
    }

    /// Insert node `b` immediately after node `a`.
    pub fn insert_after(&mut self, a: usize, b: usize) {
        let next_a = self.nodes[a].next;
        if self.tail == Some(a) {
            self.tail = Some(b);
        }
        if let Some(nx) = next_a {
            self.nodes[nx].prev = Some(b);
        }
        self.nodes[b].next = next_a;
        self.nodes[b].prev = Some(a);
        self.nodes[a].next = Some(b);
    }

    /// The most recently inserted entry (tail), or `None`.
    pub fn get_tail(&self) -> Option<AfeEntry> {
        self.tail.map(|t| self.nodes[t].entry)
    }

    /// The tail node index.
    pub fn tail_node(&self) -> Option<usize> {
        self.tail
    }

    /// The previous node of `node_idx`.
    pub fn prev_node(&self, node_idx: usize) -> Option<usize> {
        self.nodes[node_idx].prev
    }

    /// The entry at a node index.
    pub fn entry(&self, node_idx: usize) -> AfeEntry {
        self.nodes[node_idx].entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html5::element::{Attributes, Element};
    use crate::html5::html_data::NS_HTML;

    fn el(name: &str, uid: usize) -> Option<Element> {
        Some(Element::new(NS_HTML, name, Attributes::new(), uid))
    }

    #[test]
    fn test_push_and_find() {
        let elements = vec![el("b", 1), el("i", 2), el("b", 3)];
        let mut afe = ActiveFormattingElements::new();
        afe.push(0, "b");
        afe.push(1, "i");
        afe.push(2, "b");
        assert_eq!(afe.find_element_by_name("b", &elements), Some(2));
        assert_eq!(afe.find_element_by_name("i", &elements), Some(1));
        assert_eq!(afe.find_element_by_name("u", &elements), None);
    }

    #[test]
    fn test_noah_ark() {
        let _elements = [el("b", 1), el("b", 2), el("b", 3), el("b", 4)];
        let mut afe = ActiveFormattingElements::new();
        afe.push(0, "b");
        afe.push(1, "b");
        afe.push(2, "b");
        // The fourth copy triggers Noah's Ark: oldest (slot 0) is dropped.
        afe.push(3, "b");
        assert!(!afe.is_in_list(0));
        assert!(afe.is_in_list(3));
    }
}
