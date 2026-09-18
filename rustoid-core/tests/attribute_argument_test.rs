//! A template argument used inside an HTML **attribute** must resolve.
//!
//! Found by comparing `Zebra` against the wiki's own Parsoid, then isolated to a
//! direct transclusion with no redirect involved. The minimal case:
//!
//! ```text
//! {{Real|small=yes}}          Template:Real = <span data-small="{{{small|DEFAULT}}}">x</span>
//! ```
//!
//! rustoid renders `data-small="DEFAULT"`; Parsoid renders `data-small="yes"`.
//! The same reference in *text* resolves correctly, so the bug is specifically
//! attribute position.
//!
//! Why: `Frame::expand` walks only the **top level** of a token chunk, and an
//! attribute value is a `KeyValue::Tokens` *inside* the `Tag` token, so the
//! `templatearg` there is never visited during the template body expansion. It
//! survives into `expand_attributes`, which runs against the root frame — where
//! the template's arguments do not exist — so the default is taken.
//!
//! That this is reachable on real pages is not a guess: `Template:Fossil range
//! bar` and `Template:Geological range marker` both put `{{{1}}}`, `{{{2}}}` and
//! `{{{3}}}` inside `style="…"`, and infoboxes routinely do the same.

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

/// The minimal reproduction: one named argument, used in one attribute.
#[tokio::test]
async fn a_named_argument_in_an_attribute_resolves() {
    let html = render(
        "<span data-small=\"{{{small|DEFAULT}}}\">x</span>",
        "{{Real|small=yes}}",
    )
    .await;
    assert!(
        html.contains("data-small=\"yes\""),
        "the argument must resolve in attribute position: {html}"
    );
    assert!(
        !html.contains("DEFAULT"),
        "the default must not be taken when the argument is supplied: {html}"
    );
}

/// The default must still apply when the argument is absent.
///
/// Without this, "fix" could mean substituting an empty string everywhere.
#[tokio::test]
async fn an_absent_argument_in_an_attribute_takes_the_default() {
    let html = render(
        "<span data-small=\"{{{small|DEFAULT}}}\">x</span>",
        "{{Real}}",
    )
    .await;
    assert!(html.contains("data-small=\"DEFAULT\""), "{html}");
}

/// A positional argument in an attribute, which is the form the real templates
/// use (`style="…{{{1}}}…"`).
#[tokio::test]
async fn a_positional_argument_in_an_attribute_resolves() {
    let html = render("<span style=\"width:{{{1|}}}px\">x</span>", "{{Real|250}}").await;
    assert!(
        html.contains("width:250px"),
        "a positional argument must resolve in attribute position: {html}"
    );
}

/// Several arguments in one attribute, mixed with a parser function — the shape
/// `Template:Geological range marker` actually has.
#[tokio::test]
async fn several_arguments_in_one_attribute_resolve() {
    let html = render(
        "<span style=\"left:{{#expr:{{{1|0}}}+{{{2|0}}}}}px\">x</span>",
        "{{Real|3|4}}",
    )
    .await;
    assert!(
        html.contains("left:7px"),
        "arguments inside a parser function inside an attribute must resolve: {html}"
    );
}

/// Arguments nested directly in an attribute, via a parser function, with no
/// enclosing template-modelled attribute — the innermost shape.
#[tokio::test]
async fn arguments_inside_a_parser_function_in_an_attribute_resolve() {
    let html = render(
        "<span style=\"left:{{#expr:{{{1|0}}}*2}}px\">x</span>",
        "{{Real|5}}",
    )
    .await;
    assert!(
        html.contains("left:10px"),
        "a parser function inside an attribute must see the arguments: {html}"
    );
}

/// The same nesting in *text* position, to separate "a parser function does not
/// see its arguments" from "an attribute does not".
///
/// The parser function's result is wrapped in marker spans, so `10` is not
/// contiguous with the `left:` around it — the assertion is on the value, not on
/// the assembled attribute string.
#[tokio::test]
async fn arguments_inside_a_parser_function_in_text_resolve() {
    let html = render("left:{{#expr:{{{1|0}}}*2}}px", "{{Real|5}}").await;
    assert!(
        html.contains(">10</span>"),
        "a parser function in text position must see the arguments: {html}"
    );
}

/// The real `Template:Geological_range_marker`'s inner `style`, inlined.
///
/// This is the shape that motivated the fix, taken from the corpus cache rather than
/// invented: it puts `{{{1}}}` and `{{{2}}}` inside `#expr`, inside `style`, twice,
/// along with a `{{{3}}}`-guarded opacity. The arguments are required rather than
/// defaulted, so an unresolved reference renders a broken style — which is what a
/// reader would actually see wrong.
const GEOLOGICAL_STYLE: &str = "<onlyinclude><div style=\"position:absolute; height:8px; left:{{#expr:(650-{{{1}}})/650*250}}px; width:{{#expr:({{{1}}}-{{{2}}})*250/650}}px; color:inherit; background-color:#360; opacity:{{#if:{{{3|}}}|0.{{{3}}}|1}}; \"></div></onlyinclude>";

/// Arguments must resolve inside `#expr` inside `style`, as the real template uses
/// them.
///
/// The arithmetic is the assertion rather than the mere absence of `{{{`: an
/// unresolved reference leaves `{{{1}}}` in the expression and renders an error,
/// which would pass a weaker "no braces" check while still being wrong. The
/// expected values are `#expr`'s real output — `650-590` over 650 times 250 is
/// `23.076923`, since `#expr` divides in floating point.
#[tokio::test]
async fn the_real_geological_style_resolves_its_arguments() {
    let html = render(GEOLOGICAL_STYLE, "{{Real|590|540}}").await;
    assert!(
        html.contains("left:23.076923px"),
        "the first argument must resolve inside #expr inside style: {html}"
    );
    assert!(
        html.contains("width:19.230769px"),
        "the argument difference must resolve too: {html}"
    );
    assert!(
        !html.contains("{{{"),
        "no argument reference may survive into the output: {html}"
    );
}

/// The `{{{3|}}}`-guarded opacity, which mixes a defaulted and a required
/// reference in one attribute.
#[tokio::test]
async fn a_defaulted_and_required_reference_coexist_in_one_attribute() {
    // With `3` supplied, opacity is `0.5`.
    let html = render(GEOLOGICAL_STYLE, "{{Real|590|540|5}}").await;
    assert!(html.contains("opacity:0.5;"), "{html}");

    // Without it, the default `1` applies — and the required references still
    // resolve, so the two paths cannot have been conflated.
    let html = render(GEOLOGICAL_STYLE, "{{Real|590|540}}").await;
    assert!(html.contains("opacity:1;"), "{html}");
}

/// A nested template's argument must resolve against *its own* frame, not the
/// outer one — the failure mode of expanding attributes too late is that the
/// innermost template's arguments get looked up in the caller.
#[tokio::test]
async fn an_inner_template_argument_uses_the_inner_frame() {
    let source = Wiki {
        templates: [
            (
                "Template:Outer",
                "{{{outer|}}}\n{{Inner|inner={{{inner|}}}}}",
            ),
            ("Template:Inner", "<i data-v=\"{{{inner|NONE}}}\">i</i>"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    };
    let config = rustoid_core::mock::MockSiteConfig::new();
    let parser = Parser::new(&config);
    let options = ParserOptions::for_page("Test page");
    let html = parser
        .wikitext_to_html_expanded("{{Outer|outer=O|inner=I}}", &source, &options)
        .await
        .unwrap();
    assert!(
        html.contains("data-v=\"I\""),
        "the inner template's argument belongs to the inner frame: {html}"
    );
}
