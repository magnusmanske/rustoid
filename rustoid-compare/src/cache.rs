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
//! Keys are host-relative and sanitised, so a title like `Template:Foo/bar`
//! cannot escape the cache directory. A `flush` removes a wiki's directory
//! wholesale (or the whole root), which is what a `--refresh` run wants.
//!
//! Bodies are stored as separate files rather than one blob so that a large wiki
//! cache stays inspectable, diffable, and cheap to append to.

use std::collections::BTreeMap;
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
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Template => "tpl",
            Self::Module => "mod",
            Self::Rendered => "html",
            Self::SiteInfo => "siteinfo",
            Self::Entity => "entity",
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
        let canonical = self.body_path(&key);
        let path = if canonical.exists() {
            canonical
        } else {
            self.dir()
                .join("pages")
                .join(format!("{}.txt", Self::legacy_stem(&key)))
        };
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
        let path = self.body_path(&key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        std::fs::write(&path, body).map_err(|e| io_err(&path, e))?;
        self.index.entries.insert(key, meta);
        Ok(())
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

    /// Where a key's body lives.
    ///
    /// The stem is the key itself, with only `/`, `\` and NUL replaced. Colons
    /// and spaces survive deliberately: `mod:Module:Foo` and `page:Module:Foo`
    /// are different keys and must not collapse into one file, and keeping the
    /// title readable is what makes a populated cache inspectable by hand. The
    /// key is therefore recoverable from the filename, which
    /// [`reindex`](Self::reindex) relies on.
    fn body_path(&self, key: &str) -> PathBuf {
        self.dir()
            .join("pages")
            .join(format!("{}.txt", sanitize_path_separators(key)))
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

/// Escape only what cannot appear in a filename, keeping the rest verbatim.
///
/// A cache key is `<kind>:<title>`, and the title is what makes a cache
/// inspectable — `mod:Module:Hatnote list` should look like itself on disk. Only
/// `/`, `\` and NUL can actually escape a directory or truncate a path, so only
/// those are replaced, which also makes the key recoverable from the filename.
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
/// Two spellings are understood. A key is `<kind>:<title>`, and the title is what
/// makes a cache inspectable — `mod:Module:Hatnote list` should look like itself
/// on disk. Two schemes have been used for that:
///
/// - current: only `/`, `\` and NUL are replaced, so the key reads through
///   almost verbatim;
/// - pre-`sanitize_path_separators`: every `:` became `__`, which is lossy, so a
///   recovered title cannot be reconstructed exactly.
///
/// The old form is still read because a cache outlives the code that wrote it,
/// and a corpus cache is hours of rate-limited fetching — refusing to read one
/// would make recovering it pointless. A `__`-escaped title keeps its embedded
/// colons escaped, and the *recovered* title is what gets recorded, so `get`
/// looks the body up by the same string rather than by a reconstruction that
/// cannot be exact.
///
/// A stem with no recognised kind is rejected rather than assumed, so a stray
/// file in `pages/` cannot become a phantom cache entry.
fn key_from_body_stem(stem: &str) -> Option<(EntryKind, String)> {
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
            let stem = sanitize_path_separators(&WikiCache::key(kind, title));
            let (got_kind, got_title) = key_from_body_stem(&stem).expect(&stem);
            assert_eq!(got_kind, kind);
            assert_eq!(got_title, title);
        }
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
}
