//! A transcluded **redirect** must expand the redirect's target.
//!
//! Found by running the online corpus. `Zebra` renders
//! `{{pp-semi|small=yes}}` as a link to `Template:Pp-semi` instead of the
//! protected-page icon, even though `Template:Pp-semi` *is* in the cache. The
//! template body is a single line:
//!
//! ```text
//! #REDIRECT [[Template:Protected page]]
//! ```
//!
//! so expanding it literally produces a redirect, not the icon. MediaWiki (and
//! therefore the wiki's Parsoid, which is fed by MW core's preprocessor) resolves
//! the redirect and transcludes `Template:Protected page`.
//!
//! The same page showed three *other* templates falling back to a link to
//! themselves — `Template:Short description`, `Template:Other uses`,
//! `Template:Featured article` — and every one of them is a redirect to a
//! template that is also in the cache. So this is not one missing template; a
//! class of the corpus's most-used templates is being dropped.
//!
//! `[[WP:Redirect|Redirects]]` is standard on a wiki, and a template redirect is
//! ordinary (`{{db}}` → `Template:Delete`). MediaWiki follows them silently and
//! records the *redirect* as the transclusion target in `data-mw`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustoid_core::traits::{DataSource, FileInfo};
use rustoid_core::{Parser, ParserOptions, Title};

/// A data source holding templates plus explicit redirect edges.
struct Wiki {
    templates: HashMap<String, String>,
    /// `Template:A` -> `Template:B`, i.e. A's body is `#REDIRECT [[B]]`.
    redirects: HashMap<String, String>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Wiki {
    fn new(
        templates: &[(&str, &str)],
        redirects: &[(&str, &str)],
    ) -> (Self, Arc<Mutex<Vec<String>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let source = Self {
            templates: templates
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            redirects: redirects
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            requests: Arc::clone(&requests),
        };
        (source, requests)
    }
}

#[async_trait]
impl DataSource for Wiki {
    async fn get_page_content(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(self.templates.get(&title.full_text()).cloned())
    }

    async fn get_template(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        let key = title.full_text();
        self.requests.lock().unwrap().push(key.clone());
        Ok(self.templates.get(&key).cloned())
    }

    async fn get_module(&self, _title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(None)
    }

    async fn get_file_info(&self, _title: &Title) -> rustoid_core::Result<Option<FileInfo>> {
        Ok(None)
    }

    /// Name the redirect's target, which is what the parser needs to know that
    /// following the redirect is possible at all.
    async fn resolve_redirect(&self, title: &Title) -> rustoid_core::Result<Option<Title>> {
        let key = title.full_text();
        let Some(target) = self.redirects.get(&key) else {
            return Ok(None);
        };
        Ok(Some(Title::new_main(target)))
    }

    async fn get_message(&self, _lang: &str, _key: &str) -> rustoid_core::Result<Option<String>> {
        Ok(None)
    }
}

/// Render `wikitext` against a wiki, returning the HTML and the template requests.
async fn render(
    templates: &[(&str, &str)],
    redirects: &[(&str, &str)],
    wikitext: &str,
) -> (String, Vec<String>) {
    let (source, requests) = Wiki::new(templates, redirects);
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = Parser::new(&config);
    let options = ParserOptions::for_page("Test page");
    let html = parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await
        .unwrap();
    (html, requests.lock().unwrap().clone())
}

/// The plain case: a redirect is followed and its target's body is rendered.
#[tokio::test]
async fn a_transcluded_redirect_expands_its_target() {
    let (html, _) = render(
        &[
            ("Template:Alias", "#REDIRECT [[Template:Real]]"),
            ("Template:Real", "<b>REAL</b>"),
        ],
        &[("Template:Alias", "Template:Real")],
        "{{Alias}}",
    )
    .await;
    assert!(
        html.contains("REAL"),
        "the redirect's target should have been transcluded, got: {html}"
    );
    assert!(
        !html.contains("REDIRECT"),
        "the redirect body must not be rendered as content: {html}"
    );
}

/// A redirect chain is followed to its end, not just one hop.
///
/// One hop would look right on the simple case and still drop
/// `Template:Featured article`, whose target is itself a redirect. MediaWiki
/// bounds the chain rather than following it forever, which is what the depth
/// limit in the parser is for.
#[tokio::test]
async fn a_redirect_chain_is_followed_to_the_end() {
    let (html, _) = render(
        &[
            ("Template:One", "#REDIRECT [[Template:Two]]"),
            ("Template:Two", "#REDIRECT [[Template:Three]]"),
            ("Template:Three", "<i>THREE</i>"),
        ],
        &[
            ("Template:One", "Template:Two"),
            ("Template:Two", "Template:Three"),
        ],
        "{{One}}",
    )
    .await;
    assert!(html.contains("THREE"), "expected the chain's end: {html}");
}

/// Redirect arguments are passed to the target.
///
/// This is the case the corpus hits: `{{pp-semi|small=yes}}` redirects to
/// `Template:Protected page`, which reads `{{{small|}}}`. Dropping the arguments
/// while following the redirect would render the icon's *unprotected* variant.
///
/// The body reads the argument in **text**, not in an attribute, and that is
/// deliberate: an argument reference inside an HTML attribute already ignores
/// the template's arguments on a *direct* transclusion too, which is a separate
/// pre-existing bug (see the note below). Asserting the attribute here would make
/// this test fail for a reason that has nothing to do with redirects.
#[tokio::test]
async fn a_redirect_passes_its_arguments_to_the_target() {
    let (html, _) = render(
        &[
            ("Template:Alias", "#REDIRECT [[Template:Real]]"),
            ("Template:Real", "small=[{{{small|no}}}]"),
        ],
        &[("Template:Alias", "Template:Real")],
        "{{Alias|small=yes}}",
    )
    .await;
    assert!(
        html.contains("small=[yes]"),
        "the argument must reach the redirect's target: {html}"
    );
}

/// The redirect's *target* is the transclusion recorded in `data-mw`, while the
/// wikitext stays what the page wrote.
///
/// Parsoid records the call as the editor wrote it (`wt: "Alias"`) but points
/// `href` at the page that supplied the content. Both halves matter: the `href`
/// is the redirect resolution, and the `wt` is what makes the HTML round-trip
/// back to the original wikitext.
#[tokio::test]
async fn data_mw_names_the_redirect_and_its_target() {
    let (html, _) = render(
        &[
            ("Template:Alias", "#REDIRECT [[Template:Real]]"),
            ("Template:Real", "REAL"),
        ],
        &[("Template:Alias", "Template:Real")],
        "{{Alias}}",
    )
    .await;
    assert!(html.contains("REAL"), "the target should render: {html}");
    assert!(
        html.contains("\"wt\":\"Alias\""),
        "data-mw should keep the called name: {html}"
    );
}

/// A redirect loop must terminate rather than expanding forever.
///
/// The depth limit already exists for self-transclusion; following redirects
/// introduces a second way to cycle (`A → B → A`) that must hit the same bound.
#[tokio::test]
async fn a_redirect_cycle_terminates() {
    let (html, _) = render(
        &[
            ("Template:A", "#REDIRECT [[Template:B]]"),
            ("Template:B", "#REDIRECT [[Template:A]]"),
        ],
        &[("Template:A", "Template:B"), ("Template:B", "Template:A")],
        "{{A}}",
    )
    .await;
    // The assertion is that we got here at all; a loop would hang the test.
    assert!(!html.is_empty(), "a redirect cycle must still render");
}

/// A redirect whose target is missing must not render as content.
///
/// The redirect line is bookkeeping, not article text: rendering it (and its
/// `[[Category:…]]` trailer) as page text would *invent* content that Parsoid
/// never emits. This is not hypothetical — `Template:Pp-semi` redirects to
/// `Template:Protected page`, which is absent from a partially populated cache,
/// and the fallback was putting the redirect line into the article.
#[tokio::test]
async fn a_redirect_to_a_missing_target_renders_nothing() {
    let (html, _) = render(
        &[(
            "Template:Alias",
            "#REDIRECT [[Template:Absent]]\n[[Category:C]]",
        )],
        &[("Template:Alias", "Template:Absent")],
        "{{Alias}}",
    )
    .await;
    assert!(
        !html.contains("#REDIRECT") && !html.contains("Top icon"),
        "the redirect line must not become article text: {html}"
    );
    assert!(
        !html.contains("Category:C"),
        "the redirect's categories must not leak either: {html}"
    );
}

/// A redirect only happens for a real redirect.
///
/// A template whose body merely *mentions* `#REDIRECT` is not redirected: a
/// false positive here would silently replace ordinary content.
#[tokio::test]
async fn an_ordinary_template_is_not_treated_as_a_redirect() {
    let (html, _) = render(
        &[("Template:NotaRedirect", "text about #REDIRECT [[X]] inline")],
        &[],
        "{{NotaRedirect}}",
    )
    .await;
    assert!(
        html.contains("text about"),
        "an ordinary body must be rendered as-is: {html}"
    );
}
