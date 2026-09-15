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
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Template => "tpl",
            Self::Module => "mod",
            Self::Rendered => "html",
            Self::SiteInfo => "siteinfo",
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
pub struct WikiCache {
    root: PathBuf,
    host: String,
    index: WikiIndex,
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
    pub fn get(&self, kind: EntryKind, title: &str) -> Result<Option<CachedBody>> {
        let key = Self::key(kind, title);
        let Some(meta) = self.index.entries.get(&key) else {
            return Ok(None);
        };
        let path = self.body_path(&key);
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

    /// Store a body, writing the index afterwards so a crash mid-write leaves a
    /// consistent (if stale) cache rather than a dangling entry.
    pub fn put(&mut self, kind: EntryKind, title: &str, body: &str, meta: EntryMeta) -> Result<()> {
        let key = Self::key(kind, title);
        let path = self.body_path(&key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        std::fs::write(&path, body).map_err(|e| io_err(&path, e))?;
        self.index.entries.insert(key, meta);

        // An index-write failure after a successful body-write is recoverable
        // (the entry is simply absent next run), so it is reported but does not
        // need to unwind the body.
        self.write_index()
    }

    /// Persist the manifest.
    pub fn write_index(&self) -> Result<()> {
        let dir = self.dir();
        let path = dir.join("index.json");
        let json = serde_json::to_string_pretty(&self.index)
            .map_err(|e| CompareError::cache(&path, format!("serialise: {e}")))?;
        // Write-then-rename so a crash cannot leave a truncated manifest.
        let tmp = dir.join("index.json.tmp");
        std::fs::write(&tmp, json).map_err(|e| io_err(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
    }

    fn body_path(&self, key: &str) -> PathBuf {
        self.dir().join("pages").join(format!(
            "{}.txt",
            sanitize_component(&key.replace(':', "__"))
        ))
    }

    /// Remove one entry (body + metadata).
    pub fn remove(&mut self, kind: EntryKind, title: &str) -> Result<()> {
        let key = Self::key(kind, title);
        let path = self.body_path(&key);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path, e)),
        }
        self.index.entries.remove(&key);
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

fn io_err(path: &Path, e: std::io::Error) -> CompareError {
    CompareError::Cache {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
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

        // A fresh handle sees the same data, i.e. it really is on disk.
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
}
