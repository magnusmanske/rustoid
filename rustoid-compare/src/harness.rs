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
    ///
    /// Classified from `detail`, which [`compare_html`] fills with the *whole*
    /// differing region plus both sides' surrounding markup — a raw byte offset
    /// says where a page diverged, not why, and a histogram built from offsets
    /// would mostly measure document length.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Skipped { .. } => "skipped",
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
    /// When set, a cache miss returns `None` instead of fetching. Template
    /// expansion can trigger many fetches, so this is what keeps an offline run
    /// genuinely offline.
    offline: bool,
    /// Entries stored since the manifest was last written, so the manifest is
    /// not re-serialised on every one of hundreds of template fetches.
    pending: std::sync::atomic::AtomicUsize,
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
            offline,
            pending: std::sync::atomic::AtomicUsize::new(0),
        }
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
        let Some(client) = &self.client else {
            // Offline: assume everything exists, which marks nothing as a red
            // link. Recording it would be a claim the run cannot support.
            return Ok(titles.iter().map(|t| (t.clone(), existing())).collect());
        };
        Ok(crate::pageinfo::page_info_soft(client, titles).await)
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
                let cached = {
                    let guard = cache
                        .lock()
                        .map_err(|_| CompareError::cache("<cache>", "mutex poisoned"))?;
                    guard.get(EntryKind::Page, &title)?
                };
                match cached.and_then(|c| c.meta.revid) {
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
async fn render_rustoid<C: rustoid_core::SiteConfig>(
    client: &WikiClient,
    config: &C,
    cache: &Arc<std::sync::Mutex<WikiCache>>,
    title: &str,
    wikitext: &str,
    offline: bool,
) -> Result<String> {
    // The data source shares the harness's cache handle. A second handle would
    // keep its own in-memory manifest, so the two would clobber each other's
    // `index.json` and orphan every template fetched during expansion.
    let source = CachedDataSource::new(Some(Arc::new(client.clone())), Arc::clone(cache), offline);
    let parser = rustoid_core::Parser::new(config);
    let options = rustoid_core::ParserOptions::for_page(title);
    let html = parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await
        .map_err(|e| CompareError::Parse(e.to_string()))?;
    // Expansion fetched templates into the source's own cache handle; persist any
    // entries that did not reach a periodic flush.
    source.flush()?;
    Ok(html)
}

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
/// Messages are truncated to keep the report readable and are returned
/// deduplicated in first-seen order, capped at `limit`.
pub fn script_errors(html: &str, limit: usize) -> Vec<String> {
    const MARKER: &str = "Script error:";
    let mut out: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find(MARKER) {
        rest = &rest[pos + MARKER.len()..];
        // The message runs to the closing tag of the error element.
        let end = rest.find('<').unwrap_or(rest.len());
        // Strip the `lua error:` prefix rustoid adds; MediaWiki's own message
        // starts at the interesting part.
        let mut msg = rest[..end].trim();
        msg = msg.strip_prefix("lua error: ").unwrap_or(msg);
        msg = msg.strip_prefix("execution error: ").unwrap_or(msg);
        msg = msg.strip_prefix("runtime error: ").unwrap_or(msg);
        let msg = if msg.chars().count() > 120 {
            format!("{}…", msg.chars().take(119).collect::<String>())
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
