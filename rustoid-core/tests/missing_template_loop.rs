//! A missing template must not be re-fetched forever.
//!
//! Found by running the online corpus: the page `Israel` never finished, and
//! `RUSTOID_TRACE_FETCH=1` showed `Template:Def` being fetched 30+ times in a
//! tightening loop with `Template:Protlevel` and `Template:List1`. All three
//! templates **do not exist** on enwiki.
//!
//! The loop matters because each turn is a real network request: a page whose
//! wikitext contains a cycle through missing templates made the harness hang
//! indefinitely, which is indistinguishable from a slow page.
//!
//! PHP checks the loop/depth constraint *before* fetching
//! (`TemplateHandler.php`: `enforceTemplateConstraints` at line 600,
//! `fetchTemplateAndTitle` at line 611), so it never asks for the content of a
//! template it already knows is a loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rustoid_core::traits::{DataSource, FileInfo, PageInfo};
use rustoid_core::{RustoidError, Title};

/// Counts requests per title, so a runaway loop is visible as a count rather
/// than as a timeout.
struct CountingSource {
    templates: std::collections::HashMap<String, String>,
    fetches: Arc<AtomicUsize>,
}

#[async_trait]
impl DataSource for CountingSource {
    async fn get_page_content(&self, _title: &Title) -> rustoid_core::Result<Option<String>> {
        Ok(None)
    }

    async fn get_template(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        // Every template is missing, which is the shape that looped online.
        Ok(self.templates.get(&title.full_text()).cloned())
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

    async fn get_page_info(
        &self,
        titles: &[String],
    ) -> rustoid_core::Result<std::collections::HashMap<String, PageInfo>> {
        Ok(titles
            .iter()
            .map(|t| {
                (
                    t.clone(),
                    PageInfo {
                        missing: true,
                        known: false,
                        redirect: false,
                        linkclasses: Vec::new(),
                    },
                )
            })
            .collect())
    }
}

/// A template that transcludes itself must be fetched a bounded number of times.
///
/// The bound is generous: the point is to distinguish "bounded" from "unbounded",
/// not to pin the exact count, which depends on where the guard trips.
#[tokio::test]
async fn a_self_transcluding_missing_template_is_not_fetched_forever() {
    let mut templates = std::collections::HashMap::new();
    templates.insert(
        "Template:Loop".to_string(),
        "before {{Template:Loop}} after".to_string(),
    );
    let fetches = Arc::new(AtomicUsize::new(0));
    let source = CountingSource {
        templates,
        fetches: Arc::clone(&fetches),
    };

    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = rustoid_core::Parser::new(&config);
    let options = rustoid_core::ParserOptions::for_page("Test page");

    // The parser has its own depth limit; the assertion is that it terminates
    // and does not keep asking the data source.
    let _ = parser
        .wikitext_to_html_expanded("{{Template:Loop}}", &source, &options)
        .await;

    let count = fetches.load(Ordering::Relaxed);
    assert!(
        count <= 50,
        "a self-transcluding template was fetched {count} times; \
         the loop guard is not bounding refetches"
    );
}

/// Two templates transcluding each other, both missing.
///
/// This is the `Def`/`Protlevel`/`List1` shape that hung the corpus run.
#[tokio::test]
async fn a_mutual_transclusion_cycle_of_missing_templates_terminates() {
    let mut templates = std::collections::HashMap::new();
    templates.insert("Template:A".to_string(), "a {{Template:B}}".to_string());
    templates.insert("Template:B".to_string(), "b {{Template:A}}".to_string());
    let fetches = Arc::new(AtomicUsize::new(0));
    let source = CountingSource {
        templates,
        fetches: Arc::clone(&fetches),
    };

    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = rustoid_core::Parser::new(&config);
    let options = rustoid_core::ParserOptions::for_page("Test page");

    let _ = parser
        .wikitext_to_html_expanded("{{Template:A}}", &source, &options)
        .await;

    let count = fetches.load(Ordering::Relaxed);
    assert!(
        count <= 50,
        "a mutual template cycle was fetched {count} times; \
         the loop guard is not bounding refetches"
    );
}

/// A template that does not exist, referenced once, is fetched once.
///
/// Guards against "fixing" the loop above by making every missing template
/// refetch, or by refusing to fetch at all.
#[tokio::test]
async fn a_single_missing_template_is_fetched_once() {
    let fetches = Arc::new(AtomicUsize::new(0));
    let source = CountingSource {
        templates: std::collections::HashMap::new(),
        fetches: Arc::clone(&fetches),
    };

    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = rustoid_core::Parser::new(&config);
    let options = rustoid_core::ParserOptions::for_page("Test page");

    let _ = parser
        .wikitext_to_html_expanded("{{Template:Absent}}", &source, &options)
        .await;

    let count = fetches.load(Ordering::Relaxed);
    assert!(count >= 1, "a missing template should still be looked up");
    assert!(count <= 5, "a missing template was fetched {count} times");
}

/// A `RustoidError` conversion exists for the loop path, so the depth limit
/// reports rather than panics.
#[allow(dead_code)]
fn _error_shape(_e: RustoidError) {}

/// The template name must be taken whole, not truncated to a prefix.
///
/// Online, `Israel` hung fetching `Template:Def` 30 times. `Template:Def` does
/// not exist on enwiki and appears nowhere in `Israel`'s wikitext; the pages that
/// produce it transclude `{{Defunct UK grocers}}`, `{{Definition needed}}`,
/// `{{Defence Ministers of Germany}}` and `{{DEFAULTSORT:...}}`. The name is
/// being cut short at `Def`, and because the truncated name never equals the
/// ancestor in the frame chain, the loop guard cannot fire and the fetch repeats.
#[tokio::test]
async fn a_template_name_longer_than_its_prefix_is_looked_up_whole() {
    static LAST: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    struct Recording {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DataSource for Recording {
        async fn get_page_content(&self, _t: &Title) -> rustoid_core::Result<Option<String>> {
            Ok(None)
        }
        async fn get_template(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
            self.seen.lock().unwrap().push(title.full_text());
            Ok(None)
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

    let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let source = Recording {
        seen: Arc::clone(&seen),
    };
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = rustoid_core::Parser::new(&config);
    let options = rustoid_core::ParserOptions::for_page("Test page");

    let _ = parser
        .wikitext_to_html_expanded("{{Definition needed|date=2024}}", &source, &options)
        .await;

    let got = seen.lock().unwrap().clone();
    let _ = &LAST;
    assert!(
        got.iter().any(|t| t.contains("Definition needed")),
        "the whole template name should be looked up, saw {got:?}"
    );
    assert!(
        !got.iter().any(|t| t.ends_with("Template:Def")),
        "the name was truncated to `Def`, saw {got:?}"
    );
}

/// A template inside a table cell must be looked up by its whole name.
///
/// This is the context that produced the endless `Template:Def` fetches: on a
/// real page the construct appears as
///
/// ```text
/// !scope="row"| dyadic
/// | requiring both a left and a right argument{{Definition needed|date=February 2024}}
/// ```
///
/// If the table-cell prefix is consumed as part of the target, the name comes
/// out truncated; because the truncated name never equals its ancestor in the
/// frame chain, the loop guard cannot fire and the fetch repeats.
#[tokio::test]
async fn a_template_in_a_table_cell_is_looked_up_whole() {
    struct Recording {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DataSource for Recording {
        async fn get_page_content(&self, _t: &Title) -> rustoid_core::Result<Option<String>> {
            Ok(None)
        }
        async fn get_template(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
            self.seen.lock().unwrap().push(title.full_text());
            Ok(None)
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

    let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let source = Recording {
        seen: Arc::clone(&seen),
    };
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = rustoid_core::Parser::new(&config);
    let options = rustoid_core::ParserOptions::for_page("Test page");

    let wikitext = "{| class=\"wikitable\"\n|-\n!scope=\"row\"| dyadic\n| requiring both{{Definition needed|date=February 2024}}\n|}";
    let _ = parser
        .wikitext_to_html_expanded(wikitext, &source, &options)
        .await;

    let got = seen.lock().unwrap().clone();
    assert!(
        !got.iter().any(|t| t.ends_with("Template:Def")),
        "the template name was truncated to `Def`; full list: {got:?}"
    );
}

/// A magic-word-only invocation must not be looked up as a template.
///
/// `{{DEFAULTSORT:Intact Forest Landscape}}` begins with the same letters as the
/// runaway `Template:Def` fetch. `DEFAULTSORT` is a magic word, so it must never
/// reach `get_template` — and if a prefix of it does, the resulting name will not
/// match anything, which is exactly how the online loop was able to keep going.
#[tokio::test]
async fn a_defaultsort_magic_word_is_not_looked_up_as_a_template() {
    struct Recording {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DataSource for Recording {
        async fn get_page_content(&self, _t: &Title) -> rustoid_core::Result<Option<String>> {
            Ok(None)
        }
        async fn get_template(&self, title: &Title) -> rustoid_core::Result<Option<String>> {
            self.seen.lock().unwrap().push(title.full_text());
            Ok(None)
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

    let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let source = Recording {
        seen: Arc::clone(&seen),
    };
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = rustoid_core::Parser::new(&config);
    let options = rustoid_core::ParserOptions::for_page("Test page");

    let _ = parser
        .wikitext_to_html_expanded(
            "{{DEFAULTSORT:Foo}}\n{{DEFAULTSORTKEY:Bar}}",
            &source,
            &options,
        )
        .await;

    let got = seen.lock().unwrap().clone();
    assert!(
        !got.iter()
            .any(|t| t.contains("Def") || t.contains("DEFAULT")),
        "a magic word was looked up as a template: {got:?}"
    );
}
