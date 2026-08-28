//! Diff markers and diff-query helpers for selective serialization.
//!
//! Faithful ports of PHP Parsoid's `Html2Wt\DiffMarkers` and the read-only
//! half of `Html2Wt\DiffUtils`. These inspect the diff annotations the DOM-diff
//! pass leaves on nodes:
//!
//! - `mw:DiffMarker/<value>` `meta` elements inserted before a text/comment
//!   node (or a deleted/moved element) recorded a non-element `deleted`
//!   /`moved`/`inserted` mark.
//! - the `data-parsoid-diff` attribute (`{"diff":[<mark>,...]}`) records
//!   element-level marks.
//!
//! The mutation half (`addDiffMark`/`setDiffMark`/`prependTypedMeta`), which
//! requires inserting `meta` nodes and writing rich attributes on a mutable
//! DOM, is layered on once the DOM-diff pass itself is ported; these
//! read-helpers are its consumers and are fully faithful on their own.

use crate::dom::node::{Node, NodeKind};

/// The set of diff marker kinds, faithful to PHP's `enum DiffMarkers: string`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DiffMarkers {
    Deleted,
    Inserted,
    Moved,
    ChildrenChanged,
    SubtreeChanged,
    ModifiedWrapper,
}

impl DiffMarkers {
    /// The wire value, faithful to the PHP backing string.
    pub fn value(self) -> &'static str {
        match self {
            DiffMarkers::Deleted => "deleted",
            DiffMarkers::Inserted => "inserted",
            DiffMarkers::Moved => "moved",
            DiffMarkers::ChildrenChanged => "children-changed",
            DiffMarkers::SubtreeChanged => "subtree-changed",
            DiffMarkers::ModifiedWrapper => "modified-wrapper",
        }
    }

    /// Parse a marker from its wire value (PHP `DiffMarkers::from`). `None` for
    /// unknown values.
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "deleted" => Some(DiffMarkers::Deleted),
            "inserted" => Some(DiffMarkers::Inserted),
            "moved" => Some(DiffMarkers::Moved),
            "children-changed" => Some(DiffMarkers::ChildrenChanged),
            "subtree-changed" => Some(DiffMarkers::SubtreeChanged),
            "modified-wrapper" => Some(DiffMarkers::ModifiedWrapper),
            _ => None,
        }
    }
}

/// The set of diff markers carried on an element via `data-parsoid-diff`.
/// Faithful to PHP's `NodeData\DataParsoidDiff` (a set backed by an
/// `array<string,true>`, serialized as `{"diff":[<mark>,…]}` with sorted keys).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataParsoidDiff {
    diff: Vec<String>,
}

impl DataParsoidDiff {
    pub fn new() -> Self {
        Self::default()
    }

    /// `DataParsoidDiff::isEmpty`.
    pub fn is_empty(&self) -> bool {
        self.diff.is_empty()
    }

    /// `DataParsoidDiff::addDiffMarker` — add `mark` to the set (idempotent).
    pub fn add_diff_marker(&mut self, mark: DiffMarkers) {
        if !self.diff.iter().any(|m| m == mark.value()) {
            self.diff.push(mark.value().to_string());
        }
    }

    /// `DataParsoidDiff::hasDiffMarker` — is `mark` present?
    pub fn has_diff_marker(&self, mark: DiffMarkers) -> bool {
        self.diff.iter().any(|m| m == mark.value())
    }

    /// `DataParsoidDiff::hasOnlyDiffMarkers` — no marks other than the given
    /// ones are present.
    pub fn has_only_diff_markers(&self, marks: &[DiffMarkers]) -> bool {
        let present = marks.iter().filter(|m| self.has_diff_marker(**m)).count();
        present == self.diff.len()
    }

    /// Serialize to the `data-parsoid-diff` JSON value (`{"diff":[...]}`), with
    /// markers sorted for a stable, faithful representation.
    pub fn to_json(&self) -> String {
        let mut markers = self.diff.clone();
        markers.sort();
        serde_json::json!({ "diff": markers }).to_string()
    }

    /// Parse a `data-parsoid-diff` JSON value. Unknown marker strings are
    /// dropped (PHP `DiffMarkers::from` would throw, but the wire never
    /// produces them; dropping is the non-panicking faithful-superset).
    pub fn from_json(json: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        let arr = v.get("diff")?.as_array()?;
        let mut dpd = DataParsoidDiff::new();
        for item in arr {
            if let Some(mark_str) = item.as_str()
                && let Some(mark) = DiffMarkers::from_value(mark_str)
            {
                dpd.add_diff_marker(mark);
            }
        }
        Some(dpd)
    }
}

/// Read-only diff-query helpers, faithful to `Html2Wt\DiffUtils`.
pub struct DiffUtils;

impl DiffUtils {
    /// `DiffUtils::getDiffMark` — the parsed `data-parsoid-diff` of an element
    /// node (or `None` for non-elements / absent attribute). The empty set is
    /// normalized to `None`, matching PHP's `getDataParsoidDiff` returning a
    /// (possibly empty) object whose `hasDiffMarker` calls all short-circuit on
    /// absence — but callers here use `Option` to signal "no annotation".
    pub fn get_diff_mark(node: &Node) -> Option<DataParsoidDiff> {
        if !matches!(node.kind, NodeKind::Element(_)) {
            return None;
        }
        node.get_attr("data-parsoid-diff")
            .and_then(DataParsoidDiff::from_json)
    }

    /// `DiffUtils::hasDiffMarkers` — the node has structured diff markers or is a
    /// literal `mw:DiffMarker` meta.
    pub fn has_diff_markers(node: &Node) -> bool {
        Self::get_diff_mark(node).is_some() || Self::is_diff_marker(node, None)
    }

    /// `DiffUtils::isDiffMarker` — a `meta` element whose `typeof` matches
    /// `mw:DiffMarker/…` (optionally constrained to a specific `mark`).
    pub fn is_diff_marker(node: &Node, mark: Option<DiffMarkers>) -> bool {
        if !matches!(node.kind, NodeKind::Element(_)) {
            return false;
        }
        if crate::html::wts_utils::node_name(node) != "meta" {
            return false;
        }
        match mark {
            None => crate::html::dom_utils::match_type_of(node, "^mw:DiffMarker/").is_some(),
            Some(m) => {
                crate::html::dom_utils::has_type_of(node, &format!("mw:DiffMarker/{}", m.value()))
            }
        }
    }

    /// `DiffUtils::hasDiffMark` — `deleted`/non-element `inserted` marks live on
    /// the *preceding* `mw:DiffMarker` meta; every other mark lives in the
    /// node's own `data-parsoid-diff`.
    pub fn has_diff_mark(node: &Node, prev: Option<&Node>, mark: DiffMarkers) -> bool {
        if mark == DiffMarkers::Deleted
            || (mark == DiffMarkers::Inserted && !matches!(node.kind, NodeKind::Element(_)))
        {
            match prev {
                Some(prev) => Self::is_diff_marker(prev, Some(mark)),
                None => false,
            }
        } else {
            Self::get_diff_mark(node).is_some_and(|d| d.has_diff_marker(mark))
        }
    }

    /// `DiffUtils::hasInsertedDiffMark`.
    pub fn has_inserted_diff_mark(node: &Node, prev: Option<&Node>) -> bool {
        Self::has_diff_mark(node, prev, DiffMarkers::Inserted)
    }

    /// `DiffUtils::maybeDeletedNode` — an element that is a `deleted` diff marker.
    pub fn maybe_deleted_node(node: &Node) -> bool {
        matches!(node.kind, NodeKind::Element(_))
            && Self::is_diff_marker(node, Some(DiffMarkers::Deleted))
    }

    /// `DiffUtils::isDeletedBlockNode` — a deleted block node (has
    /// `data-is-block`).
    pub fn is_deleted_block_node(node: &Node) -> bool {
        Self::maybe_deleted_node(node) && node.get_attr("data-is-block").is_some()
    }

    /// `DiffUtils::directChildrenChanged`.
    pub fn direct_children_changed(node: &Node) -> bool {
        Self::get_diff_mark(node).is_some_and(|d| d.has_diff_marker(DiffMarkers::ChildrenChanged))
    }

    /// `DiffUtils::onlySubtreeChanged` — only `subtree-changed`/`children-changed`
    /// marks are present (and at least one is).
    pub fn only_subtree_changed(node: &Node) -> bool {
        match Self::get_diff_mark(node) {
            None => false,
            Some(d) => d.has_only_diff_markers(&[
                DiffMarkers::SubtreeChanged,
                DiffMarkers::ChildrenChanged,
            ]),
        }
    }

    /// `DiffUtils::subtreeUnchanged` — no marks, or only `modified-wrapper`.
    pub fn subtree_unchanged(node: &Node) -> bool {
        match Self::get_diff_mark(node) {
            None => true,
            Some(d) => d.has_only_diff_markers(&[DiffMarkers::ModifiedWrapper]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, Node};

    fn meta_marker(ty: &str) -> Node {
        let mut meta = Node::element(ElementKind::Other("meta".to_string()));
        meta.set_attr("typeof", ty);
        meta
    }

    fn element_with_diff(marks: &[DiffMarkers]) -> Node {
        let mut el = Node::element(ElementKind::Span);
        let mut dpd = DataParsoidDiff::new();
        for m in marks {
            dpd.add_diff_marker(*m);
        }
        el.set_attr("data-parsoid-diff", dpd.to_json());
        el
    }

    #[test]
    fn test_marker_values_round_trip() {
        for m in [
            DiffMarkers::Deleted,
            DiffMarkers::Inserted,
            DiffMarkers::Moved,
            DiffMarkers::ChildrenChanged,
            DiffMarkers::SubtreeChanged,
            DiffMarkers::ModifiedWrapper,
        ] {
            assert_eq!(DiffMarkers::from_value(m.value()), Some(m));
        }
        assert_eq!(DiffMarkers::from_value("bogus"), None);
    }

    #[test]
    fn test_is_diff_marker() {
        let deleted = meta_marker("mw:DiffMarker/deleted");
        assert!(DiffUtils::is_diff_marker(
            &deleted,
            Some(DiffMarkers::Deleted)
        ));
        assert!(!DiffUtils::is_diff_marker(
            &deleted,
            Some(DiffMarkers::Inserted)
        ));
        assert!(DiffUtils::is_diff_marker(&deleted, None));

        // Not a meta => not a marker.
        let span = Node::element(ElementKind::Span);
        assert!(!DiffUtils::is_diff_marker(&span, None));
    }

    #[test]
    fn test_has_diff_mark_element_vs_text() {
        let el = element_with_diff(&[DiffMarkers::ChildrenChanged]);
        assert!(DiffUtils::has_diff_mark(
            &el,
            None,
            DiffMarkers::ChildrenChanged
        ));
        assert!(!DiffUtils::has_diff_mark(&el, None, DiffMarkers::Deleted));

        // Text node `deleted` mark lives on the preceding marker meta.
        let text = Node::text("x");
        let prev = meta_marker("mw:DiffMarker/deleted");
        assert!(DiffUtils::has_diff_mark(
            &text,
            Some(&prev),
            DiffMarkers::Deleted
        ));
        assert!(!DiffUtils::has_diff_mark(&text, None, DiffMarkers::Deleted));
    }

    #[test]
    fn test_subtree_unchanged_and_only_subtree_changed() {
        let clean = Node::element(ElementKind::Span);
        assert!(DiffUtils::subtree_unchanged(&clean));
        assert!(!DiffUtils::only_subtree_changed(&clean));

        let modified = element_with_diff(&[DiffMarkers::ModifiedWrapper]);
        assert!(DiffUtils::subtree_unchanged(&modified));

        let subtree =
            element_with_diff(&[DiffMarkers::SubtreeChanged, DiffMarkers::ChildrenChanged]);
        assert!(DiffUtils::only_subtree_changed(&subtree));
        assert!(!DiffUtils::subtree_unchanged(&subtree));
    }

    #[test]
    fn test_is_deleted_block_node() {
        let mut deleted = meta_marker("mw:DiffMarker/deleted");
        assert!(DiffUtils::maybe_deleted_node(&deleted));
        assert!(!DiffUtils::is_deleted_block_node(&deleted));
        deleted.set_attr("data-is-block", "1");
        assert!(DiffUtils::is_deleted_block_node(&deleted));
    }
}
