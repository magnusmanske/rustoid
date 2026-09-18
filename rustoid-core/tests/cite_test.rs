//! Cite: `<ref>` renders a numbered marker, `<references>` the note list.
//!
//! The tests assert the *shape* Parsoid emits, taken from the cached output of
//! `Zebra` (`rustoid-compare`'s corpus cache) rather than invented. That matters
//! because Cite's HTML is dense with derived ids and classes, and a plausible
//! guess is indistinguishable from the real thing without a reference.
//!
//! Two facts about the reference are load-bearing for reading these tests:
//!
//! - The `<sup>` marker's `id` and the note's `id` use *different* separators
//!   (`cite_ref-NAME_1-0` vs `cite_note-NAME-1`). That is Cite's output.
//! - A note used once renders its back-link bare with a `↑`; used twice or more,
//!   the links are wrapped in `mw-cite-backlink` and labelled `1`, `2`, …

use std::collections::HashMap;

use async_trait::async_trait;
use rustoid_core::ext::cite::CiteState;
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
    let source = Wiki {
        templates: HashMap::new(),
    };
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = Parser::new(&config);
    let options = ParserOptions::for_page("Zebra");
    parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await
        .unwrap()
}

/// The state machine against the real page's first reference.
///
/// `Zebra` opens with `<ref name="Badenhorst2019"/>` inside a template, and the
/// cached Parsoid output for it is exactly these ids.
#[test]
fn the_pages_first_reference_yields_cites_ids() {
    let mut st = CiteState::new();
    let marker = st.add("Badenhorst2019", "", "", true);
    assert_eq!(marker, "cite_ref-Badenhorst2019_1-0");
    assert_eq!(st.references[0].note_id(), "cite_note-Badenhorst2019-1");
    // The number is 1 because it is the first ref *used*, not because it is first
    // in the source — which is what makes this a document-order problem.
    assert_eq!(st.references[0].number, 1);
    assert_eq!(st.references[0].label(), "1");
}

/// What the parser currently does with a `<ref>`, recorded rather than asserted.
///
/// Cite dispatch into the pipeline is not wired yet, so this documents the
/// starting point: the ref survives as an element rather than becoming a marker.
/// When the wiring lands, this test is where the marker shape gets asserted.
#[tokio::test]
async fn a_ref_is_currently_left_as_an_element() {
    let html = render("Text<ref name=\"a\">Body</ref> more.").await;
    println!("REF RENDER: {html}");
}
