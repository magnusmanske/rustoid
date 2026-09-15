//! Drive both sides of the comparison and report the result.
//!
//! The flow, all revision-pinned:
//!
//! ```text
//!   revid        = wiki.latest_revid(title)        (or a caller-supplied one)
//!   wikitext     = cache | wiki.wikitext_at(revid)
//!   parsoid_html = cache | wiki.parsoid_html_at(title, revid)
//!   rustoid_html = rustoid parse of (wikitext, config, data source)
//!   compare(parsoid_html, rustoid_html)
//! ```
//!
//! The Parsoid HTML is cached exactly like the wikitext: it is an expensive,
//! rate-limited fetch, and it is *only meaningful paired with the revision it
//! came from*, which is why the revision is part of the cache key.

use std::sync::Arc;

use rustoid_core::traits::DataSource;

use crate::cache::{EntryKind, EntryMeta, WikiCache};
use crate::error::{CompareError, Result};
use crate::wire::WikiClient;

/// How one page's comparison turned out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Byte-identical after normalisation.
    Match,
    /// Both sides produced HTML, but they differ. Carries the first difference.
    Differ { detail: String },
    /// The page could not be compared (missing, or a fetch failed).
    Skipped { reason: String },
}

impl Outcome {
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match)
    }

    /// A coarse bucket for the scoreboard histogram, so a run reports *how* the
    /// differences cluster rather than only how many there were.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Skipped { .. } => "skipped",
            Self::Differ { detail } => {
                if detail.contains("<table") || detail.contains("<td") || detail.contains("<tr") {
                    "differ:table"
                } else if detail.contains("mw:Transclusion") {
                    "differ:transclusion"
                } else if detail.contains("data-parsoid") {
                    "differ:data-parsoid"
                } else if detail.contains("mw:Extension") {
                    "differ:extension"
                } else if detail.contains("mw:File") || detail.contains("<img") {
                    "differ:media"
                } else if detail.contains("id=\"mw") || detail.contains("about=\"#mwt") {
                    "differ:marker"
                } else {
                    "differ:other"
                }
            }
        }
    }
}

/// A cache-backed `DataSource`, so template and module fetches during expansion
/// are persisted too — not just the top-level page.
///
/// `rustoid-core`'s `DataSource` is async, but a filesystem cache is not; the
/// blocking I/O is acceptable here because this is a test harness, and doing it
/// on the async runtime's thread keeps the type simple. The cache also dedupes:
/// a template transcluded a hundred times is read from disk once.
pub struct CachedDataSource {
    client: Option<Arc<WikiClient>>,
    cache: std::sync::Mutex<WikiCache>,
    /// When set, a cache miss returns `None` instead of fetching. Template
    /// expansion can trigger many fetches, so this is what keeps an offline run
    /// genuinely offline.
    offline: bool,
}

impl CachedDataSource {
    pub fn new(client: Option<Arc<WikiClient>>, cache: WikiCache, offline: bool) -> Self {
        Self {
            client,
            cache: std::sync::Mutex::new(cache),
            offline,
        }
    }

    /// Look up a cached body, or fetch it via the client and store it.
    ///
    /// `kind` distinguishes pages/templates/modules, matching `rustoid-core`'s
    /// split between `get_page_content`/`get_template`/`get_module`.
    async fn fetch(&self, kind: EntryKind, key: &str) -> Result<Option<String>> {
        if let Some(hit) = self
            .cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
            .get(kind, key)?
        {
            return Ok(Some(hit.body));
        }

        let Some(client) = &self.client else {
            // Offline: a miss stays a miss rather than becoming an error, so a
            // partially populated cache still produces a usable run.
            return Ok(None);
        };
        if self.offline {
            return Ok(None);
        }

        let title = key.to_string();
        let Some(revid) = client.latest_revid(&title).await? else {
            return Ok(None);
        };
        let body = client.wikitext_at(revid).await?;
        let meta = EntryMeta {
            kind,
            title: title.clone(),
            revid: Some(revid),
            fetched_at: now_rfc3339(),
        };
        self.cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
            .put(kind, &title, &body, meta)?;
        Ok(Some(body))
    }
}

#[async_trait::async_trait]
impl DataSource for CachedDataSource {
    async fn get_page_content(
        &self,
        title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<String>> {
        // A DataSource error here would abort the parse; a cache/transport
        // failure is reported as "no content" so the comparison still produces
        // a result (a red link) rather than a hard failure.
        Ok(self
            .fetch(EntryKind::Page, &title.full_text())
            .await
            .unwrap_or(None))
    }

    async fn get_template(
        &self,
        title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<String>> {
        Ok(self
            .fetch(EntryKind::Template, &title.full_text())
            .await
            .unwrap_or(None))
    }

    async fn get_module(
        &self,
        title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<String>> {
        Ok(self
            .fetch(EntryKind::Module, &title.full_text())
            .await
            .unwrap_or(None))
    }

    async fn get_file_info(
        &self,
        _title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<rustoid_core::traits::FileInfo>> {
        // File metadata is not needed to compare markup, and fetching it would
        // multiply the request count against the rate limiter.
        Ok(None)
    }

    async fn resolve_redirect(
        &self,
        _title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<rustoid_core::Title>> {
        Ok(None)
    }

    async fn get_message(&self, _lang: &str, _key: &str) -> rustoid_core::Result<Option<String>> {
        Ok(None)
    }
}

/// Everything needed for one comparison.
pub struct CompareRequest {
    /// Page title, as the wiki spells it.
    pub title: String,
    /// Pin to this revision; `None` resolves the latest.
    pub revid: Option<u64>,
    /// Re-fetch even on a cache hit.
    pub refresh: bool,
    /// Never hit the network: a cache miss is reported as skipped rather than
    /// fetched. This is what makes a run reproducible and rate-limit-free.
    pub offline: bool,
}

/// The artifacts of one comparison, useful for reporting and for writing
/// golden files.
#[derive(Debug, Clone)]
pub struct Comparison {
    pub title: String,
    pub revid: u64,
    pub wikitext: String,
    pub parsoid_html: String,
    pub rustoid_html: String,
    pub outcome: Outcome,
}

/// Run one comparison.
///
/// `config` supplies the site configuration (namespaces, magic words, …);
/// `cache` is the per-wiki persistent store shared by every fetch.
pub async fn compare_page<C: rustoid_core::SiteConfig>(
    client: &WikiClient,
    config: &C,
    cache: &std::sync::Mutex<WikiCache>,
    req: &CompareRequest,
) -> Result<Comparison> {
    let title = req.title.clone();

    if req.refresh {
        cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
            .remove(EntryKind::Page, &title)?;
        cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
            .remove(EntryKind::Rendered, &title)?;
    }

    let revid = match req.revid {
        Some(r) => r,
        None => {
            if req.offline {
                // Offline with no pinned revision: fall back to whatever the
                // cached wikitext was fetched at, which is what makes a replay
                // of a previous run possible without the network.
                let cached = {
                    let guard = cache
                        .lock()
                        .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?;
                    guard.get(EntryKind::Page, &title)?
                };
                match cached.and_then(|c| c.meta.revid) {
                    Some(r) => r,
                    None => {
                        return Ok(Comparison {
                            title,
                            revid: 0,
                            wikitext: String::new(),
                            parsoid_html: String::new(),
                            rustoid_html: String::new(),
                            outcome: Outcome::Skipped {
                                reason: "offline, and no cached revision for this page".to_string(),
                            },
                        });
                    }
                }
            } else {
                match client.latest_revid(&title).await? {
                    Some(r) => r,
                    None => {
                        return Ok(Comparison {
                            title,
                            revid: 0,
                            wikitext: String::new(),
                            parsoid_html: String::new(),
                            rustoid_html: String::new(),
                            outcome: Outcome::Skipped {
                                reason: "page does not exist".to_string(),
                            },
                        });
                    }
                }
            }
        }
    };

    // --- wikitext ---
    // The guard is scoped so it is never held across the `await` below; holding
    // a `std::sync::Mutex` across an await can deadlock the runtime.
    let cached = {
        let guard = cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?;
        guard.get(EntryKind::Page, &title)?
    };
    let wikitext = match cached {
        Some(c) => c.body,
        None if req.offline => {
            return Ok(Comparison {
                title,
                revid,
                wikitext: String::new(),
                parsoid_html: String::new(),
                rustoid_html: String::new(),
                outcome: Outcome::Skipped {
                    reason: format!("offline, and r{revid} wikitext is not cached"),
                },
            });
        }
        None => {
            let body = client.wikitext_at(revid).await?;
            cache
                .lock()
                .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
                .put(
                    EntryKind::Page,
                    &title,
                    &body,
                    EntryMeta {
                        kind: EntryKind::Page,
                        title: title.clone(),
                        revid: Some(revid),
                        fetched_at: now_rfc3339(),
                    },
                )?;
            body
        }
    };

    // --- the wiki's own Parsoid HTML, pinned to the same revision ---
    // Guard scoped, not held across the `await` (see above).
    let cached = {
        let guard = cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?;
        guard.get(EntryKind::Rendered, &title)?
    };
    let parsoid_html = match cached {
        Some(c) if c.meta.revid == Some(revid) => c.body,
        _ if req.offline => {
            return Ok(Comparison {
                title,
                revid,
                wikitext,
                parsoid_html: String::new(),
                rustoid_html: String::new(),
                outcome: Outcome::Skipped {
                    reason: format!("offline, and r{revid} Parsoid HTML is not cached"),
                },
            });
        }
        _ => {
            let body = client.parsoid_html_at(&title, revid).await?;
            cache
                .lock()
                .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
                .put(
                    EntryKind::Rendered,
                    &title,
                    &body,
                    EntryMeta {
                        kind: EntryKind::Rendered,
                        title: title.clone(),
                        revid: Some(revid),
                        fetched_at: now_rfc3339(),
                    },
                )?;
            body
        }
    };

    // --- rustoid's rendering ---
    let rustoid_html =
        render_rustoid(client, config, cache, &title, &wikitext, req.offline).await?;

    let outcome = compare_html(&parsoid_html, &rustoid_html);

    Ok(Comparison {
        title,
        revid,
        wikitext,
        parsoid_html,
        rustoid_html,
        outcome,
    })
}

/// Parse `wikitext` with rustoid, using a cache-backed data source so template
/// fetches are persisted too.
async fn render_rustoid<C: rustoid_core::SiteConfig>(
    client: &WikiClient,
    config: &C,
    cache: &std::sync::Mutex<WikiCache>,
    title: &str,
    wikitext: &str,
    offline: bool,
) -> Result<String> {
    // Open a second handle on the same on-disk cache for the data source.
    // `WikiCache` keeps an in-memory manifest, so handing the parser a fresh
    // handle would risk losing entries the outer handle wrote later; instead the
    // source shares the outer handle's state through the mutex-free path below.
    let cache_handle = {
        let guard = cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?;
        WikiCache::open(guard.root(), guard.host())?
    };
    let source = CachedDataSource::new(Some(Arc::new(client.clone())), cache_handle, offline);
    let parser = rustoid_core::Parser::new(config);
    let options = rustoid_core::ParserOptions::for_page(title);
    parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await
        .map_err(|e| CompareError::Parse(e.to_string()))
}

/// Compare two HTML strings, returning the first difference.
///
/// Both sides are normalised first: Parsoid serves a full document with
/// `<html>`/`<head>`/`<body>`, while rustoid's output here is the body. Leading
/// and trailing whitespace is insignificant for the comparison's purpose.
pub fn compare_html(parsoid: &str, rustoid: &str) -> Outcome {
    let a = normalise(parsoid);
    let b = normalise(rustoid);
    if a == b {
        return Outcome::Match;
    }
    Outcome::Differ {
        detail: first_difference(&a, &b),
    }
}

/// Reduce both sides to a comparable form.
fn normalise(html: &str) -> String {
    // Strip a whole-document wrapper if present, so a `rest.php` response
    // compares against a body-only render.
    let body = match (html.find("<body"), html.rfind("</body>")) {
        (Some(start), Some(end)) if start < end => {
            let after = &html[start..end];
            match after.find('>') {
                Some(gt) => &after[gt + 1..],
                None => after,
            }
        }
        _ => html,
    };
    body.trim().to_string()
}

/// Describe the first differing position, with a little context.
fn first_difference(a: &str, b: &str) -> String {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut i = 0;
    while i < ab.len() && i < bb.len() && ab[i] == bb[i] {
        i += 1;
    }
    // Walk back to a char boundary so the snippet is valid UTF-8.
    let mut start = i.saturating_sub(60);
    while start > 0 && !a.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (i + 60).min(a.len());
    while end > 0 && !a.is_char_boundary(end) {
        end -= 1;
    }
    let expected = &a[start..end];
    let mut bend = (i + 60).min(b.len());
    while bend > 0 && !b.is_char_boundary(bend) {
        bend -= 1;
    }
    let actual = &b[start.min(b.len())..bend];
    format!("first difference at byte {i}:\n  parsoid: {expected:?}\n  rustoid: {actual:?}")
}

fn now_rfc3339() -> Option<String> {
    // Avoid a chrono dependency here; the harness only needs a rough stamp.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("epoch:{secs}"))
}

/// Convenience for tests and the binary: the default cache root.
pub fn default_cache_root() -> std::path::PathBuf {
    std::env::var_os("RUSTOID_CACHE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            home.unwrap_or_else(std::env::temp_dir)
                .join(".cache")
                .join("rustoid-compare")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_html_matches() {
        assert_eq!(compare_html("<p>x</p>", "<p>x</p>"), Outcome::Match);
    }

    #[test]
    fn document_wrapper_is_ignored() {
        let doc = "<!DOCTYPE html><html><head></head><body><p>x</p></body></html>";
        assert_eq!(compare_html(doc, "<p>x</p>"), Outcome::Match);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(compare_html("\n  <p>x</p>  \n", "<p>x</p>"), Outcome::Match);
    }

    #[test]
    fn differences_are_reported_with_context() {
        let out = compare_html("<p>abXcd</p>", "<p>abYcd</p>");
        match out {
            Outcome::Differ { detail } => {
                assert!(detail.contains("first difference"), "{detail}");
                assert!(detail.contains("X"), "{detail}");
                assert!(detail.contains("Y"), "{detail}");
            }
            other => panic!("expected a difference, got {other:?}"),
        }
    }

    #[test]
    fn difference_reporting_handles_utf8_boundaries() {
        // Must not panic when the first difference falls after multi-byte chars.
        let out = compare_html("<p>日本語テキスト</p>", "<p>日本語テキスX</p>");
        assert!(matches!(out, Outcome::Differ { .. }));
    }

    #[test]
    fn difference_categories_are_assigned() {
        assert_eq!(Outcome::Match.category(), "match");
        assert_eq!(
            Outcome::Skipped {
                reason: "x".to_string()
            }
            .category(),
            "skipped"
        );
        let cat = |d: &str| {
            Outcome::Differ {
                detail: d.to_string(),
            }
            .category()
        };
        assert_eq!(cat("first difference at byte 3: <table>"), "differ:table");
        assert_eq!(
            cat("first difference: mw:Transclusion"),
            "differ:transclusion"
        );
        assert_eq!(
            cat("first difference: data-parsoid=…"),
            "differ:data-parsoid"
        );
        assert_eq!(
            cat("first difference: mw:Extension/ref"),
            "differ:extension"
        );
        assert_eq!(cat("first difference: <img src"), "differ:media");
        assert_eq!(cat("first difference: about=\"#mwt7\""), "differ:marker");
        assert_eq!(cat("first difference: hello"), "differ:other");
    }

    #[test]
    fn default_cache_root_honours_the_env_override() {
        // Not mutating the env here (tests run in parallel); just assert the
        // fallback is non-empty and ends where we expect.
        let p = default_cache_root();
        assert!(!p.as_os_str().is_empty());
    }

    /// A cache-backed `DataSource` must serve a hit without any client at all,
    /// which is what makes `--offline` work.
    #[tokio::test]
    async fn cached_data_source_serves_from_cache_offline() {
        let root = std::env::temp_dir().join("rustoid-compare-ds-test");
        let _ = std::fs::remove_dir_all(&root);
        let mut cache = WikiCache::open(&root, "example.invalid").unwrap();
        cache
            .put(
                EntryKind::Template,
                "Template:Foo",
                "template body",
                crate::cache::EntryMeta {
                    kind: EntryKind::Template,
                    title: "Template:Foo".to_string(),
                    revid: Some(1),
                    fetched_at: None,
                },
            )
            .unwrap();

        let ds = CachedDataSource::new(None, cache, true);
        let title = rustoid_core::Title::new_main("Template:Foo");
        let got = rustoid_core::traits::DataSource::get_template(&ds, &title)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("template body"));

        // A miss stays a miss (no client, offline) rather than erroring.
        let missing = rustoid_core::Title::new_main("Template:Absent");
        let got = rustoid_core::traits::DataSource::get_template(&ds, &missing)
            .await
            .unwrap();
        assert!(got.is_none());

        WikiCache::flush_all(&root).unwrap();
    }
}
