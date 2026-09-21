//! Existence checks for link resolution, batched.
//!
//! `rustoid-core`'s `DataSource::get_page_info` has a default implementation
//! that derives existence from `get_page_content`. That is a poor fit for a
//! live wiki: `AddRedLinks` collects *every* wikilink title on a page and asks
//! about them in one batch, so deriving existence by fetching content means one
//! or two HTTP requests per link.
//!
//! The cost is not theoretical. A single run against `Israel` downloaded 2558
//! articles that are not in the corpus — `13 (number)`, `1896 Summer Olympics`
//! and so on — purely to decide whether each link was red. Each also landed in
//! the page cache under `EntryKind::Page`, so the cache filled with articles
//! that were never going to be compared.
//!
//! MediaWiki's `action=query&prop=info` answers existence for up to 50 titles
//! per request, without sending any wikitext. One request per 50 links, rather
//! than 100.

use std::collections::HashMap;

use rustoid_core::traits::PageInfo;

use crate::error::{CompareError, Result};
use crate::wire::WikiClient;

/// MediaWiki's limit for anonymous clients is 50 titles per query.
const TITLES_PER_QUERY: usize = 50;

/// Ask the wiki whether each title exists.
///
/// Returns a map keyed by the title *as given*, because `AddRedLinks` looks
/// results up by the exact string it asked about. Titles the wiki does not
/// recognise at all are reported as missing rather than omitted, so a caller
/// cannot mistake "unknown" for "exists".
pub async fn page_info(
    client: &WikiClient,
    titles: &[String],
) -> Result<HashMap<String, PageInfo>> {
    let mut out = HashMap::with_capacity(titles.len());
    for chunk in titles.chunks(TITLES_PER_QUERY) {
        let body = client.page_info_json(chunk).await?;
        let parsed: InfoResponse =
            serde_json::from_str(&body).map_err(|e| CompareError::Response {
                url: client.wiki().api_url(),
                message: format!("page info parse: {e}"),
            })?;

        // Start every requested title as missing, then mark the ones the wiki
        // knows about. The API normalises some titles (`Foo_Bar` -> `Foo Bar`),
        // so match on both the requested and the returned spelling.
        let mut asked: HashMap<String, String> = HashMap::new();
        for t in chunk {
            asked.insert(normalise(t), t.clone());
        }

        for info in parsed.query.pages {
            let Some(title) = info.title else { continue };
            let missing = info.missing.unwrap_or(false);
            let key = normalise(&title);
            let entry = PageInfo {
                missing,
                known: !missing,
                redirect: info.redirect.is_some(),
                linkclasses: Vec::new(),
            };
            if let Some(original) = asked.get(&key) {
                out.insert(original.clone(), entry.clone());
            }
            // Also key by the wiki's own spelling, which is what a link target
            // resolves to after normalisation.
            out.insert(title, entry);
        }
    }

    // Anything the API never mentioned does not exist.
    for t in titles {
        out.entry(t.clone()).or_insert(PageInfo {
            missing: true,
            known: false,
            redirect: false,
            linkclasses: Vec::new(),
        });
    }
    Ok(out)
}

/// Protection levels per action, for titles a module may ask about through
/// `mw.title`.
///
/// The response models protection as a *list* of `{type, level, expiry}` per
/// action, and Scribunto exposes only the level as an array's first item. An
/// action the wiki does not list is not protected, and is therefore absent from
/// the returned map rather than present with an empty level — that distinction
/// is what `Module:Effective protection level` reads.
///
/// A failure is reported as "nothing is protected" rather than as an error, for
/// the same reason [`page_info_soft`] exists: an unreachable wiki should degrade
/// into a conservative answer, not abort a parse.
pub async fn title_protection(
    client: &WikiClient,
    titles: &[String],
) -> HashMap<String, HashMap<String, Vec<String>>> {
    let mut out = HashMap::new();
    for chunk in titles.chunks(TITLES_PER_QUERY) {
        let Ok(body) = client.protection_json(chunk).await else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<ProtectionResponse>(&body) else {
            continue;
        };
        for page in parsed.query.pages {
            let Some(title) = page.title else { continue };
            let mut levels = HashMap::new();
            for (action, entries) in page.protection.unwrap_or_default() {
                if let Some(level) = entries.into_iter().next().and_then(|e| e.level) {
                    levels.insert(action, vec![level]);
                }
            }
            out.insert(title, levels);
        }
    }
    out
}

/// Normalise a title for comparison: underscores are spaces, and the first
/// letter is case-insensitive on a default wiki.
fn normalise(title: &str) -> String {
    let mut s = title.replace('_', " ");
    if let Some(first) = s.chars().next() {
        let upper: String = first.to_uppercase().collect();
        s.replace_range(0..first.len_utf8(), &upper);
    }
    s
}

#[derive(serde::Deserialize)]
struct InfoResponse {
    query: InfoQuery,
}

#[derive(serde::Deserialize)]
struct InfoQuery {
    #[serde(default)]
    pages: Vec<PageEntry>,
}

#[derive(serde::Deserialize)]
struct PageEntry {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    missing: Option<bool>,
    /// Present only for redirects; its presence is the signal.
    #[serde(default)]
    redirect: Option<serde_json::Value>,
    /// Keyed by action (`"edit"`, `"move"`), then a list of applied
    /// restrictions. Absent entirely for an unprotected page.
    #[serde(default)]
    protection: Option<HashMap<String, Vec<Protection>>>,
}

#[derive(serde::Deserialize)]
struct Protection {
    #[serde(default)]
    level: Option<String>,
}

#[derive(serde::Deserialize)]
struct ProtectionResponse {
    query: ProtectionQuery,
}

#[derive(serde::Deserialize)]
struct ProtectionQuery {
    #[serde(default)]
    pages: Vec<PageEntry>,
}

/// Existence checks that fail soft, for use from the parser.
///
/// A failure here would abort the whole parse, and red-link marking is cosmetic
/// next to the rest of the document, so an unreachable wiki is reported as "every
/// title exists" (which suppresses spurious red links) rather than as an error.
pub async fn page_info_soft(client: &WikiClient, titles: &[String]) -> HashMap<String, PageInfo> {
    match page_info(client, titles).await {
        Ok(info) => info,
        Err(_) => titles
            .iter()
            .map(|t| {
                (
                    t.clone(),
                    PageInfo {
                        missing: false,
                        known: true,
                        redirect: false,
                        linkclasses: Vec::new(),
                    },
                )
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_treats_underscores_as_spaces() {
        assert_eq!(normalise("Foo_Bar"), "Foo Bar");
        assert_eq!(normalise("foo bar"), "Foo bar");
        // A leading character outside ASCII must not panic on a byte split.
        assert_eq!(normalise("étoile"), "Étoile");
    }

    #[test]
    fn a_missing_page_is_reported_as_missing() {
        let json = r#"{"query":{"pages":[
            {"ns":0,"title":"Exists","pageid":1},
            {"ns":0,"title":"Absent","missing":true}
        ]}}"#;
        let parsed: InfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.query.pages.len(), 2);
        assert_eq!(parsed.query.pages[1].missing, Some(true));
        assert!(parsed.query.pages[0].missing.is_none());
    }

    #[test]
    fn a_redirect_is_detected_by_the_presence_of_the_key() {
        let json = r#"{"query":{"pages":[{"ns":0,"title":"R","pageid":2,"redirect":{}}]}}"#;
        let parsed: InfoResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.query.pages[0].redirect.is_some());
    }
}
