//! `<templatestyles>` — an extension tag whose content is a stylesheet.
//!
//! The tag loads a `.css` page and inlines its text, scoped to the page's
//! content. Both halves matter and they are separate jobs:
//!
//! - **Scoping** ([`scope`]) prefixes every selector with `.mw-parser-output`,
//!   so a template's styles cannot reach the surrounding interface. A selector
//!   that targets `html` or `body` is left alone, which is the documented escape
//!   hatch for skin-dependent rules.
//! - **Rendering** ([`render`]) reproduces the exact text Parsoid emits, because
//!   the comparison is byte-for-byte. That is narrower than minifying CSS: the
//!   at-rule *prelude* is preserved as written (`@media all and (min-width:500px)`
//!   and `@media(min-width:640px)` both appear in cached output), while
//!   declarations, comments and whitespace in the body are normalised.
//!
//! Both were derived from the cached Parsoid output rather than from the CSS
//! specification, and the tests below are transcribed from real stylesheets.
//! Getting them subtly wrong is invisible: the CSS still renders, it just does
//! not match.

/// A sanitised stylesheet, ready to inline.
///
/// The two fields are returned together because they must agree: the revision id
/// is half of the `data-mw-deduplicate` key, and a key naming a revision that
/// does not match the text is worse than no stylesheet at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stylesheet {
    /// The page's revision id, as it appears in `TemplateStyles:r…`.
    pub revid: u64,
    /// The scoped, normalised CSS.
    pub css: String,
}

/// Scope `css` to the page content and normalise it the way Parsoid does.
///
/// `wrapper` is the `wrapper` attribute's value, if the tag carried one. It is
/// inserted *under* the default scope, not instead of it —
/// `wrapper=".tmulti"` scopes to `.mw-parser-output .tmulti`, which is how
/// `Module:Multiple image` constrains its stylesheet to the `<div
/// class="tmulti">` it wraps its output in. Verified against the transform
/// endpoint with `wrapper=".foo"`.
pub fn render(css: &str, wrapper: Option<&str>) -> String {
    let scope = match wrapper {
        Some(w) => format!(".mw-parser-output {w}"),
        None => ".mw-parser-output".to_string(),
    };
    let stripped = strip_comments(css);
    let mut out = String::with_capacity(stripped.len());
    // Nesting depth of `@`-rules. A declaration is only normalised inside a
    // rule, so the text between at-rules is left alone.
    let mut depth = 0usize;
    let mut buf = String::new();
    let mut chars = stripped.chars();

    for c in chars.by_ref() {
        match c {
            '{' => {
                // Everything before `{` is a selector or an at-rule prelude.
                let head = buf.trim().to_string();
                buf.clear();
                if head.starts_with('@') {
                    out.push_str(&crate::pipeline::css::value(&head));
                    out.push('{');
                } else {
                    out.push_str(&crate::pipeline::css::scope_selector(&head, &scope));
                    out.push('{');
                }
                depth += 1;
            }
            '}' => {
                // The last declaration before `}` has no `;` after it, so it is
                // still in `buf` and must be flushed here.
                if depth > 0 {
                    out.push_str(&normalise_declaration(&buf));
                    depth -= 1;
                }
                buf.clear();
                // A trailing `;` before `}` is dropped.
                while out.ends_with(';') {
                    out.pop();
                }
                out.push('}');
            }
            ';' => {
                if depth > 0 {
                    out.push_str(&normalise_declaration(&buf));
                    buf.clear();
                    out.push(';');
                } else {
                    // A stray `;` between rules carries no meaning.
                    buf.clear();
                }
            }
            _ => buf.push(c),
        }
    }
    // Anything left after the last `}` is trailing whitespace or a stray
    // fragment; neither belongs in the output.
    while out.ends_with(';') {
        out.pop();
    }
    out
}

/// Remove comments, which the sanitiser drops entirely (`/* @noflip */` and
/// `/* {{pp|small=y}} */` are both gone from the rendered output).
fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            // An unterminated comment swallows the remainder.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Normalise one declaration: `font-style: italic` → `font-style:italic`.
///
/// The value is re-serialised by the CSS tokeniser ([`crate::pipeline::css`]),
/// which is where the spaces around `:`, after `,`, around a chained function
/// and inside `! important` go, and where strings are re-quoted.
fn normalise_declaration(raw: &str) -> String {
    let text = raw.trim();
    if text.is_empty() {
        return String::new();
    }
    let Some((property, value)) = text.split_once(':') else {
        // Not a declaration (a nested prelude that never got a brace, say);
        // pass it through trimmed rather than mangling it.
        return text.to_string();
    };
    format!(
        "{}:{}",
        property.trim(),
        crate::pipeline::css::value(value.trim())
    )
}

/// Build the `<style>` element Parsoid emits for a stylesheet.
///
/// The attributes are not decorative — the comparison is byte-for-byte, so they
/// are reproduced exactly as the cached output has them:
///
/// ```html
/// <style data-mw-deduplicate="TemplateStyles:r1368532237"
///        typeof="mw:Extension/templatestyles" about="#mwt3"
///        data-mw='{"name":"templatestyles","attrs":{"src":"…"},"body":{"extsrc":""}}'>
/// ```
///
/// `revid` is the fetched revision, which is what makes the dedup key match.
///
/// `has_body` decides the one field that is *not* constant. A literal
/// `<templatestyles src="…"/>` in template source has no content, and the service
/// serves it without a `body`:
///
/// ```html
/// <style … data-mw='{"name":"templatestyles","attrs":{"src":"Plainlist/styles.css"}}'>
/// ```
///
/// while the `frame:extensionTag{name='templatestyles', args={src=…}}` form that
/// modules emit is a tag *pair* with empty content, and carries
/// `"body":{"extsrc":""}`. Both were checked against the live transform
/// endpoint; the stylesheet itself is not round-trippable source, so where a
/// `body` exists its `extsrc` is empty and the CSS lives in the element's text.
///
/// The `about` id is taken by the caller, which knows where in the document
/// sequence the stylesheet belongs: a `<templatestyles>` is resolved while
/// expansion runs, so its id follows the transclusions around it rather than
/// trailing the whole page.
pub fn style_node(
    css: &str,
    revid: u64,
    src: &str,
    wrapper: Option<&str>,
    about: &str,
    has_body: bool,
) -> crate::dom::node::Node {
    use crate::dom::node::{ElementKind, Node};

    let mut style = Node::element(ElementKind::Other("style".to_string()));
    // Attribute order matters for a byte comparison, and this is the order the
    // cached Parsoid output uses. A `wrapper` is part of the dedup key — the
    // endpoint answers `TemplateStyles:r…/mw-parser-output/.tmulti` for one —
    // because the same sheet scoped two ways is two different stylesheets.
    let dedup = match wrapper {
        Some(w) => format!("TemplateStyles:r{revid}/mw-parser-output/{w}"),
        None => format!("TemplateStyles:r{revid}"),
    };
    style.set_attr("data-mw-deduplicate", dedup);
    style.set_attr("typeof", "mw:Extension/templatestyles");
    style.set_attr("about", about);
    let attrs = match wrapper {
        Some(w) => format!(r#""src":"{src}","wrapper":"{w}""#),
        None => format!(r#""src":"{src}""#),
    };
    style.set_attr(
        "data-mw",
        if has_body {
            format!(r#"{{"name":"templatestyles","attrs":{{{attrs}}},"body":{{"extsrc":""}}}}"#)
        } else {
            format!(r#"{{"name":"templatestyles","attrs":{{{attrs}}}}}"#)
        },
    );
    style.push_child(Node::text(css));
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from `Module:Hatnote/styles.css` and its rendered
    /// form in the cached Parsoid output of 45 corpus pages.
    #[test]
    fn hatnote_stylesheet_matches_parsoid() {
        let raw = r#"/* {{pp|small=y}} */
.hatnote {
	font-style: italic;
}

/* Limit structure CSS to divs because of [[Module:Hatnote inline]] */
div.hatnote {
	/* @noflip */
	padding-left: 1.6em;
	margin-bottom: 0.5em;
}

.hatnote i {
	font-style: normal;
}

/* Templatestyles causes an 'empty' span between hatnotes.
 * See also [[phab:T200206]] for later */
.hatnote + .mw-empty-elt + .hatnote {
	margin-top: -0.5em;
}

@media print {
	body.ns-0 .hatnote {
		display: none !important;
	}
}"#;
        let expected = ".mw-parser-output .hatnote{font-style:italic}\
.mw-parser-output div.hatnote{padding-left:1.6em;margin-bottom:0.5em}\
.mw-parser-output .hatnote i{font-style:normal}\
.mw-parser-output .hatnote+.mw-empty-elt+.hatnote{margin-top:-0.5em}\
@media print{body.ns-0 .mw-parser-output .hatnote{display:none!important}}";
        assert_eq!(render(raw, None), expected);
    }

    /// `Module:Infobox/styles.css`: the `@media` prelude keeps its spacing while
    /// the body is normalised, and `html`-scoped rules escape the scope.
    #[test]
    fn infobox_stylesheet_matches_parsoid() {
        let raw = "@media(min-width:640px){\n.ib{width:22em}\n}\n\
                   @media screen{\nhtml.skin-theme-clientpref-night .ib>div{b:1}\n}";
        let expected = "@media(min-width:640px){.mw-parser-output .ib{width:22em}}\
@media screen{html.skin-theme-clientpref-night .mw-parser-output .ib>div{b:1}}";
        assert_eq!(render(raw, None), expected);
    }

    /// Scoping applies to each selector in a list, and the `body`/`html`
    /// exception is per-selector — a mixed list is partly scoped.
    #[test]
    fn selector_lists_are_scoped_individually() {
        let raw = ".a, body.ns-0 .b, html.x .c, .d{b:1}";
        assert_eq!(
            render(raw, None),
            ".mw-parser-output .a,body.ns-0 .mw-parser-output .b,\
             html.x .mw-parser-output .c,.mw-parser-output .d{b:1}"
        );
    }

    /// A bare `body` or a child combinator is *not* the documented exception: the
    /// manual is explicit that a descendant combinator is required.
    #[test]
    fn only_descendant_combinators_escape_the_scope() {
        assert_eq!(render("body{a:1}", None), ".mw-parser-output body{a:1}");
        assert_eq!(
            render("body>.x{a:1}", None),
            ".mw-parser-output body>.x{a:1}"
        );
        // The escape hatch is for the `body` element only: the scope is inserted
        // after it, giving `body .mw-parser-output .x`. Verified against the cached
        // Parsoid output, where every `body…` selector reads this way.
        assert_eq!(
            render("body .x{a:1}", None),
            "body .mw-parser-output .x{a:1}"
        );
        assert_eq!(
            render("html .x{a:1}", None),
            "html .mw-parser-output .x{a:1}"
        );
        // The qualified and `:not(…)` forms seen in the cached output.
        assert_eq!(
            render("body.ns-0 .x{a:1}", None),
            "body.ns-0 .mw-parser-output .x{a:1}"
        );
        assert_eq!(
            render("body:not(.skin-minerva) .x{a:1}", None),
            "body:not(.skin-minerva) .mw-parser-output .x{a:1}"
        );
        // A second `body` further along is an ordinary selector and is scoped.
        assert_eq!(
            render(".a body .x{b:1}", None),
            ".mw-parser-output .a body .x{b:1}"
        );
    }

    /// The `wrapper` attribute is inserted *under* the default scope:
    /// `.mw-parser-output <wrapper>`, which is how a module constrains its
    /// styles to the element it wraps its output in. Verified against the
    /// transform endpoint (`wrapper=".foo"` scopes to `.mw-parser-output .foo`).
    #[test]
    fn a_wrapper_is_inserted_under_the_scope() {
        assert_eq!(
            render(".a{b:1}", Some(".sandbox")),
            ".mw-parser-output .sandbox .a{b:1}"
        );
        // The `html`/`body` prefix is left alone and the scope follows it: the
        // exception is for the document element, not for what comes after it.
        assert_eq!(
            render("body .a{b:1}", Some(".sandbox")),
            "body .mw-parser-output .sandbox .a{b:1}"
        );
    }

    /// The stashed `<style>` carries the wrapper into the dedup key and the
    /// `data-mw` attrs, which is the cached output's exact shape for
    /// `Multiple image/styles.css`: the endpoint answers
    /// `TemplateStyles:r…/mw-parser-output/.tmulti`.
    #[test]
    fn style_node_records_the_wrapper() {
        let node = style_node(
            "a{b:1}",
            42,
            "X/styles.css",
            Some(".tmulti"),
            "#mwt7",
            false,
        );
        assert_eq!(
            node.get_attr("data-mw-deduplicate"),
            Some("TemplateStyles:r42/mw-parser-output/.tmulti")
        );
        assert_eq!(
            node.get_attr("data-mw"),
            Some(r#"{"name":"templatestyles","attrs":{"src":"X/styles.css","wrapper":".tmulti"}}"#)
        );
        // Without a wrapper the key is the bare revision, as every cached
        // stylesheet but that one shows.
        let plain = style_node("a{b:1}", 42, "X/styles.css", None, "#mwt7", false);
        assert_eq!(
            plain.get_attr("data-mw-deduplicate"),
            Some("TemplateStyles:r42")
        );
        assert_eq!(
            plain.get_attr("data-mw"),
            Some(r#"{"name":"templatestyles","attrs":{"src":"X/styles.css"}}"#)
        );
    }

    /// A hex escape is re-serialised with its terminating space: the sanitiser
    /// consumes the whitespace that ends the escape and writes one back, so
    /// `Hlist/styles.css`'s `content:"\a0· "` reaches the page as
    /// `content:"\a0 · "`. A non-hex escape decodes to its character, which the
    /// string serialiser then writes literally (`:` needs no escape).
    #[test]
    fn a_hex_escape_keeps_its_terminating_space() {
        assert_eq!(
            render(r#"a{content:"\a0· "}"#, None),
            ".mw-parser-output a{content:\"\\a0 · \"}"
        );
        assert_eq!(
            render(r#"a{content:"x\:y"}"#, None),
            ".mw-parser-output a{content:\"x:y\"}"
        );
    }

    /// A string and a function are both self-delimiting, so the sanitiser's
    /// serialiser glues them: `content: " " counter(listitem) "\a0"` reaches
    /// the page as `content:" "counter(listitem)"\a0 "` (`Hlist/styles.css`).
    #[test]
    fn a_string_glues_to_a_function() {
        assert_eq!(
            render(r#"a{content: " " counter(listitem) "\a0"}"#, None),
            r#".mw-parser-output a{content:" "counter(listitem)"\a0 "}"#
        );
    }

    /// Whitespace and comments inside declarations are normalised, which is
    /// where a naive "join the lines" approach goes wrong.
    #[test]
    fn declarations_are_normalised() {
        assert_eq!(render(".a{ b : 1 }", None), ".mw-parser-output .a{b:1}");
        assert_eq!(
            render(".a{ b : 1 ; c : 2 }", None),
            ".mw-parser-output .a{b:1;c:2}"
        );
        // `! important` loses its space but keeps its meaning.
        assert_eq!(
            render(".a{ b : 1 ! important }", None),
            ".mw-parser-output .a{b:1!important}"
        );
        // A value's internal spacing is meaningful and is kept.
        assert_eq!(
            render(".a{margin:0 auto;font:12px/1.5 sans-serif}", None),
            ".mw-parser-output .a{margin:0 auto;font:12px/1.5 sans-serif}"
        );
        // …but a space chaining one function to the next goes.
        assert_eq!(
            render(".a{filter:invert(1) brightness(55%) contrast(250%)}", None),
            ".mw-parser-output .a{filter:invert(1)brightness(55%)contrast(250%)}"
        );
    }

    /// Spaces around `%`, `/` and `)` go; spaces *between values* stay. Both
    /// spellings occur side by side in a single real stylesheet, which is why the
    /// distinction has to be made by what the space joins.
    #[test]
    fn value_separators_are_kept_but_component_joins_are_not() {
        // A four-value shorthand: the spaces separate values and survive, while
        // the ones after `%` do not.
        assert_eq!(
            render(".a{border-radius:0 500% 0 0}", None),
            ".mw-parser-output .a{border-radius:0 500%0 0}"
        );
        assert_eq!(
            render(".a{border-radius:0 100% 100% 0 / 50%}", None),
            ".mw-parser-output .a{border-radius:0 100%100%0/50%}"
        );
        // A value with no `%` keeps every separator.
        assert_eq!(
            render(".a{margin:0.5em 0 1em 1em}", None),
            ".mw-parser-output .a{margin:0.5em 0 1em 1em}"
        );
        assert_eq!(
            render(".a{border:1px solid #aaa}", None),
            ".mw-parser-output .a{border:1px solid #aaa}"
        );
    }

    /// `calc()` keeps the spaces around its operators — inside it they are the
    /// operators, so dropping them changes the value. Whitespace elsewhere in the
    /// function (after `(`, before `)`, around a comma) is insignificant.
    #[test]
    fn calc_keeps_its_spaces() {
        assert_eq!(
            render(".a{width:calc(100% - 0.7em)}", None),
            ".mw-parser-output .a{width:calc(100% - 0.7em)}"
        );
        assert_eq!(
            render(".a{width:calc( 1px + 2px )}", None),
            ".mw-parser-output .a{width:calc(1px + 2px)}"
        );
    }

    /// A trailing `;` before `}` is dropped, as in the cached output.
    #[test]
    fn a_trailing_semicolon_is_dropped() {
        assert_eq!(render(".a{b:1;}", None), ".mw-parser-output .a{b:1}");
    }

    /// An empty stylesheet is valid and produces nothing, which is the correct
    /// answer for a page whose CSS the sanitiser emptied.
    #[test]
    fn an_empty_stylesheet_renders_empty() {
        assert_eq!(render("", None), "");
        assert_eq!(render("/* only a comment */", None), "");
        assert_eq!(render("   \n\t ", None), "");
    }

    /// At-rule preludes are preserved as written, including the two spellings
    /// that both appear in the cached output.
    #[test]
    fn at_rule_preludes_are_preserved() {
        assert_eq!(
            render("@media all and (min-width:500px){.a{b:1}}", None),
            "@media all and (min-width:500px){.mw-parser-output .a{b:1}}"
        );
        assert_eq!(
            render("@media(min-width:640px){.a{b:1}}", None),
            "@media(min-width:640px){.mw-parser-output .a{b:1}}"
        );
        assert_eq!(
            render("@supports(clip-path:circle(50%)){.a{b:1}}", None),
            "@supports(clip-path:circle(50%)){.mw-parser-output .a{b:1}}"
        );
    }

    /// A media *type* keeps the space after the at-rule name, a bare condition
    /// does not. Both sources below spell it `@media (…)`, so the space can only
    /// be removed by inspecting what follows it.
    #[test]
    fn a_bare_condition_joins_the_at_rule_name() {
        for source in [
            "@media (max-width: 720px) {.a{b:1}}",
            "@media (max-width:720px){.a{b:1}}",
        ] {
            assert_eq!(
                render(source, None),
                "@media(max-width:720px){.mw-parser-output .a{b:1}}",
                "for {source:?}"
            );
        }
        // A media type keeps its space, in every source spelling.
        for source in [
            "@media screen {.a{b:1}}",
            "@media  screen and (max-width: 640px) {.a{b:1}}",
        ] {
            let out = render(source, None);
            assert!(out.starts_with("@media screen"), "for {source:?}: {out}");
        }
    }

    /// A space before `)` goes, as it does in a declaration value. The cached
    /// Parsoid output closes every condition tightly — `(max-width:500px)`,
    /// `(prefers-color-scheme:dark)` — and holds no space before a `)` in any
    /// at-rule prelude. `Template:Col-float/styles.css` and
    /// `Template:Hidden begin/styles.css` are written `( max-width: 720px )`,
    /// which is where this showed up.
    #[test]
    fn a_space_before_a_closing_paren_goes() {
        assert_eq!(
            render("@media all and ( max-width: 720px ) {.a{b:1}}", None),
            "@media all and (max-width:720px){.mw-parser-output .a{b:1}}"
        );
        // A nested condition closes tightly too. This is the real source
        // spelling of `@media screen and (prefers-color-scheme: dark)`.
        assert_eq!(
            render(
                "@media screen and ( prefers-color-scheme: dark ) {.a{b:1}}",
                None
            ),
            "@media screen and (prefers-color-scheme:dark){.mw-parser-output .a{b:1}}"
        );
    }

    /// `and` after a closing parenthesis loses the space before it, but only in
    /// that position: `@media all and (…)` keeps both spaces because a media
    /// type precedes it.
    #[test]
    fn and_after_a_condition_loses_its_leading_space() {
        assert_eq!(
            render("@media (min-width:640px) and (hover:hover){.a{b:1}}", None),
            "@media(min-width:640px)and (hover:hover){.mw-parser-output .a{b:1}}"
        );
        assert_eq!(
            render("@media all and (min-width:720px){.a{b:1}}", None),
            "@media all and (min-width:720px){.mw-parser-output .a{b:1}}"
        );
    }
}
