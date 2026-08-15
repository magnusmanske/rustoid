/// Core traits for data access and site configuration.
///
/// These traits abstract over different data sources (MediaWiki API, indexed dump, mock)
/// so the parser core doesn't depend on any specific backend.
use async_trait::async_trait;
use std::collections::HashMap;

use crate::error::Result;
use crate::title::Title;

// ---------------------------------------------------------------------------
// Data source trait
// ---------------------------------------------------------------------------

/// Abstract data source for wiki content.
///
/// Implementations may fetch from a MediaWiki API, a local indexed dump,
/// or an in-memory mock for testing.
#[async_trait]
pub trait DataSource: Send + Sync {
    /// Fetch the raw wikitext for a page by title.
    async fn get_page_content(&self, title: &Title) -> Result<Option<String>>;

    /// Fetch the fully-expanded wikitext of a template page.
    ///
    /// This is used for template transclusion: when the parser encounters
    /// `{{TemplateName}}`, it calls `get_template` to fetch the template source,
    /// then recursively expands it.
    async fn get_template(&self, title: &Title) -> Result<Option<String>>;

    /// Fetch the source code of a Lua (Scribunto) module.
    async fn get_module(&self, title: &Title) -> Result<Option<String>>;

    /// Fetch metadata for a file (image, audio, video, etc.).
    async fn get_file_info(&self, title: &Title) -> Result<Option<FileInfo>>;

    /// Resolve a redirect to its target page.
    /// Returns `None` if the page is not a redirect.
    async fn resolve_redirect(&self, title: &Title) -> Result<Option<Title>>;

    /// Fetch an i18n message from the MediaWiki namespace.
    /// Returns `None` if the message does not exist.
    async fn get_message(&self, lang: &str, key: &str) -> Result<Option<String>>;
}

/// Metadata for a file (image, audio, video).
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// Canonical file title (without namespace prefix).
    pub title: String,
    /// MIME type, e.g. `"image/jpeg"`.
    pub mime_type: String,
    /// File size in bytes.
    pub size: u64,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// URL to the file's description page.
    pub description_url: String,
    /// URL to the raw file itself.
    pub file_url: String,
    /// Thumbnail URLs keyed by width (e.g. `"120"`, `"300"`).
    pub thumb_urls: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Site configuration trait
// ---------------------------------------------------------------------------

/// Static site configuration needed by the parser.
///
/// This covers namespace definitions, interwiki maps, magic word
/// aliases, and other wiki-specific settings.
pub trait SiteConfig: Send + Sync {
    /// All configured namespaces by ID.
    fn namespaces(&self) -> &HashMap<i32, NamespaceInfo>;

    /// Interwiki prefix map (e.g. `"wikipedia"` → `"https://en.wikipedia.org/wiki/$1"`).
    fn interwiki_map(&self) -> &HashMap<String, InterwikiInfo>;

    /// Magic word aliases for this wiki (localized names like `"ANCHORENCODE"` etc.).
    fn magic_words(&self) -> &MagicWordMap;

    /// Recognized extension tag names (e.g. `"ref"`, `"gallery"`, `"poem"`).
    fn extension_tags(&self) -> &[String];

    /// The base URL for constructing full URLs (e.g. `"https://en.wikipedia.org"`).
    fn server_url(&self) -> &str;

    /// The article path template (e.g. `"/wiki/$1"`).
    fn article_path(&self) -> &str;

    /// The wiki's content language code (e.g. `"en"`).
    fn language_code(&self) -> &str;

    /// The wiki's script path (e.g. `"/w"`).
    fn script_path(&self) -> &str {
        "/w"
    }

    /// Resolve a canonical namespace name (e.g. "Media", "File", "Category")
    /// to its namespace ID. Mirrors PHP's `SiteConfig::canonicalNamespaceId`.
    /// Returns `None` if the namespace is not configured.
    fn canonical_namespace_id(&self, canonical: &str) -> Option<i32> {
        self.namespaces()
            .iter()
            .find(|(_, info)| info.canonical == canonical)
            .map(|(id, _)| *id)
    }

    /// Resolve a (canonical or localized) namespace name to its ID.
    /// Mirrors PHP's `SiteConfig::namespaceId`.
    fn namespace_id(&self, name: &str) -> Option<i32> {
        let normalized = crate::util::normalize_namespace_name(name.trim());
        for (&id, info) in self.namespaces() {
            let canon_lower = crate::util::normalize_namespace_name(&info.canonical);
            if canon_lower == normalized {
                return Some(id);
            }
            if info
                .aliases
                .iter()
                .any(|a| crate::util::normalize_namespace_name(a) == normalized)
            {
                return Some(id);
            }
        }
        None
    }

    /// The URL for uploading a file (used by media/file links). Mirrors PHP's
    /// `SiteConfig::getUploadUrl` (a sensible default, overridable).
    fn get_upload_url(&self, _title: &str) -> String {
        format!("{}/index.php?title=Special:Upload", self.server_url())
    }
}

/// Information about a namespace.
#[derive(Debug, Clone)]
pub struct NamespaceInfo {
    /// Canonical name (e.g. `"Template"`, `"Category"`).
    pub canonical: String,
    /// Localized name(s).
    pub aliases: Vec<String>,
    /// Whether this namespace treats its content as case-sensitive page names.
    pub case_sensitive: bool,
    /// The content model used for pages in this namespace (e.g. `"wikitext"`, `"Scribunto"`).
    pub default_content_model: String,
}

/// Interwiki prefix configuration.
#[derive(Debug, Clone)]
pub struct InterwikiInfo {
    /// The URL template (e.g. `"https://en.wikipedia.org/wiki/$1"`).
    pub url: String,
    /// Whether this is a local interwiki (resolved within the same wiki farm).
    pub local: bool,
    /// Whether transclusions across this interwiki prefix are allowed.
    pub transclusion_allowed: bool,
    /// Whether this is a local interwiki prefix (matched before namespace
    /// lookup; empty title means main page). Mirrors PHP's `localinterwiki`.
    pub localinterwiki: Option<bool>,
    /// The language code if this prefix is a language link (e.g. `"de"`).
    /// Mirrors PHP's `language`.
    pub language: Option<String>,
    /// Whether this is an extra-language link. Mirrors PHP's `extralanglink`.
    pub extralanglink: Option<bool>,
    /// If true, strip the `http:`/`https:` scheme from the absolute href.
    /// Mirrors PHP's `protorel`.
    pub protorel: Option<bool>,
}

impl InterwikiInfo {
    /// Convenience constructor with the common fields set.
    pub fn new(url: impl Into<String>, local: bool) -> Self {
        Self {
            url: url.into(),
            local,
            transclusion_allowed: false,
            localinterwiki: None,
            language: None,
            extralanglink: None,
            protorel: None,
        }
    }
}

/// Mapping of magic word names to their behavior.
///
/// Keys are canonical English names; values are the localized aliases
/// for each. The parser core uses canonical names internally and resolves
/// localized names via this map.
pub type MagicWordMap = HashMap<String, MagicWordEntry>;

/// A magic word entry with its localized aliases.
#[derive(Debug, Clone)]
pub struct MagicWordEntry {
    /// The canonical English name (e.g. `"img_thumbnail"`).
    pub canonical: String,
    /// Whether this is case-sensitive when matching.
    pub case_sensitive: bool,
    /// All localized aliases for this magic word.
    pub aliases: Vec<String>,
}

// ---------------------------------------------------------------------------
// Extension handler trait
// ---------------------------------------------------------------------------

/// Handler for custom extension tags (e.g. `<ref>`, `<gallery>`, `<poem>`).
///
/// When the parser encounters an extension tag, it delegates to an
/// `ExtensionHandler` that knows how to process it.
#[async_trait]
pub trait ExtensionHandler: Send + Sync {
    /// Process an extension tag and return the HTML to embed.
    ///
    /// - `name` — the tag name (e.g. `"ref"`).
    /// - `attrs` — tag attributes (key-value pairs).
    /// - `body` — the content between open and close tags (empty for self-closing).
    /// - `source` — the data source for any lookups needed.
    async fn handle(
        &self,
        name: &str,
        attrs: &[(String, String)],
        body: &str,
        source: &dyn DataSource,
    ) -> Result<String>;
}
