//! Argument references must not be mistaken for transclusions.
//!
//! Found by running the online corpus. The page `Israel` never finished:
//! `RUSTOID_TRACE_FETCH=1` showed `Template:Def` requested thousands of times.
//! `Template:Def` does not exist on enwiki, and the string `Def` appears in no
//! input as a transclusion — so the parser was manufacturing the request.
//!
//! The source of it is a three-brace argument reference:
//!
//! ```text
//! Template:Yesno-no  →  {{safesubst:<noinclude />yesno|{{{1}}}|...|def={{{def|no}}}}}
//! Template:Yesno     →  ... |#default = {{{def|{{{yes|yes}}}}}}
//! ```
//!
//! `{{{def|no}}}` contains the substring `{{def|`. If the argument reference is
//! not recognised as one — because the enclosing call is itself unusual, e.g. a
//! `safesubst:` target carrying `<noinclude />`, or because the braces nest three
//! deep — the text is re-tokenized and `{{def|no}}` is read as a transclusion of
//! `Template:Def`.
//!
//! That is worse than a wrong render. Because the cycle around it repeats, the
//! bogus request recurs too: 1813 requests for `Template:Def` in one page parse,
//! 54379 requests in total for a warm-cache run that never terminated. Online
//! each miss is a network round trip, which is what made the page look like it
//! had hung.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustoid_core::traits::{DataSource, FileInfo};
use rustoid_core::{Parser, ParserOptions, Title};

/// A data source that records every template it is asked for.
///
/// Requests are recorded rather than fetches, because the bug is a request for
/// a title that no input mentions — visible whether or not the template exists.
struct Recording {
    templates: HashMap<String, String>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Recording {
    fn new(templates: &[(&str, &str)]) -> (Self, Arc<Mutex<Vec<String>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let source = Self {
            templates: templates
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            requests: Arc::clone(&requests),
        };
        (source, requests)
    }
}

#[async_trait]
impl DataSource for Recording {
    async fn get_page_content(&self, _title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(None)
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

    async fn resolve_redirect(&self, _title: &Title) -> rustoid_core::Result<Option<Title>> {
        Ok(None)
    }

    async fn get_message(&self, _lang: &str, _key: &str) -> rustoid_core::Result<Option<String>> {
        Ok(None)
    }
}

/// Parse `wikitext`, returning every template title requested.
async fn requests_for(templates: &[(&str, &str)], wikitext: &str) -> Vec<String> {
    let (source, requests) = Recording::new(templates);
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = Parser::new(&config);
    let options = ParserOptions::for_page("Test page");
    let _ = parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await;
    requests.lock().unwrap().clone()
}

/// The real `Template:Yesno` body, which nests arguments three deep.
const YESNO: &str = "{{<includeonly>safesubst:</includeonly>#switch: {{<includeonly>safesubst:</includeonly>lc: {{{1|¬}}} }}\n |no\n |n\n |f\n |false\n |off\n |0        = {{{no|}}}\n |         = {{{blank|{{{no|}}}}}}\n |¬        = {{{¬|}}}\n |yes\n |y\n |t\n |true\n |on\n |1        = {{{yes|yes}}}\n |#default = {{{def|{{{yes|yes}}}}}}\n}}<noinclude>\n{{Documentation}}\n</noinclude>";

/// The real `Template:Yesno-no` body, whose last argument is `{{{def|no}}}`.
const YESNO_NO: &str = "{{safesubst:<noinclude />yesno|{{{1}}}|yes={{{yes|yes}}}|no={{{no|no}}}|blank={{{blank|no}}}|¬={{{¬|no}}}|def={{{def|no}}}}}<noinclude>\n{{Documentation|Template:Yesno/doc}}\n</noinclude>";

/// An argument name must never be turned into a template request.
///
/// This is the reproduction of the runaway. The assertion is deliberately about
/// the *request*, not the HTML: the HTML merely looked wrong, whereas the
/// request is what cost thousands of round trips.
#[tokio::test]
async fn an_argument_reference_is_not_requested_as_a_template() {
    let got = requests_for(
        &[("Template:Yesno", YESNO), ("Template:Yesno-no", YESNO_NO)],
        "{{Yesno-no|}}",
    )
    .await;

    let bogus: Vec<&String> = got
        .iter()
        .filter(|t| t.contains("Def") || t.contains("def"))
        .collect();
    assert!(
        bogus.is_empty(),
        "an argument name was requested as a template: {bogus:?}\nall requests: {got:?}"
    );
}

/// The simplest form of the same mistake: `{{{def|no}}}` alone.
#[tokio::test]
async fn a_three_brace_argument_is_not_a_transclusion() {
    let got = requests_for(&[], "{{{def|no}}}").await;
    assert!(
        got.is_empty(),
        "a bare argument reference produced requests: {got:?}"
    );
}

/// A nested argument used as a named parameter value.
#[tokio::test]
async fn a_nested_argument_value_is_not_a_transclusion() {
    let got = requests_for(&[("Template:Outer", "x")], "{{Outer|def={{{def|no}}}}}").await;
    assert!(
        !got.iter().any(|t| t.to_lowercase().contains("def")),
        "the argument value was read as a transclusion: {got:?}"
    );
}

/// The nested three-deep form from `Template:Yesno`'s `#default` branch.
#[tokio::test]
async fn a_three_deep_nested_argument_is_not_a_transclusion() {
    let got = requests_for(
        &[("Template:Outer", "x")],
        "{{Outer|v={{{def|{{{yes|yes}}}}}}}}",
    )
    .await;
    assert!(
        !got.iter().any(|t| t.to_lowercase().contains("def")),
        "deeply nested arguments were read as a transclusion: {got:?}"
    );
}

/// A self-transcluding template must be fetched a bounded number of times.
#[tokio::test]
async fn a_self_transcluding_template_is_not_requested_forever() {
    let got = requests_for(&[("Template:Loop", "before {{Loop}} after")], "{{Loop}}").await;
    assert!(
        got.len() <= 50,
        "a self-transcluding template was requested {} times",
        got.len()
    );
}

/// Two templates transcluding each other must terminate too.
#[tokio::test]
async fn a_mutual_transclusion_cycle_terminates() {
    let got = requests_for(
        &[("Template:A", "a {{B}}"), ("Template:B", "b {{A}}")],
        "{{A}}",
    )
    .await;
    assert!(
        got.len() <= 50,
        "a mutual cycle produced {} requests",
        got.len()
    );
}

/// A single missing template is requested once, not repeatedly.
#[tokio::test]
async fn a_single_missing_template_is_requested_once() {
    let got = requests_for(&[], "{{Absent}}").await;
    assert_eq!(got, vec!["Template:Absent".to_string()]);
}
