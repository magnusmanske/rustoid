//! MediaWiki API data source.
//!
//! Fetches page content, templates, modules, and file info from a
//! MediaWiki installation's Action API. Enabled via the `mwapi` feature.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{Result, RustoidError};
use crate::title::Title;
use crate::traits::{DataSource, FileInfo};

/// A DataSource backed by the MediaWiki Action API.
///
/// Caches fetched pages in memory with an optional TTL.
pub struct MediaWikiApiDataSource {
    client: reqwest::Client,
    api_url: String,
    /// Simple in-memory page cache.
    cache: Mutex<HashMap<String, (String, Instant)>>,
    /// Cache TTL.
    ttl: Duration,
}

/// Response from `action=parse&prop=text`.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ParseResponse {
    parse: Option<ParseContent>,
    error: Option<ApiError>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ParseContent {
    title: Option<String>,
    #[serde(rename = "wikitext")]
    wikitext: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ApiError {
    code: Option<String>,
    info: Option<String>,
}

/// Response from `action=query&prop=revisions&rvprop=content`.
#[derive(Debug, Deserialize)]
struct QueryResponse {
    query: Option<QueryContent>,
}

#[derive(Debug, Deserialize)]
struct QueryContent {
    pages: Option<HashMap<String, PageInfo>>,
}

#[derive(Debug, Deserialize)]
struct PageInfo {
    #[allow(dead_code)]
    title: Option<String>,
    #[allow(dead_code)]
    pageid: Option<u64>,
    #[allow(dead_code)]
    ns: Option<i32>,
    revisions: Option<Vec<Revision>>,
    #[allow(dead_code)]
    redirect: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Revision {
    #[serde(rename = "*")]
    content: Option<String>,
}

/// Response from action=query&meta=siteinfo&siprop=namespacealiases|namespaces.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct SiteInfoResponse {
    query: Option<SiteInfoQuery>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct SiteInfoQuery {
    namespaces: Option<HashMap<String, NamespaceEntry>>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct NamespaceEntry {
    id: i32,
    #[serde(rename = "*")]
    name: Option<String>,
    canonical: Option<String>,
}

impl MediaWikiApiDataSource {
    /// Create a new API-backed data source.
    ///
    /// `api_url` should be the full path to api.php,
    /// e.g. `"https://en.wikipedia.org/w/api.php"`.
    pub fn new(api_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("rustoid/0.1.0 (https://github.com/magnusmanske/rustoid)")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| RustoidError::DataSource(e.to_string()))?;

        Ok(Self {
            client,
            api_url: api_url.into(),
            cache: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(300), // 5 minutes
        })
    }

    /// Create with custom cache TTL.
    pub fn with_ttl(api_url: impl Into<String>, ttl_seconds: u64) -> Result<Self> {
        let mut ds = Self::new(api_url)?;
        ds.ttl = Duration::from_secs(ttl_seconds);
        Ok(ds)
    }

    /// Get cached content, checking TTL.
    fn get_cached(&self, key: &str) -> Option<String> {
        let cache = self.cache.lock().unwrap();
        cache
            .get(key)
            .filter(|(_, ts)| ts.elapsed() < self.ttl)
            .map(|(content, _)| content.clone())
    }

    /// Store content in cache.
    fn cache_put(&self, key: String, content: String) {
        let mut cache = self.cache.lock().unwrap();
        cache.insert(key, (content, Instant::now()));
    }

    /// Fetch raw page content from the API.
    async fn fetch_page_content(&self, title: &str) -> Result<Option<String>> {
        let cache_key = format!("page:{title}");
        if let Some(cached) = self.get_cached(&cache_key) {
            return Ok(Some(cached));
        }

        let params = [
            ("action", "query"),
            ("prop", "revisions"),
            ("rvprop", "content"),
            ("rvslots", "main"),
            ("titles", title),
            ("format", "json"),
            ("formatversion", "2"),
            ("redirects", "1"),
        ];

        let resp = self
            .client
            .get(&self.api_url)
            .query(&params)
            .send()
            .await
            .map_err(|e| RustoidError::DataSource(format!("HTTP error: {e}")))?;

        let body: QueryResponse = resp
            .json()
            .await
            .map_err(|e| RustoidError::DataSource(format!("JSON parse error: {e}")))?;

        if let Some(query) = body.query
            && let Some(pages) = query.pages
        {
            for (_id, page) in pages {
                if let Some(revisions) = page.revisions
                    && let Some(rev) = revisions.first()
                    && let Some(content) = &rev.content
                {
                    self.cache_put(cache_key, content.clone());
                    return Ok(Some(content.clone()));
                }
            }
        }

        Ok(None)
    }

    /// Fetch template content from the API.
    async fn fetch_template(&self, title: &str) -> Result<Option<String>> {
        // Try the Template namespace first
        let template_title = if !title.contains(':') {
            format!("Template:{title}")
        } else {
            title.to_string()
        };
        self.fetch_page_content(&template_title).await
    }

    /// Fetch Lua module source.
    async fn fetch_module(&self, title: &str) -> Result<Option<String>> {
        let module_title = if !title.contains(':') {
            format!("Module:{title}")
        } else {
            title.to_string()
        };
        self.fetch_page_content(&module_title).await
    }

    /// Fetch file info from the API.
    async fn fetch_file_info(&self, title: &str) -> Result<Option<FileInfo>> {
        let params = [
            ("action", "query"),
            ("prop", "imageinfo"),
            ("iiprop", "url|size|mime"),
            ("titles", title),
            ("format", "json"),
            ("formatversion", "2"),
        ];

        let resp = self
            .client
            .get(&self.api_url)
            .query(&params)
            .send()
            .await
            .map_err(|e| RustoidError::DataSource(format!("HTTP error: {e}")))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| RustoidError::DataSource(format!("JSON parse error: {e}")))?;

        if let Some(pages) = body["query"]["pages"].as_array() {
            for page in pages {
                if let Some(imageinfo) = page["imageinfo"].as_array()
                    && let Some(info) = imageinfo.first()
                {
                    return Ok(Some(FileInfo {
                        title: page["title"].as_str().unwrap_or("").to_string(),
                        mime_type: info["mime"].as_str().unwrap_or("").to_string(),
                        size: info["size"].as_u64().unwrap_or(0),
                        width: info["width"].as_u64().unwrap_or(0) as u32,
                        height: info["height"].as_u64().unwrap_or(0) as u32,
                        description_url: info["descriptionurl"].as_str().unwrap_or("").to_string(),
                        file_url: info["url"].as_str().unwrap_or("").to_string(),
                        thumb_urls: HashMap::new(),
                        bad_file: page["badfile"].as_bool().unwrap_or(false),
                    }));
                }
            }
        }

        Ok(None)
    }

    /// Resolve a redirect.
    async fn fetch_redirect(&self, title: &str) -> Result<Option<String>> {
        let content = self.fetch_page_content(title).await?;
        if let Some(text) = content {
            // Check if the content starts with #REDIRECT [[Target]]
            if let Some(rest) = text.trim().strip_prefix("#REDIRECT") {
                if let Some(_rest) = rest.trim().strip_prefix("#redirect") {
                    return Ok(None);
                }
                let rest = rest.trim();
                if let Some(start) = rest.find("[[")
                    && let Some(end) = rest[start..].find("]]")
                {
                    let target = rest[start + 2..start + end].to_string();
                    return Ok(Some(target));
                }
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl DataSource for MediaWikiApiDataSource {
    async fn get_page_content(&self, title: &Title) -> Result<Option<String>> {
        self.fetch_page_content(&title.full_text()).await
    }

    async fn get_template(&self, title: &Title) -> Result<Option<String>> {
        self.fetch_template(&title.full_text()).await
    }

    async fn get_module(&self, title: &Title) -> Result<Option<String>> {
        self.fetch_module(&title.full_text()).await
    }

    async fn get_file_info(&self, title: &Title) -> Result<Option<FileInfo>> {
        self.fetch_file_info(&title.full_text()).await
    }

    async fn resolve_redirect(&self, title: &Title) -> Result<Option<Title>> {
        if let Some(target) = self.fetch_redirect(&title.full_text()).await? {
            Ok(Some(Title::new_main(target)))
        } else {
            Ok(None)
        }
    }

    async fn get_message(&self, _lang: &str, key: &str) -> Result<Option<String>> {
        // Fetch from MediaWiki namespace
        let msg_title = format!("MediaWiki:{key}");
        self.fetch_page_content(&msg_title).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_api_source() {
        let source = MediaWikiApiDataSource::new("https://en.wikipedia.org/w/api.php");
        assert!(source.is_ok());
    }

    #[test]
    fn test_create_with_ttl() {
        let source = MediaWikiApiDataSource::with_ttl("https://en.wikipedia.org/w/api.php", 60);
        assert!(source.is_ok());
    }

    #[test]
    fn test_cache_put_get() {
        let source = MediaWikiApiDataSource::new("https://example.com/api.php").unwrap();
        source.cache_put("key".to_string(), "value".to_string());
        let cached = source.get_cached("key");
        assert_eq!(cached, Some("value".to_string()));
    }

    #[test]
    fn test_cache_miss() {
        let source = MediaWikiApiDataSource::new("https://example.com/api.php").unwrap();
        let cached = source.get_cached("nonexistent");
        assert_eq!(cached, None);
    }

    #[test]
    fn test_cache_expiry() {
        let source = MediaWikiApiDataSource::with_ttl("https://example.com/api.php", 0).unwrap();
        source.cache_put("key".to_string(), "value".to_string());
        // With TTL 0, it should be expired immediately
        std::thread::sleep(std::time::Duration::from_millis(1));
        let cached = source.get_cached("key");
        assert_eq!(cached, None);
    }
}
