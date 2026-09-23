//! `mw.title` — the title object, against Scribunto's `mw.title.lua`.
//!
//! The behavioural contract lives in Scribunto's own `mw.title.lua` (and its
//! `TitleLibrary.php` backing); where a detail was ambiguous it was checked
//! against Lua rather than reasoned about. The tests below pin the parts the
//! corpus actually depends on.

use rustoid_core::Parser;
use rustoid_core::ParserOptions;
use rustoid_core::mock::{MockDataSource, MockSiteConfig};

/// Run `code` as a module body and return the string it produces.
async fn eval(code: &str) -> String {
    let module = format!("local p = {{}}\nfunction p.main(frame)\n{code}\nend\nreturn p");
    let config = MockSiteConfig::new();
    let source = MockDataSource::new();
    source.add_module("Module:T", &module);
    let parser = Parser::new(&config);
    let html = parser
        .wikitext_to_html_expanded(
            "{{#invoke:T|main}}",
            &source,
            &ParserOptions::for_page("Test"),
        )
        .await
        .unwrap_or_else(|e| panic!("parse failed: {e}"));
    // The module's own output is the text between the wrapper's tags.
    let body = html.split("<body>").nth(1).unwrap_or(&html);
    body.split("</body>").next().unwrap_or(body).to_string()
}

/// `tostring` on a title yields its prefixed text.
///
/// This is load-bearing, not cosmetic: `Module:Flagg` does
/// `require(tostring(mw.title.new('Module:CountryData')))`, and without it the
/// module was asked to load `table: 0x…`, which no fetch can ever satisfy.
#[tokio::test]
async fn tostring_gives_the_prefixed_text() {
    let out = eval(
        r#"
        local t = mw.title.new('Module:CountryData')
        return tostring(t) .. '|' .. tostring(mw.title.new('Foo'))
        "#,
    )
    .await;
    assert!(out.contains("Module:CountryData"), "got: {out}");
    assert!(out.contains("Foo"), "got: {out}");
}

/// `require(tostring(title))` is the pattern the failure was reported through.
#[tokio::test]
async fn a_title_stringifies_into_a_requirable_module_name() {
    let out = eval(
        r#"
        local name = tostring(mw.title.new('Module:Yesno'))
        if name ~= 'Module:Yesno' then
            return 'wrong: ' .. name
        end
        return 'ok'
        "#,
    )
    .await;
    assert!(out.contains("ok"), "got: {out}");
}

/// A title's subpage fields follow Scribunto's `'^[^/]*().*()/[^/]*$'` match.
///
/// The expectations below were produced by running that pattern in Lua, not
/// derived by reading it: `Foo/` *is* a subpage (with an empty
/// `subpageText`), and so is a leading-slash `/Foo`.
#[tokio::test]
async fn subpage_fields_match_scribunto() {
    let out = eval(
        r#"
        local t = mw.title.new('Foo/Bar/Baz')
        return tostring(t.isSubpage) .. '|' .. t.rootText .. '|' .. t.baseText .. '|' .. t.subpageText
        "#,
    )
    .await;
    assert!(out.contains("true|Foo|Foo/Bar|Baz"), "got: {out}");
}

#[tokio::test]
async fn a_title_without_a_slash_is_not_a_subpage() {
    let out = eval(
        r#"
        local t = mw.title.new('Foo')
        return tostring(t.isSubpage) .. '|' .. t.rootText .. '|' .. t.subpageText
        "#,
    )
    .await;
    assert!(out.contains("false|Foo|Foo"), "got: {out}");
}

#[tokio::test]
async fn a_trailing_slash_is_a_subpage_with_an_empty_tail() {
    let out = eval(
        r#"
        local t = mw.title.new('Foo/')
        return tostring(t.isSubpage) .. '|' .. t.rootText .. '|[' .. t.subpageText .. ']'
        "#,
    )
    .await;
    assert!(out.contains("true|Foo|[]"), "got: {out}");
}

/// `subPageTitle` builds a child title, which is how sandbox variants are named.
#[tokio::test]
async fn sub_page_title_appends_a_path_segment() {
    let out = eval(
        r#"
        local t = mw.title.new('Module:CountryData')
        return tostring(t:subPageTitle('sandbox'))
        "#,
    )
    .await;
    assert!(out.contains("Module:CountryData/sandbox"), "got: {out}");
}

/// Titles compare by identity, not by table reference.
///
/// The `__lt` order is Scribunto's `lt`: interwiki, then namespace, then text.
/// The expected values here come from running that function in Lua — `Foo` is
/// *not* less than `Bar`, which is the reverse of the intuitive reading.
#[tokio::test]
async fn titles_compare_by_identity() {
    let out = eval(
        r#"
        local a = mw.title.new('Foo')
        local b = mw.title.new('Foo')
        local c = mw.title.new('Bar')
        return tostring(a == b) .. '|' .. tostring(a == c) .. '|' .. tostring(c < a) .. '|' .. tostring(a < c)
        "#,
    )
    .await;
    assert!(out.contains("true|false|true|false"), "got: {out}");
}

/// `isSubpageOf`, `inNamespace` and `inNamespaces` are the guards modules use
/// before formatting a title.
///
/// The mock site puts Module in namespace 828, not enwiki's 10, so the array
/// form is exercised with 0 and 828.
#[tokio::test]
async fn namespace_membership_helpers() {
    let out = eval(
        r#"
        local a = mw.title.new('Module:Foo/Bar')
        local b = mw.title.new('Module:Foo')
        return tostring(a:isSubpageOf(b))
            .. '|' .. tostring(a:inNamespace('Module'))
            .. '|' .. tostring(a:inNamespaces(0, 828))
            .. '|' .. tostring(a:inNamespace(0))
        "#,
    )
    .await;
    assert!(out.contains("true|true|true|false"), "got: {out}");
}

/// A namespace with no talk space reports `canTalk == false`, which is what
/// modules branch on before offering a talk link.
#[tokio::test]
async fn special_namespace_cannot_talk() {
    let out = eval(
        r#"
        local t = mw.title.new('Special:Movepage')
        local a = mw.title.new('Module:Foo')
        return tostring(t.canTalk) .. '|' .. tostring(a.canTalk)
        "#,
    )
    .await;
    assert!(out.contains("false|true"), "got: {out}");
}

/// `fragment` is a read-write field: modules set it to build a section link.
#[tokio::test]
async fn fragment_is_settable() {
    let out = eval(
        r#"
        local t = mw.title.new('Foo')
        t.fragment = 'Section'
        return t.fragment
        "#,
    )
    .await;
    assert!(out.contains("Section"), "got: {out}");
}

/// Every title answers the methods its namespace and text alone determine.
///
/// These used to be reachable only on titles built by *derived* paths, because
/// the facts-dependent answers were consulted first and returned nil for
/// anything they did not know, hiding the generic ones. `Module:Documentation`
/// builds "create this page" links with `canonicalUrl`, and its absence made the
/// module stop at "attempt to call method 'canonicalUrl' (a nil value)".
#[tokio::test]
async fn every_title_answers_the_generic_methods() {
    let out = eval(
        r#"
        local direct = mw.title.new('Module:Foo')
        local derived = mw.title.new('Module:Foo'):subPageTitle('sandbox')
        return tostring(direct.canonicalUrl) .. '|' .. tostring(derived.canonicalUrl)
            .. '|' .. tostring(direct.talkPageTitle) .. '|' .. tostring(derived.talkPageTitle)
        "#,
    )
    .await;
    assert!(!out.contains("nil"), "got: {out}");
}

/// A facts-dependent field must survive alongside the generic ones.
///
/// `exists` comes from the preloaded page data, so it travels on the instance
/// rather than the shared metatable; the two lookups have to compose.
#[tokio::test]
async fn facts_fields_still_resolve_with_the_generic_ones() {
    let out = eval(
        r#"
        local t = mw.title.new('Foo')
        return tostring(t.canonicalUrl ~= nil) .. '|' .. tostring(t.exists)
            .. '|' .. tostring(t.getContent)
        "#,
    )
    .await;
    assert!(out.contains("true|"), "got: {out}");
}
