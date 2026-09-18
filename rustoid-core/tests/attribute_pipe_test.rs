//! A `|` must not split an argument list when it sits inside an HTML attribute
//! value.
//!
//! This was suspected to be a bug — recorded in `ONLINE-PARITY.md` as "a `|`
//! inside an attribute splits a parser-function argument" — because
//! `Template:Geological range marker` appeared to lose an `#ifexpr` branch.
//! Investigating it showed the tokenizer is already correct, and the apparent
//! failure had two other causes, neither of them this:
//!
//! - `{{#if:1|B}}` returning `B` *is* correct: `#if` yields its second argument
//!   when the condition is non-empty. An expectation of "unwrap the parser
//!   function" was simply wrong.
//! - `{{Real|5}}` binds `5` to `{{{1}}}`, not `{{{3}}}` — anonymous parameters
//!   are positional, so `{{{3}}}` in that template legitimately has no value.
//!   (help:Templates: "Identifying parameters by order … works only with
//!   anonymous parameters.")
//!
//! The tests are kept because they pin behaviour that is easy to break: the
//! tokenizer treats a recognized HTML tag as an atom precisely so that an
//! attribute's `|` cannot split the enclosing list, and that protection is
//! load-bearing for real templates (`style="…"` containing a parser function).
//! A regression here would be silent — the branch would render as raw wikitext.

use std::collections::HashMap;

use async_trait::async_trait;
use rustoid_core::traits::{DataSource, FileInfo};
use rustoid_core::{Parser, ParserOptions, Title};

struct Wiki {
    templates: HashMap<String, String>,
}

#[async_trait]
impl DataSource for Wiki {
    async fn get_page_content(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(self.templates.get(&title.full_text()).cloned())
    }

    async fn get_template(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(self.templates.get(&title.full_text()).cloned())
    }

    async fn get_module(&self, _t: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(None)
    }

    async fn get_file_info(&self, _t: &Title) -> rustoid_core::Result<Option<FileInfo>> {
        Ok(None)
    }

    async fn resolve_redirect(&self, _t: &Title) -> rustoid_core::Result<Option<Title>> {
        Ok(None)
    }

    async fn get_message(&self, _l: &str, _k: &str) -> rustoid_core::Result<Option<String>> {
        Ok(None)
    }
}

async fn render(template_body: &str, wikitext: &str) -> String {
    let source = Wiki {
        templates: [("Template:Real", template_body)]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = Parser::new(&config);
    let options = ParserOptions::for_page("Test page");
    parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await
        .unwrap()
}

/// A `|` in a quoted attribute value of an inline tag inside a parser-function
/// branch must not split the branch.
#[tokio::test]
async fn a_pipe_in_a_div_style_does_not_split_a_parser_function_argument() {
    let html = render(
        "<onlyinclude>{{#ifexpr:1>0|<div style=\"a:1; b:2; \">Y</div>|N}}</onlyinclude>",
        "{{Real}}",
    )
    .await;
    assert!(
        html.contains("Y"),
        "the taken branch must survive its attribute's pipes: {html}"
    );
}

/// A nested parser function inside an attribute must still be evaluated, which is
/// what the atom rule protects: if the attribute were re-scanned as text, `#expr`
/// would render literally rather than as its value.
#[tokio::test]
async fn a_parser_function_inside_an_attribute_is_evaluated() {
    let html = render(
        "<onlyinclude><div style=\"w:{{#expr:2+3}}px\">Y</div></onlyinclude>",
        "{{Real}}",
    )
    .await;
    assert!(html.contains("w:5px"), "the #expr must evaluate: {html}");
}

/// A parser function *with pipes* inside an attribute is evaluated too.
///
/// The pipes here are argument separators belonging to the inner call, and the
/// distinguishing detail is that `#if`'s second argument is the true branch — so
/// `B` is the correct output, not an unexpanded fragment.
#[tokio::test]
async fn a_piped_parser_function_inside_an_attribute_is_evaluated() {
    let html = render(
        "<onlyinclude><div style=\"w:{{#if:1|B}}px\">Y</div></onlyinclude>",
        "{{Real}}",
    )
    .await;
    assert!(html.contains("w:Bpx"), "{html}");
    assert!(
        !html.contains("{{#if"),
        "the call must not survive as wikitext: {html}"
    );
}

/// A genuine top-level `|` must still split, so the atom rule cannot be
/// over-applied.
#[tokio::test]
async fn a_real_pipe_still_splits_template_arguments() {
    let html = render("<onlyinclude>{{{1}}}-{{{2}}}</onlyinclude>", "{{Real|a|b}}").await;
    assert!(
        html.contains("a-b"),
        "arguments must still split on |: {html}"
    );
}

/// An argument holding an attribute with a `|` must not split the argument list.
#[tokio::test]
async fn a_pipe_in_a_template_argument_attribute_does_not_split() {
    let html = render(
        "<onlyinclude>{{{1}}}</onlyinclude>",
        "{{Real|<span data-x=\"p|q\">Z</span>}}",
    )
    .await;
    assert!(
        html.contains('Z'),
        "the argument must survive its attribute's pipe: {html}"
    );
}

/// Anonymous parameters are positional: `{{Real|5}}` defines `{{{1}}}` and leaves
/// `{{{3}}}` undefined, so its default applies.
///
/// Pinned because it looks like a bug and is not — it was misdiagnosed as one
/// while investigating the attribute question above.
#[tokio::test]
async fn anonymous_arguments_bind_positionally_not_by_name() {
    let html = render(
        "<onlyinclude>{{{1|}}}!{{{3|none}}}</onlyinclude>",
        "{{Real|5}}",
    )
    .await;
    assert!(
        html.contains("5!none"),
        "the value binds to the first parameter and the third is undefined: {html}"
    );
}
