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
                linkclasses: link_classes(&info.pageprops),
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
/// The response models protection as a *flat array* of `{type, level, expiry}`
/// — the action is the `type` field, not a key — and Scribunto exposes only the
/// level as an array's first item. Reading the flat shape as a map keyed by
/// action yields nothing at all, silently, which is why the wire format is worth
/// stating: `get_title_protection` returning empty looks identical to a page
/// that is genuinely unprotected.
///
/// An action the wiki does not list is not protected, and is therefore absent
/// from the returned map rather than present with an empty level — that
/// distinction is what `Module:Effective protection level` reads.
///
/// The result is keyed by the title *as requested*, not by the wiki's own
/// spelling. The wiki normalises some spellings (underscores to spaces), and a
/// caller looks its answer up by the string it asked with, so keying by the
/// reply would drop it; the same reasoning [`page_info`] documents.
///
/// A failure is reported as "nothing is protected" rather than as an error, for
/// the same reason [`page_info_soft`] exists: an unreachable wiki should degrade
/// into a conservative answer, not abort a parse.
pub async fn title_protection(
    client: &WikiClient,
    titles: &[String],
) -> HashMap<String, rustoid_core::traits::ProtectionEntry> {
    use rustoid_core::traits::ProtectionEntry;

    let mut out = HashMap::new();
    for chunk in titles.chunks(TITLES_PER_QUERY) {
        let Ok(body) = client.protection_json(chunk).await else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<ProtectionResponse>(&body) else {
            continue;
        };
        let mut asked: HashMap<String, String> = HashMap::new();
        for t in chunk {
            asked.insert(normalise(t), t.clone());
        }
        for page in parsed.query.pages {
            let Some(title) = page.title else { continue };
            let mut entry = ProtectionEntry::default();
            for restriction in page.protection.unwrap_or_default() {
                let Some(action) = restriction.kind else {
                    continue;
                };
                let Some(level) = restriction.level else {
                    continue;
                };
                // Both lists come from the same entries and so stay index-
                // aligned, which is what `ProtectionEntry` promises: a level and
                // its expiry describe one restriction.
                entry.levels.entry(action.clone()).or_default().push(level);
                entry
                    .expiries
                    .entry(action)
                    .or_default()
                    .push(to_wiki_expiry(
                        restriction.expiry.as_deref().unwrap_or_default(),
                    ));
            }
            // Key by the request where one is recognisable, so the caller finds
            // its answer; fall back to the wiki's spelling otherwise.
            match asked.get(&normalise(&title)) {
                Some(original) => out.insert(original.clone(), entry.clone()),
                None => out.insert(title.clone(), entry.clone()),
            };
            out.insert(title, entry);
        }
    }
    // Anything the API never mentioned is unprotected, present with an empty
    // entry so a caller cannot mistake "unknown" for "no such title".
    for t in titles {
        out.entry(t.clone()).or_default();
    }
    out
}

/// The link classes a page's properties imply.
///
/// The wiki's `DataAccess` marks a link to a disambiguation page with
/// `mw-disambig`, and that class reaches the output: the served
/// `[[Bicycle (disambiguation)]]` is
/// `<a … class="mw-disambig">`, while a link without it is not. The property
/// comes from the `__DISAMBIG__` magic word and is *not* derivable from the
/// target's body, which is why it is requested here rather than guessed.
fn link_classes(pageprops: &Option<serde_json::Value>) -> Vec<String> {
    let Some(props) = pageprops.as_ref().and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    let mut classes = Vec::new();
    if props.contains_key("disambiguation") {
        classes.push("mw-disambig".to_string());
    }
    classes
}

/// Convert a protection expiry to the form MediaWiki exposes to the parser.
///
/// `action=query&prop=info&inprop=protection` reports the expiry as ISO 8601
/// (`2026-11-28T18:01:22Z`), but `{{PROTECTIONEXPIRY:…}}` and Scribunto's
/// `title.protectionLevels` hand the script the raw DB form
/// (`20261128180122`), and the literal `"infinity"` for no expiry.
/// `Module:Effective protection expiry` matches exactly fourteen digits and
/// raises `malformed expiry timestamp` otherwise, so the ISO spelling does not
/// merely look different — it breaks the module. Values already in either
/// accepted form pass through unchanged.
fn to_wiki_expiry(expiry: &str) -> String {
    if expiry.is_empty() || expiry == "infinity" {
        return expiry.to_string();
    }
    if expiry.len() == 14 && expiry.bytes().all(|b| b.is_ascii_digit()) {
        return expiry.to_string();
    }
    // `YYYY-MM-DDTHH:MM:SSZ` (or an offset, which MediaWiki does not emit).
    let Some((date, rest)) = expiry.split_once('T') else {
        return expiry.to_string();
    };
    let date: String = date.chars().filter(char::is_ascii_digit).collect();
    let time: String = rest
        .split(['Z', '+'])
        .next()
        .unwrap_or(rest)
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let combined = format!("{date}{time}");
    if combined.len() == 14 {
        combined
    } else {
        expiry.to_string()
    }
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
    /// `prop=pageprops` output, e.g. `{"disambiguation": ""}`. An object of
    /// property name to value (usually the empty string).
    #[serde(default)]
    pageprops: Option<serde_json::Value>,
    /// Present as a flat list for an unprotected page is *absent*; for a
    /// protected one it is `[{type, level, expiry}, …]`, where the action is the
    /// `type` field rather than a map key.
    #[serde(default)]
    protection: Option<Vec<Protection>>,
}

#[derive(serde::Deserialize)]
struct Protection {
    /// The action: `"edit"`, `"move"`, `"create"`, `"upload"`.
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    level: Option<String>,
    /// MediaWiki's 14-digit `YYYYMMDDHHMMSS`, or the literal `"infinity"`.
    #[serde(default)]
    expiry: Option<String>,
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

    /// `Module:Effective protection expiry` matches exactly fourteen digits, so
    /// the API's ISO spelling of a finite expiry must be converted to
    /// MediaWiki's DB form. `"infinity"` and an already-raw value must survive
    /// unchanged.
    #[test]
    fn protection_expiry_is_the_raw_db_form_the_module_matches() {
        assert_eq!(to_wiki_expiry("2026-11-28T18:01:22Z"), "20261128180122");
        assert_eq!(to_wiki_expiry("infinity"), "infinity");
        assert_eq!(to_wiki_expiry("20261128180122"), "20261128180122");
        assert_eq!(to_wiki_expiry(""), "");
        // Anything unrecognisable is passed through rather than mangled.
        assert_eq!(to_wiki_expiry("whenever"), "whenever");
    }

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

    /// `disambiguation` is a page *property*, sent only when requested with
    /// `prop=pageprops&ppprop=disambiguation`. A link to such a page carries
    /// `mw-disambig`; reading anything else, or nothing, loses the class.
    #[test]
    fn a_disambiguation_page_property_becomes_the_link_class() {
        let json = r#"{"query":{"pages":[
            {"ns":0,"title":"Bicycle (disambiguation)","pageid":1,"pageprops":{"disambiguation":""}},
            {"ns":0,"title":"Bicyclus","pageid":2}
        ]}}"#;
        let parsed: InfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            link_classes(&parsed.query.pages[0].pageprops),
            vec!["mw-disambig".to_string()]
        );
        assert!(link_classes(&parsed.query.pages[1].pageprops).is_empty());
    }

    /// The protection field is a flat array of `{type, level, expiry}`, where
    /// the *action* is `type`. Reading it as a map keyed by action deserialises
    /// to nothing, silently — every page then looks unprotected, which is
    /// indistinguishable from a page that genuinely is. This is the live shape,
    /// copied from `action=query&prop=info&inprop=protection`.
    #[test]
    fn protection_is_read_from_the_flat_array_the_wiki_sends() {
        let json = r#"{"query":{"pages":[{"ns":0,"title":"Canada","pageid":5042916,
            "protection":[
                {"type":"edit","level":"extendedconfirmed","expiry":"infinity"},
                {"type":"move","level":"sysop","expiry":"infinity"}
            ]}]}}"#;
        let parsed: ProtectionResponse = serde_json::from_str(json).unwrap();
        let page = &parsed.query.pages[0];
        let protection = page.protection.as_ref().expect("protection present");
        assert_eq!(protection.len(), 2);
        assert_eq!(protection[0].kind.as_deref(), Some("edit"));
        assert_eq!(protection[0].level.as_deref(), Some("extendedconfirmed"));
        assert_eq!(protection[0].expiry.as_deref(), Some("infinity"));
        assert_eq!(protection[1].kind.as_deref(), Some("move"));
    }

    /// An unprotected page omits the field entirely, which must read as "no
    /// restriction" rather than as a parse failure.
    #[test]
    fn an_unprotected_page_has_no_field_at_all() {
        let json = r#"{"query":{"pages":[{"ns":0,"title":"Plain","pageid":1}]}}"#;
        let parsed: ProtectionResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.query.pages[0].protection.is_none());
    }
}
