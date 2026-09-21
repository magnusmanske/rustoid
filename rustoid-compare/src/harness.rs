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
use crate::siteconfig::WikiSiteConfig;
use crate::wire::WikiClient;

/// How one page's comparison turned out.
///
/// Not `Eq`: [`Outcome::Stalled`] carries a wall-clock duration, which is a
/// measurement rather than part of the outcome's identity. Nothing compares two
/// outcomes for equality anyway — the scoreboard matches on variants.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Byte-identical after normalisation.
    Match,
    /// Both sides produced HTML, but they differ. Carries the first difference.
    Differ { detail: String },
    /// The page could not be compared (missing, or a fetch failed).
    Skipped { reason: String },
    /// rustoid did not finish rendering within the per-page cap.
    ///
    /// Deliberately *not* a failure or a skip. A page that does not terminate is
    /// its own class of bug — an expansion blow-up, a template cycle — and it
    /// says something different about the port than a `Differ` does. Before this
    /// existed, one such page stalled an entire corpus run, which is why the
    /// blow-up took hours to measure: the scoreboard never printed at all.
    Stalled { seconds: f64 },
}

impl Outcome {
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match)
    }

    /// A coarse bucket for the scoreboard histogram, so a run reports *how* the
    /// differences cluster rather than only how many there were.
    ///
    /// Classified from `detail`, which [`compare_html`] fills with the *whole*
    /// differing region plus both sides' surrounding markup — a raw byte offset
    /// says where a page diverged, not why, and a histogram built from offsets
    /// would mostly measure document length.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Skipped { .. } => "skipped",
            Self::Stalled { .. } => "stalled",
            Self::Differ { detail } => classify(detail),
        }
    }
}

/// Bucket a difference by the construct that most likely caused it.
///
/// Order matters: the first marker that matches wins, so the list runs from the
/// most specific construct to the least. Markers are looked for across the whole
/// difference, not just its first bytes.
fn classify(detail: &str) -> &'static str {
    // A difference that *is* only the section wrappers. Checked first because
    // the marker text is otherwise a full sentence of prose.
    if detail.starts_with("sections: ") {
        return "section-wrap";
    }

    // Ordered most-specific first. Each entry is a feature area worth its own
    // number on the scoreboard, because each is a separate body of work.
    const CATEGORIES: &[(&str, &str)] = &[
        ("mw:Extension", "extension"),
        ("mw:Transclusion", "transclusion"),
        ("mw:ExpandedAttrs", "expanded-attrs"),
        ("mw:File", "media"),
        ("<img", "media"),
        ("<table", "table"),
        ("<td", "table"),
        ("<tr", "table"),
        ("<section", "section-wrap"),
        ("data-mw", "data-mw"),
        ("data-parsoid", "data-parsoid"),
        ("mw:Nowiki", "nowiki"),
        ("mw:Entity", "entity"),
        ("mw:WikiLink", "wikilink"),
        ("mw:PageProp", "pageprop"),
        ("<ref", "cite"),
        ("mw:LanguageVariant", "language-variant"),
    ];
    for (marker, name) in CATEGORIES {
        if detail.contains(marker) {
            return name;
        }
    }
    // An `id="mwXY"`/`about="#mwtN"` mismatch with no other marker means the
    // two sides produced structurally similar HTML differing only in generated
    // attribute values, which is a parity problem in its own right.
    if detail.contains("id=\"mw") || detail.contains("about=\"#mwt") {
        return "marker-ids";
    }
    "other"
}

const CACHE_FLUSH_EVERY: usize = 64;

/// Whether `RUSTOID_TRACE_FETCH` is set, checked once per process.
fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RUSTOID_TRACE_FETCH").is_some())
}

/// A `PageInfo` for a title known to exist.
fn existing() -> rustoid_core::traits::PageInfo {
    rustoid_core::traits::PageInfo {
        missing: false,
        known: true,
        redirect: false,
        linkclasses: Vec::new(),
    }
}

/// A cache-backed `DataSource`, so template and module fetches during expansion
/// are persisted too — not just the top-level page.
///
/// `rustoid-core`'s `DataSource` is async, but a filesystem cache is not; the
/// blocking I/O is acceptable here because this is a test harness, and doing it
/// on the async runtime's thread keeps the type simple. The cache also dedupes:
/// a template transcluded a hundred times is read from disk once.
///
/// The manifest is written every [`CACHE_FLUSH_EVERY`] entries rather than on
/// each one: `WikiCache::put` does not persist by itself, because a dense
/// article transcludes hundreds of templates and rewriting the whole manifest
/// per template is quadratic. [`CachedDataSource::flush`] writes the remainder.
pub struct CachedDataSource {
    client: Option<Arc<WikiClient>>,
    /// The **shared** cache handle, not a private one.
    ///
    /// Expansion fetches templates into this. If the source owned its own
    /// handle, its manifest would be a separate in-memory copy from the
    /// harness's, and the two would overwrite each other's `index.json` — which
    /// silently orphaned hundreds of already-downloaded template bodies.
    cache: Arc<std::sync::Mutex<WikiCache>>,
    /// The wiki that holds entities, reached for `mw.wikibase`.
    ///
    /// Entities live on their own wiki (`www.wikidata.org`), so they need their
    /// own client and their own cache directory. Absent when no entity wiki is
    /// configured, which makes every entity lookup miss rather than fail — the
    /// modules then take their no-data branch, exactly as they do on a wiki with
    /// no Wikidata access.
    entities: Option<SiblingWiki>,
    /// When set, a cache miss returns `None` instead of fetching. Template
    /// expansion can trigger many fetches, so this is what keeps an offline run
    /// genuinely offline.
    offline: bool,
    /// Entries stored since the manifest was last written, so the manifest is
    /// not re-serialised on every one of hundreds of template fetches.
    pending: std::sync::atomic::AtomicUsize,
}

/// A second wiki whose content the parse can reach, with its own cache.
///
/// Cached separately because the layout is per-host: an entity's key must not
/// collide with an article title on the article wiki.
pub struct SiblingWiki {
    pub client: Arc<WikiClient>,
    pub cache: Arc<std::sync::Mutex<WikiCache>>,
}

impl CachedDataSource {
    pub fn new(
        client: Option<Arc<WikiClient>>,
        cache: Arc<std::sync::Mutex<WikiCache>>,
        offline: bool,
    ) -> Self {
        Self {
            client,
            cache,
            entities: None,
            offline,
            pending: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Same, with a wiki to read `mw.wikibase` entities from.
    #[must_use]
    pub fn with_entities(mut self, entities: Option<SiblingWiki>) -> Self {
        self.entities = entities;
        self
    }

    /// Read a body from a specific wiki's cache, fetching it when allowed.
    ///
    /// Mirrors [`fetch_with_revision`](Self::fetch_with_revision) but against a
    /// caller-chosen wiki, so entity lookups do not have to pretend to be
    /// articles on the wiki under test.
    async fn fetch_from(
        &self,
        wiki: &SiblingWiki,
        kind: EntryKind,
        key: &str,
    ) -> Result<Option<String>> {
        if trace_enabled() {
            eprintln!("req {kind:?} {key} (sibling)");
        }
        // The guard is scoped to its own statement, and that is load-bearing rather
        // than stylistic. Written as `if let Some(hit) = wiki.cache.lock()?.get(..)?`,
        // the lock guard lives to the end of the `if let` block — so the `await`
        // further down runs while holding a `std::sync::Mutex`, and a second task
        // taking the same lock deadlocks the runtime. That is the stall this
        // function caused: the process sits at 0% CPU because every worker is
        // parked on a mutex nobody will release.
        let cached = {
            let guard = wiki
                .cache
                .lock()
                .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?;
            guard.get(kind, key)?
        };
        if let Some(hit) = cached {
            return Ok(Some(hit.body));
        }
        if self.offline {
            return Ok(None);
        }

        // Entities have their own endpoint; every other kind is a wiki page.
        let body = match kind {
            EntryKind::Entity => wiki.client.entity_json(key).await?,
            _ => {
                let Some(revid) = wiki.client.latest_revid(key).await? else {
                    return Ok(None);
                };
                Some(wiki.client.wikitext_at(revid).await?)
            }
        };
        let Some(body) = body else {
            return Ok(None);
        };
        let meta = EntryMeta {
            kind,
            title: key.to_string(),
            revid: None,
            fetched_at: now_rfc3339(),
        };
        wiki.cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
            .put(kind, key, &body, meta)?;
        Ok(Some(body))
    }

    /// Persist the manifest, covering entries not yet written out.
    pub fn flush(&self) -> Result<()> {
        self.pending.store(0, std::sync::atomic::Ordering::Relaxed);
        self.cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
            .write_index()
    }

    /// Look up a cached body, or fetch it via the client and store it.
    ///
    /// `kind` distinguishes pages/templates/modules, matching `rustoid-core`'s
    /// split between `get_page_content`/`get_template`/`get_module`.
    async fn fetch(&self, kind: EntryKind, key: &str) -> Result<Option<String>> {
        Ok(self
            .fetch_with_revision(kind, key)
            .await?
            .map(|(body, _)| body))
    }

    /// Like [`fetch`](Self::fetch), but also returns the revision the body came
    /// from.
    ///
    /// `<templatestyles>` needs both. The revision is already recorded in the
    /// cache entry — it is what makes a comparison meaningful — so returning it
    /// costs nothing, and it is exact whether the body came from the cache or
    /// from the network.
    async fn fetch_with_revision(
        &self,
        kind: EntryKind,
        key: &str,
    ) -> Result<Option<(String, Option<u64>)>> {
        // Trace every *request*, before the cache is consulted, so an offline run
        // reproduces an online one's request sequence exactly. Tracing only the
        // network path hid the whole sequence offline: the loop this was written
        // to find was invisible precisely when reproducing it was cheap.
        if trace_enabled() {
            eprintln!("req {kind:?} {key}");
            // A backtrace for a chosen title, to find *who* asked for something
            // that cannot be a real template (e.g. a main-namespace title).
            if let Ok(needle) = std::env::var("RUSTOID_TRACE_BT")
                && !needle.is_empty()
                && key.contains(&needle)
            {
                eprintln!(
                    "BACKTRACE for {key}:\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
            }
        }

        // Guard scoped to its own statement: written as an `if let` scrutinee, the
        // temporary lives to the end of the block and the `await`s below would run
        // holding a `std::sync::Mutex`. This is the busiest function in the harness —
        // every template and module fetch goes through it — so a lock held here
        // deadlocks the entire run rather than one lookup.
        let cached = {
            let guard = self
                .cache
                .lock()
                .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?;
            guard.get(kind, key)?
        };
        if let Some(hit) = cached {
            return Ok(Some((hit.body, hit.meta.revid)));
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
        if trace_enabled() {
            eprintln!("fetch {kind:?} {title} -> {} bytes", body.len());
        }
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
        if self
            .pending
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
            >= CACHE_FLUSH_EVERY
        {
            self.flush()?;
        }
        Ok(Some((body, Some(revid))))
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

    /// Content *and* its revision, which `<templatestyles>` needs: the text is
    /// inlined and the revision becomes the `data-mw-deduplicate` key.
    async fn get_page_with_revision(
        &self,
        title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<(String, Option<u64>)>> {
        Ok(self
            .fetch_with_revision(EntryKind::Page, &title.full_text())
            .await
            .unwrap_or(None))
    }

    /// A Wikidata entity's JSON, from the entity wiki rather than this one.
    ///
    /// The entity id is normalised to upper case: modules pass ids straight
    /// through from article text, where `q42` and `Q42` both occur.
    async fn get_entity(&self, id: &str) -> rustoid_core::Result<Option<String>> {
        let Some(wiki) = &self.entities else {
            return Ok(None);
        };
        let key = id.trim().to_ascii_uppercase();
        if key.is_empty() {
            return Ok(None);
        }
        Ok(self
            .fetch_from(wiki, EntryKind::Entity, &key)
            .await
            .unwrap_or(None))
    }

    /// The entity id whose `enwiki` sitelink is `title`.
    ///
    /// The search itself is a request, and its answer is cached under a
    /// `sitelink:` key so an offline re-run does not repeat it. A title with no
    /// entity caches as an empty body, because a page that has no entity today
    /// will not have one later and a corpus asks about many such titles.
    ///
    /// A failure is reported as "no entity", like every other lookup here: it
    /// must not abort the parse, and `mw.wikibase` has a defined answer for an
    /// entity it cannot see.
    async fn get_entity_id_for_page(&self, title: &str) -> rustoid_core::Result<Option<String>> {
        let Some(wiki) = &self.entities else {
            return Ok(None);
        };
        let key = format!("sitelink:{}", title.trim().replace('_', " "));

        let cached = wiki
            .cache
            .lock()
            .ok()
            .and_then(|c| c.get(EntryKind::Entity, &key).ok().flatten());
        if let Some(hit) = cached {
            return Ok(Some(hit.body).filter(|b| !b.is_empty()));
        }
        if self.offline {
            return Ok(None);
        }

        let found = wiki
            .client
            .entity_id_for_title("enwiki", title)
            .await
            .ok()
            .flatten();
        let body = found.clone().unwrap_or_default();
        let meta = EntryMeta {
            kind: EntryKind::Entity,
            title: key.clone(),
            revid: None,
            fetched_at: now_rfc3339(),
        };
        if let Ok(mut guard) = wiki.cache.lock() {
            let _ = guard.put(EntryKind::Entity, &key, &body, meta);
        }
        Ok(found)
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

    /// Existence for link resolution, asked of the wiki directly.
    ///
    /// The trait's default derives existence from `get_page_content`, which for
    /// a live wiki means fetching every linked article: `AddRedLinks` asks about
    /// *all* wikilink titles on the page in one batch, and a single run on
    /// `Israel` pulled 2558 articles that were never going to be compared. One
    /// `prop=info` request per 50 links answers the same question.
    async fn get_page_info(
        &self,
        titles: &[String],
    ) -> rustoid_core::Result<std::collections::HashMap<String, rustoid_core::traits::PageInfo>>
    {
        // `offline` is checked, not just the presence of a client. The client is
        // always built — it is needed for the entity wiki even in a run that must not
        // touch the network — so testing `client.is_none()` here sent `--offline`
        // runs to the network anyway. That is not a small leak: `AddRedLinks` asks
        // about *every* wikilink title on the page, so one offline run of `Zebra`
        // fired thousands of paced requests and appeared to hang.
        let Some(client) = self.client.as_ref().filter(|_| !self.offline) else {
            // Offline: assume everything exists, which marks nothing as a red
            // link. Recording it would be a claim the run cannot support.
            return Ok(titles.iter().map(|t| (t.clone(), existing())).collect());
        };
        Ok(crate::pageinfo::page_info_soft(client, titles).await)
    }

    async fn get_title_protection(
        &self,
        titles: &[String],
    ) -> rustoid_core::Result<
        std::collections::HashMap<String, std::collections::HashMap<String, Vec<String>>>,
    > {
        // Offline, or a wiki that cannot answer: report nothing protected. The
        // default trait method already does that, and calling it explicitly keeps
        // the reason next to the online branch rather than implicit in an absent
        // override.
        let Some(client) = self.client.as_ref().filter(|_| !self.offline) else {
            return Ok(std::collections::HashMap::new());
        };
        Ok(crate::pageinfo::title_protection(client, titles).await)
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

/// Counts of wikitext that survived parsing instead of being expanded.
///
/// This is the most informative single number at the current stage. Parsoid's
/// output never contains literal `{{…}}` in its text (only inside `data-mw`,
/// which sits in an attribute), so a rustoid body that does is one where
/// expansion was skipped rather than merely different. That distinction matters:
/// "the infobox rendered differently" and "the infobox was never rendered" are
/// different bodies of work, and the first difference at byte 1 cannot tell them
/// apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unexpanded {
    /// Literal `{{#invoke:` occurrences (Scribunto, still unwired).
    pub invoke: usize,
    /// Literal `{{` occurrences outside any tag.
    pub braces: usize,
}

impl Unexpanded {
    /// Count unexpanded wikitext in one rendering.
    ///
    /// Only text *outside* `<…>` is examined, because that is where real content
    /// lives: `data-mw`/`data-parsoid` attributes legitimately carry `{{…}}` as
    /// wikitext, and counting those would report every page as unexpanded.
    pub fn count(html: &str) -> Self {
        let mut out = Unexpanded::default();
        let mut in_tag = false;
        let bytes = html.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'<' => in_tag = true,
                b'>' => in_tag = false,
                b'{' if !in_tag && html[i..].starts_with("{{") => {
                    out.braces += 1;
                    if html[i..].starts_with("{{#invoke:") {
                        out.invoke += 1;
                    }
                    // Skip the run so `{{{` is one construct, not two.
                    while i < bytes.len() && bytes[i] == b'{' {
                        i += 1;
                    }
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// Whether any literal template syntax survived.
    pub fn any(&self) -> bool {
        self.braces > 0
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
    /// Literal unexpanded wikitext in each rendering, for diagnosis.
    pub unexpanded_rustoid: Unexpanded,
    pub unexpanded_parsoid: Unexpanded,
    pub parsoid_html: String,
    pub rustoid_html: String,
    pub outcome: Outcome,
}

impl Comparison {
    /// A page that could not be compared. Carries no renderings, so the
    /// unexpanded counts are empty by definition.
    pub fn skipped(title: String, revid: u64, wikitext: String, reason: String) -> Self {
        Self {
            title,
            revid,
            wikitext,
            unexpanded_rustoid: Unexpanded::default(),
            unexpanded_parsoid: Unexpanded::default(),
            parsoid_html: String::new(),
            rustoid_html: String::new(),
            outcome: Outcome::Skipped { reason },
        }
    }
}

/// Run one comparison.
///
/// `config` supplies the site configuration (namespaces, magic words, …);
/// `cache` is the per-wiki persistent store shared by every fetch.
pub async fn compare_page<C: rustoid_core::SiteConfig>(
    client: &WikiClient,
    config: &C,
    cache: &Arc<std::sync::Mutex<WikiCache>>,
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
                //
                // A cache recovered by `reindex` has no recorded revision, so the
                // cached Parsoid HTML's own `Special:Redirect/revision` stamp is
                // the second source. Without it a reindexed cache serves every
                // body and still reports every page as skipped, because the
                // revision — not the content — is what is missing.
                let guard = cache
                    .lock()
                    .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?;
                let from_wikitext = guard
                    .get(EntryKind::Page, &title)?
                    .and_then(|c| c.meta.revid);
                let revid = match from_wikitext {
                    Some(r) => Some(r),
                    None => guard
                        .get(EntryKind::Rendered, &title)?
                        .and_then(|c| c.meta.revid.or_else(|| parsoid_revision(&c.body))),
                };
                drop(guard);
                match revid {
                    Some(r) => r,
                    None => {
                        return Ok(Comparison::skipped(
                            title,
                            0,
                            String::new(),
                            "offline, and no cached revision for this page".to_string(),
                        ));
                    }
                }
            } else {
                match client.latest_revid(&title).await? {
                    Some(r) => r,
                    None => {
                        return Ok(Comparison::skipped(
                            title,
                            0,
                            String::new(),
                            "page does not exist".to_string(),
                        ));
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
            return Ok(Comparison::skipped(
                title,
                revid,
                String::new(),
                format!("offline, and r{revid} wikitext is not cached"),
            ));
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
        // A cache recovered by `reindex` has no recorded revision, but the HTML
        // states its own — so the body can still be matched against the wikitext
        // instead of being reported as absent. This is what makes a reindexed
        // cache usable for an offline run at all.
        Some(c) if c.meta.revid.is_none() && parsoid_revision(&c.body) == Some(revid) => c.body,
        _ if req.offline => {
            return Ok(Comparison::skipped(
                title,
                revid,
                wikitext,
                format!("offline, and r{revid} Parsoid HTML is not cached"),
            ));
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
    let Some(rustoid_html) = rustoid_html else {
        // The render hit the per-page cap. No HTML was produced, so there is
        // nothing to compare and no unexpanded count to take.
        return Ok(Comparison {
            title: title.clone(),
            revid,
            wikitext,
            unexpanded_rustoid: Unexpanded::default(),
            unexpanded_parsoid: Unexpanded::count(&parsoid_html),
            parsoid_html,
            rustoid_html: String::new(),
            outcome: Outcome::Stalled {
                seconds: page_stall_seconds(),
            },
        });
    };

    let outcome = compare_html(&parsoid_html, &rustoid_html);

    Ok(Comparison {
        title,
        revid,
        wikitext,
        unexpanded_rustoid: Unexpanded::count(&rustoid_html),
        unexpanded_parsoid: Unexpanded::count(&parsoid_html),
        parsoid_html,
        rustoid_html,
        outcome,
    })
}

/// Parse `wikitext` with rustoid, using a cache-backed data source so template
/// fetches are persisted too.
///
/// Returns `None` if the render did not finish within [`page_stall_seconds`].
/// The cap exists because an expansion that does not terminate would otherwise
/// stall the whole corpus, and a scoreboard that never prints measures nothing —
/// see [`Outcome::Stalled`].
async fn render_rustoid<C: rustoid_core::SiteConfig>(
    client: &WikiClient,
    config: &C,
    cache: &Arc<std::sync::Mutex<WikiCache>>,
    title: &str,
    wikitext: &str,
    offline: bool,
) -> Result<Option<String>> {
    // The data source shares the harness's cache handle. A second handle would
    // keep its own in-memory manifest, so the two would clobber each other's
    // `index.json` and orphan every template fetched during expansion.
    let source = CachedDataSource::new(Some(Arc::new(client.clone())), Arc::clone(cache), offline);
    let source = with_entity_wiki(source, client, cache)?;
    let parser = rustoid_core::Parser::new(config);
    // `node_ids` is on because the target is what a *wiki serves*: MediaWiki's REST
    // layer page-bundles Parsoid output and assigns each metadata-bearing element an
    // `id="mw…"`. Parsoid's standalone mode emits none, and the fixture suite (which
    // compares against standalone output) therefore leaves this off.
    let options = rustoid_core::ParserOptions {
        node_ids: true,
        strip_data_parsoid: true,
        ..rustoid_core::ParserOptions::for_page(title)
    };
    let html = match tokio::time::timeout(
        std::time::Duration::from_secs_f64(page_stall_seconds()),
        parser.wikitext_to_html_expanded(wikitext, &source, &options),
    )
    .await
    {
        Ok(Ok(html)) => html,
        Ok(Err(e)) => return Err(CompareError::Parse(e.to_string())),
        // Timed out. `timeout` cancels the future at an await point, but the
        // blocking expansion in progress may still be running, so the cache
        // handle is flushed below either way — a partial fetch is still worth
        // keeping, and the next run benefits from it.
        Err(_elapsed) => {
            source.flush()?;
            return Ok(None);
        }
    };
    // Expansion fetched templates into the source's own cache handle; persist any
    // entries that did not reach a periodic flush.
    source.flush()?;
    Ok(Some(html))
}

/// The per-page wall-clock cap, in seconds.
///
/// Generous on purpose: it is a *stall* detector, not a performance budget. A
/// real page renders in well under a second, so anything in this range is a
/// construct that is not terminating rather than one that is merely slow.
///
/// Overridable with `RUSTOID_PAGE_STALL_SECS`, because the right value depends on
/// whether the run is trying to score pages or to diagnose the one that stalls.
fn page_stall_seconds() -> f64 {
    std::env::var("RUSTOID_PAGE_STALL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60.0)
}

/// Attach the entity wiki, so `mw.wikibase` has something to read.
///
/// Entities live on Wikidata, which is a different wiki with a different cache
/// directory, so it gets its own client and cache handle. Both are opened under
/// the same cache root next to the article wiki, which is what makes an offline
/// run replayable: once fetched, entity lookups come from disk like any other.
///
/// Only Wikipedia wikis have a corresponding entity wiki, so another host gets
/// none and `mw.wikibase` reports every entity as missing.
fn with_entity_wiki(
    source: CachedDataSource,
    client: &WikiClient,
    cache: &Arc<std::sync::Mutex<WikiCache>>,
) -> Result<CachedDataSource> {
    let host = &client.wiki().host;
    if !host.ends_with("wikipedia.org") {
        return Ok(source);
    }
    let root = cache
        .lock()
        .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
        .root()
        .to_path_buf();
    let entities_cache = Arc::new(std::sync::Mutex::new(WikiCache::open(&root, ENTITY_WIKI)?));
    let entities_client = Arc::new(WikiClient::new(crate::wire::Wiki::new(ENTITY_WIKI))?);
    Ok(source.with_entities(Some(SiblingWiki {
        client: entities_client,
        cache: entities_cache,
    })))
}

/// The wiki entities are stored on.
const ENTITY_WIKI: &str = "www.wikidata.org";

/// Compare two HTML strings, returning the first difference.
///
/// Both sides are normalised first: Parsoid serves a full document with
/// `<html>`/`<head>`/`<body>`, while rustoid's output here is the body. Leading
/// and trailing whitespace is insignificant for the comparison's purpose.
///
/// Parsoid's section wrappers are then removed from *both* sides before the
/// difference is located. They are not ignored — a page that differs only by
/// them is still a `Differ`, bucketed as `section-wrap` — but they must not be
/// allowed to *hide* everything else. Parsoid emits `<section
/// data-mw-section-id="…">` as the very first thing in the document, so with the
/// wrappers left in place every single page reported its first difference at
/// byte 1, which attributed nothing and made the scoreboard unreadable.
pub fn compare_html(parsoid: &str, rustoid: &str) -> Outcome {
    let a = normalise(parsoid);
    let b = normalise(rustoid);
    if a == b {
        return Outcome::Match;
    }

    let (sa, sb) = (strip_sections(&a), strip_sections(&b));
    if sa == sb {
        return Outcome::Differ {
            detail: SECTION_ONLY.to_string(),
        };
    }
    Outcome::Differ {
        detail: first_difference(&sa, &sb),
    }
}

/// Marker `detail` for a difference that is *only* section wrappers.
const SECTION_ONLY: &str =
    "sections: the renderings agree once Parsoid's mw:section wrappers are removed";

/// Remove `<section …>` / `</section>` wrappers, keeping their contents.
///
/// These are Parsoid's `SectionWrapping` output: one wrapper per
/// heading-delimited chunk, which rustoid does not emit yet. The tags carry
/// `data-mw-section-id` (and sometimes `id`), and SectionWrapping's output is
/// flat, so a plain scan for the tags is sufficient.
fn strip_sections(html: &str) -> String {
    if !html.contains("<section") {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("<section") {
        out.push_str(&rest[..pos]);
        // Skip to the end of the opening tag. A `<section` with no `>` is not a
        // tag; keep it as literal text rather than swallowing the remainder.
        match rest[pos..].find('>') {
            Some(gt) => rest = &rest[pos + gt + 1..],
            None => {
                out.push_str(&rest[pos..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    // Closing tags carry no attributes, so a plain replace is exact.
    out.replace("</section>", "")
}

/// Read the revision a Parsoid HTML document was rendered from, if it says.
///
/// Parsoid stamps the source revision into the document as a
/// `Special:Redirect/revision/<id>` about-attribute on the root element, which is
/// the only place it appears. Extracting it is what lets a reindexed cache —
/// recovered from body files, so with no recorded revision — still be matched
/// against a wikitext revision rather than reported as absent.
///
/// Returns `None` for a document that does not state one, which is not an error:
/// the caller then falls back to whatever the manifest held.
fn parsoid_revision(html: &str) -> Option<u64> {
    const MARKER: &str = "Special:Redirect/revision/";
    let rest = &html[html.find(MARKER)? + MARKER.len()..];
    let digits = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    digits.parse().ok()
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

/// Extract the distinct Scribunto failures from a rendering.
///
/// When `#invoke` runs, the failures stop being "literal `{{#invoke:…}}` in the
/// output" and become Lua runtime errors. Those name the missing API —
/// `attempt to call a nil value (global 'gsub')` says which `mw` function to
/// implement next — so collecting them turns the scoreboard into a work list
/// instead of a count.
///
/// The traceback's *first module frame* is included, because a message like
/// `attempt to index a nil value` is otherwise unattributable: the frame names
/// the module and line. The rest of the trace is dropped, being long enough to
/// distort the byte totals.
pub fn script_errors(html: &str, limit: usize) -> Vec<String> {
    const MARKER: &str = "Script error:";
    let mut out: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find(MARKER) {
        rest = &rest[pos + MARKER.len()..];
        // The message runs to the closing tag of the error element.
        let end = rest.find('<').unwrap_or(rest.len());
        // Strip the wrappers rustoid adds; MediaWiki's own message starts at the
        // interesting part.
        let mut msg = rest[..end].trim();
        for prefix in [
            "lua error: ",
            "execution error: ",
            "runtime error: ",
            "module load error in ",
        ] {
            msg = msg.strip_prefix(prefix).unwrap_or(msg);
        }
        let msg = if msg.chars().count() > 150 {
            format!("{}…", msg.chars().take(149).collect::<String>())
        } else {
            msg.to_string()
        };
        if !out.contains(&msg) {
            out.push(msg);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// Describe the first differing position, with enough surrounding markup for
/// [`classify`] to tell *what* diverged.
///
/// The snippet is deliberately large: a 60-byte window around the first differing
/// byte usually lands several elements before the construct that actually caused
/// the divergence (a `<td>` mismatch, say, is often first visible inside an
/// attribute or a marker), which would make the scoreboard's histogram reflect
/// position rather than cause. Both sides are included, because the *shape* of
/// the divergence — extra markup on one side, a renamed attribute on the other —
/// is what identifies the feature area.
fn first_difference(a: &str, b: &str) -> String {
    const CONTEXT: usize = 400;

    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut i = 0;
    while i < ab.len() && i < bb.len() && ab[i] == bb[i] {
        i += 1;
    }

    // Walk back to a char boundary so the snippet is valid UTF-8.
    let start = floor_char_boundary(a, i.saturating_sub(CONTEXT));
    let end = floor_char_boundary(a, (i + CONTEXT).min(a.len()));
    let bend = floor_char_boundary(b, (i + CONTEXT).min(b.len()));

    let expected = &a[start..end];
    let actual = &b[start.min(b.len())..bend];
    let diverged_at = a.len() == b.len() && i == a.len();
    if diverged_at {
        return format!("length-identical but differing at the end: {expected:?}");
    }
    format!(
        "first difference at byte {i} of {} (parsoid) / {} (rustoid):\n  parsoid: {expected:?}\n  rustoid: {actual:?}",
        a.len(),
        b.len()
    )
}

/// Largest index `<= i` that is a char boundary, without panicking on overshoot.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn now_rfc3339() -> Option<String> {
    // Avoid a chrono dependency here; the harness only needs a rough stamp.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("epoch:{secs}"))
}

/// The cache key under which a wiki's `siteinfo` is stored. One entry per wiki,
/// so it is keyed by a constant rather than a title.
const SITEINFO_KEY: &str = "siteinfo";

/// Load the wiki's `siteinfo`, through the cache.
///
/// `siteinfo` changes rarely but is needed by every page, so it is cached like
/// anything else. `refresh` forces a re-fetch; `offline` turns a miss into an
/// error rather than a silent fallback, because comparing against an invented
/// configuration would produce confidently wrong diffs.
pub async fn load_site_config(
    client: Option<&Arc<WikiClient>>,
    cache: &Arc<std::sync::Mutex<WikiCache>>,
    offline: bool,
    refresh: bool,
) -> Result<WikiSiteConfig> {
    let cached = if refresh {
        None
    } else {
        cache
            .lock()
            .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
            .get(EntryKind::SiteInfo, SITEINFO_KEY)?
            .map(|c| c.body)
    };

    if let Some(body) = cached {
        return WikiSiteConfig::from_siteinfo_json(&body);
    }

    let Some(client) = client else {
        return Err(CompareError::Offline(format!(
            "siteinfo not cached and no client available ({SITEINFO_KEY})"
        )));
    };
    if offline {
        return Err(CompareError::Offline(
            "siteinfo not cached and --offline was requested".to_string(),
        ));
    }

    let body = client.siteinfo().await?;
    let config = WikiSiteConfig::from_siteinfo_json(&body)?;
    cache
        .lock()
        .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?
        .put(
            EntryKind::SiteInfo,
            SITEINFO_KEY,
            &body,
            EntryMeta {
                kind: EntryKind::SiteInfo,
                title: SITEINFO_KEY.to_string(),
                revid: None,
                fetched_at: now_rfc3339(),
            },
        )?;
    Ok(config)
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
    use rustoid_core::traits::SiteConfig;

    use super::*;

    #[test]
    fn identical_html_matches() {
        assert_eq!(compare_html("<p>x</p>", "<p>x</p>"), Outcome::Match);
    }

    /// Parsoid states the source revision in the document, and a reindexed
    /// cache depends on reading it back — a body recovered from disk has no
    /// manifest revision, so this is the only way to match it to a wikitext
    /// revision.
    #[test]
    fn the_parsoid_revision_is_read_from_the_document() {
        let doc = "<!DOCTYPE html>\n<html about=\"//en.wikipedia.org/wiki/Special:Redirect/revision/1375105737\">";
        assert_eq!(parsoid_revision(doc), Some(1375105737));
        // The real documents carry a `<link rel="dc:replaces" resource=…>` with a
        // *different* revision first, so the marker must, not the first number.
        let with_replaces = "<html about=\"//en.wikipedia.org/wiki/Special:Redirect/revision/1375105737\"><head><link rel=\"dc:replaces\" resource=\"mwr:revision/1375105498\"/>";
        assert_eq!(parsoid_revision(with_replaces), Some(1375105737));
    }

    #[test]
    fn a_document_without_a_revision_yields_none() {
        assert_eq!(parsoid_revision("<p>no revision here</p>"), None);
        assert_eq!(parsoid_revision("Special:Redirect/revision/"), None);
        // Not a number, so it is not a revision.
        assert_eq!(parsoid_revision("Special:Redirect/revision/abc"), None);
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

    /// Section wrappers must not be allowed to hide a difference in substance.
    ///
    /// Parsoid emits `<section …>` as the first thing in the document, so with
    /// the wrappers left in place every page reported its first difference at
    /// byte 1 — attributing nothing and making the scoreboard unreadable.
    #[test]
    fn section_wrappers_do_not_mask_the_real_difference() {
        let parsoid = "<section data-mw-section-id=\"0\" id=\"mwAQ\"><table><tr><td>a</td></tr></table></section>";
        let rustoid = "<table><tr><td>b</td></tr></table>";
        match compare_html(parsoid, rustoid) {
            Outcome::Differ { detail } => {
                assert_eq!(
                    Outcome::Differ {
                        detail: detail.clone()
                    }
                    .category(),
                    "table",
                    "the table difference should be attributed, not the wrapper: {detail}"
                );
            }
            other => panic!("expected a difference, got {other:?}"),
        }
    }

    /// A page differing *only* by section wrappers is still a failure, but one
    /// with its own bucket — it is one identifiable piece of work, not a
    /// mysterious diff.
    #[test]
    fn a_section_wrapper_only_difference_has_its_own_category() {
        let parsoid = "<section data-mw-section-id=\"0\" id=\"mwAQ\"><p>x</p></section>";
        assert_eq!(compare_html(parsoid, "<p>x</p>").category(), "section-wrap");
    }

    /// Stripping must not disturb an already-identical pair, nor eat content.
    #[test]
    fn stripping_sections_keeps_the_contents() {
        assert_eq!(
            compare_html("<p>a</p><p>b</p>", "<p>a</p><p>b</p>"),
            Outcome::Match
        );
        // A literal `<section` that never closes is left as text, not swallowed.
        let weird =
            "<section data-mw-section-id=\"0\"><p>x</p></section><p>literal <section here</p>";
        assert_eq!(compare_html(weird, weird), Outcome::Match);
    }

    /// The snippet must be wide enough to reach the construct that caused the
    /// diverging byte — here the `<td>` that identifies it as a table problem
    /// sits well over 100 bytes before the first differing byte.
    #[test]
    fn difference_context_reaches_the_causing_construct() {
        let filler = "x".repeat(200);
        let a = format!("<table><tr><td>{filler}A</td></tr></table>");
        let b = format!("<table><tr><td>{filler}B</td></tr></table>");
        match compare_html(&a, &b) {
            Outcome::Differ { detail } => {
                assert_eq!(
                    Outcome::Differ {
                        detail: detail.clone()
                    }
                    .category(),
                    "table",
                    "{detail}"
                );
            }
            other => panic!("expected a difference, got {other:?}"),
        }
    }

    /// A page whose two renderings are the same length but differ at the very
    /// end must still be reported, not silently treated as a match.
    #[test]
    fn a_difference_at_the_very_end_is_detected() {
        let out = compare_html("<p>abc</p>", "<p>abd</p>");
        assert!(matches!(out, Outcome::Differ { .. }), "{out:?}");
    }

    /// One rendering being a strict prefix of the other is a difference, even
    /// though the common-prefix scan runs out of bytes rather than mismatching.
    #[test]
    fn a_prefix_is_a_difference() {
        let out = compare_html("<p>abc</p>", "<p>abcdef</p>");
        assert!(matches!(out, Outcome::Differ { .. }), "{out:?}");
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
        assert_eq!(cat("first difference at byte 3: <table>"), "table");
        assert_eq!(cat("first difference: mw:Transclusion"), "transclusion");
        assert_eq!(cat("first difference: data-parsoid=…"), "data-parsoid");
        assert_eq!(cat("first difference: mw:Extension/ref"), "extension");
        assert_eq!(cat("first difference: <img src"), "media");
        assert_eq!(cat("first difference: about=\"#mwt7\""), "marker-ids");
        assert_eq!(cat("first difference: hello"), "other");
    }

    /// The buckets must be ordered most-specific-first, or a page whose only
    /// real problem is Lua would be reported as an extension difference just
    /// because a `mw:Extension` marker happens to appear inside the snippet.
    #[test]
    fn classification_prefers_the_most_specific_marker() {
        let cat = |d: &str| {
            Outcome::Differ {
                detail: d.to_string(),
            }
            .category()
        };
        // A difference containing both an extension and a transclusion marker is
        // reported as the extension, which is the narrower diagnosis.
        assert_eq!(cat("mw:Extension/ref … mw:Transclusion"), "extension");
        // An `id="mw…"` mismatch alongside a real construct is not a marker-id
        // problem; the construct wins.
        assert_eq!(cat("<table id=\"mwAB\""), "table");
        assert_eq!(cat("mw:ExpandedAttrs about=\"#mwt3\""), "expanded-attrs");
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

        let ds = CachedDataSource::new(None, Arc::new(std::sync::Mutex::new(cache)), true);
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

    /// The siteinfo must come from the cache when present, with no client at all.
    /// This is what makes a `--offline` run reproduce the online one exactly.
    #[tokio::test]
    async fn site_config_is_served_from_cache() {
        let root = std::env::temp_dir().join("rustoid-compare-siteinfo-test");
        let _ = std::fs::remove_dir_all(&root);
        let mut cache = WikiCache::open(&root, "example.invalid").unwrap();
        let body = r#"{"query":{"general":{"lang":"de"},
            "extensiontags":["<ref>","</ref>"],"functionhooks":["invoke"]}}"#;
        cache
            .put(
                EntryKind::SiteInfo,
                "siteinfo",
                body,
                crate::cache::EntryMeta {
                    kind: EntryKind::SiteInfo,
                    title: "siteinfo".to_string(),
                    revid: None,
                    fetched_at: None,
                },
            )
            .unwrap();
        let cache = std::sync::Mutex::new(cache);

        let cfg = load_site_config(None, &Arc::new(cache), true, false)
            .await
            .unwrap();
        assert_eq!(cfg.language_code(), "de");
        assert_eq!(cfg.extension_tags(), &["ref".to_string()]);

        WikiCache::flush_all(&root).unwrap();
    }

    /// A miss while offline is an error, not a silent fallback to an invented
    /// configuration: a wrong config would produce confidently wrong diffs.
    #[tokio::test]
    async fn site_config_miss_offline_is_an_error() {
        let root = std::env::temp_dir().join("rustoid-compare-siteinfo-miss");
        let _ = std::fs::remove_dir_all(&root);
        let cache = Arc::new(std::sync::Mutex::new(
            WikiCache::open(&root, "example.invalid").unwrap(),
        ));

        let err = load_site_config(None, &cache, true, false).await;
        assert!(matches!(err, Err(CompareError::Offline(_))), "{err:?}");

        WikiCache::flush_all(&root).unwrap();
    }

    /// A reindexed cache must survive the whole offline comparison, not merely
    /// be listed.
    ///
    /// This is the regression, end to end: a populated cache whose `index.json`
    /// was lost reported every page as `skipped`, because the manifest is what
    /// `get` consults and revisions live only there. Reindexing restores the
    /// bodies, and the Parsoid HTML's own `Special:Redirect/revision` stamp
    /// restores the revision that `compare_page` matches on.
    ///
    /// Real Parsoid HTML is used rather than a stub, because the revision stamp
    /// is exactly the part a stub would get to invent.
    #[tokio::test]
    async fn a_reindexed_cache_still_compares_offline() {
        let root = std::env::temp_dir().join("rustoid-compare-reindex-offline");
        let _ = std::fs::remove_dir_all(&root);
        let host = "example.invalid";
        let revid = 1375105737u64;
        let wikitext = "Hello {{World}}";
        let parsoid = format!(
            "<!DOCTYPE html>\n<html about=\"//{host}/wiki/Special:Redirect/revision/{revid}\">\n<body><p>Hello <b>World</b></p></body></html>"
        );

        let mut cache = WikiCache::open(&root, host).unwrap();
        for (kind, title, body) in [
            (EntryKind::SiteInfo, "siteinfo", siteinfo_body()),
            (EntryKind::Page, "Test", wikitext),
            (EntryKind::Rendered, "Test", &parsoid),
        ] {
            cache
                .put(
                    kind,
                    title,
                    body,
                    crate::cache::EntryMeta {
                        kind,
                        title: title.to_string(),
                        revid: Some(revid),
                        fetched_at: None,
                    },
                )
                .unwrap();
        }
        cache.write_index().unwrap();
        // The loss this recovers from.
        std::fs::remove_file(cache.dir().join("index.json")).unwrap();

        let cache = Arc::new(std::sync::Mutex::new(WikiCache::open(&root, host).unwrap()));
        let client = WikiClient::new(crate::wire::Wiki::new(host)).unwrap();

        // Before reindexing, the bodies are on disk but invisible — and the
        // siteinfo they include cannot even be loaded.
        assert!(
            load_site_config(None, &cache, true, false).await.is_err(),
            "an un-reindexed cache cannot serve even the site config"
        );
        let req = CompareRequest {
            title: "Test".to_string(),
            revid: None,
            refresh: false,
            offline: true,
        };

        assert_eq!(cache.lock().unwrap().reindex().unwrap(), 3);

        let config = load_site_config(None, &cache, true, false).await.unwrap();

        // Now the wikitext, the Parsoid HTML and its revision all resolve, so
        // the run reaches an actual comparison rather than a skip.
        let after = compare_page(&client, &config, &cache, &req).await.unwrap();
        assert_eq!(
            after.revid, revid,
            "the revision comes from the HTML itself"
        );
        assert_eq!(after.wikitext, wikitext);
        assert_eq!(after.parsoid_html, parsoid);
        assert!(
            !matches!(after.outcome, Outcome::Skipped { .. }),
            "a recovered cache must produce a comparison: {:?}",
            after.outcome
        );

        WikiCache::flush_all(&root).unwrap();
    }

    /// A minimal `siteinfo` body, enough for `WikiSiteConfig`.
    fn siteinfo_body() -> &'static str {
        r#"{"query":{"general":{"lang":"en"},"extensiontags":[],"functionhooks":[]}}"#
    }

    /// A data source must write into the manifest of the handle it was given.
    ///
    /// Regression: the source used to open its own handle, so its in-memory
    /// manifest was a separate copy and the two overwrote each other's
    /// `index.json`. The effect was silent — bodies land on disk either way — but
    /// hundreds of already-downloaded templates were orphaned and re-fetched on
    /// every run, which is what made a corpus run take minutes.
    #[tokio::test]
    async fn expansion_entries_reach_the_shared_manifest() {
        let root = std::env::temp_dir().join("rustoid-compare-shared-manifest");
        let _ = std::fs::remove_dir_all(&root);
        let cache = Arc::new(std::sync::Mutex::new(
            WikiCache::open(&root, "example.invalid").unwrap(),
        ));
        let ds = CachedDataSource::new(None, Arc::clone(&cache), true);

        // Store directly through the source's own path, as expansion would.
        for title in ["Template:A", "Template:B"] {
            let t = rustoid_core::Title::new_main(title);
            assert!(
                rustoid_core::traits::DataSource::get_template(&ds, &t)
                    .await
                    .unwrap()
                    .is_none(),
                "offline with an empty cache should miss"
            );
        }
        // Seed through the *shared* handle and confirm the source reads it back,
        // which is only possible if there is genuinely one handle.
        cache
            .lock()
            .unwrap()
            .put(
                EntryKind::Template,
                "Template:A",
                "body a",
                crate::cache::EntryMeta {
                    kind: EntryKind::Template,
                    title: "Template:A".to_string(),
                    revid: Some(7),
                    fetched_at: None,
                },
            )
            .unwrap();

        let t = rustoid_core::Title::new_main("Template:A");
        let got = rustoid_core::traits::DataSource::get_template(&ds, &t)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("body a"));

        // And the manifest reached by the outer handle contains it, i.e. the two
        // are the same manifest rather than two that clobber each other.
        assert!(
            cache
                .lock()
                .unwrap()
                .index()
                .entries
                .contains_key("tpl:Template:A")
        );

        WikiCache::flush_all(&root).unwrap();
    }
}
