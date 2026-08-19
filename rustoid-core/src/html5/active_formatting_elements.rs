//! The list of active formatting elements (AFE), including the Noah's Ark
//! clause. Ports `Wikimedia\RemexHtml\TreeBuilder\ActiveFormattingElements`.
//!
//! Entries are either a scope marker or a reference to an element. Like the PHP
//! implementation (which stores the `Element` object itself), we store a clone
//! of the `Element`, so the AFE can answer name/attribute queries and
//! reconstruction without depending on the (possibly already-popped) stack.
//! Element identity is `Element::uid`.

use std::collections::HashMap;

use super::element::Element;

/// An entry in the active-formatting-elements list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfeEntry {
    Marker,
    Bookmark,
    Element(Element),
}

/// A node in the AFE arena (doubly-linked list of entries).
#[derive(Debug, Clone)]
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

    /// Insert a scope marker at the tail.
    pub fn insert_marker(&mut self) {
        let idx = self.append(AfeEntry::Marker);
        if idx > 0 {
            self.noah.push(HashMap::new());
        }
    }

    /// Push an element onto the AFE list, applying the Noah's Ark clause.
    pub fn push(&mut self, element: &Element) {
        let node_idx = self.append(AfeEntry::Element(element.clone()));

        let noah_key = element.noah_key();
        // Noah's Ark clause: >3 identical copies before a marker → drop oldest.
        let to_remove = {
            let table = self.noah.last_mut().expect("noah stack is non-empty");
            let bucket = table.entry(noah_key.clone()).or_default();
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
        table.entry(noah_key).or_default().push(node_idx);
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
            if !self.noah.is_empty() {
                self.noah.pop();
            }
        } else if let Some(table) = self.noah.first_mut() {
            table.clear();
        }
        if tail.is_none() {
            self.head = None;
        }
        self.tail = tail;
    }

    /// Find the last element with the given `html_name` before the last marker.
    pub fn find_element_by_name(&self, name: &str) -> Option<&Element> {
        let mut cur = self.tail;
        while let Some(ci) = cur {
            match &self.nodes[ci].entry {
                AfeEntry::Marker | AfeEntry::Bookmark => break,
                AfeEntry::Element(e) => {
                    if e.html_name == name {
                        return Some(e);
                    }
                }
            }
            cur = self.nodes[ci].prev;
        }
        None
    }

    /// Is the element (by `uid`) in the AFE *live* list?
    pub fn is_in_list(&self, uid: usize) -> bool {
        self.node_of(uid).is_some()
    }

    /// The live node index for a given element `uid`, if present.
    pub fn node_of(&self, uid: usize) -> Option<usize> {
        let mut cur = self.head;
        while let Some(ci) = cur {
            if let AfeEntry::Element(e) = &self.nodes[ci].entry
                && e.uid == uid
            {
                return Some(ci);
            }
            cur = self.nodes[ci].next;
        }
        None
    }

    /// Remove an element (by `uid`) from the AFE list.
    pub fn remove(&mut self, uid: usize) {
        if let Some(node_idx) = self.node_of(uid) {
            self.remove_node(node_idx);
        }
    }

    /// Replace the entry for element `old_uid` with `new_elt` (which need not
    /// already be in the list). Mirrors `ActiveFormattingElements::replace`.
    pub fn replace(&mut self, old_uid: usize, new_elt: &Element) {
        let Some(node_idx) = self.node_of(old_uid) else {
            return;
        };
        self.replace_index(node_idx, new_elt);
    }

    /// Insert a marker immediately after the element `uid`.
    pub fn insert_marker_after(&mut self, uid: usize) {
        if let Some(a) = self.node_of(uid) {
            self.insert_after_node(a, AfeEntry::Marker);
            self.noah.push(HashMap::new());
        }
    }

    /// Insert an element immediately after the element `uid`.
    pub fn insert_element_after(&mut self, uid: usize, elt: &Element) {
        if let Some(a) = self.node_of(uid) {
            let node_idx = self.insert_after_node(a, AfeEntry::Element(elt.clone()));
            self.add_to_noah(node_idx, &elt.noah_key());
        }
    }

    /// Insert a bookmark entry (a distinct marker) after the element `uid`,
    /// returning the new node index (used by the adoption agency algorithm).
    pub fn insert_bookmark_after(&mut self, uid: usize) -> Option<usize> {
        let a = self.node_of(uid)?;
        Some(self.insert_after_node(a, AfeEntry::Bookmark))
    }

    /// Insert a bookmark entry immediately after node `a` returning the new
    /// node index.
    pub fn insert_bookmark_after_index(&mut self, a: usize) -> usize {
        self.insert_after_node(a, AfeEntry::Bookmark)
    }

    /// Remove the entry at node index `node_idx`.
    pub fn remove_index(&mut self, node_idx: usize) {
        self.remove_node(node_idx);
    }

    /// Replace the entry at node index `node_idx` with `new_elt`.
    pub fn replace_index(&mut self, node_idx: usize, new_elt: &Element) {
        self.remove_index_from_noah(node_idx);
        self.nodes[node_idx].entry = AfeEntry::Element(new_elt.clone());
        self.add_to_noah(node_idx, &new_elt.noah_key());
    }

    /// The most recently inserted entry (tail), or `None`.
    pub fn get_tail(&self) -> Option<&AfeEntry> {
        self.tail.map(|t| &self.nodes[t].entry)
    }

    /// The tail node index.
    pub fn tail_node(&self) -> Option<usize> {
        self.tail
    }

    /// The head node index.
    pub fn head_node(&self) -> Option<usize> {
        self.head
    }

    /// The previous node of `node_idx`.
    pub fn prev_node(&self, node_idx: usize) -> Option<usize> {
        self.nodes[node_idx].prev
    }

    /// The next node of `node_idx`.
    pub fn next_node(&self, node_idx: usize) -> Option<usize> {
        self.nodes[node_idx].next
    }

    /// The entry at a node index.
    pub fn entry(&self, node_idx: usize) -> &AfeEntry {
        &self.nodes[node_idx].entry
    }

    /// Append an entry to the tail and return its node index.
    fn append(&mut self, entry: AfeEntry) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(AfeNode {
            entry,
            prev: self.tail,
            next: None,
        });
        if let Some(tail) = self.tail {
            self.nodes[tail].next = Some(idx);
        } else {
            self.head = Some(idx);
        }
        self.tail = Some(idx);
        idx
    }

    /// Insert `entry` immediately after node `a`, returning the new node index.
    fn insert_after_node(&mut self, a: usize, entry: AfeEntry) -> usize {
        let idx = self.nodes.len();
        let next_a = self.nodes[a].next;
        self.nodes.push(AfeNode {
            entry,
            prev: Some(a),
            next: next_a,
        });
        if self.tail == Some(a) {
            self.tail = Some(idx);
        }
        if let Some(nx) = next_a {
            self.nodes[nx].prev = Some(idx);
        }
        self.nodes[a].next = Some(idx);
        idx
    }

    /// Add a node index to its Noah bucket.
    fn add_to_noah(&mut self, node_idx: usize, key: &str) {
        self.noah
            .last_mut()
            .expect("noah stack is non-empty")
            .entry(key.to_string())
            .or_default()
            .push(node_idx);
    }

    /// Remove a node index from all Noah buckets.
    fn remove_index_from_noah(&mut self, node_idx: usize) {
        for table in self.noah.iter_mut() {
            for bucket in table.values_mut() {
                bucket.retain(|&n| n != node_idx);
            }
        }
    }

    /// Unlink a node (by node index) and clear its Noah memberships.
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
        self.remove_index_from_noah(node_idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html5::element::Attributes;
    use crate::html5::html_data::NS_HTML;

    fn el(name: &str, uid: usize) -> Element {
        Element::new(NS_HTML, name, Attributes::new(), uid)
    }

    #[test]
    fn test_push_and_find() {
        let mut afe = ActiveFormattingElements::new();
        afe.push(&el("b", 1));
        afe.push(&el("i", 2));
        afe.push(&el("b", 3));
        assert_eq!(afe.find_element_by_name("b").map(|e| e.uid), Some(3));
        assert_eq!(afe.find_element_by_name("i").map(|e| e.uid), Some(2));
        assert_eq!(afe.find_element_by_name("u"), None);
    }

    #[test]
    fn test_noah_ark() {
        let mut afe = ActiveFormattingElements::new();
        afe.push(&el("b", 1));
        afe.push(&el("b", 2));
        afe.push(&el("b", 3));
        // The fourth copy triggers Noah's Ark: oldest (uid 1) is dropped.
        afe.push(&el("b", 4));
        assert!(!afe.is_in_list(1));
        assert!(afe.is_in_list(4));
    }

    #[test]
    fn test_replace_and_insert_after() {
        let mut afe = ActiveFormattingElements::new();
        afe.push(&el("b", 1));
        afe.push(&el("i", 2));
        // Replace <i> (uid 2) with a fresh <i> (uid 3).
        afe.replace(2, &el("i", 3));
        assert!(!afe.is_in_list(2));
        assert!(afe.is_in_list(3));
        assert_eq!(afe.find_element_by_name("i").map(|e| e.uid), Some(3));

        // Insert a marker after <b> (uid 1).
        afe.insert_marker_after(1);
        // The order is b, marker, i (find tail is still i).
        assert_eq!(afe.find_element_by_name("i").map(|e| e.uid), Some(3));
    }
}
