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

/// An empty function name runs `main`.
///
/// `Template:Citation needed` calls `{{#invoke:Unsubst||date=…}}` and is on
/// hundreds of thousands of pages, so the empty form must expand.
#[tokio::test]
async fn an_empty_function_name_runs_main() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return "main ran with " .. tostring(frame.args['date'])
        end
        return p
    "#;
    let html = expand(
        &[("Module:Unsubst", module)],
        "{{#invoke:Unsubst||date=2024}}",
    )
    .await;
    assert!(html.contains("main ran with 2024"), "got: {html}");
}

/// `require` must serve Scribunto's own libraries as well as wiki modules.
#[tokio::test]
async fn scribunto_libraries_are_requirable_from_a_module() {
    let module = r#"
        local libraryUtil = require('libraryUtil')
        local p = {}
        function p.main(frame)
            libraryUtil.checkType('main', 1, frame.args[1], 'string')
            return require('ustring').upper(frame.args[1])
        end
        return p
    "#;
    let html = expand(&[("Module:Lib", module)], "{{#invoke:Lib|main|loud}}").await;
    assert!(html.contains("LOUD"), "got: {html}");
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
/// `mw.site.namespaces` must be a real table: Hatnote indexes it directly and
/// stops dead when it is missing.
#[tokio::test]
async fn mw_site_namespaces_is_available() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local ns = mw.site.namespaces
            return tostring(ns[0].name) .. '|' .. tostring(ns[10].canonicalName)
                .. '|' .. tostring(ns['Template'].id)
        end
        return p
    "#;
    let html = expand(&[("Module:NS", module)], "{{#invoke:NS|main}}").await;
    assert!(html.contains("|Template|10"), "got: {html}");
}

/// `mw.language.getContentLanguage()` must exist and carry `formatNum`.
#[tokio::test]
async fn mw_language_content_language_is_available() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local lang = mw.language.getContentLanguage()
            return lang:getCode() .. '/' .. lang:formatNum(1234567)
        end
        return p
    "#;
    let html = expand(&[("Module:Lang", module)], "{{#invoke:Lang|main}}").await;
    assert!(html.contains("en/"), "got: {html}");
}

/// Lua's string functions coerce numbers, and modules rely on it: a numeric
/// argument must not fail, but a missing one must name the function rather than
/// report a bare conversion error.
#[tokio::test]
async fn mw_text_functions_coerce_numbers_and_name_themselves() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local ok, err = pcall(mw.text.trim, nil)
            return mw.text.trim(42) .. '/' .. tostring(ok) .. '/' .. tostring(err)
        end
        return p
    "#;
    let html = expand(&[("Module:Coerce", module)], "{{#invoke:Coerce|main}}").await;
    assert!(html.contains("42/false"), "got: {html}");
    assert!(
        html.contains("trim"),
        "the error should name the function: {html}"
    );
}

/// `mw.isSubsting()`, `mw.text.listToText` and the namespace indexes modules
/// cross-reference.
#[tokio::test]
async fn misc_mw_helpers_needed_by_real_modules() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return tostring(mw.isSubsting())
                .. '/' .. mw.text.listToText({'a', 'b', 'c'})
                .. '/' .. tostring(mw.site.subjectNamespaces[0].id)
        end
        return p
    "#;
    let html = expand(&[("Module:Misc", module)], "{{#invoke:Misc|main}}").await;
    assert!(html.contains("false/a, b and c/0"), "got: {html}");
}

/// Scribunto adds `table.clone`; a module that calls it must not stop there.
#[tokio::test]
async fn table_clone_is_available() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local t = { a = 1, b = 2 }
            local c = table.clone(t)
            c.a = 9
            return tostring(t.a) .. tostring(c.a)
        end
        return p
    "#;
    let html = expand(&[("Module:Clone", module)], "{{#invoke:Clone|main}}").await;
    assert!(html.contains("19"), "got: {html}");
}

/// `mw.html` must build a real tree, and `create(nil)` must give a tagless
/// builder — the documented form that stopped 37 corpus pages.
#[tokio::test]
async fn mw_html_builds_and_accepts_a_tagless_builder() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local tagless = mw.html.create():wikitext('bare'):done()
            local div = mw.html.create('div'):addClass('a', 'b'):attr('id', 'x')
                :wikitext('inner'):done()
            -- `allDone()` returns the root *node*, so it is rendered with
            -- `tostring` rather than concatenated directly.
            local nested = tostring(mw.html.create('span'):tag('b'):wikitext('deep'):allDone())
            return tostring(tagless) .. '|' .. tostring(div) .. '|' .. nested
        end
        return p
    "#;
    let html = expand(&[("Module:Html", module)], "{{#invoke:Html|main}}").await;
    // `mw.html` builds HTML, which the parser emits as elements, so the
    // attributes are checked on the raw output rather than on the stripped text.
    assert!(
        html.contains("bare"),
        "tagless builder lost its text: {html}"
    );
    assert!(
        html.contains("class=\"a b\"") && html.contains("id=\"x\""),
        "attributes: {html}"
    );
    assert!(text_only(&html).contains("deep"), "nested tag: {html}");
}

/// `done()` and `allDone()` return nodes, not strings, so a chain may keep
/// calling methods after them. `Module:Spoken Wikipedia` writes
/// `res:...:done():newline()`, which called a method on a rendered string while
/// `allDone` returned one — the manual says it walks to the root *node*.
#[tokio::test]
async fn mw_html_done_and_all_done_return_nodes() {
    let module = r#"
        local p = {}
        function p.main(frame)
            -- `done()` on a root returns the root, so this chain works.
            local chained = mw.html.create('div'):wikitext('a'):done():newline():wikitext('b')
            -- `allDone()` from a nested tag returns the outermost node, which
            -- still has the methods.
            local deep = mw.html.create('div'):tag('b'):wikitext('x'):allDone():newline()
            return tostring(chained) .. '|' .. tostring(deep)
        end
        return p
    "#;
    let html = expand(&[("Module:Html2", module)], "{{#invoke:Html2|main}}").await;
    assert!(
        text_only(&html).contains('a') && text_only(&html).contains('b'),
        "chained after done: {html}"
    );
    assert!(
        text_only(&html).contains('x'),
        "chained after allDone: {html}"
    );
}

/// `frame:getParent().args` must expose the *calling template's* arguments.
///
/// Modules rely on this: `Module:Infobox` and `Module:Check for conflicting
/// parameters` both read it on their first lines, and returning nil stopped 22
/// corpus pages.
#[tokio::test]
async fn get_parent_exposes_the_calling_templates_args() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local parent = frame:getParent()
            if parent == nil then return "no parent" end
            return "parent said " .. tostring(parent.args['name'])
                .. "/" .. tostring(parent.args[1])
        end
        return p
    "#;
    let config = MockSiteConfig::new();
    let source = MockDataSource::new();
    source.add_module("Module:UsesParent", module);
    // The invoke is made *from inside* Template:Wrapper, so its arguments are
    // the parent frame's.
    source.add_template("Template:Wrapper", "{{#invoke:UsesParent|main}}");
    let parser = Parser::new(&config);
    let html = parser
        .wikitext_to_html_expanded(
            "{{Wrapper|first|name=bob}}",
            &source,
            &ParserOptions::for_page("Test"),
        )
        .await
        .unwrap();
    let body = text_only(&html);
    assert!(body.contains("parent said bob/first"), "got: {body}");
}

/// A direct `{{#invoke:…}}` from the page has no parent frame, and Scribunto
/// returns nil for `getParent()` there.
#[tokio::test]
async fn a_direct_invoke_has_no_parent() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return tostring(frame:getParent())
        end
        return p
    "#;
    let html = expand(&[("Module:NoParent", module)], "{{#invoke:NoParent|main}}").await;
    assert!(text_only(&html).contains("nil"), "got: {html}");
}

/// `ustring.sub` must clamp every index combination without panicking.
///
/// This is not hypothetical: an inverted range — `sub(s, 5, -1)` on a short
/// string, which `Help:Introduction` produced — panicked the whole process,
/// taking down a corpus run with it.
///
/// The expected values are taken from Lua 5.4's own `string.sub`, which
/// `ustring.sub` mirrors, rather than from reasoning about the formula.
#[tokio::test]
async fn ustring_sub_clamps_instead_of_panicking() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local u = mw.ustring
            return table.concat({
                '[' .. u.sub('abc', 5, -1) .. ']',   -- start past the end
                '[' .. u.sub('abc', 2) .. ']',       -- from 2 to the end
                '[' .. u.sub('abc', -2) .. ']',      -- last two
                '[' .. u.sub('abc', 1, -2) .. ']',   -- all but the last
                '[' .. u.sub('abc', 2, 1) .. ']',    -- inverted explicitly
                '[' .. u.sub('abc', 0, 99) .. ']',   -- clamped both ways
                '[' .. u.sub('abc', -99) .. ']',     -- start far before
                '[' .. u.sub('abc', 1, -99) .. ']',  -- end far before
                '[' .. u.sub('h\xc3\xa9llo', 2, 3) .. ']', -- codepoints, not bytes
            }, ' ')
        end
        return p
    "#;
    let html = expand(&[("Module:Sub", module)], "{{#invoke:Sub|main}}").await;
    let body = text_only(&html);
    // The ASCII cases, verified against Lua 5.4's own `string.sub`. The parser
    // splits text runs, so only the tail is contiguous.
    assert!(
        body.contains("[] [bc] [bc] [ab] [] [abc] [abc] []"),
        "got: {body}"
    );
    // The codepoint case: byte slicing would cut the two-byte `é` in half.
    // Checked on the raw output, because the parser may split the text run.
    assert!(
        html.contains("[él]") || body.contains("[él]"),
        "got: {html}"
    );
}

/// `frame:getParent():getTitle()` must name the calling template.
///
/// `Module:Labelled list hatnote` and `Module:Arguments` both read it, and
/// without it the parent frame was nil and the module stopped.
#[tokio::test]
async fn get_parent_exposes_the_calling_templates_title() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return "called from " .. frame:getParent():getTitle()
        end
        return p
    "#;
    let config = MockSiteConfig::new();
    let source = MockDataSource::new();
    source.add_module("Module:Titled", module);
    source.add_template("Template:Wrapper", "{{#invoke:Titled|main}}");
    let parser = Parser::new(&config);
    let html = parser
        .wikitext_to_html_expanded("{{Wrapper}}", &source, &ParserOptions::for_page("Test"))
        .await
        .unwrap();
    let body = text_only(&html);
    assert!(body.contains("called from Template:Wrapper"), "got: {body}");
}

/// `mw.title.equals` compares two title objects.
#[tokio::test]
async fn mw_title_equals_compares_titles() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local a = mw.title.new('Foo')
            local b = mw.title.new('Foo')
            local c = mw.title.new('Bar')
            return tostring(mw.title.equals(a, b)) .. '/' .. tostring(mw.title.equals(a, c))
                .. '/' .. tostring(mw.title.equals(a, nil))
        end
        return p
    "#;
    let html = expand(&[("Module:Eq", module)], "{{#invoke:Eq|main}}").await;
    let body = text_only(&html);
    assert!(body.contains("true/false/false"), "got: {body}");
}

/// `mw.clone` must deep-copy, keep cycles terminating, and preserve metatables.
///
/// Modules copy shared configuration before editing it; a shallow copy would let
/// one invocation corrupt another's data.
#[tokio::test]
async fn mw_clone_deep_copies_and_terminates_on_cycles() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local original = { nested = { value = 1 }, list = { 'a' } }
            original.self = original  -- a cycle must not hang the clone
            local copy = mw.clone(original)
            copy.nested.value = 99
            copy.list[1] = 'z'
            return tostring(original.nested.value) .. tostring(copy.nested.value)
                .. '/' .. original.list[1] .. copy.list[1]
                .. '/' .. tostring(copy.self == copy)
        end
        return p
    "#;
    let html = expand(&[("Module:Clone", module)], "{{#invoke:Clone|main}}").await;
    let body = text_only(&html);
    assert!(body.contains("199/az/true"), "got: {body}");
}

/// `mw.getContentLanguage()` and a namespace *name* in `mw.title.new`.
#[tokio::test]
async fn content_language_and_named_namespace_arguments() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local lang = mw.getContentLanguage()
            local t = mw.title.new('Foo', 'Template')
            return lang:getCode() .. '/' .. t.namespace .. '/' .. t.prefixedText
        end
        return p
    "#;
    let html = expand(&[("Module:CL", module)], "{{#invoke:CL|main}}").await;
    let body = text_only(&html);
    assert!(body.contains("en/10/Template:Foo"), "got: {body}");
}

/// `mw.title.getCurrentTitle():getContent()` must return the page being parsed.
///
/// This is the dominant use of `getContent` (`Module:Footnotes/anchor_id_list`,
/// `Module:Engvar detect`), and it needs no fetch: rustoid is parsing it.
#[tokio::test]
async fn current_title_content_is_the_page_source() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local content = mw.title.getCurrentTitle():getContent() or 'NIL'
            return 'len=' .. tostring(#content) .. ' has=' .. tostring(content:find('MAGICWORD') ~= nil)
        end
        return p
    "#;
    let config = MockSiteConfig::new();
    let source = MockDataSource::new();
    source.add_module("Module:Content", module);
    let parser = Parser::new(&config);
    let html = parser
        .wikitext_to_html_expanded(
            "MAGICWORD here {{#invoke:Content|main}}",
            &source,
            &ParserOptions::for_page("Test"),
        )
        .await
        .unwrap();
    let body = text_only(&html);
    assert!(body.contains("has=true"), "got: {body}");
}

/// Existence and content of *other* pages come from the preload pass, which
/// scans the module for `mw.title.new('…')` literals.
#[tokio::test]
async fn other_titles_get_preloaded_facts() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local present = mw.title.new('Template:Wrapper')
            local absent = mw.title.new('Template:NotThere')
            return tostring(present.exists) .. '/' .. tostring(absent.exists)
        end
        return p
    "#;
    let config = MockSiteConfig::new();
    let source = MockDataSource::new();
    source.add_module("Module:Facts", module);
    source.add_template("Template:Wrapper", "wrapped");
    let parser = Parser::new(&config);
    let html = parser
        .wikitext_to_html_expanded(
            "{{#invoke:Facts|main}}",
            &source,
            &ParserOptions::for_page("Test"),
        )
        .await
        .unwrap();
    let body = text_only(&html);
    assert!(body.contains("true/false"), "got: {body}");
}

/// `mw.addWarning` must exist and accept any stringable argument.
///
/// `Module:Labelled list hatnote` and `Module:Clade` call it, and without it the
/// module stops rather than warning.
#[tokio::test]
async fn mw_add_warning_is_available() {
    let module = r#"
        local p = {}
        function p.main(frame)
            mw.addWarning('something to note')
            mw.addWarning(42)
            return 'continued'
        end
        return p
    "#;
    let html = expand(&[("Module:Warn", module)], "{{#invoke:Warn|main}}").await;
    assert!(text_only(&html).contains("continued"), "got: {html}");
}

/// `title:fullUrl()` is a method, not a field.
#[tokio::test]
async fn title_full_url_is_a_method() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return mw.title.new('Foo'):fullUrl()
        end
        return p
    "#;
    let html = expand(&[("Module:Url", module)], "{{#invoke:Url|main}}").await;
    assert!(text_only(&html).contains("/wiki/Foo"), "got: {html}");
}

/// The loop `Module:Namespace detect/data` runs over the namespace tables.
#[tokio::test]
async fn site_namespace_tables_support_the_usual_iteration() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local main = mw.site.subjectNamespaces[0].name
            local count = 0
            local missing = ''
            for nsid, ns in pairs(mw.site.subjectNamespaces) do
                if ns.name == nil or ns.id == nil then missing = missing .. tostring(nsid) end
                count = count + 1
            end
            return 'main=[' .. main .. '] count=' .. count .. ' missing=' .. missing
        end
        return p
    "#;
    let html = expand(&[("Module:NSI", module)], "{{#invoke:NSI|main}}").await;
    let body = text_only(&html);
    // `text_only` strips the brackets (the parser reads `[...]` as a wikilink),
    // so the checks are on the parts either side.
    assert!(body.contains("main="), "got: {body}");
    assert!(body.contains("count="), "got: {body}");
    assert!(body.contains("missing="), "got: {body}");
    // A non-zero count means the table iterated, and an empty `missing` means
    // every entry carried both fields.
    let count: usize = body
        .split("count=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(count > 10, "namespaces did not iterate: {body}");
}

/// Every `mw.site` namespace entry must carry `aliases` as a *table*.
///
/// `Module:Namespace detect/data` iterates `ipairs(ns.aliases)` for every
/// namespace in order to build its parameter mappings. A missing field is not a
/// recoverable nil there: the error surfaces inside a for iterator, which Lua
/// cannot attribute to a line, so it appeared as a bare "attempt to index a nil
/// value" against the whole module — 31 corpus pages, unattributable.
#[tokio::test]
async fn every_site_namespace_has_an_aliases_table() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local bad, count = 0, 0
            for _, ns in pairs(mw.site.subjectNamespaces) do
                count = count + 1
                -- Iterating is the operation that failed; type() alone would
                -- not have caught a non-iterable value.
                for _, alias in ipairs(ns.aliases) do
                    if type(alias) ~= 'string' then bad = bad + 1 end
                end
            end
            return 'count=' .. count .. ' bad=' .. bad
        end
        return p
    "#;
    let html = expand(&[("Module:NSAlias", module)], "{{#invoke:NSAlias|main}}").await;
    let body = text_only(&html);
    assert!(
        body.contains("bad=0"),
        "a namespace alias was not a string: {body}"
    );
    assert!(
        body.contains("count="),
        "namespaces did not iterate: {body}"
    );
}

/// A namespace can be named by its *alias* as well as its canonical name.
#[tokio::test]
async fn namespace_alias_resolves_to_its_id() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return tostring(mw.title.new('Foo', 'Template').namespace)
        end
        return p
    "#;
    let html = expand(&[("Module:NsAlias", module)], "{{#invoke:NsAlias|main}}").await;
    assert!(text_only(&html).contains("10"), "got: {html}");
}

/// `mw.html`'s `tag()` must return a builder with the same methods.
#[tokio::test]
async fn html_nested_tag_supports_wikitext_and_attrs() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local root = mw.html.create('div')
            local inner = root:tag('span')
            inner:attr('title', 'T')
            inner:wikitext('body')
            return root:done()
        end
        return p
    "#;
    let html = expand(&[("Module:HN", module)], "{{#invoke:HN|main}}").await;
    assert!(html.contains("title=\"T\""), "got: {html}");
    assert!(text_only(&html).contains("body"), "got: {html}");
}

/// `mw.language` objects need `ucfirst`/`lcfirst`; `Module:Footnotes` calls
/// `Lang_obj:ucfirst(name)` to canonicalise a template name.
#[tokio::test]
async fn language_objects_have_case_functions() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local lang = mw.getContentLanguage()
            return lang:ucfirst('article') .. '/' .. lang:lcfirst('Article')
        end
        return p
    "#;
    let html = expand(&[("Module:Langcase", module)], "{{#invoke:Langcase|main}}").await;
    assert!(text_only(&html).contains("Article/article"), "got: {html}");
}

/// The exact `mw.html` chain `Module:Navbar` builds.
#[tokio::test]
async fn html_chained_builder_calls() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local ul = mw.html.create('ul')
            ul:tag('li')
                :attr('class', 'nv-view')
                :cssText('white-space:nowrap;')
                :wikitext('text')
                :done()
                :wikitext('tail')
                :done()
            return ul:done()
        end
        return p
    "#;
    let html = expand(&[("Module:Chain", module)], "{{#invoke:Chain|main}}").await;
    let body = text_only(&html);
    assert!(body.contains("text"), "got: {html}");
    assert!(body.contains("tail"), "got: {html}");
}

/// `mw.log` and `mw.logObject` must exist as no-ops.
///
/// rustoid has no debug console to write to, but a module that logs must not
/// stop: `Module:Footnotes/anchor_id_list` calls `mw.logObject` directly.
#[tokio::test]
async fn logging_helpers_are_no_ops() {
    let module = r#"
        local p = {}
        function p.main(frame)
            mw.log('plain')
            mw.logObject({ a = 1 }, 'prefix')
            mw.log.warn('nested')
            return 'logged'
        end
        return p
    "#;
    let html = expand(&[("Module:Logs", module)], "{{#invoke:Logs|main}}").await;
    assert!(text_only(&html).contains("logged"), "got: {html}");
}

/// `mw.html:node()` inserts an existing node; `done()` returns the *parent*.
///
/// Scribunto's manual is explicit that `done()` returns the parent instance
/// rather than a string, which is what makes
/// `:tag('li'):attr(..):done():wikitext(..)` chain back up. Returning a string
/// broke every such chain (`Module:Navbar` and `Module:Sidebar` both build one).
#[tokio::test]
async fn html_node_inserts_a_child_and_done_goes_up() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local ul = mw.html.create('ul')
            ul:tag('li'):wikitext('first'):done():wikitext('AFTER')
            local separate = mw.html.create('li'):wikitext('inserted')
            ul:node(separate)
            return ul:done()
        end
        return p
    "#;
    let html = expand(&[("Module:Node", module)], "{{#invoke:Node|main}}").await;
    let body = text_only(&html);
    assert!(body.contains("first"), "got: {html}");
    assert!(
        body.contains("AFTER"),
        "done() did not return the parent: {html}"
    );
    assert!(body.contains("inserted"), "node() did not insert: {html}");
}

/// `mw.language:formatDate` must produce month names — `Module:Citation/CS1`
/// iterates `F` and `M` across a year to build its month tables.
#[tokio::test]
async fn language_format_date_supports_month_names() {
    let module = r#"
        local p = {}
        function p.main(frame)
            local lang = mw.getContentLanguage()
            return lang:formatDate('F', '2022-3-1') .. '/' .. lang:formatDate('M', '2022-12-1')
                .. '/' .. lang:formatDate('Y-m-d', '2022-3-4')
        end
        return p
    "#;
    let html = expand(&[("Module:FmtDate", module)], "{{#invoke:FmtDate|main}}").await;
    let body = text_only(&html);
    assert!(body.contains("March/Dec"), "got: {body}");
}

/// An argument's *tags* must reach the module, not just its text.
///
/// `tokensToString` has no arm for a plain tag, so reassembling the `#invoke`
/// arguments by stringifying them dropped the tags and handed the module the
/// bare text. The live service gives `{{#invoke:String|len|<div>X</div>}}` the
/// answer **12** — the literal tag characters — where the stringified form gave
/// **1**. The argument is now read from its `srcOffsets`, which is the wikitext
/// as written, the same source-range technique template parameters already use.
#[tokio::test]
async fn module_arguments_keep_their_tags() {
    let module = r#"
        local p = {}
        function p.main(frame)
            return #frame.args[1]
        end
        return p
    "#;
    let html = expand(
        &[("Module:Len", module)],
        "{{#invoke:Len|main|<div>X</div>}}",
    )
    .await;
    assert!(
        text_only(&html).contains("12"),
        "tags were dropped from the argument: {html}"
    );
}
