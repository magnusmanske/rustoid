//! `<templatestyles>` end to end: a stylesheet is fetched, scoped, and inlined.
//!
//! This is the extension tag behind `frame:extensionTag('templatestyles', …)`,
//! which `Module:Citation/CS1` and a dozen other modules call. It needs a page
//! fetch, so it runs as an async pass in the parser rather than in the
//! synchronous extension handler.
//!
//! The expected output is transcribed from the cached Parsoid output of the
//! corpus pages, not from the CSS specification: the comparison is byte-for-byte,
//! so "renders correctly" and "matches" are different properties and only the
//! second one counts here.

use rustoid_core::mock::{MockDataSource, MockSiteConfig};
use rustoid_core::{Parser, ParserOptions};

/// Expand `wikitext`, with an optional stylesheet page registered as `title`.
async fn expand(wikitext: &str, stylesheet: Option<(&str, &str, u64)>) -> String {
    let mut config = MockSiteConfig::new();
    // A parser function or tag the mock does not register resolves as a broken
    // call rather than reaching the implementation.
    config.add_extension_tag("templatestyles");
    let source = MockDataSource::new();
    if let Some((title, body, revid)) = stylesheet {
        source.add_template(title, body);
        source.set_revision(title, revid);
    }
    let parser = Parser::new(&config);
    parser
        .wikitext_to_html_expanded(wikitext, &source, &ParserOptions::for_page("Test"))
        .await
        .unwrap_or_else(|e| panic!("parse failed: {e}"))
}

/// The worked example: `Module:Hatnote/styles.css` at revision 1368532237, whose
/// rendered form appears on 45 corpus pages.
#[tokio::test]
async fn a_stylesheet_is_fetched_scoped_and_inlined() {
    let css = "/* {{pp|small=y}} */\n.hatnote {\n\tfont-style: italic;\n}\n\n\
               @media print {\n\tbody.ns-0 .hatnote {\n\t\tdisplay: none !important;\n\t}\n}";
    let html = expand(
        "<templatestyles src=\"Module:Hatnote/styles.css\" />",
        Some(("Module:Hatnote/styles.css", css, 1368532237)),
    )
    .await;

    // The whole element, as Parsoid emits it. Split across lines only for
    // readability; the assertions below pin the parts that matter.
    assert!(
        html.contains(r#"data-mw-deduplicate="TemplateStyles:r1368532237""#),
        "dedup key comes from the fetched revision: {html}"
    );
    assert!(
        html.contains(r#"typeof="mw:Extension/templatestyles""#),
        "extension marker: {html}"
    );
    assert!(
        html.contains(
            r#"data-mw='{"name":"templatestyles","attrs":{"src":"Module:Hatnote/styles.css"},"body":{"extsrc":""}}'"#
        ),
        "data-mw records the source page: {html}"
    );
    assert!(
        html.contains(
            ">.mw-parser-output .hatnote{font-style:italic}\
@media print{body.ns-0 .mw-parser-output .hatnote{display:none!important}}</style>"
        ),
        "the css is scoped and minified, and `body` keeps the scope after it: {html}"
    );
}

/// A bare `src` is in the Template namespace, which is how the corpus's most
/// common stylesheet is written (`Hlist/styles.css`).
#[tokio::test]
async fn a_bare_src_resolves_in_the_template_namespace() {
    let html = expand(
        "<templatestyles src=\"Hlist/styles.css\" />",
        Some(("Template:Hlist/styles.css", ".a{b:1}", 7)),
    )
    .await;
    assert!(
        html.contains(r#"attrs":{"src":"Hlist/styles.css"}"#),
        "the attribute is kept as written, not rewritten to the full title: {html}"
    );
    assert!(
        html.contains(".mw-parser-output .a{b:1}"),
        "the stylesheet was found and inlined: {html}"
    );
}

/// The `wrapper` attribute replaces the scope, which is what keeps a sandbox
/// copy of a stylesheet separate from the live one.
#[tokio::test]
async fn a_wrapper_replaces_the_scope() {
    let html = expand(
        "<templatestyles src=\"X.css\" wrapper=\".sandbox\" />",
        Some(("Template:X.css", ".a{b:1}", 1)),
    )
    .await;
    assert!(
        html.contains(".sandbox .a{b:1}"),
        "wrapper used as the scope: {html}"
    );
    assert!(
        !html.contains(".mw-parser-output .a"),
        "the default scope must not also be applied: {html}"
    );
}

/// A stylesheet that cannot be resolved is left as the unexpanded tag, so the
/// failure is visible in the diff rather than silently emitting an empty style.
#[tokio::test]
async fn an_unresolvable_stylesheet_is_left_alone() {
    // No such page.
    let html = expand("<templatestyles src=\"Nope/styles.css\" />", None).await;
    assert!(
        html.contains(r#"name="templatestyles""#) && html.contains("<extension"),
        "a missing page leaves the tag visible: {html}"
    );
    assert!(!html.contains("<style"), "nothing was inlined: {html}");

    // A page with no revision cannot be pinned, so it is not inlined either.
    let source = MockDataSource::new();
    source.add_template("Template:Y.css", ".a{b:1}");
    let mut config = MockSiteConfig::new();
    config.add_extension_tag("templatestyles");
    let parser = Parser::new(&config);
    let html = parser
        .wikitext_to_html_expanded(
            "<templatestyles src=\"Y.css\" />",
            &source,
            &ParserOptions::for_page("Test"),
        )
        .await
        .unwrap();
    assert!(
        !html.contains("<style"),
        "an unresolvable revision must not produce a guessed dedup key: {html}"
    );
}

/// A stylesheet that resolves but sanitises to nothing produces an empty
/// `<style>`, which is what Parsoid does for a comment-only page.
#[tokio::test]
async fn an_empty_stylesheet_still_emits_the_element() {
    let html = expand(
        "<templatestyles src=\"Empty.css\" />",
        Some(("Template:Empty.css", "/* nothing here */", 3)),
    )
    .await;
    assert!(
        html.contains(r#"data-mw-deduplicate="TemplateStyles:r3""#),
        "the element is still emitted: {html}"
    );
    assert!(
        html.contains("></style>"),
        "with no stylesheet text: {html}"
    );
}

/// The module form of the tag, reached the way the corpus reaches it: through
/// `frame:extensionTag`, which lowers to `#tag`.
#[tokio::test]
async fn the_lua_frame_method_reaches_the_same_handler() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return frame:extensionTag('templatestyles', '', { src = 'Z.css' })
        end
        return p
    "#;
    let mut config = MockSiteConfig::new();
    config.add_extension_tag("templatestyles");
    let source = MockDataSource::new();
    source.add_module("Module:T", module);
    source.add_template("Template:Z.css", ".z{color:red}");
    source.set_revision("Template:Z.css", 99);
    let parser = Parser::new(&config);
    let html = parser
        .wikitext_to_html_expanded(
            "{{#invoke:T|main}}",
            &source,
            &ParserOptions::for_page("Test"),
        )
        .await
        .unwrap();
    assert!(
        html.contains(r#"data-mw-deduplicate="TemplateStyles:r99""#),
        "the module's call inlined the stylesheet: {html}"
    );
    assert!(
        html.contains(".mw-parser-output .z{color:red}"),
        "scoped css: {html}"
    );
}
