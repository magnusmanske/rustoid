//! Cite end to end: `<ref>` markers and the `<references>` note list.
//!
//! The unit tests in `ext::cite` pin the data model and the DOM shapes. These
//! check the integration: that Cite runs inside a real parse, that both halves
//! agree on numbering, and that the list renders where the tag is.
//!
//! Shapes come from the cached Parsoid output of `Zebra`, which is why the ids and
//! classes asserted here are the specific ones a wiki serves.

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

async fn render(wikitext: &str) -> String {
    render_with(wikitext, &[]).await
}

async fn render_with(wikitext: &str, templates: &[(&str, &str)]) -> String {
    let source = Wiki {
        templates: templates
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = Parser::new(&config);
    let options = ParserOptions::for_page("Zebra");
    parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await
        .unwrap()
}

/// A `<ref>` becomes a marker with Cite's classes and ids.
#[tokio::test]
async fn a_ref_renders_a_marker() {
    let html = render("Text<ref>Body</ref> more.").await;
    assert!(
        html.contains("<sup class=\"mw-ref reference\""),
        "the ref must become a sup marker: {html}"
    );
    assert!(html.contains("typeof=\"mw:Extension/ref\""), "{html}");
    assert!(html.contains("class=\"mw-reflink-text\""), "{html}");
    assert!(html.contains("class=\"cite-bracket\""), "{html}");
    assert!(
        !html.contains("name=\"ref\""),
        "the raw extension element must be gone: {html}"
    );
}

/// The marker's anchor points at the note, using Cite's two id spellings.
///
/// The marker is `cite_ref-<name>_<n>-<use>` and the note `cite_note-<name>-<n>`,
/// with *different* separators. Getting one wrong produces a page whose footnote
/// links go nowhere.
#[tokio::test]
async fn the_marker_links_to_its_note() {
    let html = render("A<ref name=\"src\">Body</ref>").await;
    assert!(html.contains("id=\"cite_ref-src_1-0\""), "{html}");
    assert!(
        html.contains("href=\"./Zebra#cite_note-src-1\""),
        "the anchor must target the note: {html}"
    );
}

/// `<references>` renders the note list, with the note's body inside it.
#[tokio::test]
async fn references_renders_the_note_list() {
    let html = render("A<ref name=\"src\">Note body</ref>\n<references/>").await;
    assert!(
        html.contains("mw-references references"),
        "the list must render: {html}"
    );
    assert!(
        html.contains("id=\"cite_note-src-1\""),
        "the note must exist: {html}"
    );
    assert!(
        html.contains("Note body"),
        "the note's body belongs in the list, not at the call site: {html}"
    );
    assert!(
        html.contains("mw-reference-text"),
        "the body span must be marked: {html}"
    );
}

/// A note's body must not appear as *text* at the call site — only its marker does.
///
/// This is the property that makes Cite a two-part feature: a `<ref>`'s content is
/// rendered in the list, so a naive implementation leaves the text in both places.
///
/// The body legitimately appears a second time inside the marker's `data-mw` as
/// `extsrc`, which is where Parsoid records the ref's wikitext for round-tripping.
/// So the assertion is on text outside any tag, not on a raw substring.
#[tokio::test]
async fn the_note_body_is_not_rendered_at_the_call_site() {
    let html = render("A<ref name=\"src\">UNIQUE_NOTE_TEXT</ref>\n<references/>").await;
    let occurrences = visible_text(&html).matches("UNIQUE_NOTE_TEXT").count();
    assert_eq!(
        occurrences, 1,
        "the body belongs in the list exactly once: {html}"
    );
}

/// The text outside any `<…>` in a rendering.
///
/// Attribute values are excluded: a `data-mw` blob legitimately quotes wikitext,
/// so counting it would report every ref as duplicated.
fn visible_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Numbering follows first use, not source position of the list.
#[tokio::test]
async fn notes_are_numbered_by_first_use() {
    let html = render("A<ref>one</ref>B<ref>two</ref>\n<references/>").await;
    assert!(html.contains("cite_note--1"), "{html}");
    assert!(html.contains("cite_note--2"), "{html}");
    // The markers show the numbers.
    assert!(html.contains(">[</span>1<span"), "{html}");
    assert!(html.contains(">[</span>2<span"), "{html}");
}

/// A named ref used twice shares one note and gets two back-links.
///
/// The second use is self-closing, which is the whole point of naming a ref.
#[tokio::test]
async fn a_repeated_named_ref_shares_one_note() {
    let html = render("A<ref name=\"s\">body</ref>B<ref name=\"s\"/>\n<references/>").await;
    assert_eq!(
        html.matches("id=\"cite_note-s-1\"").count(),
        1,
        "one note for two uses: {html}"
    );
    assert!(html.contains("cite_ref-s_1-0"), "{html}");
    assert!(
        html.contains("cite_ref-s_1-1"),
        "the second use gets its own marker id: {html}"
    );
    assert!(
        html.contains("mw-cite-backlink"),
        "two uses are wrapped in the back-link span: {html}"
    );
}

/// A single use renders a bare back-link with an arrow, not a wrapped numbered one.
#[tokio::test]
async fn a_single_use_has_a_bare_arrow_backlink() {
    let html = render("A<ref name=\"s\">body</ref>\n<references/>").await;
    assert!(
        !html.contains("mw-cite-backlink"),
        "one use must not be wrapped: {html}"
    );
    assert!(
        html.contains("\u{2191}"),
        "the back-link text is an arrow: {html}"
    );
}

/// A ref with no matching `<references>` still renders a marker.
///
/// The alternative — dropping the ref because its list is absent — would silently
/// lose a citation, which is worse than a marker pointing at a missing anchor.
#[tokio::test]
async fn a_ref_without_a_references_tag_still_renders() {
    let html = render("Text<ref>Body</ref>").await;
    assert!(html.contains("mw-ref reference"), "{html}");
    assert!(!html.contains("name=\"ref\""), "{html}");
}

/// A ref inside a transclusion is collected like any other.
///
/// This is the common real case: citations come from infobox parameters
/// (`{{Fossil range|…|ref=<ref name=…/>}}`), so a Cite implementation that only
/// understood top-level refs would miss most of a page's citations.
#[tokio::test]
async fn a_ref_inside_a_template_is_collected() {
    let html = render_with(
        "{{Box|cite=<ref name=\"intpl\">from a template</ref>}}\n<references/>",
        &[("Template:Box", "<span>{{{cite|}}}</span>")],
    )
    .await;
    assert!(
        html.contains("cite_ref-intpl_1-0"),
        "the ref in the template must be collected: {html}"
    );
    assert!(html.contains("from a template"), "{html}");
}

/// A group numbers independently of the main group.
#[tokio::test]
async fn a_group_numbers_separately() {
    let html =
        render("A<ref group=\"notes\">n</ref>B<ref>m</ref>\n<references group=\"notes\"/>").await;
    assert!(html.contains("cite_note--1"), "{html}");
    assert!(
        html.contains("data-mw-group=\"notes\""),
        "the list carries its group: {html}"
    );
}

/// The real entry point: `{{reflist}}` emits a `<references>` tag.
///
/// This is how nearly every article gets its note list, so the `<references>`
/// element almost always arrives *through* a template rather than as literal
/// wikitext. A Cite implementation that only found a literal `<references>` would
/// render markers pointing at a list that never appears.
///
/// `Template:Reflist` is a large module-backed template on enwiki; the contract
/// being tested is the `#tag:references` call it lowers to, which is why a minimal
/// stand-in is faithful here.
#[tokio::test]
async fn a_reflist_template_renders_the_list() {
    let html = render_with(
        "A<ref>one</ref>\n{{reflist}}",
        &[(
            "Template:Reflist",
            "<div class=\"reflist\">{{#tag:references}}</div>",
        )],
    )
    .await;
    assert!(
        html.contains("mw-references references"),
        "the list must render from inside the template: {html}"
    );
    assert!(html.contains("cite_note--1"), "{html}");
}

/// An empty `<references>` and a `<references>` with no refs both must not crash.
#[tokio::test]
async fn a_references_tag_with_no_refs_renders_an_empty_list() {
    let html = render("No refs here.\n<references/>").await;
    assert!(
        html.contains("mw-references references"),
        "an empty list is still a list: {html}"
    );
}
