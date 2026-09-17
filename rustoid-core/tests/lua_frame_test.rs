//! `frame:expandTemplate` and `frame:preprocess` end to end.
//!
//! These are the frame methods that need the parser itself, so they cannot run
//! inside Lua: the call is deferred, expanded with the real async pipeline, and
//! the module re-run with the answer in place. The tests below cover the
//! interesting consequences of that design — arguments computed at runtime,
//! several calls in one module, and the module's own output still being parsed.
//!
//! `Module:Multiple image` and `Module:ConvertIB` are the corpus modules that
//! forced this; they call `expandTemplate` with argument values the wikitext
//! never contained, which is why preloading cannot answer them.

use rustoid_core::mock::{MockDataSource, MockSiteConfig};
use rustoid_core::{Parser, ParserOptions};

/// Expand `wikitext` with `modules` as `Module:` pages and `templates` as
/// `Template:` pages.
async fn expand(modules: &[(&str, &str)], templates: &[(&str, &str)], wikitext: &str) -> String {
    let mut config = MockSiteConfig::new();
    // A parser function the mock does not register would resolve as a *broken*
    // call rather than reaching the implementation, so the ones these tests use
    // are declared here.
    config.add_function_hook("uc");
    let config = config;
    let source = MockDataSource::new();
    for (title, body) in modules {
        source.add_module(title, body);
    }
    for (title, body) in templates {
        source.add_template(title, body);
    }
    let parser = Parser::new(&config);
    parser
        .wikitext_to_html_expanded(wikitext, &source, &ParserOptions::for_page("Test"))
        .await
        .unwrap_or_else(|e| panic!("parse failed: {e}"))
}

#[tokio::test]
async fn expand_template_with_a_computed_argument() {
    // The argument is built by the module, so no preload could have known it.
    let module = r#"
        local p = {}
        function p.main(frame)
            local who = "wor" .. "ld"
            return frame:expandTemplate{ title = "Hello", args = { who } }
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[("Template:Hello", "hello {{{1}}}")],
        "{{#invoke:T|main}}",
    )
    .await;
    assert!(html.contains("hello world"), "got: {html}");
}

#[tokio::test]
async fn expand_template_passes_named_arguments() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return frame:expandTemplate{ title = "Pair", args = { a = "1", b = "2" } }
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[("Template:Pair", "{{{a}}}-{{{b}}}")],
        "{{#invoke:T|main}}",
    )
    .await;
    assert!(html.contains("1-2"), "got: {html}");
}

/// A module may reach `expandTemplate` many times; each call must get its own
/// answer, which is what the request keying is for.
#[tokio::test]
async fn several_expand_template_calls_each_get_their_answer() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local out = {}
            for i = 1, 3 do
                out[i] = frame:expandTemplate{ title = "Echo", args = { tostring(i) } }
            end
            return table.concat(out, "|")
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[("Template:Echo", "<b>{{{1}}}</b>")],
        "{{#invoke:T|main}}",
    )
    .await;
    // Each call gets its own answer: three bold elements, one per argument.
    for i in 1..=3 {
        assert!(html.contains(&format!("<b>{i}</b>")), "missing {i}: {html}");
    }
}

/// The same call twice must also work: the second lookup finds the answer the
/// first one cached.
#[tokio::test]
async fn a_repeated_call_reuses_its_answer() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local once = frame:expandTemplate{ title = "Echo", args = { "x" } }
            local twice = frame:expandTemplate{ title = "Echo", args = { "x" } }
            return once .. twice
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[("Template:Echo", "[{{{1}}}]<b>y</b>")],
        "{{#invoke:T|main}}",
    )
    .await;
    // The answer is HTML, so the expansion's own `<b>` arrives as markup and
    // the second, cached answer is identical to the first.
    let occurrences = html.matches("[x]<b>y</b>").count();
    assert_eq!(occurrences, 2, "got: {html}");
}

#[tokio::test]
async fn preprocess_expands_wikitext() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return frame:preprocess("{{Greet}}")
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[("Template:Greet", "hi there")],
        "{{#invoke:T|main}}",
    )
    .await;
    assert!(html.contains("hi there"), "got: {html}");
}

/// `frame:extensionTag` is lowered to a `#tag` parser-function call, so both of
/// its documented spellings must build the same request. Only the named-table
/// form was accepted, which broke 25 corpus pages when
/// `Module:Citation/CS1` — which writes `frame:extensionTag('templatestyles', '',
/// {src = …})` — started using it.
///
/// The tag itself cannot be asserted end to end, because rustoid's parser has no
/// `#tag` yet; what is checked is that the call is well-formed and reaches the
/// parser instead of failing in the argument parser.
#[tokio::test]
#[allow(non_snake_case)]
async fn extension_tag_accepts_both_documented_spellings() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local positional = frame:extensionTag('nowiki', 'raw', { src = 'x' })
            local named = frame:extensionTag{ name = 'nowiki', content = 'raw', args = { src = 'x' } }
            return type(positional) .. '|' .. type(named)
        end
        return p
    "#;
    let html = expand(&[("Module:T", module)], &[], "{{#invoke:T|main}}").await;
    // Both calls returned a value rather than raising.
    assert!(html.contains("string|string"), "got: {html}");
}

/// `preprocess` returns *text*, so a module can test it — the point of the
/// method, and something a lazily-returned token could not do.
#[tokio::test]
async fn preprocess_result_is_readable_by_lua() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local out = frame:preprocess("{{Greet}}")
            if out:find("hi") then
                return "matched"
            end
            return "no match: " .. out
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[("Template:Greet", "hi there")],
        "{{#invoke:T|main}}",
    )
    .await;
    assert!(html.contains("matched"), "got: {html}");
}

/// `frame:getParent()` returns a *frame*, so it has the parser-calling methods
/// too. `Module:Noinclude` writes `frame:getParent():preprocess(...)`, and the
/// parent used to be a bare table of `args`, which made that a call to a nil
/// method on three corpus pages.
#[tokio::test]
async fn the_parent_frame_exposes_preprocess() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return frame:getParent():preprocess("{{Greet}}")
        end
        return p
    "#;
    // The module is invoked *from* a template, which is what gives it a parent.
    let html = expand(
        &[("Module:T", module)],
        &[
            ("Template:Greet", "hi there"),
            ("Template:Wrapper", "{{#invoke:T|main}}"),
        ],
        "{{Template:Wrapper}}",
    )
    .await;
    assert!(html.contains("hi there"), "got: {html}");
}

/// The parent frame's `args` are the *calling template's*, and its parser
/// methods expand in the parent's scope, which is the whole point of asking for
/// the parent rather than using `frame`.
#[tokio::test]
async fn the_parent_frame_expands_in_its_own_scope() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local parent = frame:getParent()
            return parent:preprocess("{{{1|none}}}")
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[("Template:Wrapper", "{{#invoke:T|main}}")],
        "{{Template:Wrapper|from the caller}}",
    )
    .await;
    assert!(html.contains("from the caller"), "got: {html}");
}

/// The parent frame also has `expandTemplate`, for the same reason: it is a
/// frame, not a stub. `Module:Noinclude` is the corpus case for `preprocess`;
/// this pins the general property so a future stub cannot narrow the parent
/// again.
#[tokio::test]
async fn the_parent_frame_exposes_expand_template() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return frame:getParent():expandTemplate{ title = "Greet", args = {} }
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[
            ("Template:Greet", "hi there"),
            ("Template:Wrapper", "{{#invoke:T|main}}"),
        ],
        "{{Template:Wrapper}}",
    )
    .await;
    assert!(html.contains("hi there"), "got: {html}");
}

/// A module that never reads the answer must not pay for expanding it, and must
/// not break: this is why `expandTemplate` produces a lazily-resolved token.
#[tokio::test]
async fn an_unused_expand_template_call_is_harmless() {
    let module = r#"
        local p = {}
        function p.main(frame)
            frame:expandTemplate{ title = "Never", args = { "x" } }
            return "done"
        end
        return p
    "#;
    let html = expand(&[("Module:T", module)], &[], "{{#invoke:T|main}}").await;
    assert!(html.contains("done"), "got: {html}");
}

/// An `expandTemplate` answer that is embedded in the module's output must be
/// parsed as wikitext, exactly as if the template had been written on the page.
#[tokio::test]
async fn an_embedded_answer_is_parsed() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return "before " .. frame:expandTemplate{ title = "Bold", args = {} } .. " after"
        end
        return p
    "#;
    let html = expand(
        &[("Module:T", module)],
        &[("Template:Bold", "'''strong'''")],
        "{{#invoke:T|main}}",
    )
    .await;
    assert!(html.contains(">strong</b>"), "got: {html}");
    assert!(html.contains("before"), "got: {html}");
    assert!(html.contains("after"), "got: {html}");
}

/// An error from the expansion must be visible rather than silently dropped.
#[tokio::test]
async fn a_missing_template_leaves_a_redlink() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return frame:expandTemplate{ title = "Absent", args = {} }
        end
        return p
    "#;
    let html = expand(&[("Module:T", module)], &[], "{{#invoke:T|main}}").await;
    assert!(html.contains("Absent"), "got: {html}");
}

/// `callParserFunction` is the same problem as `expandTemplate` and must work
/// the same way.
#[tokio::test]
async fn call_parser_function_is_deferred_too() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return frame:callParserFunction{ name = "uc", args = { "shout" } }
        end
        return p
    "#;
    let html = expand(&[("Module:T", module)], &[], "{{#invoke:T|main}}").await;
    assert!(html.contains("SHOUT"), "got: {html}");
}

/// `callParserFunction` has three documented spellings and modules write all
/// of them: the named table, `(name, args)`, and `(name, ...)`. Only the first
/// was accepted, so `frame:callParserFunction('ns', 0)` failed with "expects a
/// table with a name" — which Module:Italic title and Module:Coordinates both
/// hit.
#[tokio::test]
#[allow(non_snake_case)]
async fn call_parser_function_accepts_every_documented_form() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local named = frame:callParserFunction{ name = 'uc', args = { 'one' } }
            local table_arg = frame:callParserFunction('uc', { 'two' })
            local spread = frame:callParserFunction('uc', 'three')
            return named .. table_arg .. spread
        end
        return p
    "#;
    let html = expand(&[("Module:T", module)], &[], "{{#invoke:T|main}}").await;
    assert!(html.contains("ONE"), "named table: {html}");
    assert!(html.contains("TWO"), "table argument: {html}");
    assert!(html.contains("THREE"), "spread arguments: {html}");
}

/// A module returning something other than a string still has to produce
/// output rather than vanishing or panicking.
#[tokio::test]
async fn a_numeric_return_is_stringified() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return 42
        end
        return p
    "#;
    let html = expand(&[("Module:T", module)], &[], "{{#invoke:T|main}}").await;
    assert!(html.contains("42"), "got: {html}");
}
