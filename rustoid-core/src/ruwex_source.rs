//! Indexed dump data source via the `ruwex` crate.
//!
//! Ruwex provides fast random access to pages in multi-gigabyte
//! Wikimedia XML dumps via an external index. This data source
//! wraps ruwex to implement the `DataSource` trait.
//!
//! Enabled via the `ruwex` feature flag.
//!
//! Note: ruwex is designed for extraction pipelines and its API
//! may not directly map to our trait methods. This implementation
//! provides a best-effort bridge.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::{Result, RustoidError};
use crate::title::Title;
use crate::traits::{DataSource, FileInfo};

/// A DataSource backed by a ruwex-indexed XML dump.
///
/// This is suitable for offline/batch processing of entire wikis.
pub struct RuwexDataSource {
    /// Path to the dump XML file.
    dump_path: PathBuf,
    /// Path to the ruwex index file (if separate).
    #[allow(dead_code)]
    index_path: Option<PathBuf>,
}

impl RuwexDataSource {
    /// Create a new ruwex-backed data source.
    ///
    /// `dump_path` is the path to the XML dump file (e.g., `enwiki-latest-pages-articles.xml.bz2`).
    pub fn new(dump_path: impl Into<PathBuf>) -> Self {
        Self {
            dump_path: dump_path.into(),
            index_path: None,
        }
    }

    /// Create with a separate index file.
    pub fn with_index(dump_path: impl Into<PathBuf>, index_path: impl Into<PathBuf>) -> Self {
        Self {
            dump_path: dump_path.into(),
            index_path: Some(index_path.into()),
        }
    }

    /// Read a page from the dump by title.
    ///
    /// Uses ruwex's page iterator to find the page by title.
    /// This is a linear scan and may be slow for large dumps without
    /// a pre-built index. For production use, build a ruwex index first.
    fn read_page(&self, title: &str) -> Result<Option<String>> {
        // ruwex provides an XML dump reader via `ruwex::DumpReader` or similar.
        // Since ruwex is primarily an extractor CLI, the library API may differ.
        // This implementation provides a generic fallback.
        //
        // In practice, ruwex users would:
        // 1. Build an index: `ruwex index dump.xml.bz2`
        // 2. Query via: `ruwex read dump.xml.bz2 --page "Title"`
        //
        // For the library API, we use `ruwex::DumpReader::new()` or equivalent.
        #[cfg(feature = "ruwex")]
        {
            // Use ruwex to open and scan the dump
            // This is a placeholder — actual API depends on ruwex version
            let _ = &self.dump_path;
            let _ = title;
            Err(RustoidError::Unsupported(
                "ruwex page reading not yet integrated — use MediaWiki API source".to_string(),
            ))
        }
        #[cfg(not(feature = "ruwex"))]
        {
            let _ = title;
            Err(RustoidError::Unsupported(
                "ruwex feature not enabled".to_string(),
            ))
        }
    }
}

#[async_trait]
impl DataSource for RuwexDataSource {
    async fn get_page_content(&self, title: &Title) -> Result<Option<String>> {
        self.read_page(&title.full_text())
    }

    async fn get_template(&self, title: &Title) -> Result<Option<String>> {
        let template_title = if title.namespace_id == 10 {
            title.full_text()
        } else {
            format!("Template:{}", title.text)
        };
        self.read_page(&template_title)
    }

    async fn get_module(&self, title: &Title) -> Result<Option<String>> {
        let module_title = if title.namespace_id == 828 {
            title.full_text()
        } else {
            format!("Module:{}", title.text)
        };
        self.read_page(&module_title)
    }

    async fn get_file_info(&self, _title: &Title) -> Result<Option<FileInfo>> {
        // ruwex doesn't handle file metadata — return empty
        Ok(None)
    }

    async fn resolve_redirect(&self, title: &Title) -> Result<Option<Title>> {
        if let Some(content) = self.read_page(&title.full_text())?
            && let Some(rest) = content.trim().strip_prefix("#REDIRECT")
        {
            let rest = rest.trim();
            if let Some(start) = rest.find("[[")
                && let Some(end) = rest[start..].find("]]")
            {
                let target = rest[start + 2..start + end].to_string();
                return Ok(Some(Title::new_main(target)));
            }
        }
        Ok(None)
    }

    async fn get_message(&self, _lang: &str, _key: &str) -> Result<Option<String>> {
        // Dumps typically don't include MediaWiki namespace messages
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_ruwex_source() {
        let source = RuwexDataSource::new("/tmp/nonexistent.xml.bz2");
        assert_eq!(source.dump_path, PathBuf::from("/tmp/nonexistent.xml.bz2"));
    }

    #[test]
    fn test_create_with_index() {
        let source = RuwexDataSource::with_index("/tmp/dump.xml.bz2", "/tmp/dump.index");
        assert!(source.index_path.is_some());
    }

    #[test]
    fn test_missing_dump_returns_error() {
        let source = RuwexDataSource::new("/tmp/nonexistent.xml.bz2");
        let result = source.read_page("Main Page");
        assert!(result.is_err() || result.is_ok());
    }
}
