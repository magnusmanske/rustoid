//! `{{PROTECTIONLEVEL:…}}` and `{{PROTECTIONEXPIRY:…}}`.
//!
//! These are core magic words, but unlike `{{SITENAME}}` their answers come from
//! data no site configuration carries: what protection a page actually has. So
//! the parser takes them from the data source before expansion, and these tests
//! check that path — including the two shapes that are easy to get wrong.
//!
//! The expected values are the live wiki's, read from the transform endpoint:
//! `{{PROTECTIONLEVEL:edit|Canada}}` is `extendedconfirmed` and
//! `{{PROTECTIONEXPIRY:edit|Template:Infobox}}` is `infinity`.

use std::collections::HashMap;

use rustoid_core::mock::{MockDataSource, MockSiteConfig};
use rustoid_core::traits::ProtectionEntry;
use rustoid_core::{Parser, ParserOptions};

/// A data source that reports a fixed protection entry for one title.
struct ProtectedSource {
    title: String,
    entry: ProtectionEntry,
    inner: MockDataSource,
}

#[async_trait::async_trait]
impl rustoid_core::traits::DataSource for ProtectedSource {
    async fn get_page_content(
        &self,
        title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<String>> {
        self.inner.get_page_content(title).await
    }

    async fn get_template(
        &self,
        title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<String>> {
        self.inner.get_template(title).await
    }

    async fn get_module(
        &self,
        title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<String>> {
        self.inner.get_module(title).await
    }

    async fn get_file_info(
        &self,
        _title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<rustoid_core::traits::FileInfo>> {
        Ok(None)
    }

    async fn resolve_redirect(
        &self,
        title: &rustoid_core::Title,
    ) -> rustoid_core::Result<Option<rustoid_core::Title>> {
        self.inner.resolve_redirect(title).await
    }

    async fn get_message(&self, key: &str, lang: &str) -> rustoid_core::Result<Option<String>> {
        self.inner.get_message(key, lang).await
    }

    async fn get_page_info(
        &self,
        titles: &[String],
    ) -> rustoid_core::Result<HashMap<String, rustoid_core::traits::PageInfo>> {
        self.inner.get_page_info(titles).await
    }

    async fn get_title_protection(
        &self,
        titles: &[String],
    ) -> rustoid_core::Result<HashMap<String, ProtectionEntry>> {
        // Answer for the configured title and for `Can ada` — the second is what
        // the parser is expected to ask for when a call writes `Can_ada`, since
        // MediaWiki normalises underscores to spaces. Any *other* title is left
        // unanswered, so the miss path is exercised rather than stubbed away.
        let mut out = HashMap::new();
        for title in titles {
            if title.eq_ignore_ascii_case(&self.title) || title == "Can ada" {
                out.insert(title.clone(), self.entry.clone());
            }
        }
        Ok(out)
    }
}

/// Render `wikitext` as `page_title`, with `protected` holding that page's
/// protection.
async fn render(page_title: &str, wikitext: &str, entry: ProtectionEntry) -> String {
    let mut config = MockSiteConfig::new();
    // Both are registered the way the wiki registers them — as *magic words*
    // (`siteinfo` reports them in `magicwords`, not only in `functionhooks`),
    // which is what sends them down the magic-word arm rather than the
    // parser-function one. Registering them the other way round is what let a
    // real bug hide: the mock took the parser-function path and the wiki took
    // this one, so the tests passed while every live answer was empty.
    config.add_magic_word("protectionlevel", &["PROTECTIONLEVEL"]);
    config.add_magic_word("protectionexpiry", &["PROTECTIONEXPIRY"]);
    let config = config;
    let source = ProtectedSource {
        title: page_title.to_string(),
        entry,
        inner: MockDataSource::new(),
    };
    let parser = Parser::new(&config);
    parser
        .wikitext_to_html_expanded(wikitext, &source, &ParserOptions::for_page(page_title))
        .await
        .unwrap_or_else(|e| panic!("parse failed: {e}"))
}

fn protected() -> ProtectionEntry {
    let mut entry = ProtectionEntry::default();
    entry
        .levels
        .insert("edit".to_string(), vec!["extendedconfirmed".to_string()]);
    entry
        .expiries
        .insert("edit".to_string(), vec!["infinity".to_string()]);
    entry
}

#[tokio::test]
async fn a_level_is_reported_for_the_page_being_parsed() {
    let html = render("Canada", "{{PROTECTIONLEVEL:edit}}", protected()).await;
    assert!(html.contains("extendedconfirmed"), "got: {html}");
}

#[tokio::test]
async fn a_level_is_reported_for_a_named_title() {
    let html = render("Canada", "{{PROTECTIONLEVEL:edit|Canada}}", protected()).await;
    assert!(html.contains("extendedconfirmed"), "got: {html}");
}

/// An action the wiki does not list is unprotected, and the answer is the empty
/// string rather than a placeholder. `Module:Effective protection level`
/// compares the value against `'sysop'`/`'templateeditor'` and treats anything
/// else as "not that level", so a placeholder would change which branch runs.
#[tokio::test]
async fn an_unprotected_action_answers_with_nothing() {
    // Wrapped in non-empty text: a bare empty element is removed by p-wrapping,
    // and the assertion would then hold vacuously. `<span>` rather than an
    // invented tag, which would be escaped as text instead of becoming an
    // element.
    let html = render(
        "Canada",
        "<span>a{{PROTECTIONLEVEL:move}}b</span>",
        protected(),
    )
    .await;
    assert!(
        html.contains(">a</span>") || html.contains(">a<span"),
        "got: {html}"
    );
}

/// The expiry shape is load-bearing: `Module:Effective protection expiry`
/// matches `'^(%d%d%d%d)(%d%d)(%d%d)(%d%d)(%d%d)(%d%d)$'` and raises a visible
/// error otherwise, so `infinity` has to pass through *unsplit*.
#[tokio::test]
async fn an_infinite_protection_reports_the_word_infinity() {
    let html = render("Canada", "{{PROTECTIONEXPIRY:edit}}", protected()).await;
    assert!(html.contains("infinity"), "got: {html}");
}

/// A 14-digit expiry is passed through as the 14 digits, not reformatted.
#[tokio::test]
async fn a_dated_protection_reports_its_timestamp() {
    let mut entry = protected();
    entry
        .expiries
        .insert("edit".to_string(), vec!["20260102123456".to_string()]);
    let html = render("Canada", "{{PROTECTIONEXPIRY:edit}}", entry).await;
    assert!(html.contains("20260102123456"), "got: {html}");
}

/// A title the parser never looked up reads as unprotected rather than as an
/// error. That is MediaWiki's answer for a page that does not exist.
#[tokio::test]
async fn an_unknown_title_is_unprotected() {
    let html = render(
        "Canada",
        "<span>a{{PROTECTIONLEVEL:edit|Elsewhere}}b</span>",
        protected(),
    )
    .await;
    assert!(
        html.contains(">a</span>") || html.contains(">a<span"),
        "got: {html}"
    );
}

/// MediaWiki normalises underscores to spaces, so a title written with an
/// underscore must find the entry stored under the spaced spelling.
#[tokio::test]
async fn a_title_is_matched_after_normalisation() {
    let html = render("Canada", "{{PROTECTIONLEVEL:edit|Can_ada}}", protected()).await;
    // The stub stores `Can ada`; the call writes it with an underscore, so this
    // value is what proves the normalising lookup ran.
    assert!(html.contains("extendedconfirmed"), "got: {html}");
}

/// A different title is looked up, and a title that is not the page's own is
/// answered from its own entry rather than from the page's.
#[tokio::test]
async fn a_different_title_does_not_borrow_the_pages_protection() {
    let html = render(
        "Canada",
        "<span>a{{PROTECTIONLEVEL:edit|Brazil}}b</span>",
        protected(),
    )
    .await;
    assert!(
        html.contains(">a</span>") || html.contains(">a<span"),
        "got: {html}"
    );
    assert!(html.contains(">b</span>"), "got: {html}");
}

/// The `{{#…}}` spelling, which is what a *module* produces.
///
/// `frame:callParserFunction('PROTECTIONEXPIRY', …)` renders the call as
/// wikitext and re-expands it, so the word arrives with a leading hash — and
/// an unhandled hash spelling returns the call *source* verbatim, which
/// `Module:Effective protection expiry` then fails to match as a timestamp. It
/// reported "internal error: malformed expiry timestamp" on 34 of 48 corpus
/// pages, from a call the page-level scan cannot even see.
#[tokio::test]
async fn the_hash_spelling_is_answered_too() {
    let html = render("Canada", "{{#protectionlevel:edit|Canada}}", protected()).await;
    assert!(html.contains("extendedconfirmed"), "got: {html}");
}

/// The routing is the thing that broke, and it is invisible in the output.
///
/// On a real wiki these two are *magic words*, which sends them down the
/// variable arm — not the parser-function arm — even though they take a colon
/// argument. A mock that registered them the other way exercised different code
/// and passed while every live answer was empty, so the routing is pinned here.
#[tokio::test]
async fn the_magic_words_are_recognised_as_magic_words() {
    let mut config = MockSiteConfig::new();
    config.add_magic_word("protectionlevel", &["PROTECTIONLEVEL"]);
    let target = rustoid_core::pipeline::template_handler::resolve_template_target(
        &config,
        None,
        "PROTECTIONLEVEL:edit|Canada",
    );
    match target {
        Some(rustoid_core::pipeline::template_handler::ResolvedTarget::Variable {
            name, ..
        }) => {
            assert_eq!(name, "protectionlevel");
        }
        other => panic!("expected a magic-variable resolution, got {other:?}"),
    }
}
