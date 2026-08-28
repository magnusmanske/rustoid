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
    /// to its namespace ID. Mirrors PHP's `SiteConfig::canonicalNamespaceId`,
    /// which takes an all-lowercase name and matches case-insensitively.
    /// Returns `None` if the namespace is not configured.
    fn canonical_namespace_id(&self, canonical: &str) -> Option<i32> {
        let target = crate::util::normalize_namespace_name(canonical);
        self.namespaces()
            .iter()
            .find(|(_, info)| crate::util::normalize_namespace_name(&info.canonical) == target)
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

    /// `SiteConfig::interwikiMapNoNamespaces` — the interwiki map with entries
    /// that conflict with a namespace name removed (namespace wins).
    fn interwiki_map_no_namespaces(&self) -> Vec<(String, InterwikiInfo)> {
        self.interwiki_map()
            .iter()
            .filter(|(key, _)| self.namespace_id(key).is_none())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// `SiteConfig::interwikiMatcher` — match an href against the interwiki URL
    /// patterns, returning the interwiki prefix and the matched title target.
    /// Language interwikis are escaped with a leading `:`.
    ///
    /// NOTE: the `local` interwiki shortcuts (`./$prefix:$title` and
    /// `$prefix%3A$title`) are not yet emitted into the pattern set; the
    /// full-URL and protocol-relative forms are matched faithfully.
    fn interwiki_matcher(&self, href: &str) -> Option<(String, String)> {
        // Build patterns, preferring language matches over non-language ones.
        let mut keys = Vec::new();
        let mut patterns: Vec<regex::Regex> = Vec::new();

        // Two passes: language first, then non-language.
        for prefer_lang in [true, false] {
            for (key, iw) in self.interwiki_map_no_namespaces() {
                let is_lang = iw.language.is_some();
                if is_lang != prefer_lang {
                    continue;
                }
                let url = &iw.url;
                let protocol_relative = url.starts_with("//") || iw.protorel == Some(true);
                let url_clean = if iw.protorel == Some(true) {
                    url.trim_start_matches("http:")
                        .trim_start_matches("https:")
                        .to_string()
                } else {
                    url.clone()
                };
                // Escape the URL template first (matching `preg_quote`), then
                // replace the escaped `$1` placeholder with a capture group.
                let pattern_body = regex_escape(&url_clean).replace("\\$1", "(.*?)");
                let regex_body = if protocol_relative {
                    format!("(?:https?:)?{pattern_body}")
                } else {
                    pattern_body
                };
                let Ok(re) = regex::Regex::new(&format!("^{regex_body}$")) else {
                    continue;
                };
                keys.push(key);
                patterns.push(re);
            }
        }

        // Language interwikis are escaped with a leading colon (handled inline
        // below by re-checking the interwiki entry's language flag).
        for (idx, key) in keys.iter().enumerate() {
            let iw = self
                .interwiki_map_no_namespaces()
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone());
            let is_lang = iw.map(|i| i.language.is_some()).unwrap_or(false);
            if let Some(caps) = patterns[idx].captures(href) {
                let target = caps
                    .get(1)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                let escaped_key = if is_lang {
                    format!(":{key}")
                } else {
                    key.clone()
                };
                return Some((escaped_key, target));
            }
        }
        None
    }

    /// `SiteConfig::getExtResourceURLPatternMatcher` — match an href against the
    /// RFC/ISBN/PMID magic-link URL patterns, returning the magic-link type and
    /// the matched reference. Returns `None` on no match.
    fn ext_resource_url_pattern_match(&self, text: &str) -> Option<(String, String)> {
        use regex::Regex;
        // The localized Special namespace / Booksources aliases are not plumbed
        // through this trait yet, so the ISBN URL pattern uses a conservative
        // approximation (Special:Booksources). RFC/PMID match their canonical
        // host paths.
        if self.magic_link_enabled("RFC") {
            let re = Regex::new(r"[^/]*//datatracker\.ietf\.org/doc/html/rfc([A-Za-z0-9]+)").ok();
            if let Some(re) = re
                && let Some(caps) = re.captures(text)
                && let Some(m) = caps.get(1)
            {
                return Some(("RFC".to_string(), m.as_str().to_string()));
            }
        }
        if self.magic_link_enabled("PMID") {
            let re =
                Regex::new(r"[^/]*//www\.ncbi\.nlm\.nih\.gov/pubmed/([A-Za-z0-9]+)\?dopt=Abstract")
                    .ok();
            if let Some(re) = re
                && let Some(caps) = re.captures(text)
                && let Some(m) = caps.get(1)
            {
                return Some(("PMID".to_string(), m.as_str().to_string()));
            }
        }
        None
    }

    /// The URL for uploading a file (used by media/file links). Mirrors PHP's
    /// `SiteConfig::getUploadUrl` (a sensible default, overridable).
    fn get_upload_url(&self, _title: &str) -> String {
        format!("{}/index.php?title=Special:Upload", self.server_url())
    }

    /// Prefix to prepend to a page title to link to that page, relative to
    /// the base URI. Mirrors PHP's `SiteConfig::relativeLinkPrefix`.
    fn relative_link_prefix(&self) -> &str {
        "./"
    }

    /// Whether the named magic-link syntax ("ISBN", "PMID", or "RFC") is
    /// enabled on this wiki. Mirrors PHP's `SiteConfig::magicLinkEnabled`,
    /// which defaults to `true` for graceful upgrades.
    fn magic_link_enabled(&self, _which: &str) -> bool {
        true
    }

    /// Extra attributes to add to external links, keyed by attribute name.
    /// Mirrors PHP's `SiteConfig::getExternalLinkAttribs`, which defaults to
    /// `rel = ["nofollow"]` (i.e. `$wgNoFollowLinks = true`). The `class` key
    /// holds the class tokens to *add* (not replace) to any existing `class`.
    fn external_link_attribs(&self, _href: &str) -> Vec<(String, Vec<String>)> {
        vec![("rel".to_string(), vec!["nofollow".to_string()])]
    }

    /// Whether `ParsoidExperimentalParserFunctionOutput` should generate
    /// v3.x HTML for parser functions (i.e. a `mw:ParserFunction/<name>`
    /// `typeof` and a `"parserfunction"` `data-mw` parts key). Mirrors PHP's
    /// `SiteConfig::getMWConfigValue('ParsoidExperimentalParserFunctionOutput')`,
    /// which is an opt-in experimental flag (default `false`).
    fn parsoid_experimental_parser_function_output(&self) -> bool {
        false
    }

    /// The set of URL protocol schemes valid on this wiki, e.g.
    /// `["//", "http://", "https://", "ftp://", "ftps://", "mailto:", "news:",
    /// "tel:"]`. Mirrors PHP's `SiteConfig::getProtocols`.
    fn protocols(&self) -> &[&'static str] {
        // Scheme-only entries are written without `//` (e.g. `mailto:`, `irc:`)
        // and URL-scheme entries with `//` (e.g. `http://`), matching the
        // `proto` produced by `Sanitizer::splitUrl` (scheme + optional `//`).
        &[
            "//",
            "http://",
            "https://",
            "ftp://",
            "ftps://",
            "mailto:",
            "news:",
            "irc:",
            "ircs:",
            "gopher://",
            "mms://",
            "tel:",
            "nntp://",
        ]
    }

    /// Whether `potential_link` *begins with* a valid protocol scheme (anchored
    /// at the start of the string). Mirrors PHP's `SiteConfig::hasValidProtocol`.
    fn has_valid_protocol(&self, potential_link: &str) -> bool {
        self.protocols()
            .iter()
            .any(|p| potential_link.starts_with(p))
    }

    /// Whether `potential_link` *contains* a valid protocol scheme at a word
    /// boundary (used by the wikitext-escape autolink fast-path). Mirrors PHP's
    /// `SiteConfig::findValidProtocol`.
    fn find_valid_protocol(&self, potential_link: &str) -> bool {
        self.protocols().iter().any(|p| {
            // `p` must appear at a word boundary (start-of-string or after a
            // non-word char), mirroring the `(?:\W|^)` lookbehind in PHP.
            potential_link.match_indices(p).any(|(idx, _)| {
                idx == 0 || {
                    potential_link[..idx]
                        .chars()
                        .last()
                        .map(|c| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(true)
                }
            })
        })
    }

    /// Whether `name` (already lower-cased) is a configured extension/tag name.
    /// Mirrors PHP's `SiteConfig::isExtensionTag`.
    fn is_extension_tag(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.extension_tags()
            .iter()
            .any(|t| t.to_ascii_lowercase() == lower)
    }

    /// The link-trail regular expression (body, without delimiters/flags) for
    /// this wiki, e.g. enwiki's `[a-z]+`. Mirrors PHP's `SiteConfig::linkTrailRegex`.
    /// Returns `None` for wikis with no link trail.
    fn link_trail_regex(&self) -> Option<&'static str> {
        Some("[a-z]+")
    }

    /// The link-prefix regular expression (body, without delimiters/flags) for
    /// languages with link prefixes (e.g. Hebrew, Arabic). Mirrors PHP's
    /// `SiteConfig::linkPrefixRegex`. Returns `None` for wikis without a link prefix.
    fn link_prefix_regex(&self) -> Option<&'static str> {
        None
    }

    /// The set of characters legal in a page title, used as a regex character
    /// class body (without the enclosing `[]`). Mirrors PHP's
    /// `SiteConfig::legalTitleChars()`. Used to URL-encode fragment hashes in
    /// link targets. The default is the standard MediaWiki first-letter set.
    fn legal_title_chars(&self) -> &'static str {
        " %!\"$&'()*,\\-./0-9:;=?@A-Z\\^_`a-z~\\x80-\\xff+"
    }
}

/// Escape a regex body's metacharacters for the `regex` crate (approximating
/// PHP's `preg_quote` for `/`). Used to embed interwiki URL templates.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r"\.+*?()|[]{}^$#&-".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `SiteConfig` for exercising the default protocol helpers.
    struct TestConfig;

    impl SiteConfig for TestConfig {
        fn namespaces(&self) -> &HashMap<i32, NamespaceInfo> {
            static N: std::sync::OnceLock<HashMap<i32, NamespaceInfo>> = std::sync::OnceLock::new();
            N.get_or_init(HashMap::new)
        }
        fn interwiki_map(&self) -> &HashMap<String, InterwikiInfo> {
            static M: std::sync::OnceLock<HashMap<String, InterwikiInfo>> =
                std::sync::OnceLock::new();
            M.get_or_init(HashMap::new)
        }
        fn magic_words(&self) -> &MagicWordMap {
            static W: std::sync::OnceLock<MagicWordMap> = std::sync::OnceLock::new();
            W.get_or_init(HashMap::new)
        }
        fn extension_tags(&self) -> &[String] {
            static T: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            T.get_or_init(|| vec!["ref".to_string(), "gallery".to_string()])
        }
        fn server_url(&self) -> &str {
            "https://en.wikipedia.org"
        }
        fn article_path(&self) -> &str {
            "/wiki/$1"
        }
        fn language_code(&self) -> &str {
            "en"
        }
    }

    #[test]
    fn test_has_valid_protocol() {
        let c = TestConfig;
        assert!(c.has_valid_protocol("https://example.com"));
        assert!(c.has_valid_protocol("//example.com"));
        assert!(c.has_valid_protocol("mailto:a@b.c"));
        assert!(!c.has_valid_protocol("example.com"));
        // Anchored: a protocol later in the string does not count.
        assert!(!c.has_valid_protocol("foo https://example.com"));
    }

    #[test]
    fn test_find_valid_protocol() {
        let c = TestConfig;
        assert!(c.find_valid_protocol("see https://example.com now"));
        assert!(c.find_valid_protocol("https://example.com"));
        assert!(!c.find_valid_protocol("no protocol here"));
    }

    #[test]
    fn test_is_extension_tag() {
        let c = TestConfig;
        assert!(c.is_extension_tag("ref"));
        assert!(c.is_extension_tag("REF")); // lower-cased before matching
        assert!(!c.is_extension_tag("div"));
    }

    #[test]
    fn test_interwiki_matcher() {
        let c = crate::mock::MockSiteConfig::new();
        // `commons` → `https://commons.wikimedia.org/wiki/$1`.
        let m = c.interwiki_matcher("https://commons.wikimedia.org/wiki/Foo");
        assert_eq!(m, Some(("commons".to_string(), "Foo".to_string())));
        // No match for an unrelated URL.
        assert_eq!(c.interwiki_matcher("https://example.com/x"), None);
    }

    #[test]
    fn test_ext_resource_url_pattern_match() {
        let c = crate::mock::MockSiteConfig::new();
        let m = c.ext_resource_url_pattern_match("https://datatracker.ietf.org/doc/html/rfc1234");
        assert_eq!(m, Some(("RFC".to_string(), "1234".to_string())));
        assert_eq!(
            c.ext_resource_url_pattern_match("https://example.com"),
            None
        );
    }
}
