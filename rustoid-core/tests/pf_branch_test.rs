//! A parser function returns a *branch*, and a branch is wikitext.
//!
//! The three failures pinned here all looked like unrelated bugs:
//!
//! - `{{#ifeq:1|1|{{Large|1=x}}|z}}` emitted `<template Large="" 1="x">` — the
//!   raw token, serialized by the DOM builder as an element named after it.
//! - `{{#if:1|X|Y}}{{PAGENAME}}` numbered its wrappers `#mwt1` and `#mwt3`, so
//!   every element after a parser function carried an id one too high.
//! - `{{#ifeq:S|exclude||[[Category:{{P|d}}]]}}` rendered as literal text.
//!
//! They share a cause in the token stream: a branch's tokens were spliced
//! verbatim instead of being fed back through the expander the way PHP's
//! handler return values are. The wikitext is the *same* in each case; what
//! differs is what the branch holds — a template, a page-scoped word, a link
//! whose target is templated.

use std::collections::HashMap;

use async_trait::async_trait;
use rustoid_core::traits::{DataSource, FileInfo};
use rustoid_core::{Parser, ParserOptions, Title};

struct Wiki {
    pages: HashMap<String, String>,
}

#[async_trait]
impl DataSource for Wiki {
    async fn get_page_content(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(self.pages.get(&title.full_text()).cloned())
    }

    async fn get_template(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(self.pages.get(&title.full_text()).cloned())
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

async fn render(templates: &[(&str, &str)], wikitext: &str) -> String {
    let source = Wiki {
        pages: templates
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

/// A template call in a `#if`/`#ifeq` branch is expanded, not leaked as a
/// `template` token for the DOM builder to name an element after.
#[tokio::test]
async fn a_template_in_a_parser_function_branch_expands() {
    let html = render(
        &[("Template:Large", "LARGE")],
        "A{{#ifeq:1|1|{{Large}}|z}}C",
    )
    .await;
    assert!(html.contains("LARGE"), "branch did not expand: {html}");
    assert!(
        !html.contains("<template"),
        "the raw template token leaked: {html}"
    );
}

/// About ids are taken by the expansions that are wrapped, so two wrapped
/// expansions in a row number consecutively. Taking an id up front for every
/// token and discarding it for a parser function left a gap.
#[tokio::test]
async fn parser_function_wrappers_number_consecutively() {
    let html = render(&[], "{{#if:1|X|Y}}{{PAGENAME}}").await;
    assert!(
        html.contains("about=\"#mwt1\""),
        "first wrapper should be #mwt1: {html}"
    );
    assert!(
        html.contains("about=\"#mwt2\""),
        "second wrapper should be #mwt2 (no gap): {html}"
    );
    assert!(
        !html.contains("about=\"#mwt3\""),
        "an id was skipped: {html}"
    );
}

/// A wikilink whose target holds a template call is a token like any other, so
/// the enclosing parser function still resolves. The tokenizer read the inner
/// `}}` as "still inside the link" and abandoned the whole scan.
#[tokio::test]
async fn a_link_holding_a_template_does_not_break_the_call() {
    let html = render(
        &[("Template:P", "Page")],
        "{{#ifeq:S|exclude||[[Category:{{P}}]]}}",
    )
    .await;
    assert!(
        html.contains("Category:Page"),
        "the branch did not resolve to a link: {html}"
    );
    assert!(
        !visible_text(&html).contains("{{"),
        "the call was emitted as literal text: {html}"
    );
}

/// A template in a `{{{p|…}}}` default is expanded, and the `{{{`/`}}}` are not
/// left behind as text. The scanner counted the nested template's `}}` as part
/// of the tplarg's `}}}`, so the closer came up one brace short and the whole
/// reference spilled out.
#[tokio::test]
async fn a_template_in_a_tplarg_default_expands() {
    let html = render(&[("Template:Large", "LARGE")], "A{{{p|{{Large}}}}}C").await;
    assert!(html.contains("LARGE"), "default did not expand: {html}");
    assert!(
        !visible_text(&html).contains("{{"),
        "the tplarg was emitted as literal text: {html}"
    );
}

/// Only the branches that stay *tokens* are marked as expanded attributes.
///
/// Core answers `#if`/`#ifeq`/`#ifexpr`/`#iferror` with `trim($frame->expand(…))`
/// — a string, which the parser re-tokenizes — so the wikitext target inside such
/// a branch is already resolved when the attribute pass sees it, and the service
/// does not mark it `mw:ExpandedAttrs`. The same target written directly on the
/// page *is* marked, and so is one in a `#switch` branch, because core answers
/// that with the branch's original tokens (`PPFrame::RECOVER_ORIG`).
#[tokio::test]
async fn only_non_text_branches_are_marked_as_expanded_attributes() {
    const TARGET: &str = "[[Category:{{PAGENAME}}]]";

    let page = render(&[], TARGET).await;
    assert!(
        page.contains("mw:ExpandedAttrs"),
        "page-level target: {page}"
    );

    let switch = render(&[], &format!("{{{{#switch:1|1={TARGET}}}}}")).await;
    assert!(
        switch.contains("mw:ExpandedAttrs"),
        "a #switch branch keeps its tokens: {switch}"
    );

    for (label, wikitext) in [
        ("if", format!("{{{{#if:1|{TARGET}}}}}")),
        ("ifeq", format!("{{{{#ifeq:1|1|{TARGET}}}}}")),
        ("ifexpr", format!("{{{{#ifexpr:1|{TARGET}}}}}")),
        (
            "iferror",
            format!("{{{{#iferror:{{{{#expr:1/0}}}}|{TARGET}}}}}"),
        ),
    ] {
        let html = render(&[], &wikitext).await;
        assert!(
            html.contains("Category:Test"),
            "#{label} lost the link: {html}"
        );
        assert!(
            !html.contains("mw:ExpandedAttrs"),
            "#{label} expands its branch to text, so it must not be marked: {html}"
        );
    }
}

/// The text outside tags. A `data-mw`/`data-parsoid` attribute legitimately
/// carries source wikitext, so a whole-document search for `{{` reports every
/// correctly-rendered transclusion as unexpanded.
fn visible_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}
