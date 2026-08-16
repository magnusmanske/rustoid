//! Faithful port of RemexHtml's `Attributes` and `Element` types, used by the
//! tree builder. `Element` carries the per-element state the `Stack` and
//! `ActiveFormattingElements` link together.

use std::fmt;

/// An ordered set of attributes.
///
/// Ports `Wikimedia\RemexHtml\Tokenizer\Attributes`. Keys may appear more than
/// once before de-duplication, but Parsoid feeds simple `name => value` pairs
/// (via `TreeBuilderStage::kvArrToAttr`), so we keep a plain ordered list and
/// a `get` that returns the first match.
#[derive(Debug, Clone, Default)]
pub struct Attributes {
    entries: Vec<(String, String)>,
}

impl Attributes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        Self { entries: pairs }
    }

    /// The number of attributes (may include duplicates).
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Get a value by name (mirrors `ArrayAccess`), case-sensitive.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn get_values(&self) -> &[(String, String)] {
        &self.entries
    }

    /// Add attributes from `other`, not overwriting existing names.
    pub fn merge(&mut self, other: &Attributes) {
        for (k, v) in &other.entries {
            if self.get(k).is_none() {
                self.entries.push((k.clone(), v.clone()));
            }
        }
    }

    /// Shallow clone (mirrors the PHP `clone()` default, which returns `$this`).
    pub fn cloned(&self) -> Attributes {
        self.clone()
    }
}

/// An element node under construction, with the tree-builder link fields.
///
/// Ports `Wikimedia\RemexHtml\TreeBuilder\Element`. The `stackIndex`,
/// `prevAFE`/`nextAFE`/`nextNoah` fields are used by `Stack` and
/// `ActiveFormattingElements`; they live here (like the PHP `Element`), rather
/// than in a separate side table, to mirror the PHP design exactly.
#[derive(Clone)]
pub struct Element {
    pub namespace: String,
    pub name: String,
    pub html_name: String,
    pub attrs: Attributes,
    pub is_virtual: bool,
    /// Link in the CachingStack scope list.
    pub next_elt_in_scope: Option<usize>,
    /// Current stack index, or `None` if not in the stack.
    pub stack_index: Option<usize>,
    /// Previous AFE entry (Element index or marker sentinel).
    pub prev_afe: Option<AfeLink>,
    /// Next AFE entry (Element index or marker sentinel).
    pub next_afe: Option<AfeLink>,
    /// Next element in the Noah's Ark bucket (element index).
    pub next_noah: Option<usize>,
    /// User data attached by the handler (the DOM node id).
    pub user_data: usize,
    /// Unique id.
    pub uid: usize,
}

/// A link in the active-formatting-elements list: either an element (by index)
/// or a scope marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfeLink {
    Element(usize),
    Marker,
}

impl Element {
    pub fn new(namespace: &str, name: &str, attrs: Attributes, uid: usize) -> Self {
        let html_name = if namespace == super::html_data::NS_HTML {
            name.to_string()
        } else if namespace == super::html_data::NS_MATHML {
            format!("mathml {name}")
        } else if namespace == super::html_data::NS_SVG {
            format!("svg {name}")
        } else {
            format!("{namespace} {name}")
        };
        Element {
            namespace: namespace.to_string(),
            name: name.to_string(),
            html_name,
            attrs,
            is_virtual: false,
            next_elt_in_scope: None,
            stack_index: None,
            prev_afe: None,
            next_afe: None,
            next_noah: None,
            user_data: 0,
            uid,
        }
    }

    /// Is the element a MathML text integration point?
    pub fn is_mathml_text_integration(&self) -> bool {
        self.namespace == super::html_data::NS_MATHML
            && matches!(self.name.as_str(), "mi" | "mo" | "mn" | "ms" | "mtext")
    }

    /// Is the element an HTML integration point?
    pub fn is_html_integration(&self) -> bool {
        if self.namespace == super::html_data::NS_MATHML {
            if let Some(enc) = self.attrs.get("encoding") {
                let enc = enc.to_ascii_lowercase();
                enc == "text/html" || enc == "application/xhtml+xml"
            } else {
                false
            }
        } else if self.namespace == super::html_data::NS_SVG {
            matches!(self.name.as_str(), "foreignObject" | "desc" | "title")
        } else {
            false
        }
    }

    /// A string key for the Noah's Ark algorithm. We approximate PHP's
    /// `serialize([htmlName, attrs])` with a stable string of name + sorted
    /// attributes, which is sufficient to group identical elements.
    pub fn noah_key(&self) -> String {
        let mut pairs: Vec<(&str, &str)> = self
            .attrs
            .get_values()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        pairs.sort_unstable();
        let mut key = self.html_name.clone();
        for (k, v) in pairs {
            key.push('\x1f');
            key.push_str(k);
            key.push('\x1e');
            key.push_str(v);
        }
        key
    }
}

impl fmt::Debug for Element {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.html_name, self.uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_element_html_name() {
        let e = Element::new(
            super::super::html_data::NS_HTML,
            "div",
            Attributes::new(),
            1,
        );
        assert_eq!(e.html_name, "div");
    }

    #[test]
    fn test_element_mathml_name() {
        let e = Element::new(
            super::super::html_data::NS_MATHML,
            "mi",
            Attributes::new(),
            1,
        );
        assert_eq!(e.html_name, "mathml mi");
    }

    #[test]
    fn test_html_integration() {
        let svg = Element::new(
            super::super::html_data::NS_SVG,
            "foreignObject",
            Attributes::new(),
            1,
        );
        assert!(svg.is_html_integration());

        let mathml = Element::new(
            super::super::html_data::NS_MATHML,
            "annotation-xml",
            Attributes::from_pairs(vec![("encoding".to_string(), "text/html".to_string())]),
            2,
        );
        assert!(mathml.is_html_integration());
    }

    #[test]
    fn test_attributes_merge() {
        let mut a = Attributes::from_pairs(vec![("x".to_string(), "1".to_string())]);
        let b = Attributes::from_pairs(vec![
            ("x".to_string(), "2".to_string()),
            ("y".to_string(), "3".to_string()),
        ]);
        a.merge(&b);
        assert_eq!(a.get("x"), Some("1"));
        assert_eq!(a.get("y"), Some("3"));
    }
}
