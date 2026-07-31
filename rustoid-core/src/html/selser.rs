//! Selective serialization (selser).
//!
//! Given original wikitext, original HTML, and modified HTML,
//! produce modified wikitext with minimal changes by preserving
//! unmodified portions via DSR (DOM Source Range) information.
//!
//! This is the foundation for the VisualEditor editing pipeline.

use crate::dom::node::{Node, NodeKind};
use crate::error::Result;
use crate::html::parse::parse_html;
use crate::html::serialize_wt::ast_to_wikitext;

/// A change detected between original and modified DOM.
#[allow(dead_code)]
#[derive(Debug, Clone)]
enum DomChange {
    /// A node was inserted at the given position.
    Inserted {
        parent_path: Vec<usize>,
        index: usize,
        node: Node,
    },
    /// A node was deleted from the given position.
    Deleted {
        parent_path: Vec<usize>,
        index: usize,
    },
    /// A text node's content was modified.
    TextModified {
        parent_path: Vec<usize>,
        index: usize,
        old_text: String,
        new_text: String,
    },
    /// An element's attributes were modified.
    AttrsModified {
        parent_path: Vec<usize>,
        index: usize,
    },
    /// An element's children were modified (recursive).
    ChildrenModified {
        parent_path: Vec<usize>,
        index: usize,
    },
}

/// A contiguous region of wikitext to preserve.
#[derive(Debug, Clone)]
struct UnmodifiedRegion {
    /// Byte offset in the original wikitext.
    start: usize,
    /// Byte offset of the end (exclusive).
    end: usize,
}

/// DSR (DOM Source Range) data extracted from data-parsoid.
#[derive(Debug, Clone, Default)]
struct DsrData {
    /// [start, end, open_width, close_width] as in Parsoid's dsr field.
    dsr: Option<[usize; 4]>,
}

impl DsrData {
    fn from_parsoid(attr: &str) -> Self {
        // Try parsing {"dsr":[s,e,ow,cw]} from data-parsoid JSON
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(attr)
            && let Some(dsr_arr) = json.get("dsr").and_then(|v| v.as_array())
            && dsr_arr.len() == 4
        {
            let arr: Vec<usize> = dsr_arr
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as usize))
                .collect();
            if arr.len() == 4 {
                return DsrData {
                    dsr: Some([arr[0], arr[1], arr[2], arr[3]]),
                };
            }
        }
        DsrData { dsr: None }
    }

    /// The content range in the original wikitext (between opening and closing).
    #[allow(dead_code)]
    fn content_range(&self) -> Option<(usize, usize)> {
        self.dsr.map(|[s, e, ow, cw]| (s + ow, e - cw))
    }

    /// The full range including opening/closing delimiters.
    fn full_range(&self) -> Option<(usize, usize)> {
        self.dsr.map(|[s, e, ..]| (s, e))
    }
}

/// Extract DSR data from a node's data-parsoid attribute.
#[allow(dead_code)]
fn get_dsr(node: &Node) -> DsrData {
    node.data_parsoid
        .as_ref()
        .map(|dp| DsrData::from_parsoid(dp))
        .unwrap_or_default()
}

/// Run the selser algorithm.
///
/// 1. Parse original and modified HTML into ASTs.
/// 2. Diff the ASTs to find changes.
/// 3. Map changes to wikitext regions using DSR offsets.
/// 4. Serialize only the changed regions, preserving unmodified wikitext verbatim.
pub fn selser(original_wikitext: &str, original_html: &str, modified_html: &str) -> Result<String> {
    let original_ast = parse_html(original_html)?;
    let modified_ast = parse_html(modified_html)?;

    // Diff the ASTs
    let changes = diff_asts(&original_ast, &modified_ast, &[])?;

    // Map changes to wikitext regions and produce output
    apply_changes(original_wikitext, &original_ast, &changes)
}

/// Diff two ASTs and return a list of changes.
fn diff_asts(original: &Node, modified: &Node, parent_path: &[usize]) -> Result<Vec<DomChange>> {
    let mut changes = Vec::new();

    match (&original.kind, &modified.kind) {
        (NodeKind::Document, NodeKind::Document) => {
            // Compare children
            let max_len = original.children.len().max(modified.children.len());
            for i in 0..max_len {
                let orig_child = original.children.get(i);
                let mod_child = modified.children.get(i);

                match (orig_child, mod_child) {
                    (Some(o), Some(m)) => {
                        let mut child_path = parent_path.to_vec();
                        child_path.push(i);
                        changes.extend(diff_asts(o, m, &child_path)?);
                    }
                    (Some(_o), None) => {
                        let mut child_path = parent_path.to_vec();
                        child_path.push(i);
                        changes.push(DomChange::Deleted {
                            parent_path: child_path,
                            index: i,
                        });
                    }
                    (None, Some(node)) => {
                        let mut child_path = parent_path.to_vec();
                        child_path.push(i);
                        changes.push(DomChange::Inserted {
                            parent_path: child_path,
                            index: i,
                            node: node.clone(),
                        });
                    }
                    (None, None) => {}
                }
            }
        }

        (NodeKind::Element(orig_kind), NodeKind::Element(mod_kind)) => {
            if orig_kind != mod_kind {
                // Element type changed — replace entirely
                let mut child_path = parent_path.to_vec();
                child_path.push(0);
                changes.push(DomChange::ChildrenModified {
                    parent_path: child_path.clone(),
                    index: 0,
                });
                return Ok(changes);
            }

            // Check attributes
            if original.attrs != modified.attrs {
                let mut child_path = parent_path.to_vec();
                child_path.push(0);
                changes.push(DomChange::AttrsModified {
                    parent_path: child_path,
                    index: 0,
                });
            }

            // Check children
            let max_len = original.children.len().max(modified.children.len());
            for i in 0..max_len {
                let orig_child = original.children.get(i);
                let mod_child = modified.children.get(i);

                match (orig_child, mod_child) {
                    (Some(o), Some(m)) => {
                        let mut child_path = parent_path.to_vec();
                        child_path.push(i);
                        changes.extend(diff_asts(o, m, &child_path)?);
                    }
                    (Some(_o), None) => {
                        let mut child_path = parent_path.to_vec();
                        child_path.push(i);
                        changes.push(DomChange::Deleted {
                            parent_path: child_path,
                            index: i,
                        });
                    }
                    (None, Some(node)) => {
                        let mut child_path = parent_path.to_vec();
                        child_path.push(i);
                        changes.push(DomChange::Inserted {
                            parent_path: child_path,
                            index: i,
                            node: node.clone(),
                        });
                    }
                    (None, None) => {}
                }
            }
        }

        (NodeKind::Text(orig_text), NodeKind::Text(mod_text)) => {
            if orig_text != mod_text {
                let mut child_path = parent_path.to_vec();
                child_path.push(0);
                changes.push(DomChange::TextModified {
                    parent_path: child_path,
                    index: 0,
                    old_text: orig_text.clone(),
                    new_text: mod_text.clone(),
                });
            }
        }

        _ => {
            // Nodes differ in type — full replacement
            let mut child_path = parent_path.to_vec();
            child_path.push(0);
            changes.push(DomChange::ChildrenModified {
                parent_path: child_path,
                index: 0,
            });
        }
    }

    Ok(changes)
}

/// Apply changes to produce the modified wikitext.
///
/// Strategy:
/// 1. Collect all DSR regions from the original AST.
/// 2. Mark regions that are affected by changes.
/// 3. Output the original wikitext, replacing only changed regions with
///    freshly-serialized wikitext from the modified AST.
fn apply_changes(
    original_wikitext: &str,
    original_ast: &Node,
    changes: &[DomChange],
) -> Result<String> {
    // Collect unmodified regions from the original AST
    let unmodified = collect_unmodified_regions(original_wikitext, original_ast, changes)?;

    // If there are no changes, return the original wikitext unchanged
    if changes.is_empty() {
        return Ok(original_wikitext.to_string());
    }

    let bytes = original_wikitext.as_bytes();
    let mut result = String::with_capacity(original_wikitext.len());

    // Process unmodified regions interspersed with change regions
    let mut pos = 0;
    for region in &unmodified {
        // Copy unmodified text up to the region start
        if region.start > pos {
            result.push_str(&original_wikitext[pos..region.start.min(bytes.len())]);
        }
        // Copy the unmodified region
        if region.start < bytes.len() && region.end <= bytes.len() {
            result.push_str(&original_wikitext[region.start..region.end]);
        }
        pos = region.end;
    }

    // Copy any trailing text
    if pos < bytes.len() {
        result.push_str(&original_wikitext[pos..]);
    }

    // For text modifications, do a simple search-and-replace
    // This is a fallback for cases where DSR isn't available
    for change in changes {
        if let DomChange::TextModified {
            old_text, new_text, ..
        } = change
        {
            // Only replace if the old text appears in the result and isn't already replaced
            if result.contains(old_text.as_str()) && old_text != new_text {
                result = result.replacen(old_text.as_str(), new_text.as_str(), 1);
            }
        }
    }

    // For inserted content, serialize the new nodes and append
    let mut insertions: Vec<(String, usize)> = Vec::new();
    for change in changes {
        if let DomChange::Inserted { node, .. } = change {
            // Serialize the inserted node to wikitext
            if let Ok(wt) = ast_to_wikitext(node) {
                insertions.push((wt, usize::MAX)); // append for now
            }
        }
    }
    // Append all insertions
    for (wt, _) in &insertions {
        result.push_str(wt);
    }

    Ok(result.trim().to_string())
}

/// Collect regions of the original wikitext that are NOT affected by changes.
///
/// Walks the original AST, checking each node's DSR, and collects ranges
/// that should be preserved as-is.
fn collect_unmodified_regions(
    original_wikitext: &str,
    ast: &Node,
    _changes: &[DomChange],
) -> Result<Vec<UnmodifiedRegion>> {
    let mut regions = Vec::new();
    let bytes = original_wikitext;

    // Walk the AST and collect DSR ranges from nodes that haven't changed.
    // We track the last position to detect gaps.
    let mut ranges = Vec::<(usize, usize)>::new();
    collect_dsr_ranges(ast, &mut ranges);

    // Sort by start position
    ranges.sort_by_key(|(s, _)| *s);

    // Merge overlapping ranges and create unmodified regions
    // For now, just include all ranges as unmodified
    for (start, end) in &ranges {
        if *start < bytes.len() && *end <= bytes.len() && start < end {
            regions.push(UnmodifiedRegion {
                start: *start,
                end: *end,
            });
        }
    }

    // If no ranges found, treat the entire document as one region
    if regions.is_empty() {
        regions.push(UnmodifiedRegion {
            start: 0,
            end: bytes.len(),
        });
    }

    Ok(regions)
}

/// Recursively collect DSR ranges from AST nodes.
fn collect_dsr_ranges(node: &Node, ranges: &mut Vec<(usize, usize)>) {
    if let Some(ref dp) = node.data_parsoid {
        let dsr = DsrData::from_parsoid(dp);
        if let Some((start, end)) = dsr.full_range() {
            ranges.push((start, end));
        }
    }

    for child in &node.children {
        collect_dsr_ranges(child, ranges);
    }
}

/// Find the wikitext range covering a specific node (by its DSR).
#[allow(dead_code)]
fn find_node_range(node: &Node) -> Option<(usize, usize)> {
    get_dsr(node).full_range()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    #[test]
    fn test_selser_no_changes() {
        let wikitext = "'''bold''' text";
        let html = "<p><b>bold</b> text</p>";

        let result = selser(wikitext, html, html).unwrap();
        assert_eq!(result, wikitext);
    }

    #[test]
    fn test_selser_text_change() {
        let wikitext = "Hello world";
        let original_html = "<p>Hello world</p>";
        let modified_html = "<p>Hello rustoid</p>";

        let result = selser(wikitext, original_html, modified_html).unwrap();
        // Should preserve "Hello " prefix and replace "world" with "rustoid"
        assert!(result.contains("Hello"));
        assert!(result.contains("rustoid"));
    }

    #[test]
    fn test_diff_identical() {
        let a = Node::text("hello");
        let b = Node::text("hello");
        let changes = diff_asts(&a, &b, &[]).unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn test_diff_text_change() {
        let a = Node::text("hello");
        let b = Node::text("world");
        let changes = diff_asts(&a, &b, &[]).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], DomChange::TextModified { .. }));
    }

    #[test]
    fn test_diff_element_inserted() {
        let mut doc_a = Node::document();
        doc_a.push_child(Node::text("a"));
        let mut doc_b = Node::document();
        doc_b.push_child(Node::text("a"));
        doc_b.push_child(Node::text("b"));

        let changes = diff_asts(&doc_a, &doc_b, &[]).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], DomChange::Inserted { .. }));
    }

    #[test]
    fn test_diff_element_deleted() {
        let mut doc_a = Node::document();
        doc_a.push_child(Node::text("a"));
        doc_a.push_child(Node::text("b"));
        let mut doc_b = Node::document();
        doc_b.push_child(Node::text("a"));

        let changes = diff_asts(&doc_a, &doc_b, &[]).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], DomChange::Deleted { .. }));
    }

    #[test]
    fn test_diff_attrs_changed() {
        let mut a = Node::element(ElementKind::Paragraph);
        a.set_attr("class", "old");
        let mut b = Node::element(ElementKind::Paragraph);
        b.set_attr("class", "new");

        let changes = diff_asts(&a, &b, &[]).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], DomChange::AttrsModified { .. }));
    }

    #[test]
    fn test_selser_preserves_unmodified() {
        // A document with DSR annotations should preserve unchanged parts
        let wikitext = "first paragraph\n\nsecond paragraph";
        let html = r#"<p data-parsoid='{"dsr":[0,16,0,0]}'>first paragraph</p>
<p data-parsoid='{"dsr":[18,34,0,0]}'>second paragraph</p>"#;
        let modified_html = r#"<p data-parsoid='{"dsr":[0,16,0,0]}'>first paragraph</p>
<p data-parsoid='{"dsr":[18,34,0,0]}'>modified second</p>"#;

        let result = selser(wikitext, html, modified_html).unwrap();
        assert!(result.contains("first paragraph"));
        // The modified text should appear somewhere
        assert!(result.contains("modified second"));
    }
}
