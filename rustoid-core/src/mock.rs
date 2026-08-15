//! Mock implementations of `DataSource` and `SiteConfig` for testing.
//!
//! These provide in-memory data sources useful for unit tests and
//! for running the Parsoid parser test suite without an external API.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use crate::error::Result;
use crate::title::Title;
use crate::traits::{
    DataSource, FileInfo, InterwikiInfo, MagicWordEntry, MagicWordMap, NamespaceInfo, SiteConfig,
};

// ---------------------------------------------------------------------------
// MockDataSource
// ---------------------------------------------------------------------------

/// An in-memory data source for testing.
///
/// Pages, templates, modules, and file info are added via builder methods.
pub struct MockDataSource {
    pages: RwLock<HashMap<String, String>>,
    templates: RwLock<HashMap<String, String>>,
    modules: RwLock<HashMap<String, String>>,
    files: RwLock<HashMap<String, FileInfo>>,
    redirects: RwLock<HashMap<String, String>>,
    messages: RwLock<HashMap<(String, String), String>>,
}

impl MockDataSource {
    /// Create an empty mock data source.
    pub fn new() -> Self {
        Self {
            pages: RwLock::new(HashMap::new()),
            templates: RwLock::new(HashMap::new()),
            modules: RwLock::new(HashMap::new()),
            files: RwLock::new(HashMap::new()),
            redirects: RwLock::new(HashMap::new()),
            messages: RwLock::new(HashMap::new()),
        }
    }

    /// Add a page with the given title and wikitext content.
    pub fn add_page(&self, title: &str, content: &str) {
        self.pages
            .write()
            .unwrap()
            .insert(title.to_string(), content.to_string());
    }

    /// Add a template page.
    pub fn add_template(&self, title: &str, content: &str) {
        self.templates
            .write()
            .unwrap()
            .insert(title.to_string(), content.to_string());
    }

    /// Add a Lua module source.
    pub fn add_module(&self, title: &str, content: &str) {
        self.modules
            .write()
            .unwrap()
            .insert(title.to_string(), content.to_string());
    }

    /// Add file info for an image/media file.
    pub fn add_file(&self, title: &str, info: FileInfo) {
        self.files.write().unwrap().insert(title.to_string(), info);
    }

    /// Add a redirect mapping (source title → target title).
    pub fn add_redirect(&self, from: &str, to: &str) {
        self.redirects
            .write()
            .unwrap()
            .insert(from.to_string(), to.to_string());
    }

    /// Add an i18n message.
    pub fn add_message(&self, lang: &str, key: &str, value: &str) {
        self.messages
            .write()
            .unwrap()
            .insert((lang.to_string(), key.to_string()), value.to_string());
    }
}

impl Default for MockDataSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DataSource for MockDataSource {
    async fn get_page_content(&self, title: &Title) -> Result<Option<String>> {
        let key = title.full_text();
        // Check pages first, then templates (since templates are also pages)
        if let Some(content) = self.pages.read().unwrap().get(&key) {
            return Ok(Some(content.clone()));
        }
        Ok(self.templates.read().unwrap().get(&key).cloned())
    }

    async fn get_template(&self, title: &Title) -> Result<Option<String>> {
        let key = title.full_text();
        Ok(self.templates.read().unwrap().get(&key).cloned())
    }

    async fn get_module(&self, title: &Title) -> Result<Option<String>> {
        let key = title.full_text();
        Ok(self.modules.read().unwrap().get(&key).cloned())
    }

    async fn get_file_info(&self, title: &Title) -> Result<Option<FileInfo>> {
        let key = title.full_text();
        Ok(self.files.read().unwrap().get(&key).cloned())
    }

    async fn resolve_redirect(&self, title: &Title) -> Result<Option<Title>> {
        let key = title.full_text();
        if let Some(target) = self.redirects.read().unwrap().get(&key) {
            Ok(Some(Title::new_main(target.clone())))
        } else {
            Ok(None)
        }
    }

    async fn get_message(&self, lang: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .messages
            .read()
            .unwrap()
            .get(&(lang.to_string(), key.to_string()))
            .cloned())
    }
}

// ---------------------------------------------------------------------------
// MockSiteConfig
// ---------------------------------------------------------------------------

/// A minimal site configuration for testing, modeled on English Wikipedia.
pub struct MockSiteConfig {
    namespaces: HashMap<i32, NamespaceInfo>,
    interwiki_map: HashMap<String, InterwikiInfo>,
    magic_words: MagicWordMap,
    extension_tags: Vec<String>,
    server_url: String,
    article_path: String,
    language_code: String,
}

impl MockSiteConfig {
    /// Create a new mock config with enwiki-like defaults.
    pub fn new() -> Self {
        let mut config = Self {
            namespaces: HashMap::new(),
            interwiki_map: HashMap::new(),
            magic_words: HashMap::new(),
            extension_tags: vec![
                "ref".to_string(),
                "references".to_string(),
                "gallery".to_string(),
                "poem".to_string(),
                "source".to_string(),
                "syntaxhighlight".to_string(),
                "math".to_string(),
                "chem".to_string(),
                "hiero".to_string(),
                "timeline".to_string(),
                "graph".to_string(),
                "mapframe".to_string(),
                "maplink".to_string(),
                "indicator".to_string(),
                "templatedata".to_string(),
                "templatestyles".to_string(),
            ],
            server_url: "https://en.wikipedia.org".to_string(),
            article_path: "/wiki/$1".to_string(),
            language_code: "en".to_string(),
        };

        // Register standard MediaWiki namespaces
        config.add_namespace(-2, "Media", &[], true, "wikitext");
        config.add_namespace(-1, "Special", &[], true, "wikitext");
        config.add_namespace(0, "Main", &[], true, "wikitext");
        config.add_namespace(1, "Talk", &[], true, "wikitext");
        config.add_namespace(2, "User", &[], true, "wikitext");
        config.add_namespace(3, "User talk", &[], true, "wikitext");
        config.add_namespace(4, "Project", &["Wikipedia"], true, "wikitext");
        config.add_namespace(5, "Project talk", &["Wikipedia talk"], true, "wikitext");
        config.add_namespace(6, "File", &["Image"], true, "wikitext");
        config.add_namespace(7, "File talk", &["Image talk"], true, "wikitext");
        config.add_namespace(8, "MediaWiki", &[], true, "wikitext");
        config.add_namespace(9, "MediaWiki talk", &[], true, "wikitext");
        config.add_namespace(10, "Template", &[], true, "wikitext");
        config.add_namespace(11, "Template talk", &[], true, "wikitext");
        config.add_namespace(12, "Help", &[], true, "wikitext");
        config.add_namespace(13, "Help talk", &[], true, "wikitext");
        config.add_namespace(14, "Category", &[], true, "wikitext");
        config.add_namespace(15, "Category talk", &[], true, "wikitext");
        config.add_namespace(828, "Module", &[], true, "Scribunto");
        config.add_namespace(829, "Module talk", &[], true, "wikitext");

        // Register some interwiki prefixes
        config.add_interwiki("wikipedia", "https://en.wikipedia.org/wiki/$1", true);
        config.add_interwiki("wiktionary", "https://en.wiktionary.org/wiki/$1", true);
        config.add_interwiki("wikibooks", "https://en.wikibooks.org/wiki/$1", true);
        config.add_interwiki("wikiquote", "https://en.wikiquote.org/wiki/$1", true);
        config.add_interwiki("commons", "https://commons.wikimedia.org/wiki/$1", true);
        config.add_interwiki("meta", "https://meta.wikimedia.org/wiki/$1", true);
        config.add_interwiki("mw", "https://www.mediawiki.org/wiki/$1", true);

        // Register language prefixes (language links, not plain interwikis).
        config.add_language_interwiki("de", "https://de.wikipedia.org/wiki/$1");
        config.add_language_interwiki("fr", "https://fr.wikipedia.org/wiki/$1");

        // Register common magic words (English)
        config.add_magic_word("toc", &["__TOC__", "__NOTOC__", "__FORCETOC__"]);
        config.add_magic_word(
            "noeditsection",
            &["__NOEDITSECTION__", "__NEWSECTIONLINK__"],
        );
        config.add_magic_word("nogallery", &["__NOGALLERY__"]);
        config.add_magic_word("hiddencat", &["__HIDDENCAT__"]);
        config.add_magic_word("index", &["__INDEX__", "__NOINDEX__"]);
        config.add_magic_word("staticredirect", &["__STATICREDIRECT__"]);
        config.add_magic_word("displaytitle", &["DISPLAYTITLE"]);
        config.add_magic_word(
            "defaultsort",
            &["DEFAULTSORT", "DEFAULTSORTKEY", "DEFAULTCATEGORYSORT"],
        );
        config.add_magic_word("pagename", &["PAGENAME", "PAGENAMEE"]);
        config.add_magic_word("fullpagename", &["FULLPAGENAME", "FULLPAGENAMEE"]);
        config.add_magic_word("currentyear", &["CURRENTYEAR"]);
        config.add_magic_word("currentmonth", &["CURRENTMONTH", "CURRENTMONTHNAME"]);
        config.add_magic_word("currentday", &["CURRENTDAY", "CURRENTDAY2"]);
        config.add_magic_word("currenttime", &["CURRENTTIME"]);
        config.add_magic_word("numberofarticles", &["NUMBEROFARTICLES"]);
        config.add_magic_word("numberofpages", &["NUMBEROFPAGES"]);
        config.add_magic_word("numberofusers", &["NUMBEROFUSERS"]);
        config.add_magic_word("numberofedits", &["NUMBEROFEDITS"]);
        config.add_magic_word("namespace", &["NAMESPACE", "NAMESPACENUMBER"]);
        config.add_magic_word("revisionid", &["REVISIONID"]);
        config.add_magic_word("revisionday", &["REVISIONDAY", "REVISIONDAY2"]);
        config.add_magic_word("revisionyear", &["REVISIONYEAR"]);
        config.add_magic_word("revisiontimestamp", &["REVISIONTIMESTAMP"]);
        config.add_magic_word("server", &["SERVER", "SERVERNAME"]);
        config.add_magic_word("sitename", &["SITENAME"]);
        config.add_magic_word("img_thumbnail", &["thumbnail", "thumb"]);
        config.add_magic_word("img_right", &["right"]);
        config.add_magic_word("img_left", &["left"]);
        config.add_magic_word("img_center", &["center", "centre"]);
        config.add_magic_word("img_framed", &["framed", "frame"]);
        config.add_magic_word("img_frameless", &["frameless"]);

        config
    }

    fn add_namespace(
        &mut self,
        id: i32,
        canonical: &str,
        aliases: &[&str],
        case_sensitive: bool,
        content_model: &str,
    ) {
        self.namespaces.insert(
            id,
            NamespaceInfo {
                canonical: canonical.to_string(),
                aliases: aliases.iter().map(|s| s.to_string()).collect(),
                case_sensitive,
                default_content_model: content_model.to_string(),
            },
        );
    }

    fn add_interwiki(&mut self, prefix: &str, url: &str, local: bool) {
        self.interwiki_map
            .insert(prefix.to_string(), InterwikiInfo::new(url, local));
    }

    fn add_language_interwiki(&mut self, prefix: &str, url: &str) {
        let mut info = InterwikiInfo::new(url, true);
        info.language = Some(prefix.to_string());
        info.extralanglink = Some(true);
        self.interwiki_map.insert(prefix.to_string(), info);
    }

    fn add_magic_word(&mut self, canonical: &str, aliases: &[&str]) {
        self.magic_words.insert(
            canonical.to_string(),
            MagicWordEntry {
                canonical: canonical.to_string(),
                case_sensitive: true,
                aliases: aliases.iter().map(|s| s.to_string()).collect(),
            },
        );
    }
}

impl Default for MockSiteConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl SiteConfig for MockSiteConfig {
    fn namespaces(&self) -> &HashMap<i32, NamespaceInfo> {
        &self.namespaces
    }

    fn interwiki_map(&self) -> &HashMap<String, InterwikiInfo> {
        &self.interwiki_map
    }

    fn magic_words(&self) -> &MagicWordMap {
        &self.magic_words
    }

    fn extension_tags(&self) -> &[String] {
        &self.extension_tags
    }

    fn server_url(&self) -> &str {
        &self.server_url
    }

    fn article_path(&self) -> &str {
        &self.article_path
    }

    fn language_code(&self) -> &str {
        &self.language_code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_page_content() {
        let source = MockDataSource::new();
        source.add_page("Main Page", "'''Hello''' world!");

        let title = Title::new_main("Main Page");
        let content = source.get_page_content(&title).await.unwrap();
        assert_eq!(content, Some("'''Hello''' world!".to_string()));
    }

    #[tokio::test]
    async fn test_mock_template() {
        let source = MockDataSource::new();
        source.add_template("Template:Foo", "bar");

        let title = Title::new(10, "Foo");
        let content = source.get_template(&title).await.unwrap();
        assert_eq!(content, Some("bar".to_string()));
    }

    #[tokio::test]
    async fn test_mock_redirect() {
        let source = MockDataSource::new();
        source.add_redirect("Old Page", "New Page");

        let title = Title::new_main("Old Page");
        let target = source.resolve_redirect(&title).await.unwrap();
        assert_eq!(target, Some(Title::new_main("New Page")));
    }

    #[test]
    fn test_mock_config_namespaces() {
        let config = MockSiteConfig::new();
        assert!(config.namespaces().contains_key(&0));
        assert!(config.namespaces().contains_key(&10));
        assert_eq!(config.namespaces().get(&6).unwrap().canonical, "File");
    }

    #[test]
    fn test_mock_config_interwiki() {
        let config = MockSiteConfig::new();
        assert!(config.interwiki_map().contains_key("wikipedia"));
        assert!(config.interwiki_map().contains_key("commons"));
    }
}
