//! `{{MediaWiki:…}}` transcludes an **interface message**, not a page.
//!
//! Found by running the online corpus. `Template:Compact TOC` transcludes
//! `{{MediaWiki:Toc}}`, and the service renders the message text ("Contents")
//! although `MediaWiki:Toc` has no page — `prop=info` reports it `missing` but
//! `known`, because a MediaWiki-namespace title resolves through `MessageCache`.
//! rustoid asked the page table, found nothing, and rendered a redlink.
//!
//! The other half of the rule is that the namespace is not a fallback for *any*
//! name: `MediaWiki:Sitename` is not a message key on enwiki (it is an
//! `allmessages` miss), and the service renders that one as a redlink.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustoid_core::traits::{DataSource, FileInfo};
use rustoid_core::{Parser, ParserOptions, Title};

/// A data source holding templates and interface messages.
struct Wiki {
    templates: HashMap<String, String>,
    messages: HashMap<(String, String), String>,
    /// Every `get_message` call, so a test can assert the key's normalisation.
    asked: Arc<Mutex<Vec<(String, String)>>>,
}

impl Wiki {
    fn new(templates: &[(&str, &str)], messages: &[(&str, &str, &str)]) -> Self {
        Self {
            templates: templates
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            messages: messages
                .iter()
                .map(|(l, k, v)| ((l.to_string(), k.to_string()), v.to_string()))
                .collect(),
            asked: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl DataSource for Wiki {
    async fn get_page_content(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(self.templates.get(&title.full_text()).cloned())
    }

    async fn get_template(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(self.templates.get(&title.full_text()).cloned())
    }

    async fn get_module(&self, _title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(None)
    }

    async fn get_file_info(
        &self,
        _title: &Title,
        _width: Option<u32>,
        _height: Option<u32>,
    ) -> rustoid_core::Result<Option<FileInfo>> {
        Ok(None)
    }

    async fn get_message(&self, lang: &str, key: &str) -> rustoid_core::Result<Option<String>> {
        self.asked
            .lock()
            .unwrap()
            .push((lang.to_string(), key.to_string()));
        Ok(self
            .messages
            .get(&(lang.to_string(), key.to_string()))
            .cloned())
    }

    async fn resolve_redirect(&self, _title: &Title) -> rustoid_core::Result<Option<Title>> {
        Ok(None)
    }
}

async fn render(
    messages: &[(&str, &str, &str)],
    wikitext: &str,
) -> (String, Vec<(String, String)>) {
    let source = Wiki::new(&[], messages);
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = Parser::new(&config);
    let options = ParserOptions::for_page("Test page");
    let html = parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await
        .unwrap();
    let asked = source.asked.lock().unwrap().clone();
    (html, asked)
}

/// A message is rendered as a transclusion of the text the message cache holds.
#[tokio::test]
async fn a_mediawiki_transclusion_renders_the_message() {
    let (html, asked) = render(&[("en", "toc", "Contents")], "A {{MediaWiki:Toc}} B").await;
    assert!(
        html.contains(">Contents<"),
        "the message text should be rendered: {html}"
    );
    assert!(
        !html.contains("MediaWiki:Toc</a>"),
        "a message must not render as a redlink: {html}"
    );
    assert_eq!(
        asked,
        vec![("en".to_string(), "toc".to_string())],
        "the message key is the title with a lower-cased first letter"
    );
}

/// A name the wiki has no message for is still a redlink, which is what the
/// service serves for `MediaWiki:Sitename`.
#[tokio::test]
async fn a_name_that_is_not_a_message_stays_a_redlink() {
    let (html, asked) = render(&[("en", "toc", "Contents")], "A {{MediaWiki:Sitename}} B").await;
    assert!(
        html.contains("MediaWiki:Sitename"),
        "the name should stay visible: {html}"
    );
    assert!(
        !html.contains("Contents"),
        "no message must be invented: {html}"
    );
    assert_eq!(asked, vec![("en".to_string(), "sitename".to_string())]);
}
