//! Build a `SiteConfig` from a wiki's own `siteinfo`.
//!
//! `rustoid-core`'s `MockSiteConfig` hardcodes an enwiki-*shaped* configuration:
//! 26 extension tags, a hand-written magic-word list, and no interwiki map. That
//! is fine for the fixture suite, which pins its own expectations, but it silently
//! caps what an online comparison can represent — a wiki tag that is absent from
//! the list is not treated as an extension at all.
//!
//! The gap is real, not theoretical. Against live en.wikipedia.org:
//!
//! | | mock | live enwiki |
//! |---|---|---|
//! | extension tags | 26 (incl. test-only `pwraptest`, `divtag`, `sealtag`) | 28 (incl. `charinsert`, `inputbox`, `imagemap`, `categorytree`, `section`, `score`, `langconvert`, …) |
//! | function hooks | 8 | 112 |
//! | magic words | ~60 | 285 |
//! | namespaces | 0 (canonical only) | 30 |
//! | interwiki | none | 781 |
//!
//! So this module parses `action=query&meta=siteinfo` and produces a config that
//! reflects the wiki actually under test. It is intentionally a separate type
//! rather than a mutation of `MockSiteConfig`, so the fixture suite keeps its
//! pinned configuration.

use std::collections::HashMap;

use rustoid_core::traits::{
    InterwikiInfo, MagicWordEntry, MagicWordMap, NamespaceInfo, SiteConfig,
};
use serde::Deserialize;

use crate::error::{CompareError, Result};

/// A `SiteConfig` built from a wiki's `siteinfo` response.
#[derive(Debug, Default)]
pub struct WikiSiteConfig {
    namespaces: HashMap<i32, NamespaceInfo>,
    interwiki_map: HashMap<String, InterwikiInfo>,
    magic_words: MagicWordMap,
    function_hooks: Vec<String>,
    extension_tags: Vec<String>,
    server_url: String,
    article_path: String,
    language_code: String,
}

impl WikiSiteConfig {
    /// Parse a `siteinfo` JSON response body.
    ///
    /// Tolerates missing sections: a wiki may omit any of them, and a partial
    /// config still beats the hardcoded mock. Unknown extra keys are ignored.
    pub fn from_siteinfo_json(json: &str) -> Result<Self> {
        let parsed: SiteInfoResponse =
            serde_json::from_str(json).map_err(|e| CompareError::Response {
                url: "<siteinfo>".to_string(),
                message: format!("siteinfo parse: {e}"),
            })?;
        let q = parsed.query;

        let mut cfg = WikiSiteConfig::default();

        // `general` carries the server/article path and language.
        if let Some(g) = q.general {
            if let Some(server) = g.server {
                cfg.server_url = server;
            }
            if let Some(path) = g.articlepath {
                cfg.article_path = path;
            }
            if let Some(lang) = g.lang {
                cfg.language_code = lang;
            }
        }

        // Localized aliases arrive separately, keyed by namespace id rather
        // than nested under the namespace they alias.
        let mut extra_aliases: HashMap<i32, Vec<String>> = HashMap::new();
        if let Some(aliases) = q.namespacealiases {
            for alias in aliases {
                if let Some(text) = alias.alias() {
                    extra_aliases
                        .entry(alias.id)
                        .or_default()
                        .push(text.to_string());
                }
            }
        }

        // `namespaces` is keyed by id-as-string in formatversion=2.
        if let Some(ns) = q.namespaces {
            for (id, entry) in ns {
                // Trust the key only as a string; prefer the explicit `id` field
                // when present, since that is what the wiki itself reports.
                let id_num = match entry.id.or_else(|| id.parse::<i32>().ok()) {
                    Some(id) => id,
                    None => continue,
                };
                // The localized name is the `*` key; the canonical name is a
                // separate field. Both are usable spellings.
                let mut aliases: Vec<String> = Vec::new();
                if let Some(name) = entry.localized_name() {
                    aliases.push(name.to_string());
                }
                if let Some(extra) = extra_aliases.remove(&id_num) {
                    aliases.extend(extra);
                }
                aliases.dedup();
                cfg.namespaces.insert(
                    id_num,
                    NamespaceInfo {
                        canonical: entry.canonical.unwrap_or_default(),
                        aliases,
                        // `case` is `first-letter` (insensitive) or
                        // `case-sensitive`.
                        case_sensitive: entry.case.as_deref() == Some("case-sensitive"),
                        default_content_model: entry.defaultcontentmodel.unwrap_or_default(),
                    },
                );
            }
        }

        // `extensiontags` come as `<ref>`, but a live wiki also lists the
        // closing form `</ref>`; rustoid keys on the bare name.
        if let Some(tags) = q.extensiontags {
            cfg.extension_tags = tags
                .into_iter()
                .filter_map(|t| {
                    let t = t.trim();
                    let inner = t
                        .strip_prefix("</")
                        .or_else(|| t.strip_prefix('<'))
                        .and_then(|s| s.strip_suffix('>'))
                        .unwrap_or(t);
                    let inner = inner.trim_end_matches('/').trim();
                    if inner.is_empty() {
                        None
                    } else {
                        Some(inner.to_lowercase())
                    }
                })
                .collect();
            cfg.extension_tags.sort();
            cfg.extension_tags.dedup();
        }

        if let Some(hooks) = q.functionhooks {
            cfg.function_hooks = hooks.into_iter().map(|h| h.to_lowercase()).collect();
            cfg.function_hooks.sort();
            cfg.function_hooks.dedup();
        }

        // `magicwords` entries look like
        // `{"name":"!","aliases":["!"],"case-sensitive":true}`.
        if let Some(words) = q.magicwords {
            for w in words {
                if w.name.is_empty() {
                    continue;
                }
                let aliases = if w.aliases.is_empty() {
                    vec![w.name.clone()]
                } else {
                    w.aliases
                };
                cfg.magic_words.insert(
                    w.name.clone(),
                    MagicWordEntry {
                        canonical: w.name,
                        case_sensitive: w.case_sensitive.unwrap_or(false),
                        aliases,
                    },
                );
            }
        }

        // `interwikimap` entries carry a prefix and either a url or a local flag.
        if let Some(iw) = q.interwikimap {
            for entry in iw {
                let Some(prefix) = entry.prefix else { continue };
                cfg.interwiki_map.insert(
                    prefix.clone(),
                    InterwikiInfo {
                        url: entry.url.unwrap_or_default(),
                        local: entry.local,
                        transclusion_allowed: false,
                        localinterwiki: None,
                        language: entry.language,
                        extralanglink: None,
                        protorel: None,
                        prefix: Some(prefix),
                    },
                );
            }
        }

        Ok(cfg)
    }

    /// Number of configured extension tags (for reporting).
    pub fn extension_tag_count(&self) -> usize {
        self.extension_tags.len()
    }

    /// Number of configured function hooks (for reporting).
    pub fn function_hook_count(&self) -> usize {
        self.function_hooks.len()
    }

    /// Number of configured magic words (for reporting).
    pub fn magic_word_count(&self) -> usize {
        self.magic_words.len()
    }

    /// Number of configured namespaces (for reporting).
    pub fn namespace_count(&self) -> usize {
        self.namespaces.len()
    }

    /// Number of interwiki prefixes (for reporting).
    pub fn interwiki_count(&self) -> usize {
        self.interwiki_map.len()
    }
}

impl SiteConfig for WikiSiteConfig {
    fn namespaces(&self) -> &HashMap<i32, NamespaceInfo> {
        &self.namespaces
    }

    fn interwiki_map(&self) -> &HashMap<String, InterwikiInfo> {
        &self.interwiki_map
    }

    fn magic_words(&self) -> &MagicWordMap {
        &self.magic_words
    }

    fn function_hooks(&self) -> &[String] {
        &self.function_hooks
    }

    fn extension_tags(&self) -> &[String] {
        &self.extension_tags
    }

    fn server_url(&self) -> &str {
        if self.server_url.is_empty() {
            "https://en.wikipedia.org"
        } else {
            &self.server_url
        }
    }

    fn article_path(&self) -> &str {
        if self.article_path.is_empty() {
            "/wiki/$1"
        } else {
            &self.article_path
        }
    }

    fn language_code(&self) -> &str {
        if self.language_code.is_empty() {
            "en"
        } else {
            &self.language_code
        }
    }
}

// ---- wire types ----

#[derive(Debug, Deserialize)]
struct SiteInfoResponse {
    query: SiteInfoQuery,
}

#[derive(Debug, Deserialize, Default)]
struct SiteInfoQuery {
    #[serde(default)]
    general: Option<General>,
    #[serde(default)]
    namespaces: Option<HashMap<String, NamespaceEntry>>,
    #[serde(default)]
    namespacealiases: Option<Vec<NamespaceAlias>>,
    #[serde(default)]
    magicwords: Option<Vec<MagicWord>>,
    #[serde(default)]
    functionhooks: Option<Vec<String>>,
    #[serde(default)]
    extensiontags: Option<Vec<String>>,
    #[serde(default)]
    interwikimap: Option<Vec<InterwikiEntry>>,
}

#[derive(Debug, Deserialize, Default)]
struct General {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    articlepath: Option<String>,
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct NamespaceEntry {
    #[serde(default)]
    id: Option<i32>,
    #[serde(default)]
    canonical: Option<String>,
    /// `name` in formatversion=2; formatversion=1 spells it `*`.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    case: Option<String>,
    #[serde(default)]
    defaultcontentmodel: Option<String>,
    /// Everything else, captured loosely. Values are not all strings (`subpages`
    /// is a bool in formatversion=2 and an empty string in formatversion=1),
    /// so a typed map would reject a perfectly valid response.
    #[serde(flatten)]
    raw: HashMap<String, serde_json::Value>,
}

impl NamespaceEntry {
    /// The localized namespace name, whichever format version spelled it.
    fn localized_name(&self) -> Option<&str> {
        let from_name = self.name.as_deref();
        let from_star = self.raw.get("*").and_then(|v| v.as_str());
        from_name.or(from_star).filter(|n| !n.is_empty())
    }
}

#[derive(Debug, Deserialize, Default)]
struct NamespaceAlias {
    #[serde(default)]
    id: i32,
    #[serde(default, rename = "*")]
    alias_fv1: Option<String>,
    #[serde(flatten)]
    raw: HashMap<String, serde_json::Value>,
}

impl NamespaceAlias {
    fn alias(&self) -> Option<&str> {
        // Neither format version puts the alias under a stable field name, so
        // take the shortest string value; `id` is already consumed above.
        self.raw
            .values()
            .filter_map(|v| v.as_str())
            .filter(|a| !a.is_empty())
            .min_by_key(|a| a.len())
            .or(self.alias_fv1.as_deref())
    }
}

#[derive(Debug, Deserialize, Default)]
struct MagicWord {
    #[serde(default)]
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default, rename = "case-sensitive")]
    case_sensitive: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct InterwikiEntry {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    local: bool,
    #[serde(default)]
    language: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed but structurally faithful `siteinfo` response.
    const SAMPLE: &str = r#"{
      "batchcomplete": true,
      "query": {
        "general": {
          "sitename": "Wikipedia",
          "server": "//en.wikipedia.org",
          "articlepath": "/wiki/$1",
          "lang": "en"
        },
        "namespaces": {
          "0":  { "id": 0,  "case": "first-letter", "name": "",     "content": true },
          "10": { "id": 10, "case": "first-letter", "name": "Template", "canonical": "Template", "content": false }
        },
        "namespacealiases": [
          { "id": 10, "alias": "TM" }
        ],
        "magicwords": [
          { "name": "!", "aliases": ["!"], "case-sensitive": true },
          { "name": "PAGENAME", "aliases": ["PAGENAME"], "case-sensitive": true }
        ],
        "functionhooks": ["anchorencode", "canonicalurl", "fullurl", "if", "invoke"],
        "extensiontags": ["<pre>", "<nowiki>", "<ref>", "</ref>", "<references>", "<gallery>"],
        "interwikimap": [
          { "prefix": "wikipedia", "url": "https://en.wikipedia.org/wiki/$1", "local": true },
          { "prefix": "commons", "url": "https://commons.wikimedia.org/wiki/$1" }
        ]
      }
    }"#;

    fn cfg() -> WikiSiteConfig {
        WikiSiteConfig::from_siteinfo_json(SAMPLE).unwrap()
    }

    /// Formatversion=2 spells the localized namespace name `name`, not `*`; if
    /// only the formatversion=1 spelling is understood, every lookup misses.
    #[test]
    fn formatversion_two_namespace_shape_is_understood() {
        let c = WikiSiteConfig::from_siteinfo_json(
            r#"{"query":{
              "namespaces":{
                "0": {"id":0,"case":"first-letter","name":"","content":true,"subpages":false},
                "10":{"id":10,"case":"first-letter","name":"Vorlage","canonical":"Template","content":false,"subpages":true}
              },
              "namespacealiases":[{"id":10,"alias":"V"}]
            }}"#,
        )
        .unwrap();
        let ns10 = c.namespaces().get(&10).expect("namespace 10");
        assert_eq!(ns10.canonical, "Template");
        assert_eq!(ns10.aliases, vec!["Vorlage".to_string(), "V".to_string()]);
        // Namespace 0's localized name is the empty string; it must not become
        // an alias, or it would match every title prefix.
        assert!(c.namespaces().get(&0).unwrap().aliases.is_empty());
    }

    /// Formatversion=1 spells both the name and the alias `*`.
    #[test]
    fn formatversion_one_namespace_shape_is_understood() {
        let c = WikiSiteConfig::from_siteinfo_json(
            r#"{"query":{
              "namespaces":{
                "0": {"id":0,"case":"first-letter","content":"","*":""},
                "10":{"id":10,"case":"first-letter","subpages":"","canonical":"Template","*":"Vorlage"}
              },
              "namespacealiases":[{"id":10,"*":"V"}]
            }}"#,
        )
        .unwrap();
        let ns10 = c.namespaces().get(&10).expect("namespace 10");
        assert_eq!(ns10.aliases, vec!["Vorlage".to_string(), "V".to_string()]);
        assert!(c.namespaces().get(&0).unwrap().aliases.is_empty());
    }

    #[test]
    fn parses_general_metadata() {
        let c = cfg();
        assert_eq!(c.server_url(), "//en.wikipedia.org");
        assert_eq!(c.article_path(), "/wiki/$1");
        assert_eq!(c.language_code(), "en");
    }

    #[test]
    fn extension_tags_lose_their_angle_brackets() {
        let c = cfg();
        let tags = c.extension_tags();
        assert!(tags.contains(&"ref".to_string()), "{tags:?}");
        assert!(tags.contains(&"nowiki".to_string()), "{tags:?}");
        // The raw form must not leak through, or lookups would never match.
        assert!(!tags.iter().any(|t| t.contains('<')), "{tags:?}");
    }

    #[test]
    fn function_hooks_are_exposed() {
        let c = cfg();
        assert!(c.function_hooks().contains(&"invoke".to_string()));
        assert!(c.function_hooks().contains(&"anchorencode".to_string()));
    }

    #[test]
    fn namespaces_are_keyed_by_id() {
        let c = cfg();
        assert_eq!(c.namespaces().get(&10).unwrap().canonical, "Template");
        // `first-letter` means *not* case-sensitive for page names.
        assert!(!c.namespaces().get(&10).unwrap().case_sensitive);
        // The localized name and any registered aliases are both spellings.
        assert_eq!(
            c.namespaces().get(&10).unwrap().aliases,
            vec!["Template".to_string(), "TM".to_string()]
        );
        // Namespace 0 has an empty localized name; that must not become an alias.
        assert!(c.namespaces().get(&0).unwrap().aliases.is_empty());
    }

    #[test]
    fn magic_words_carry_aliases() {
        let c = cfg();
        let bang = c.magic_words().get("!").expect("`!` magic word");
        assert_eq!(bang.aliases, vec!["!".to_string()]);
        assert!(bang.case_sensitive);
    }

    #[test]
    fn interwiki_map_is_populated() {
        let c = cfg();
        assert_eq!(c.interwiki_map().len(), 2);
        assert!(c.interwiki_map().contains_key("commons"));
    }

    #[test]
    fn missing_sections_fall_back_to_defaults() {
        // A wiki that omits everything still yields a usable config.
        let c = WikiSiteConfig::from_siteinfo_json(r#"{"query":{}}"#).unwrap();
        assert_eq!(c.language_code(), "en");
        assert_eq!(c.server_url(), "https://en.wikipedia.org");
        assert!(c.extension_tags().is_empty());
        assert_eq!(c.extension_tag_count(), 0);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(WikiSiteConfig::from_siteinfo_json("not json").is_err());
        assert!(WikiSiteConfig::from_siteinfo_json("").is_err());
    }

    #[test]
    fn tags_are_lowercased_and_deduped() {
        let c = WikiSiteConfig::from_siteinfo_json(
            r#"{"query":{"extensiontags":["<Ref>","<ref>","<REF>","</ref>","</Ref>"]}}"#,
        )
        .unwrap();
        assert_eq!(c.extension_tags(), &["ref".to_string()]);
    }

    #[test]
    fn tag_parsing_tolerates_self_closing_and_bare_forms() {
        let c = WikiSiteConfig::from_siteinfo_json(
            r#"{"query":{"extensiontags":["<nowiki/>","ref","<>",""]}}"#,
        )
        .unwrap();
        let tags = c.extension_tags();
        assert!(tags.contains(&"nowiki".to_string()), "{tags:?}");
        assert!(tags.contains(&"ref".to_string()), "{tags:?}");
        assert!(!tags.iter().any(|t| t.is_empty()), "{tags:?}");
    }
}
