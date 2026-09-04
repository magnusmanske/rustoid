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

/// Look up a canonical title key in a map, falling back to a first-letter
/// case-insensitive match on the *title part* (after the `Namespace:` prefix).
/// MediaWiki titles are first-letter case-insensitive, so a fixture seeded as
/// `Template:test` must still resolve when queried as `Template:Test`. Mirrors
/// the `ucfirst` title normalization applied by `Title::newFromText`.
fn case_insensitive_get<T: Clone>(map: &RwLock<HashMap<String, T>>, key: &str) -> Option<T> {
    // Fast path: the exact key is already handled by the caller; only fall back
    // here when the caller's exact lookup missed. We toggle the case of the
    // first character of the title part (after the last ':').
    let (prefix, rest) = match key.rsplit_once(':') {
        Some((p, r)) if !p.is_empty() => (p, r),
        _ => return None,
    };
    let mut alternate = String::with_capacity(key.len());
    alternate.push_str(prefix);
    alternate.push(':');
    let mut chars = rest.chars();
    match chars.next() {
        Some(c) if c.is_ascii() => {
            if c.is_ascii_uppercase() {
                alternate.extend(c.to_lowercase());
            } else {
                alternate.extend(c.to_uppercase());
            }
        }
        Some(c) => alternate.push(c),
        None => {}
    }
    alternate.push_str(chars.as_str());
    map.read().unwrap().get(&alternate).cloned()
}

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
        // Check pages first, then templates (since templates are also pages).
        if let Some(content) = self.pages.read().unwrap().get(&key) {
            return Ok(Some(content.clone()));
        }
        if let Some(content) = case_insensitive_get(&self.pages, &key) {
            return Ok(Some(content.clone()));
        }
        Ok(self.templates.read().unwrap().get(&key).cloned())
    }

    async fn get_template(&self, title: &Title) -> Result<Option<String>> {
        let key = title.full_text();
        Ok(self
            .templates
            .read()
            .unwrap()
            .get(&key)
            .cloned()
            .or_else(|| case_insensitive_get(&self.templates, &key)))
    }

    async fn get_module(&self, title: &Title) -> Result<Option<String>> {
        let key = title.full_text();
        Ok(self
            .modules
            .read()
            .unwrap()
            .get(&key)
            .cloned()
            .or_else(|| case_insensitive_get(&self.modules, &key)))
    }

    async fn get_file_info(&self, title: &Title) -> Result<Option<FileInfo>> {
        // Files are keyed by their canonical prefixed DB key (English `File:`
        // prefix + underscore-separated name), independent of the localized
        // content-language alias (e.g. `Dosiero:`/`Файл:` map to the same file).
        let dbkey = title.get_dbkey();
        let canon = crate::title::namespace_prefix(title.namespace_id);
        let key = if canon.is_empty() {
            dbkey
        } else {
            format!("{canon}:{dbkey}")
        };
        Ok(self
            .files
            .read()
            .unwrap()
            .get(&key)
            .cloned()
            .or_else(|| case_insensitive_get(&self.files, &key)))
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
    parsoid_experimental_parser_function_output: bool,
    /// `wgExternalLinkTarget`: target attribute for external links (default none).
    external_link_target: Option<String>,
    /// `wgNoFollowLink`: whether external links get `rel="nofollow"`.
    no_follow_links: bool,
    /// `wgNoFollowDomainExceptions`: domains exempt from `nofollow`.
    no_follow_domain_exceptions: Vec<String>,
    /// Localized namespace names per language (language code → namespace ID →
    /// localized name), mirroring PHP's `SiteConfig::namespaceName`.
    localized_namespace_names: HashMap<String, HashMap<i32, String>>,
}

impl MockSiteConfig {
    /// Create a new mock config with enwiki-like defaults.
    pub fn new() -> Self {
        let mut config = Self {
            namespaces: HashMap::new(),
            interwiki_map: HashMap::new(),
            magic_words: HashMap::new(),
            extension_tags: vec![
                "nowiki".to_string(),
                "pre".to_string(),
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
                // Parser-test hooks (ParserHook.php).
                "tag".to_string(),
                "statictag".to_string(),
                "asidetag".to_string(),
                "pwraptest".to_string(),
                "divtag".to_string(),
                "spantag".to_string(),
                "embedtag".to_string(),
                "sealtag".to_string(),
            ],
            server_url: "https://en.wikipedia.org".to_string(),
            article_path: "/wiki/$1".to_string(),
            language_code: "en".to_string(),
            parsoid_experimental_parser_function_output: false,
            external_link_target: None,
            no_follow_links: true,
            no_follow_domain_exceptions: Vec::new(),
            localized_namespace_names: HashMap::new(),
        };

        // Register standard MediaWiki namespaces. `case_sensitive` reflects
        // whether the first letter of a title is case-insensitive (MediaWiki's
        // default `$wgCapitalLinks = true`): most content namespaces are
        // first-letter case-insensitive (`false`); the Media (-2), MediaWiki (8),
        // and MediaWiki talk (9) namespaces are fully case-sensitive (`true`).
        config.add_namespace(-2, "Media", &[], true, "wikitext");
        config.add_namespace(-1, "Special", &[], false, "wikitext");
        config.add_namespace(0, "Main", &[], false, "wikitext");
        config.add_namespace(1, "Talk", &[], false, "wikitext");
        config.add_namespace(2, "User", &[], false, "wikitext");
        config.add_namespace(3, "User talk", &[], false, "wikitext");
        config.add_namespace(4, "Project", &["Wikipedia"], false, "wikitext");
        config.add_namespace(5, "Project talk", &["Wikipedia talk"], false, "wikitext");
        config.add_namespace(6, "File", &["Image"], false, "wikitext");
        config.add_namespace(7, "File talk", &["Image talk"], false, "wikitext");
        config.add_namespace(8, "MediaWiki", &[], true, "wikitext");
        config.add_namespace(9, "MediaWiki talk", &[], true, "wikitext");
        config.add_namespace(10, "Template", &[], false, "wikitext");
        config.add_namespace(11, "Template talk", &[], false, "wikitext");
        config.add_namespace(12, "Help", &[], false, "wikitext");
        config.add_namespace(13, "Help talk", &[], false, "wikitext");
        config.add_namespace(14, "Category", &[], false, "wikitext");
        config.add_namespace(15, "Category talk", &[], false, "wikitext");
        config.add_namespace(828, "Module", &[], false, "Scribunto");
        config.add_namespace(829, "Module talk", &[], false, "wikitext");

        // Register some interwiki prefixes.
        // NOTE: the upstream Parsoid parser-test site config uses `http://`
        // (not `https://`) as its canonical protocol, and the fixtures embed
        // these URLs verbatim (e.g. `http://en.wikipedia.org/wiki/Foo`).
        config.add_interwiki("wikipedia", "http://en.wikipedia.org/wiki/$1", true);
        config.add_interwiki("wiktionary", "http://en.wiktionary.org/wiki/$1", true);
        config.add_interwiki("wikibooks", "http://en.wikibooks.org/wiki/$1", true);
        config.add_interwiki("wikiquote", "http://en.wikiquote.org/wiki/$1", true);
        config.add_interwiki("commons", "http://commons.wikimedia.org/wiki/$1", true);
        config.add_interwiki("meta", "http://meta.wikimedia.org/wiki/$1", true);
        config.add_interwiki("mw", "http://www.mediawiki.org/wiki/$1", true);
        // Interwiki prefixes used by the upstream parser fixtures.
        config.add_interwiki("meatball", "http://www.usemod.com/cgi-bin/mb.pl?$1", false);

        // Register language prefixes (language links, not plain interwikis).
        config.add_language_interwiki("en", "http://en.wikipedia.org/wiki/$1");
        config.add_language_interwiki("de", "http://de.wikipedia.org/wiki/$1");
        config.add_language_interwiki("fr", "http://fr.wikipedia.org/wiki/$1");

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
        // The `redirect` magic word. Each synonym includes the leading `#` and
        // is matched case-insensitively, mirroring `MagicWord::getRegex`.
        config.add_magic_word(
            "redirect",
            &[
                "#REDIRECT",
                "#TILVÍSUN",
                "#WEITERLEITUNG",
                "#REDIRECCIÓN",
                "#REDIRECTION",
                "#OMDIRIGERING",
                "#DOORVERWIJZING",
            ],
        );
        config.add_magic_word("currentyear", &["CURRENTYEAR"]);
        config.add_magic_word("currentmonth", &["CURRENTMONTH", "CURRENTMONTHNAME"]);
        config.add_magic_word("currentday", &["CURRENTDAY", "CURRENTDAY2"]);
        config.add_magic_word("currenttime", &["CURRENTTIME"]);
        config.add_magic_word("numberofarticles", &["NUMBEROFARTICLES"]);
        config.add_magic_word("numberofpages", &["NUMBEROFPAGES"]);
        config.add_magic_word("numberofusers", &["NUMBEROFUSERS"]);
        config.add_magic_word("numberofedits", &["NUMBEROFEDITS"]);
        config.add_magic_word("namespace", &["NAMESPACE", "NAMESPACENUMBER"]);
        // `{{ns:index}}` / `{{nse:index}}` map a namespace index (or localized
        // name) to its canonical name (`nse` returns the empty-form variant).
        config.add_magic_word("ns", &["ns"]);
        config.add_magic_word("nse", &["nse"]);
        config.add_magic_word("revisionid", &["REVISIONID"]);
        config.add_magic_word("revisionday", &["REVISIONDAY", "REVISIONDAY2"]);
        config.add_magic_word("revisionyear", &["REVISIONYEAR"]);
        config.add_magic_word("revisiontimestamp", &["REVISIONTIMESTAMP"]);
        config.add_magic_word("server", &["SERVER", "SERVERNAME"]);
        config.add_magic_word("sitename", &["SITENAME"]);
        config.add_magic_word("img_thumbnail", &["thumb", "thumbnail"]);
        config.add_magic_word("img_manualthumb", &["thumbnail=$1", "thumb=$1"]);
        config.add_magic_word("img_right", &["right"]);
        config.add_magic_word("img_left", &["left"]);
        config.add_magic_word("img_none", &["none"]);
        config.add_magic_word("img_center", &["center", "centre"]);
        config.add_magic_word("img_framed", &["frame", "framed", "enframed"]);
        config.add_magic_word("img_frameless", &["frameless"]);
        config.add_magic_word("img_border", &["border"]);
        config.add_magic_word("img_baseline", &["baseline"]);
        config.add_magic_word("img_sub", &["sub"]);
        config.add_magic_word("img_super", &["super", "sup"]);
        config.add_magic_word("img_top", &["top"]);
        config.add_magic_word("img_text_top", &["text-top"]);
        config.add_magic_word("img_middle", &["middle"]);
        config.add_magic_word("img_bottom", &["bottom"]);
        config.add_magic_word("img_text_bottom", &["text-bottom"]);
        config.add_magic_word("img_link", &["link=$1"]);
        config.add_magic_word("img_alt", &["alt=$1"]);
        config.add_magic_word("img_page", &["page=$1", "page $1"]);
        config.add_magic_word("img_upright", &["upright", "upright=$1", "upright $1"]);
        config.add_magic_word("img_width", &["$1px"]);
        config.add_magic_word("img_class", &["class=$1"]);
        config.add_magic_word("img_lang", &["lang=$1"]);
        // The `!` magic word (`{{!}}` → literal `|`, or a `<td>` inside a
        // template) — used to emit pipe characters from templates.
        config.add_magic_word("!", &["!"]);

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
        let mut info = InterwikiInfo::new(url, local);
        info.prefix = Some(prefix.to_string());
        self.interwiki_map.insert(prefix.to_string(), info);
    }

    fn add_language_interwiki(&mut self, prefix: &str, url: &str) {
        let mut info = InterwikiInfo::new(url, true);
        info.language = Some(prefix.to_string());
        info.extralanglink = Some(true);
        info.prefix = Some(prefix.to_string());
        // Language links are protocol-relative by default (strip http:/https:).
        info.protorel = Some(true);
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

    /// Enable/disable v3 parser-function output (for `!! config` sections in
    /// the parser-test fixtures).
    pub fn set_parsoid_experimental_parser_function_output(&mut self, enabled: bool) {
        self.parsoid_experimental_parser_function_output = enabled;
    }

    /// Register an additional extension tag name (e.g. `i18ntag`, `i18nattr` for
    /// the `i18next` parser-test option). Mirrors PHP's
    /// `SiteConfig::registerParserTestExtension`.
    pub fn add_extension_tag(&mut self, name: &str) {
        let lower = name.to_lowercase();
        if !self.extension_tags.contains(&lower) {
            self.extension_tags.push(lower);
        }
    }

    /// Set `wgExternalLinkTarget` (the `target` attribute for external links).
    pub fn set_external_link_target(&mut self, target: &str) {
        self.external_link_target = Some(target.to_string());
    }

    /// Set `wgNoFollowLinks` (whether external links get `rel="nofollow"`).
    pub fn set_no_follow_links(&mut self, enabled: bool) {
        self.no_follow_links = enabled;
    }

    /// Add a `wgNoFollowDomainExceptions` entry.
    pub fn add_no_follow_domain_exception(&mut self, domain: &str) {
        self.no_follow_domain_exceptions.push(domain.to_string());
    }

    /// Set the content language and register localized namespace names + media
    /// option aliases for it (mirrors PHP's `SiteConfig` localization used by
    /// the parser-test `language=` option). Covers the languages the media
    /// fixture exercises (`es`, `fa`).
    pub fn set_language(&mut self, lang: &str) {
        self.language_code = lang.to_string();
        let mut ns = HashMap::new();
        match lang {
            "es" => {
                ns.insert(6, "Archivo".to_string()); // File
                // Localized namespace aliases (so `Archivo:` resolves to File).
                if let Some(info) = self.namespaces.get_mut(&6) {
                    info.aliases.push("Archivo".to_string());
                    info.aliases.push("archivo".to_string());
                }
                // Localized media option aliases (localized form first, per
                // MediaWiki's "local first, English last" convention).
                self.add_magic_word(
                    "img_manualthumb",
                    &["miniatura=$1", "thumbnail=$1", "thumb=$1"],
                );
                self.add_magic_word("img_thumbnail", &["miniatura", "thumbnail", "thumb"]);
                self.add_magic_word("img_left", &["izquierda", "left"]);
                self.add_magic_word("img_link", &["enlace=$1", "link=$1"]);
            }
            "de" => {
                ns.insert(6, "Datei".to_string()); // File
                if let Some(info) = self.namespaces.get_mut(&6) {
                    info.aliases.push("Datei".to_string());
                    info.aliases.push("datei".to_string());
                    info.aliases.push("Bild".to_string());
                    info.aliases.push("bild".to_string());
                }
                // German media option aliases (localized-first).
                self.add_magic_word("img_thumbnail", &["miniatur", "thumb", "thumbnail"]);
                self.add_magic_word("img_left", &["links", "left"]);
                self.add_magic_word("img_right", &["rechts", "right"]);
                self.add_magic_word("img_center", &["zentriert", "center", "centre"]);
                // German `lang=` for SVGs (localized `sprache=$1` first).
                self.add_magic_word("img_lang", &["sprache=$1", "lang=$1"]);
            }
            "ru" => {
                ns.insert(6, "Файл".to_string()); // File
                if let Some(info) = self.namespaces.get_mut(&6) {
                    info.aliases.push("Файл".to_string());
                    info.aliases.push("файл".to_string());
                }
                // Russian media option aliases (localized-first).
                self.add_magic_word(
                    "img_thumbnail",
                    &["мини", "миниатюра", "thumbnail", "thumb"],
                );
                self.add_magic_word("img_right", &["справа", "right"]);
                self.add_magic_word("img_left", &["слева", "left"]);
                self.add_magic_word("img_center", &["центр", "center", "centre"]);
            }
            "fa" => {
                ns.insert(6, "فایل".to_string()); // File
                if let Some(info) = self.namespaces.get_mut(&6) {
                    info.aliases.push("فایل".to_string());
                }
                self.add_magic_word("img_thumbnail", &["بندانگشتی", "thumb", "thumbnail"]);
            }
            "eo" => {
                ns.insert(6, "Dosiero".to_string()); // File
                if let Some(info) = self.namespaces.get_mut(&6) {
                    info.aliases.push("Dosiero".to_string());
                    info.aliases.push("dosiero".to_string());
                }
                // Esperanto width suffix: `100ra` → `img_width` value `100`
                // (localized `ra` first, English `px` last).
                self.add_magic_word("img_width", &["$1ra", "$1px"]);
                // Esperanto align and format aliases (localized-first).
                self.add_magic_word("img_left", &["maldekstra", "left"]);
                self.add_magic_word("img_right", &["dekstra", "right"]);
                self.add_magic_word("img_center", &["centre", "center"]);
            }
            "sr" => {
                ns.insert(6, "Датотека".to_string()); // File (Serbian)
                if let Some(info) = self.namespaces.get_mut(&6) {
                    info.aliases.push("Датотека".to_string());
                    info.aliases.push("датотека".to_string());
                }
                self.add_magic_word("img_thumbnail", &["мини", "thumbnail", "thumb"]);
            }
            _ => {}
        }
        if !ns.is_empty() {
            self.localized_namespace_names.insert(lang.to_string(), ns);
        }
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

    fn namespace_name(&self, ns: i32) -> Option<String> {
        self.localized_namespace_names
            .get(&self.language_code)
            .and_then(|m| m.get(&ns).cloned())
            .or_else(|| self.namespaces.get(&ns).map(|info| info.canonical.clone()))
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

    fn get_upload_url(&self, title: &str) -> String {
        // The parser-test harness uses a relative Special:Upload URL with the
        // destination file name, url-encoded except the `:`/`*` kept literal
        // (mirrors `MockApiHelper`-backed `getUploadUrl`).
        let encoded = crate::util::urlencode(title)
            .replace("%3A", ":")
            .replace("%2A", "*");
        format!("./Special:Upload?wpDestFile={encoded}")
    }

    fn parsoid_experimental_parser_function_output(&self) -> bool {
        self.parsoid_experimental_parser_function_output
    }

    fn external_link_attribs(&self, href: &str) -> Vec<(String, Vec<String>)> {
        // `wgNoFollowDomainExceptions` exempts matching domains from `nofollow`.
        let nofollow = self.no_follow_links
            && !self
                .no_follow_domain_exceptions
                .iter()
                .any(|d| href.contains(d));

        let mut attribs: Vec<(String, Vec<String>)> = Vec::new();
        if nofollow {
            attribs.push(("rel".to_string(), vec!["nofollow".to_string()]));
        }
        if let Some(target) = &self.external_link_target {
            attribs.push(("target".to_string(), vec![target.clone()]));
        }
        attribs
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
