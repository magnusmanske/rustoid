//! `{{#invoke:…}}` end to end, through the parser.
//!
//! The engine's own tests cover the Lua runtime; these cover the wiring, which
//! is where the corpus found the gap: `#invoke` used to fall through to the
//! parser-function default and be emitted as literal `{{#invoke:…}}` text, which
//! is why 43 of 44 corpus pages failed.

use rustoid_core::mock::{MockDataSource, MockSiteConfig};
use rustoid_core::{Parser, ParserOptions};

/// Expand `wikitext` with `modules` available as `Module:` pages.
async fn expand(modules: &[(&str, &str)], wikitext: &str) -> String {
    let config = MockSiteConfig::new();
    let source = MockDataSource::new();
    for (title, body) in modules {
        source.add_module(title, body);
    }
    let parser = Parser::new(&config);
    parser
        .wikitext_to_html_expanded(wikitext, &source, &ParserOptions::for_page("Test"))
        .await
        .unwrap_or_else(|e| panic!("parse failed: {e}"))
}

const GREET: &str = r#"
    local p = {}
    function p.main(frame)
        return "hello " .. frame.args[1]
    end
    return p
"#;

/// Everything outside `<…>`: the rendered text, which is where unexpanded
/// wikitext would appear. Attributes legitimately carry `{{…}}` as `data-mw`
/// provenance, so a raw `contains("{{")` would ask the wrong question.
fn text_only(html: &str) -> String {
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

#[tokio::test]
async fn invoke_calls_a_module_function() {
    let html = expand(&[("Module:Greet", GREET)], "{{#invoke:Greet|main|world}}").await;
    assert!(html.contains("hello world"), "got: {html}");
    // The call itself must be gone from the *text*: leaving `{{#invoke:…}}`
    // there is the exact failure this wiring exists to fix. It still appears
    // inside `data-mw`, which is provenance rather than output.
    assert!(!text_only(&html).contains("#invoke"), "got: {html}");
}

#[tokio::test]
async fn invoke_passes_named_arguments() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return frame.args["who"] .. "=" .. frame.args["1"]
        end
        return p
    "#;
    let html = expand(
        &[("Module:Args", module)],
        "{{#invoke:Args|main|first|who=bob}}",
    )
    .await;
    assert!(html.contains("bob=first"), "got: {html}");
}

/// A module that requires another must be able to, which is what the preload
/// pass is for: `require` is synchronous inside Lua.
#[tokio::test]
async fn invoke_supports_require_between_modules() {
    let helper = "local m = {} m.answer = 'from helper' return m";
    let module = r#"
        local helper = require('Module:Helper')
        local p = {}
        function p.main(frame)
            return helper.answer
        end
        return p
    "#;
    let html = expand(
        &[("Module:Helper", helper), ("Module:Entry", module)],
        "{{#invoke:Entry|main}}",
    )
    .await;
    assert!(html.contains("from helper"), "got: {html}");
}

/// Module output is wikitext and must be expanded in turn: a module that returns
/// a template call is a normal pattern.
#[tokio::test]
async fn module_output_is_expanded_as_wikitext() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return "'''bold''' from lua"
        end
        return p
    "#;
    let html = expand(&[("Module:Wiki", module)], "{{#invoke:Wiki|main}}").await;
    // The module's wikitext must have been parsed, not emitted verbatim.
    assert!(html.contains(">bold</b>"), "got: {html}");
    assert!(html.contains("from lua"), "got: {html}");
    assert!(!text_only(&html).contains("'''"), "got: {html}");
}

/// A missing module must look broken rather than vanishing, so the comparison
/// can see it.
#[tokio::test]
async fn a_missing_module_reports_a_script_error() {
    let html = expand(&[], "{{#invoke:Absent|main}}").await;
    assert!(html.contains("Script error"), "got: {html}");
    assert!(html.contains("Absent"), "got: {html}");
}

/// A Lua error inside a module must not panic the parser.
#[tokio::test]
async fn a_lua_error_reports_a_script_error() {
    let module = r#"
        local p = {}
        function p.main(frame)
            error("deliberate failure")
        end
        return p
    "#;
    let html = expand(&[("Module:Boom", module)], "{{#invoke:Boom|main}}").await;
    assert!(html.contains("Script error"), "got: {html}");
}

/// A missing entry point must also be reported, not silently empty.
#[tokio::test]
async fn a_missing_function_reports_a_script_error() {
    let html = expand(&[("Module:Greet", GREET)], "{{#invoke:Greet|nope}}").await;
    assert!(html.contains("Script error"), "got: {html}");
}

/// Without a data source there is nothing to fetch a module from, so the call
/// stays as source text — the same behaviour other unimplemented parser
/// functions have.
#[test]
fn without_a_data_source_the_call_is_left_as_source() {
    let config = MockSiteConfig::new();
    let parser = Parser::new(&config);
    let html = parser
        .wikitext_to_html("{{#invoke:Greet|main}}", &ParserOptions::for_page("Test"))
        .unwrap();
    assert!(html.contains("#invoke"), "got: {html}");
}

/// Temporary probe: does the Lua path itself produce text?
#[tokio::test]
async fn probe_direct_invoke() {
    let source = MockDataSource::new();
    source.add_module("Module:Greet", GREET);
    let site = rustoid_core::lua::engine::LuaSite::from_config(&MockSiteConfig::new());
    let out = rustoid_core::lua::invoke::invoke("Greet|main|world", &source, site, "Test").await;
    println!("DIRECT RESULT: {out:?}");
}

#[tokio::test]
async fn probe_parser_output() {
    let html = expand(&[("Module:Greet", GREET)], "{{#invoke:Greet|main|world}}").await;
    println!("FULL HTML ({} bytes):\n{html}", html.len());
}
