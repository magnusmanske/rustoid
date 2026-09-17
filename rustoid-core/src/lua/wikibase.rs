//! Wikidata entities, as `mw.wikibase` presents them.
//!
//! `mw.wikibase` is the Wikibase Client extension. Modules use it to read
//! statement values, labels and sitelinks from Wikidata, and its answers reach
//! the output: `Module:Sister project links` renders a box of links derived from
//! entity claims, so a page compared against Parsoid has to see the same entity
//! data Parsoid's wiki did.
//!
//! Lua cannot fetch, so entities are gathered *before* execution, exactly as
//! module sources are (see [`crate::lua::invoke`]). Each id is stored under its
//! upper-cased form, because modules write both `Q42` and `q42`.
//!
//! An id that was not fetched is absent rather than empty, which is what lets
//! `entityExists` answer false and lets the ~19 cached modules that guard on it
//! take their own fallback — the behaviour a wiki without Wikidata shows.

use std::collections::HashMap;

/// Entities available to `mw.wikibase`, plus the indexes needed to resolve a
/// *page* to an entity.
#[derive(Debug, Clone, Default)]
pub struct Entities {
    /// Entity JSON by upper-cased id (`Q42`, `P31`).
    by_id: HashMap<String, String>,
    /// Sitelink title (as `getEntityIdForTitle` was asked for it, in the wiki's
    /// own namespace form) to upper-cased entity id.
    ///
    /// Built from the sitelinks of the fetched entities. `getEntityIdForTitle`
    /// is a *search*, not a lookup, and answering it from preloaded data means
    /// the answer is only as complete as what was fetched: an id whose entity
    /// was never fetched cannot be found this way.
    by_title: HashMap<String, String>,
    /// The entity for the page being parsed, when its sitelink was found.
    current: Option<String>,
}

impl Entities {
    /// Record an entity's JSON under `id`.
    ///
    /// Also indexes its `enwiki` sitelink, so a later `getEntityIdForTitle` for
    /// that title resolves without a second lookup.
    pub fn insert(&mut self, id: &str, json: String) {
        let id = id.trim().to_ascii_uppercase();
        if id.is_empty() {
            return;
        }
        if let Some(title) = enwiki_sitelink(&json) {
            self.by_title.insert(title, id.clone());
        }
        self.by_id.insert(id, json);
    }

    /// True when no entity was loaded, which is the common case for a wiki
    /// without Wikidata access.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The id for a page title, when the title is some entity's sitelink.
    pub fn id_for_title(&self, title: &str) -> Option<&str> {
        let key = title.trim().replace('_', " ");
        if key.is_empty() {
            return self.current.as_deref();
        }
        self.by_title.get(&key).map(String::as_str)
    }

    /// The entity id for the page being parsed.
    pub fn set_current(&mut self, id: Option<String>) {
        self.current = id.map(|i| i.trim().to_ascii_uppercase());
    }

    /// The entity id for the page being parsed, when one was resolved.
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// All `(id, json)` pairs, for handing to Lua.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_id.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// The `enwiki` sitelink in an entity's JSON, as a wiki title.
///
/// Sitelinks are keyed by site id (`enwiki`), and the value is the page title on
/// that wiki, which is what `getEntityIdForTitle` matches against.
fn enwiki_sitelink(json: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    // `Special:EntityData/<id>.json` returns `{"entities":{"Q42":{…}}}`, while
    // the bare entity is what `wbgetentities` yields. Accept both.
    let entity = parsed
        .get("entities")
        .and_then(|e| e.as_object())
        .and_then(|o| o.values().next())
        .unwrap_or(&parsed);
    let title = entity
        .get("sitelinks")?
        .get("enwiki")?
        .get("title")?
        .as_str()?;
    Some(title.replace('_', " "))
}

/// Entity ids a module mentions as literals, for preloading.
///
/// Only `/Q\d+/` and `/P\d+/` string literals are collected. A computed id
/// cannot be preloaded, so `getEntityIdForTitle` is the only route to those;
/// that is the same trade the module loader makes for `require`.
pub fn referenced_entity_ids(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        // An id only counts inside a string literal, so a `Q` in a comment or a
        // variable name is not collected.
        let quote = bytes[idx];
        if quote != b'\'' && quote != b'"' {
            idx += 1;
            continue;
        }
        let Some(close) = source[idx + 1..].find(quote as char) else {
            break;
        };
        let literal = &source[idx + 1..idx + 1 + close];
        if let Some(id) = entity_id_in(literal) {
            out.push(id);
        }
        idx += close + 2;
    }
    out.sort();
    out.dedup();
    out
}

/// The id when `text` is exactly an entity id (`Q42`, `p31`).
fn entity_id_in(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let kind = chars.next()?;
    if !matches!(kind, 'Q' | 'q' | 'P' | 'p') {
        return None;
    }
    let digits: String = chars.collect();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}{digits}", kind.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q42: &str = r#"{
        "entities": {
            "Q42": {
                "id": "Q42",
                "labels": {"en": {"language": "en", "value": "Douglas Adams"}},
                "sitelinks": {"enwiki": {"site": "enwiki", "title": "Douglas Adams"}}
            }
        }
    }"#;

    #[test]
    fn an_entity_is_indexed_by_its_sitelink() {
        let mut entities = Entities::default();
        entities.insert("Q42", Q42.to_string());
        assert_eq!(entities.id_for_title("Douglas Adams"), Some("Q42"));
        // Underscores are the same title.
        assert_eq!(entities.id_for_title("Douglas_Adams"), Some("Q42"));
    }

    #[test]
    fn an_id_is_found_whatever_its_case() {
        let mut entities = Entities::default();
        entities.insert("q42", Q42.to_string());
        assert_eq!(entities.id_for_title("Douglas Adams"), Some("Q42"));
        assert_eq!(entities.iter().count(), 1);
    }

    #[test]
    fn an_unknown_title_has_no_id() {
        let mut entities = Entities::default();
        entities.insert("Q42", Q42.to_string());
        assert_eq!(entities.id_for_title("Nobody"), None);
    }

    #[test]
    fn an_empty_title_answers_with_the_current_page_entity() {
        let mut entities = Entities::default();
        entities.insert("Q42", Q42.to_string());
        entities.set_current(Some("q42".to_string()));
        assert_eq!(entities.id_for_title(""), Some("Q42"));
    }

    #[test]
    fn a_parameter_entity_is_read_too() {
        // Properties are entities with their own id shape.
        let json = r#"{"entities": {"P31": {"id": "P31", "sitelinks": {}}}}"#;
        let mut entities = Entities::default();
        entities.insert("P31", json.to_string());
        assert_eq!(entities.id_for_title("anything"), None);
        assert!(entities.iter().any(|(id, _)| id == "P31"));
    }

    #[test]
    fn entity_ids_are_collected_only_from_literals() {
        let src = r#"
            local a = 'Q42'
            local b = "p31"
            -- Q99 is only a comment
            local c = Q5
            local d = 'not-an-id'
        "#;
        assert_eq!(referenced_entity_ids(src), vec!["P31", "Q42"]);
    }

    #[test]
    fn a_malformed_entity_is_ignored_rather_than_fatal() {
        let mut entities = Entities::default();
        // No sitelink index entry, but the id is still readable.
        entities.insert("Q1", "{".to_string());
        assert_eq!(entities.id_for_title("x"), None);
        assert!(entities.iter().any(|(id, _)| id == "Q1"));
    }
}
