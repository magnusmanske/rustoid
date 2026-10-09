//! Persistent, flushable, per-wiki wiki content cache.
//!
//! Comparing rustoid against a live wiki means repeatedly fetching the same
//! wikitext, templates and modules. Wikimedia rate-limits aggressively (a
//! handful of rapid requests already earns
//! `You are making too many requests to the API`), so every fetch is cached on
//! disk and re-runs are expected to work entirely offline.
//!
//! Layout, one directory per wiki host:
//!
//! ```text
//! <root>/
//!   en.wikipedia.org/
//!     index.json          # key -> entry metadata (revision, time, digest)
//!     pages/<key>.txt     # wikitext bodies, one file per entry
//! ```
//!
//! Keys are host-relative and encoded *injectively* into filenames, so a title
//! like `Template:Foo/bar` cannot escape the cache directory, and two distinct
//! keys can never share one file — not even on a case-insensitive filesystem,
//! where `Template:CS1 config` and `Template:Cs1 config` would otherwise collide.
//! See `escape_body_stem`.
//!
//! Bodies are stored as separate files rather than one blob so that a large wiki
//! cache stays inspectable, diffable, and cheap to append to.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CompareError, Result};

/// Kind of cached resource. Part of the cache key so that a page and a template
/// of the same title do not collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Wikitext of the page under test.
    Page,
    /// Wikitext of a transcluded template (or any page fetched to expand one).
    Template,
    /// Source of a Scribunto module.
    Module,
    /// A rendered artifact fetched from the wiki (e.g. its Parsoid HTML).
    Rendered,
    /// Raw `siteinfo` JSON for the wiki: namespaces, magic words, function
    /// hooks, extension tags, interwiki map.
    SiteInfo,
    /// A Wikidata entity's JSON, for `mw.wikibase`.
    ///
    /// Entities live on their own wiki, so this kind is cached under that wiki's
    /// directory rather than the article wiki's.
    Entity,
    /// A page's protection levels, as JSON.
    ///
    /// Protection is a fact about a page that a module reads through
    /// `title.protectionLevels`, and it cannot be derived from the page body:
    /// an offline run that has the wikitext still needs this answer, and the
    /// answer is not in the wikitext. Cached per title so a populated cache can
    /// serve it offline, exactly as it serves the body.
    Protection,
    /// A page's link-resolution metadata, as JSON: existence, redirect-ness and
    /// link classes (e.g. `mw-disambig`).
    ///
    /// Same reasoning as [`Protection`](Self::Protection) — this is a fact the
    /// body cannot supply. Without it an offline run answers "everything exists,
    /// with no classes", which is why a link to a disambiguation page lost its
    /// `mw-disambig` class and a link to an uncached-but-real page looked red.
    PageInfo,
    /// A file's media metadata, as JSON, at one requested display size.
    ///
    /// The key carries the size (`File:X.png@250`) because the wiki only returns
    /// a thumbnail URL for the width it was asked for, and the rendered `<img>`
    /// uses that returned width. Like [`Protection`](Self::Protection) this is a
    /// fact the body cannot supply: without it offline media is always
    /// `mw-broken-media`.
    FileInfo,
    /// A page's own category member names, as a JSON array, for
    /// `title.categories`.
    ///
    /// Same reasoning as [`Protection`](Self::Protection): a fact the body
    /// cannot supply, needed by an offline run that has the wikitext.
    Categories,
}

impl EntryKind {
    /// Whether this kind is a pinned comparison baseline.
    ///
    /// Only `Rendered` is: it is the Parsoid output the comparison is against,
    /// and replacing it silently orphans the revision the corpus asked for. The
    /// others are either inputs (which may legitimately be refreshed) or
    /// re-fetchable auxiliaries.
    pub fn is_rendered(self) -> bool {
        matches!(self, Self::Rendered)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Template => "tpl",
            Self::Module => "mod",
            Self::Rendered => "html",
            Self::SiteInfo => "siteinfo",
            Self::Entity => "entity",
            Self::Protection => "prot",
            Self::PageInfo => "info",
            Self::FileInfo => "file",
            Self::Categories => "cat",
        }
    }

    /// The inverse of [`as_str`](Self::as_str), for reading a key back.
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "page" => Some(Self::Page),
            "tpl" => Some(Self::Template),
            "mod" => Some(Self::Module),
            "html" => Some(Self::Rendered),
            "siteinfo" => Some(Self::SiteInfo),
            "entity" => Some(Self::Entity),
            "prot" => Some(Self::Protection),
            "info" => Some(Self::PageInfo),
            "file" => Some(Self::FileInfo),
            "cat" => Some(Self::Categories),
            _ => None,
        }
    }
}

/// Metadata recorded alongside a cached body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub kind: EntryKind,
    /// The title (or other identifier) this body was fetched for.
    pub title: String,
    /// Revision id the body was fetched at, when known. Pinning matters: the
    /// comparison is meaningless if the wikitext and the Parsoid HTML come from
    /// different revisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revid: Option<u64>,
    /// Fetch time, RFC 3339. Recorded so a run can report how stale it is, and
    /// so time-dependent output can be replayed rather than silently diverging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
}

/// On-disk manifest for one wiki. Sorted so the file is diff-friendly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WikiIndex {
    pub host: String,
    pub entries: BTreeMap<String, EntryMeta>,
}

/// A per-wiki persistent cache rooted at `<root>/<host>`.
///
/// The manifest is written by **merging** with what is on disk rather than
/// overwriting it. Two `WikiCache` handles — in the same process or, more
/// commonly, in two concurrent `rustoid-compare` runs — each hold their own
/// in-memory copy of the manifest, and whichever writes last would otherwise
/// drop the other's entries. That is not hypothetical: populating the corpus
/// while a comparison ran left 5863 body files on disk against 3877 manifest
/// entries, so ~2000 already-downloaded templates and pages were invisible and
/// had to be fetched again.
///
/// `removed` records keys this handle deleted, because a merge would otherwise
/// resurrect them.
pub struct WikiCache {
    root: PathBuf,
    host: String,
    index: WikiIndex,
    /// Keys deleted through this handle, excluded from the merge.
    removed: std::collections::HashSet<String>,
}

impl WikiCache {
    /// Open (or create) the cache for `host` under `root`.
    pub fn open(root: impl Into<PathBuf>, host: &str) -> Result<Self> {
        let root = root.into();
        let dir = root.join(sanitize_component(host));
        std::fs::create_dir_all(dir.join("pages")).map_err(|e| io_err(&dir, e))?;

        let index_path = dir.join("index.json");
        let index = match std::fs::read_to_string(&index_path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| CompareError::Cache {
                path: index_path.clone(),
                message: format!("corrupt index: {e}"),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => WikiIndex {
                host: host.to_string(),
                entries: BTreeMap::new(),
            },
            Err(e) => return Err(io_err(&index_path, e)),
        };

        Ok(Self {
            root,
            host: host.to_string(),
            index,
            removed: std::collections::HashSet::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(sanitize_component(&self.host))
    }

    pub fn len(&self) -> usize {
        self.index.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.entries.is_empty()
    }

    pub fn index(&self) -> &WikiIndex {
        &self.index
    }

    /// Build the cache key for a resource.
    fn key(kind: EntryKind, title: &str) -> String {
        format!("{}:{}", kind.as_str(), title)
    }

    /// Look up a cached body. `Ok(None)` on a miss, so callers can fall through
    /// to the network without treating a miss as an error.
    ///
    /// A body recovered by [`reindex`](Self::reindex) may sit under a filename
    /// that neither the current scheme nor a rebuild from `title` produces: a
    /// body written by the older `__`-escaped scheme is stored that way on disk,
    /// whatever the index says. Both spellings are therefore tried, canonical
    /// first, so a normal entry never pays for the second `stat`.
    pub fn get(&self, kind: EntryKind, title: &str) -> Result<Option<CachedBody>> {
        let key = Self::key(kind, title);
        // The index normally holds the key verbatim. It may also hold the
        // *path-sanitised* spelling, because a body's filename escapes `/` (so
        // `Template:Taxonomy/Equus_(Hippotigris)` is stored, and indexed, as
        // `Template:Taxonomy_Equus_(Hippotigris)`). Asking for the canonical
        // title must still find that entry: a page whose name contains a slash
        // is an ordinary subpage, and treating it as absent makes an offline
        // run report templates as missing that are sitting on disk.
        let entry = self
            .index
            .entries
            .get(&key)
            .or_else(|| self.index.entries.get(&sanitize_path_separators(&key)));
        let Some(meta) = entry else {
            return Ok(None);
        };
        let path = self.locate_body(&key);
        let body = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Index says present, body is gone: treat as a miss rather than a
                // hard error — a partially flushed cache should self-heal.
                return Ok(None);
            }
            Err(e) => return Err(io_err(&path, e)),
        };
        Ok(Some(CachedBody {
            body,
            meta: meta.clone(),
        }))
    }

    /// Store a body.
    ///
    /// The entry is visible to [`get`](Self::get) immediately, but the manifest
    /// is **not** written here. A dense article transcludes hundreds of
    /// templates, and re-serialising the whole manifest on each one is quadratic
    /// — it dominated a corpus run. Callers persist via
    /// [`write_index`](Self::write_index), which the harness does periodically
    /// during expansion and once at the end.
    ///
    /// Crash-safety is unchanged: a body is written before the manifest that
    /// references it, so a crash leaves an unreferenced body (harmless, ignored
    /// on read) rather than a dangling entry.
    pub fn put(&mut self, kind: EntryKind, title: &str, body: &str, meta: EntryMeta) -> Result<()> {
        let key = Self::key(kind, title);
        self.put_at_key(&key, body, meta)
    }

    /// Store a body under an already-built key, refusing to overwrite a pinned
    /// comparison baseline (see [`would_orphan_baseline`](Self::would_orphan_baseline)).
    ///
    /// The refusal is a silent skip rather than an error: the caller is a fetch
    /// path mid-parse, and aborting a page because a *replacement* was declined
    /// would be a worse outcome than serving the pinned body it already had. The
    /// skip is reported to stderr, because a corpus run that re-pins nothing looks
    /// identical to one that fetched nothing.
    fn put_at_key(&mut self, key: &str, body: &str, meta: EntryMeta) -> Result<()> {
        if self.would_orphan_key(key, &meta) {
            eprintln!(
                "cache: keeping pinned {} (rev {:?}); refused to overwrite with rev {:?}",
                key,
                self.index.entries.get(key).and_then(|m| m.revid),
                meta.revid
            );
            return Ok(());
        }
        let path = self.body_path(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        std::fs::write(&path, body).map_err(|e| io_err(&path, e))?;
        self.index.entries.insert(key.to_string(), meta);
        Ok(())
    }

    /// Whether replacing this entry would orphan a *pinned* comparison baseline.
    ///
    /// The rendered (Parsoid) body is the thing the whole comparison is against,
    /// and the corpus pins it to a specific revision. Overwriting it with a newer
    /// revision is not a refresh — the pinned revision is then gone from the cache
    /// and the page reports as skipped, because the pinned HTML is what the run
    /// asked for and what it can no longer find. A full online corpus run does
    /// this to every page it touches, silently.
    ///
    /// So a `Rendered` entry is only replaced when the incoming body is for the
    /// *same* revision. Wikitext and auxiliary pages are not guarded: they are
    /// re-fetchable and carry no pinned meaning of their own.
    ///
    /// A refresh that *intends* to re-pin goes through
    /// [`remove`](Self::remove) first, which is what `--refresh` does.
    pub fn would_orphan_baseline(&self, kind: EntryKind, title: &str, meta: &EntryMeta) -> bool {
        self.would_orphan_key(&Self::key(kind, title), meta)
    }

    /// [`would_orphan_baseline`](Self::would_orphan_baseline) on a key that is
    /// already built, so the guard can also sit *inside* the writer rather than
    /// relying on every caller to consult it.
    ///
    /// An incoming body for a *replaced* wiki page says nothing about which
    /// revision it is — [`EntryKind::Rendered`](EntryKind::Rendered) bodies are not
    /// wikitext and carry no revision in the request — so the comparison is
    /// against the revision the existing entry is pinned to. Only that revision's
    /// disappearance is the damage being prevented, whatever the incoming body is
    /// for.
    fn would_orphan_key(&self, key: &str, meta: &EntryMeta) -> bool {
        if !meta.kind.is_rendered() {
            return false;
        }
        match self.index.entries.get(key) {
            // Pinned to a revision the incoming body does not claim to be: a
            // replacement orphans the pinned baseline.
            Some(existing) => match existing.revid {
                Some(have) => meta.revid != Some(have),
                // A reindexed entry has no recorded revision; the body states its
                // own, and the harness can match on that, so it is not treated as
                // pinned.
                None => false,
            },
            None => false,
        }
    }

    /// Persist the manifest, merged with whatever is already on disk.
    ///
    /// Merging (rather than overwriting) is what keeps a second concurrent
    /// writer from silently dropping this handle's entries, and vice versa. An
    /// entry already on disk wins only if this handle has nothing to say about
    /// it, so this handle's own fetches always take precedence.
    pub fn write_index(&self) -> Result<()> {
        let dir = self.dir();
        let path = dir.join("index.json");

        let mut entries = self.index.entries.clone();
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(on_disk) = serde_json::from_str::<WikiIndex>(&text)
        {
            for (key, meta) in on_disk.entries {
                if self.removed.contains(&key) {
                    continue;
                }
                entries.entry(key).or_insert(meta);
            }
        }

        let merged = WikiIndex {
            host: self.index.host.clone(),
            entries,
        };
        let json = serde_json::to_string_pretty(&merged)
            .map_err(|e| CompareError::cache(&path, format!("serialise: {e}")))?;
        // Write-then-rename so a crash cannot leave a truncated manifest.
        let tmp = dir.join("index.json.tmp");
        std::fs::write(&tmp, json).map_err(|e| io_err(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
    }

    /// Where a key's body lives under the current scheme
    /// ([`escape_body_stem`]).
    fn body_path(&self, key: &str) -> PathBuf {
        self.dir()
            .join("pages")
            .join(format!("{}.txt", escape_body_stem(key)))
    }

    /// Find a key's body file, whichever filename scheme wrote it.
    ///
    /// The current, injective scheme ([`escape_body_stem`]) is tried first. Two
    /// older schemes are still read, because a corpus cache is hours of
    /// rate-limited fetching and rewriting it just to move files is not worth it:
    /// [`sanitize_path_separators`] (which collapsed `/` and `\` to `_`) and
    /// [`legacy_stem`](Self::legacy_stem) (which escaped every `:` as `__`). A
    /// body written by either is used as-is. When nothing is on disk the canonical
    /// path is returned, so a read reports `NotFound` and the caller sees a miss.
    fn locate_body(&self, key: &str) -> PathBuf {
        let pages = self.dir().join("pages");
        for stem in [
            escape_body_stem(key),
            sanitize_path_separators(key),
            Self::legacy_stem(key),
        ] {
            let path = pages.join(format!("{stem}.txt"));
            if path.exists() {
                return path;
            }
        }
        self.body_path(key)
    }

    /// The filename stem the older cache layout produced for a key.
    ///
    /// Every `:` used to become `__`. Bodies written then are still on disk, and
    /// a corpus cache is hours of rate-limited fetching, so they are worth
    /// reading rather than re-fetching. See [`key_from_body_stem`].
    fn legacy_stem(key: &str) -> String {
        key.replace(':', "__")
    }

    /// Remove one entry (body + metadata).
    ///
    /// The key is remembered as removed so that a later [`write_index`](Self::write_index)
    /// merge does not resurrect it from a manifest another handle wrote.
    pub fn remove(&mut self, kind: EntryKind, title: &str) -> Result<()> {
        let key = Self::key(kind, title);
        let path = self.body_path(&key);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path, e)),
        }
        self.index.entries.remove(&key);
        self.removed.insert(key);
        self.write_index()
    }

    /// Delete this wiki's entire cache directory. The `WikiCache` stays usable
    /// afterwards (it reopens empty), which is what `--refresh` needs.
    pub fn flush(&mut self) -> Result<()> {
        let dir = self.dir();
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&dir, e)),
        }
        self.index.entries.clear();
        // The whole directory is gone, so there is nothing left to resurrect and
        // no tombstone worth keeping.
        self.removed.clear();
        std::fs::create_dir_all(dir.join("pages")).map_err(|e| io_err(&dir, e))?;
        Ok(())
    }

    /// Flush every wiki's cache under `root`.
    pub fn flush_all(root: impl AsRef<Path>) -> Result<()> {
        let root = root.as_ref();
        match std::fs::remove_dir_all(root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(root, e)),
        }
        Ok(())
    }

    /// Rebuild the manifest from the body files on disk, recovering a cache whose
    /// `index.json` is missing or was lost mid-run.
    ///
    /// Bodies and the manifest are written separately — a body is a plain file,
    /// the manifest is metadata — so the bodies survive a crash that takes the
    /// manifest with it. Without this, that state is unrecoverable: `get` consults
    /// the manifest first, so a cache holding thousands of bodies and no index
    /// reads as empty, and every offline run reports `skipped`.
    ///
    /// The kind is recovered from the key prefix rather than guessed, so the
    /// result is exactly what the writing run would have produced. What is *not*
    /// recoverable is `revid`: it was only ever stored in the manifest, and
    /// inventing one would be worse than useless, because `compare_page` selects
    /// the cached Parsoid HTML by matching it (`c.meta.revid == Some(revid)`).
    /// A manifest without revisions therefore makes every offline comparison
    /// skip, which is why `EntryMeta::revid` is `Option` and why this is a
    /// recovery tool, not a substitute for the manifest.
    ///
    /// A body this handle has already recorded is left alone: a real fetch is
    /// authoritative, and only the gaps are filled.
    pub fn reindex(&mut self) -> Result<usize> {
        let pages = self.dir().join("pages");
        let dir = match std::fs::read_dir(&pages) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(io_err(&pages, e)),
        };

        let mut added = 0;
        for entry in dir {
            let path = entry.map_err(|e| io_err(&pages, e))?.path();
            // `index.json.tmp` and any stray file are not bodies; only `.txt` is.
            if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some((kind, title)) = key_from_body_stem(stem) else {
                continue;
            };
            let key = Self::key(kind, &title);
            if self.index.entries.contains_key(&key) {
                continue;
            }
            self.removed.remove(&key);
            self.index.entries.insert(
                key,
                EntryMeta {
                    kind,
                    // The title as recovered from the *filename*, which for a
                    // pre-`sanitize_path_separators` body is not the title the
                    // caller will ask for. `get` resolves the body through this
                    // field, so recording it is what makes the entry reachable.
                    title,
                    revid: None,
                    fetched_at: None,
                },
            );
            added += 1;
        }
        self.write_index()?;
        Ok(added)
    }

    /// Drop entries whose shared body file cannot be trusted.
    ///
    /// The older filename schemes were not injective: `/` and `\` both became `_`
    /// ([`sanitize_path_separators`]), and a case-insensitive filesystem folds the
    /// rest. Many keys that collide that way name the *same* page — a redirect
    /// alias, or a title the API normalised — so one body genuinely serves both and
    /// those entries are kept. But when the colliding keys carry *different*
    /// revisions they are different pages, and the file holds only whichever was
    /// written last: one page's wikitext is silently served for another. Nothing on
    /// disk records which, so the only correct recovery is to forget them all and
    /// let a later fetch refill them.
    ///
    /// Returns the number of entries dropped. This is a repair for a cache written
    /// before [`escape_body_stem`], so it is a separate, explicit action rather
    /// than something [`get`](Self::get) does on the fly.
    pub fn drop_colliding_bodies(&mut self) -> Result<usize> {
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        for key in self.index.entries.keys() {
            groups
                .entry(sanitize_path_separators(key).to_lowercase())
                .or_default()
                .push(key.clone());
        }

        let mut dropped = 0;
        for keys in groups.values() {
            if keys.len() < 2 {
                continue;
            }
            // A group whose members already have a file under the current scheme is
            // already separated by it, so any sharing the old scheme had is moot.
            // Guarding on *any* member keeps the repair conservative: it never
            // touches a cache a fetch has already refilled.
            if keys.iter().any(|k| self.body_path(k).exists()) {
                continue;
            }
            let revisions: HashSet<u64> = keys
                .iter()
                .filter_map(|k| self.index.entries.get(k).and_then(|m| m.revid))
                .collect();
            if revisions.len() < 2 {
                continue;
            }
            for key in keys {
                for stem in [
                    escape_body_stem(key),
                    sanitize_path_separators(key),
                    Self::legacy_stem(key),
                ] {
                    let path = self.dir().join("pages").join(format!("{stem}.txt"));
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(io_err(&path, e)),
                    }
                }
                self.index.entries.remove(key);
                self.removed.insert(key.clone());
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.write_index()?;
        }
        Ok(dropped)
    }
}

/// A cached body plus its metadata.
#[derive(Debug, Clone)]
pub struct CachedBody {
    pub body: String,
    pub meta: EntryMeta,
}

/// Make a string safe to use as a single path component.
///
/// Only ASCII alphanumerics, `.`, `-` and `_` survive; everything else becomes
/// `_`. This is deliberately strict: it is the only thing standing between a
/// wiki-supplied title and the filesystem, and a title like `../etc/passwd` or
/// `a/b` must not be able to escape the cache directory. It also collapses runs
/// and truncates, so long module titles cannot exceed filename limits.
///
/// Hosts go through this; cache *keys* go through
/// [`sanitize_path_separators`] instead, because their colons and spaces carry
/// meaning.
fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(96));
    let mut last_underscore = false;
    for c in s.chars() {
        if out.len() >= 96 {
            break;
        }
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
            out.push(c);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    // Avoid a leading dot (hidden files, and `..` has no other way in because
    // `/` can never survive).
    let trimmed = out.trim_matches('.').to_string();
    if trimmed.is_empty() {
        "empty".to_string()
    } else {
        trimmed
    }
}

/// Encode a cache key into a filename stem, injectively.
///
/// The key is what makes a cache inspectable, so only the bytes that would
/// otherwise lose information are escaped:
///
/// - `/`, `\` and NUL cannot appear in a filename at all, and a scheme that
///   merely replaced them with `_` made `Module:Citation/CS1` and
///   `Module:Citation_CS1` the same file;
/// - ASCII uppercase, which a case-insensitive filesystem (macOS by default)
///   folds together with its lowercase form, so `Template:CS1 config` and
///   `Template:Cs1 config` — two different pages — shared one file;
/// - every non-ASCII byte, for the same folding reason, and so that no Unicode
///   normalisation form is assumed.
///
/// `~` is escaped too, because it marks a truncated stem (below). Escaped bytes
/// are written `%XX` (uppercase hex), and `%` is itself escaped, so the escapes
/// are unambiguous and the key can be recovered exactly ([`unescape_body_stem`]).
fn escape_body_stem(key: &str) -> String {
    /// Prefix length, chosen so the whole name (`<prefix>~<16 hex>.txt`) stays
    /// inside the 255-byte filename limit common to ext4 and APFS.
    const MAX_STEM: usize = 230;
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        if b.is_ascii()
            && !b.is_ascii_uppercase()
            && !matches!(b, b'%' | b'/' | b'\\' | b'\0' | b'~')
        {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    if out.len() <= MAX_STEM {
        return out;
    }
    // A title long enough that escaping would overflow the filename limit: keep a
    // readable prefix and disambiguate the tail with a hash of the full key. The
    // `~` marks the stem as truncated, so it is never decoded — such a key cannot
    // be recovered by `reindex`, which is acceptable for a title at the limit.
    out.truncate(MAX_STEM);
    format!("{out}~{:016X}", fnv1a(key.as_bytes()))
}

/// A 64-bit FNV-1a hash, used only to disambiguate over-long filename stems.
///
/// A named, fixed algorithm rather than [`std::hash`], which does not promise the
/// same value across Rust releases — and these hashes end up in filenames that
/// outlive the binary that wrote them.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The inverse of [`escape_body_stem`], but only when the stem decodes cleanly.
///
/// Bodies written by the *older* schemes kept the key nearly verbatim, so a legacy
/// stem may contain a `%` that is not an escape. Decoding and requiring the result
/// to re-encode to the same stem keeps the two apart: only a stem this scheme
/// produced decodes. A `~` marks a truncated stem (see [`escape_body_stem`]),
/// which carries no recoverable key.
fn unescape_body_stem(stem: &str) -> Option<String> {
    if stem.contains('~') {
        return None;
    }
    let bytes = stem.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_digit(*bytes.get(i + 1)?)?;
            let lo = hex_digit(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    (escape_body_stem(&decoded) == stem).then_some(decoded)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Whether a stem is the truncated form produced by [`escape_body_stem`] for an
/// over-long key: an escaped prefix followed by `~` and sixteen uppercase hex
/// digits. Such a stem carries a hash, not the key, so it cannot be decoded.
fn is_truncated_stem(stem: &str) -> bool {
    match stem.rsplit_once('~') {
        Some((_, tail)) => {
            tail.len() == 16
                && tail
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        }
        None => false,
    }
}

/// Escape only what cannot appear in a filename, keeping the rest verbatim.
///
/// This is the *older* scheme, still read by [`locate_body`](WikiCache::locate_body)
/// but no longer written: it kept a key readable but was not injective, because
/// `/`, `\` and NUL all collapsed to `_`. New bodies use [`escape_body_stem`].
fn sanitize_path_separators(key: &str) -> String {
    key.replace(['/', '\\', '\0'], "_").replace("..", "__")
}

fn io_err(path: &Path, e: std::io::Error) -> CompareError {
    CompareError::Cache {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// Recover `(kind, title)` from the stem of a body file, or `None` if the file is
/// not a body.
///
/// Three schemes are understood, tried in order:
///
/// - current ([`escape_body_stem`]): the stem carries `%XX` escapes and decodes
///   exactly;
/// - the intermediate scheme ([`sanitize_path_separators`]): only `/`, `\` and NUL
///   were replaced, so the key reads almost verbatim (and is *not* recoverable
///   exactly when the title contained a separator);
/// - the original scheme ([`legacy_stem`](WikiCache::legacy_stem)): every `:`
///   became `__`, likewise lossy.
///
/// The old forms are still read because a cache outlives the code that wrote it,
/// and a corpus cache is hours of rate-limited fetching — refusing to read one
/// would make recovering it pointless. A stem with no recognised kind is rejected
/// rather than assumed, so a stray file in `pages/` cannot become a phantom entry.
fn key_from_body_stem(stem: &str) -> Option<(EntryKind, String)> {
    // A truncated stem carries a hash, not a key.
    if is_truncated_stem(stem) {
        return None;
    }
    // The current scheme, when the stem carries an escape. A stem without `%` or
    // `~` is spelled the same by every scheme, so it needs no special case here.
    if stem.contains('%')
        && let Some(key) = unescape_body_stem(stem)
        && let Some((kind, title)) = key.split_once(':')
        && let Some(kind) = EntryKind::from_str(kind)
        && !title.is_empty()
    {
        return Some((kind, title.to_string()));
    }

    let (kind, rest) = stem.split_once("__").or_else(|| stem.split_once(':'))?;
    let kind = EntryKind::from_str(kind)?;
    if rest.is_empty() {
        return None;
    }
    let title = if stem.contains("__") {
        rest.replace("__", ":")
    } else {
        rest.to_string()
    };
    Some((kind, title))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(kind: EntryKind, title: &str) -> EntryMeta {
        EntryMeta {
            kind,
            title: title.to_string(),
            revid: Some(42),
            fetched_at: Some("2026-01-01T00:00:00Z".to_string()),
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustoid-cache-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn put_then_get_round_trips() {
        let root = temp_root("roundtrip");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        cache
            .put(
                EntryKind::Page,
                "Main Page",
                "hello",
                meta(EntryKind::Page, "Main Page"),
            )
            .unwrap();

        let got = cache.get(EntryKind::Page, "Main Page").unwrap().unwrap();
        assert_eq!(got.body, "hello");
        assert_eq!(got.meta.revid, Some(42));

        // `put` writes the body but not the manifest: a fresh handle does not
        // see the entry until the manifest is written.
        let before = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert!(
            before.get(EntryKind::Page, "Main Page").unwrap().is_none(),
            "put must not implicitly persist the manifest"
        );

        cache.write_index().unwrap();

        // Now a fresh handle sees the same data, i.e. it really is on disk.
        let reopened = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert_eq!(
            reopened
                .get(EntryKind::Page, "Main Page")
                .unwrap()
                .unwrap()
                .body,
            "hello"
        );
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn kinds_do_not_collide() {
        let root = temp_root("kinds");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        cache
            .put(
                EntryKind::Page,
                "Foo",
                "as-page",
                meta(EntryKind::Page, "Foo"),
            )
            .unwrap();
        cache
            .put(
                EntryKind::Template,
                "Foo",
                "as-template",
                meta(EntryKind::Template, "Foo"),
            )
            .unwrap();
        assert_eq!(
            cache.get(EntryKind::Page, "Foo").unwrap().unwrap().body,
            "as-page"
        );
        assert_eq!(
            cache.get(EntryKind::Template, "Foo").unwrap().unwrap().body,
            "as-template"
        );
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn missing_entry_is_a_miss_not_an_error() {
        let root = temp_root("miss");
        let cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert!(cache.get(EntryKind::Page, "Nope").unwrap().is_none());
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn flush_empties_this_wiki_only() {
        let root = temp_root("flush");
        let mut en = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        en.put(EntryKind::Page, "A", "en", meta(EntryKind::Page, "A"))
            .unwrap();
        let mut de = WikiCache::open(&root, "de.wikipedia.org").unwrap();
        de.put(EntryKind::Page, "A", "de", meta(EntryKind::Page, "A"))
            .unwrap();

        en.flush().unwrap();
        assert_eq!(en.len(), 0);
        assert!(en.get(EntryKind::Page, "A").unwrap().is_none());
        // The other wiki is untouched, and the flushed cache is still usable.
        assert_eq!(de.get(EntryKind::Page, "A").unwrap().unwrap().body, "de");
        en.put(EntryKind::Page, "B", "en2", meta(EntryKind::Page, "B"))
            .unwrap();
        assert_eq!(en.get(EntryKind::Page, "B").unwrap().unwrap().body, "en2");
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn remove_drops_one_entry() {
        let root = temp_root("remove");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        cache
            .put(EntryKind::Page, "A", "a", meta(EntryKind::Page, "A"))
            .unwrap();
        cache
            .put(EntryKind::Page, "B", "b", meta(EntryKind::Page, "B"))
            .unwrap();
        cache.remove(EntryKind::Page, "A").unwrap();
        assert!(cache.get(EntryKind::Page, "A").unwrap().is_none());
        assert_eq!(cache.get(EntryKind::Page, "B").unwrap().unwrap().body, "b");
        // A removal must survive reopening, i.e. the tombstone worked.
        let reopened = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert!(reopened.get(EntryKind::Page, "A").unwrap().is_none());
        assert!(reopened.get(EntryKind::Page, "B").unwrap().is_some());
        WikiCache::flush_all(&root).unwrap();
    }

    /// Two handles writing must not drop each other's entries.
    ///
    /// Regression for a real loss: populating the corpus while a comparison ran
    /// left 5863 body files on disk against 3877 manifest entries, so ~2000
    /// already-downloaded templates and pages were invisible and had to be
    /// fetched again.
    #[test]
    fn concurrent_handles_do_not_drop_each_others_entries() {
        let root = temp_root("merge");
        let mut first = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        let mut second = WikiCache::open(&root, "en.wikipedia.org").unwrap();

        // Each handle fetches a different entry, then writes. `first` writes
        // last, so a plain overwrite would lose `second`'s entry.
        first
            .put(EntryKind::Page, "A", "a", meta(EntryKind::Page, "A"))
            .unwrap();
        second
            .put(EntryKind::Page, "B", "b", meta(EntryKind::Page, "B"))
            .unwrap();
        second.write_index().unwrap();
        first.write_index().unwrap();

        let reopened = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert!(reopened.get(EntryKind::Page, "A").unwrap().is_some());
        assert!(
            reopened.get(EntryKind::Page, "B").unwrap().is_some(),
            "the merge must keep the other handle's entry"
        );
        WikiCache::flush_all(&root).unwrap();
    }

    /// A merge must not undo a removal: the tombstone wins over the stale entry
    /// that is still on disk from the other handle.
    #[test]
    fn a_merge_does_not_resurrect_a_removed_entry() {
        let root = temp_root("merge-remove");
        let mut holder = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        holder
            .put(EntryKind::Page, "A", "a", meta(EntryKind::Page, "A"))
            .unwrap();
        holder.write_index().unwrap();

        let mut other = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        other.remove(EntryKind::Page, "A").unwrap();
        assert!(other.get(EntryKind::Page, "A").unwrap().is_none());

        // `holder` still has the entry in memory and writes again; the tombstone
        // recorded by `other` must win.
        holder.write_index().unwrap();
        let reopened = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert!(
            reopened.get(EntryKind::Page, "A").unwrap().is_none(),
            "a removed entry must not come back"
        );
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn dangling_body_is_a_miss() {
        let root = temp_root("dangling");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        cache
            .put(EntryKind::Page, "A", "a", meta(EntryKind::Page, "A"))
            .unwrap();
        let path = cache.body_path(&WikiCache::key(EntryKind::Page, "A"));
        std::fs::remove_file(path).unwrap();
        assert!(cache.get(EntryKind::Page, "A").unwrap().is_none());
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn titles_cannot_escape_the_cache_directory() {
        // Path traversal, separators, and NUL all have to be neutralised since
        // titles come from the wiki.
        for evil in [
            "../../etc/passwd",
            "a/b/c",
            "..",
            ".",
            "",
            "con:with:colons",
            &"x".repeat(500),
        ] {
            let s = sanitize_component(evil);
            assert!(!s.contains('/'), "{evil:?} -> {s:?}");
            assert!(!s.contains('\\'), "{evil:?} -> {s:?}");
            assert!(!s.contains('\0'), "{evil:?} -> {s:?}");
            assert!(!s.starts_with('.'), "{evil:?} -> {s:?}");
            assert!(s.len() <= 96, "{evil:?} -> {s:?} (len {})", s.len());
            // Joining it cannot climb out of the parent either.
            let joined = Path::new("/cache/pages").join(&s);
            assert!(joined.starts_with("/cache/pages"), "{evil:?} -> {joined:?}");
        }
    }

    #[test]
    fn body_paths_stay_inside_the_wiki_directory() {
        let root = temp_root("inside");
        let cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        let dir = cache.dir();
        for title in ["Main Page", "../../x", "Template:a/b", "a b:c"] {
            let key = WikiCache::key(EntryKind::Template, title);
            let path = cache.body_path(&key);
            assert!(path.starts_with(&dir), "{title:?} -> {path:?}");
        }
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn flush_all_removes_everything() {
        let root = temp_root("flushall");
        WikiCache::open(&root, "en.wikipedia.org")
            .unwrap()
            .put(EntryKind::Page, "A", "a", meta(EntryKind::Page, "A"))
            .unwrap();
        WikiCache::flush_all(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn body_stem_recovers_the_exact_key() {
        // Keys must survive the trip to a filename and back, colons included:
        // `mod:Module:Foo` and `page:Module:Foo` are different entries.
        for (kind, title) in [
            (EntryKind::Module, "Module:Hatnote list"),
            (EntryKind::Page, "Template:Foo"),
            (EntryKind::Template, "Template:Infobox album"),
            (EntryKind::Rendered, "Brat (album)"),
            (EntryKind::SiteInfo, "siteinfo"),
            (EntryKind::Entity, "Q42"),
        ] {
            let stem = escape_body_stem(&WikiCache::key(kind, title));
            let (got_kind, got_title) = key_from_body_stem(&stem).expect(&stem);
            assert_eq!(got_kind, kind);
            assert_eq!(got_title, title);
        }
    }

    /// The regression the injective scheme exists for: two *different* keys must
    /// never land on one file, whether they differ by case (which a
    /// case-insensitive filesystem folds) or by a separator (which the older scheme
    /// turned into `_`).
    #[test]
    fn distinct_keys_get_distinct_body_files() {
        for (a, b) in [
            ("Template:CS1 config", "Template:Cs1 config"),
            ("Module:Citation/CS1", "Module:Citation_CS1"),
            ("Template:A", "template:A"),
        ] {
            let ka = WikiCache::key(EntryKind::Template, a);
            let kb = WikiCache::key(EntryKind::Template, b);
            let (pa, pb) = (escape_body_stem(&ka), escape_body_stem(&kb));
            assert_ne!(pa, pb, "{a:?} and {b:?} share {pa:?}");
            assert_ne!(
                pa.to_lowercase(),
                pb.to_lowercase(),
                "{pa:?} and {pb:?} fold together"
            );
        }
    }

    /// A body written by the intermediate scheme (`/` and `\` to `_`, case kept)
    /// is still found and used, so a populated cache survives the scheme change.
    #[test]
    fn get_reads_a_body_written_by_the_intermediate_scheme() {
        let root = temp_root("intermediate-scheme");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        let pages = cache.dir().join("pages");
        std::fs::create_dir_all(&pages).unwrap();
        // `Template:Foo/bar` was stored, and indexed, as `tpl:Template:Foo_bar`.
        std::fs::write(pages.join("tpl:Template:Foo_bar.txt"), "body").unwrap();
        cache.reindex().unwrap();

        assert_eq!(
            cache
                .get(EntryKind::Template, "Template:Foo/bar")
                .unwrap()
                .unwrap()
                .body,
            "body"
        );
        WikiCache::flush_all(&root).unwrap();
    }

    /// A key whose escaped stem would overflow the filename limit is stored under a
    /// bounded, hashed stem; such a stem cannot be decoded, so `reindex` ignores it.
    #[test]
    fn an_over_long_key_gets_a_bounded_hashed_stem() {
        let key = WikiCache::key(EntryKind::Template, &"Ä".repeat(200));
        let stem = escape_body_stem(&key);
        assert!(stem.len() <= 247, "stem is {} bytes: {stem}", stem.len());
        assert!(is_truncated_stem(&stem), "not marked truncated: {stem}");
        assert!(key_from_body_stem(&stem).is_none());
    }

    /// The repair for a cache written by the non-injective scheme: forget the
    /// entries that share a file across *different* revisions, but keep those that
    /// only share because they name the same page.
    #[test]
    fn drop_colliding_bodies_forgets_only_genuine_collisions() {
        let root = temp_root("drop-colliding");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        // Build the legacy on-disk state: one body file shared by keys the old,
        // non-injective scheme folded together.
        let pages = cache.dir().join("pages");
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(pages.join("tpl:Template:CS1 config.txt"), "body").unwrap();
        std::fs::write(pages.join("mod:Module:List.txt"), "same").unwrap();
        let entry = |kind, title: &str, revid: u64| EntryMeta {
            kind,
            title: title.to_string(),
            revid: Some(revid),
            fetched_at: None,
        };
        // Different revisions that fold to one file: genuinely two pages, one lost.
        cache.index.entries.insert(
            "tpl:Template:CS1 config".into(),
            entry(EntryKind::Template, "Template:CS1 config", 100),
        );
        cache.index.entries.insert(
            "tpl:Template:Cs1 config".into(),
            entry(EntryKind::Template, "Template:Cs1 config", 101),
        );
        // Same revision under both spellings: one body serves both, so it stays.
        cache.index.entries.insert(
            "mod:Module:List".into(),
            entry(EntryKind::Module, "Module:List", 200),
        );
        cache.index.entries.insert(
            "mod:Module:list".into(),
            entry(EntryKind::Module, "Module:list", 200),
        );

        assert_eq!(cache.drop_colliding_bodies().unwrap(), 2);
        assert!(
            cache
                .get(EntryKind::Template, "Template:CS1 config")
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .get(EntryKind::Template, "Template:Cs1 config")
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .get(EntryKind::Module, "Module:List")
                .unwrap()
                .is_some()
        );
        assert!(
            cache
                .get(EntryKind::Module, "Module:list")
                .unwrap()
                .is_some()
        );

        // A reopened cache does not resurrect what the repair dropped.
        let reopened = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert!(
            reopened
                .get(EntryKind::Template, "Template:CS1 config")
                .unwrap()
                .is_none()
        );
        WikiCache::flush_all(&root).unwrap();
    }

    /// The repair leaves a cache that already uses the current scheme alone, so it
    /// is safe to run more than once.
    #[test]
    fn drop_colliding_bodies_is_a_no_op_on_a_current_cache() {
        let root = temp_root("drop-current");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        for (title, rev) in [("Template:CS1 config", 100), ("Template:Cs1 config", 101)] {
            cache
                .put(
                    EntryKind::Template,
                    title,
                    "body",
                    EntryMeta {
                        kind: EntryKind::Template,
                        title: title.to_string(),
                        revid: Some(rev),
                        fetched_at: None,
                    },
                )
                .unwrap();
        }
        assert_eq!(cache.drop_colliding_bodies().unwrap(), 0);
        assert!(
            cache
                .get(EntryKind::Template, "Template:CS1 config")
                .unwrap()
                .is_some()
        );
        WikiCache::flush_all(&root).unwrap();
    }

    /// The earlier filename scheme escaped every colon, and bodies written by it
    /// are still on disk. The stored form is what is recorded, so the entry
    /// stays reachable even though the title cannot be reconstructed exactly.
    #[test]
    fn body_stem_reads_the_older_colon_escaped_scheme() {
        assert_eq!(
            key_from_body_stem("mod__Module__Hatnote_list"),
            Some((EntryKind::Module, "Module:Hatnote_list".to_string()))
        );
        assert_eq!(
            key_from_body_stem("html__Zebra"),
            Some((EntryKind::Rendered, "Zebra".to_string()))
        );
        assert_eq!(
            key_from_body_stem("siteinfo__siteinfo"),
            Some((EntryKind::SiteInfo, "siteinfo".to_string()))
        );
    }

    #[test]
    fn a_file_that_is_not_a_body_is_rejected() {
        assert!(key_from_body_stem("notes").is_none());
        assert!(key_from_body_stem("bogus:Thing").is_none());
        assert!(key_from_body_stem("page:").is_none());
        assert!(key_from_body_stem("page__").is_none());
    }

    /// The regression this was written for: bodies on disk, manifest gone.
    ///
    /// A run that wrote bodies but lost its `index.json` reported the whole
    /// cache as empty, so an offline corpus run skipped every page. The bodies
    /// are the expensive part; recovering them is what `reindex` is for.
    #[test]
    fn reindex_recovers_a_cache_whose_manifest_was_lost() {
        let root = temp_root("reindex-lost");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        for (kind, title, body) in [
            (EntryKind::Page, "Zebra", "wikitext"),
            (EntryKind::Module, "Module:Hatnote list", "lua"),
            (EntryKind::Rendered, "Zebra", "<html>"),
        ] {
            cache.put(kind, title, body, meta(kind, title)).unwrap();
        }
        cache.write_index().unwrap();
        std::fs::remove_file(cache.dir().join("index.json")).unwrap();

        // The bodies outlive the manifest, but are unreachable without it.
        let mut lost = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert_eq!(lost.len(), 0);
        assert!(lost.get(EntryKind::Page, "Zebra").unwrap().is_none());

        assert_eq!(lost.reindex().unwrap(), 3);
        assert_eq!(
            lost.get(EntryKind::Page, "Zebra").unwrap().unwrap().body,
            "wikitext"
        );
        assert_eq!(
            lost.get(EntryKind::Module, "Module:Hatnote list")
                .unwrap()
                .unwrap()
                .body,
            "lua"
        );
        // Kind separation survives: two bodies for one title stay distinct.
        assert_eq!(
            lost.get(EntryKind::Rendered, "Zebra")
                .unwrap()
                .unwrap()
                .body,
            "<html>"
        );

        // The recovery is recorded, so a reopen does not need to repeat it.
        let reopened = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert_eq!(reopened.len(), 3);
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn reindex_keeps_live_entries_and_fills_only_gaps() {
        let root = temp_root("reindex-merge");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        // One entry with real metadata, one orphaned body file.
        cache
            .put(EntryKind::Page, "A", "a", meta(EntryKind::Page, "A"))
            .unwrap();
        std::fs::write(cache.dir().join("pages").join("page:B.txt"), "b").unwrap();

        assert_eq!(cache.reindex().unwrap(), 1);
        // The recorded entry keeps its revision; only the gap is filled.
        assert_eq!(
            cache.get(EntryKind::Page, "A").unwrap().unwrap().meta.revid,
            Some(42)
        );
        assert_eq!(
            cache.get(EntryKind::Page, "B").unwrap().unwrap().meta.revid,
            None
        );
        WikiCache::flush_all(&root).unwrap();
    }

    /// A removed entry must stay removed: its body file is gone, so there is
    /// nothing on disk to resurrect, and the tombstone must not be undone.
    #[test]
    fn reindex_does_not_resurrect_a_removal() {
        let root = temp_root("reindex-removed");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        cache
            .put(EntryKind::Page, "A", "a", meta(EntryKind::Page, "A"))
            .unwrap();
        cache.remove(EntryKind::Page, "A").unwrap();

        assert_eq!(cache.reindex().unwrap(), 0);
        assert!(cache.get(EntryKind::Page, "A").unwrap().is_none());
        WikiCache::flush_all(&root).unwrap();
    }

    #[test]
    fn reindex_on_an_empty_cache_is_a_no_op() {
        let root = temp_root("reindex-empty");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        assert_eq!(cache.reindex().unwrap(), 0);
        assert_eq!(cache.len(), 0);
        WikiCache::flush_all(&root).unwrap();
    }

    /// A pinned baseline can only be replaced by the same revision.
    ///
    /// This is the guard against the failure that cost a whole corpus: an online
    /// run fetched each page's *wikitext* through a path that stored the wiki's
    /// latest revision, and every `html:` entry was rewritten under it. The
    /// pinned revisions were not recoverable and six pages could no longer be
    /// compared at all.
    #[test]
    fn a_pinned_baseline_is_not_overwritten_by_another_revision() {
        let root = temp_root("guard-pinned");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        cache
            .put(
                EntryKind::Rendered,
                "Earth",
                "<html>r1375</html>",
                EntryMeta {
                    kind: EntryKind::Rendered,
                    title: "Earth".to_string(),
                    revid: Some(1375983123),
                    fetched_at: None,
                },
            )
            .unwrap();

        // An incoming body that does not claim the pinned revision — including one
        // with no revision at all, which is what the damage actually looked like.
        for revid in [None, Some(1376261019), Some(1376261019)] {
            assert!(cache.would_orphan_baseline(
                EntryKind::Rendered,
                "Earth",
                &EntryMeta {
                    kind: EntryKind::Rendered,
                    title: "Earth".to_string(),
                    revid,
                    fetched_at: None,
                },
            ));
            cache
                .put(
                    EntryKind::Rendered,
                    "Earth",
                    "<html>r1376261019</html>",
                    EntryMeta {
                        kind: EntryKind::Rendered,
                        title: "Earth".to_string(),
                        revid,
                        fetched_at: None,
                    },
                )
                .unwrap();
        }
        let kept = cache.get(EntryKind::Rendered, "Earth").unwrap().unwrap();
        assert_eq!(kept.body, "<html>r1375</html>");
        assert_eq!(kept.meta.revid, Some(1375983123));
        WikiCache::flush_all(&root).unwrap();
    }

    /// The guard is not a blanket refusal to write: the same revision is a
    /// legitimate re-store, and wikitext is never guarded at all.
    #[test]
    fn the_guard_allows_a_same_revision_restore_and_any_wikitext() {
        let root = temp_root("guard-allows");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        let rendered = |revid| EntryMeta {
            kind: EntryKind::Rendered,
            title: "Earth".to_string(),
            revid,
            fetched_at: None,
        };
        cache
            .put(EntryKind::Rendered, "Earth", "old", rendered(Some(7)))
            .unwrap();
        assert!(!cache.would_orphan_baseline(EntryKind::Rendered, "Earth", &rendered(Some(7))));
        cache
            .put(EntryKind::Rendered, "Earth", "refreshed", rendered(Some(7)))
            .unwrap();
        assert_eq!(
            cache
                .get(EntryKind::Rendered, "Earth")
                .unwrap()
                .unwrap()
                .body,
            "refreshed"
        );

        // Wikitext is re-fetchable and carries no pinned meaning of its own, so a
        // newer revision of a page or template *is* written — otherwise nothing
        // would ever refresh.
        cache
            .put(
                EntryKind::Page,
                "Earth",
                "old wt",
                meta(EntryKind::Page, "Earth"),
            )
            .unwrap();
        let mut newer = meta(EntryKind::Page, "Earth");
        newer.revid = Some(43);
        assert!(!cache.would_orphan_baseline(EntryKind::Page, "Earth", &newer));
        cache
            .put(EntryKind::Page, "Earth", "new wt", newer)
            .unwrap();
        assert_eq!(
            cache.get(EntryKind::Page, "Earth").unwrap().unwrap().body,
            "new wt"
        );
        WikiCache::flush_all(&root).unwrap();
    }

    /// An entry a `--reindex` recovered has no recorded revision, so it is not a
    /// pin: refusing to write over it would make a reindexed cache unwritable.
    #[test]
    fn a_reindexed_entry_is_not_treated_as_pinned() {
        let root = temp_root("guard-reindexed");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        cache
            .put(
                EntryKind::Rendered,
                "Earth",
                "<html>r1</html>",
                EntryMeta {
                    kind: EntryKind::Rendered,
                    title: "Earth".to_string(),
                    revid: None,
                    fetched_at: None,
                },
            )
            .unwrap();
        let incoming = EntryMeta {
            kind: EntryKind::Rendered,
            title: "Earth".to_string(),
            revid: Some(1),
            fetched_at: None,
        };
        assert!(!cache.would_orphan_baseline(EntryKind::Rendered, "Earth", &incoming));
        cache
            .put(
                EntryKind::Rendered,
                "Earth",
                "<html>r1 again</html>",
                incoming,
            )
            .unwrap();
        assert_eq!(
            cache
                .get(EntryKind::Rendered, "Earth")
                .unwrap()
                .unwrap()
                .body,
            "<html>r1 again</html>"
        );
        WikiCache::flush_all(&root).unwrap();
    }

    /// `--refresh` must still be able to re-pin, or the guard would prevent the
    /// one way to deliberately move a baseline.
    #[test]
    fn removing_an_entry_lets_it_be_re_pinned() {
        let root = temp_root("guard-repin");
        let mut cache = WikiCache::open(&root, "en.wikipedia.org").unwrap();
        let rendered = |revid| EntryMeta {
            kind: EntryKind::Rendered,
            title: "Earth".to_string(),
            revid,
            fetched_at: None,
        };
        cache
            .put(EntryKind::Rendered, "Earth", "old", rendered(Some(7)))
            .unwrap();
        assert!(cache.would_orphan_baseline(EntryKind::Rendered, "Earth", &rendered(Some(8))));

        cache.remove(EntryKind::Rendered, "Earth").unwrap();
        assert!(!cache.would_orphan_baseline(EntryKind::Rendered, "Earth", &rendered(Some(8))));
        cache
            .put(EntryKind::Rendered, "Earth", "new", rendered(Some(8)))
            .unwrap();
        assert_eq!(
            cache
                .get(EntryKind::Rendered, "Earth")
                .unwrap()
                .unwrap()
                .meta
                .revid,
            Some(8)
        );
        WikiCache::flush_all(&root).unwrap();
    }
}
