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
/// `wrapper` is the `wrapper` attribute's value, if the tag carried one: it
/// replaces `.mw-parser-output` as the scope, which is how a template's sandbox
/// copy can be compared side by side with the live one.
pub fn render(css: &str, wrapper: Option<&str>) -> String {
    let scope = wrapper.unwrap_or(".mw-parser-output");
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
                    out.push_str(&normalise_at_prelude(&head));
                    out.push('{');
                } else {
                    out.push_str(&scope_selector(&head, scope));
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
/// Whitespace around `:` and at either end goes; `! important` becomes
/// `!important`. Inside the value, whitespace is *also* removed after commas
/// (`rect(0, 0, 0, 0)` → `rect(0,0,0,0)`, `var(--x, #fff)` → `var(--x,#fff)`),
/// but a value's other internal spacing (`margin: 0 auto`, `12px/1.5`) is
/// meaningful and kept. Single quotes become double, which is what the
/// sanitiser emits for a font family.
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
    let value = value.trim();
    let value = match value.find('!') {
        Some(bang) => format!("{}!{}", value[..bang].trim_end(), value[bang + 1..].trim()),
        None => value.to_string(),
    };
    format!("{}:{}", property.trim(), normalise_value(&value))
}

/// Tidy an at-rule prelude: `@media (max-width: 720px)` → `@media(max-width:720px)`.
///
/// A media *type* (`all`, `screen`, `print`) keeps the space that follows the
/// at-rule name; a bare parenthesised condition does not. Within a condition,
/// spaces after `:`, after `,` and before `)` go, and `and` loses the space
/// before it when it follows a `)`.
///
/// The rules are transcribed from every at-rule prelude in the cached Parsoid
/// output, where all of these appear:
///
/// ```text
/// @media all and (max-width:500px){
/// @media print{
/// @media(max-width:640px){
/// @media(min-width:640px)and (hover:hover)and (pointer:fine){
/// ```
fn normalise_at_prelude(prelude: &str) -> String {
    // The at-rule name (`@media`) ends at the first space or `(`. What follows
    // decides how the prelude is spaced, so split it off up front instead of
    // threading a flag through a character loop.
    let name_end = prelude.find([' ', '(']).unwrap_or(prelude.len());
    let (name, remainder) = prelude.split_at(name_end);
    let mut out = String::with_capacity(prelude.len());
    out.push_str(name);

    // A condition (a `(…)`) joins the name; a media *type* keeps a single space.
    // Runs of whitespace collapse either way, so `@media  screen` cannot leak a
    // double space into the output.
    let rest = remainder.trim_start_matches(' ');
    if !rest.starts_with('(') {
        // A media type follows, so the separating space is significant. The
        // remainder is non-empty whenever that is the case.
        out.push(' ');
    }

    // Within what remains, spaces after `:` and `,` go, a space before `)`
    // goes, and `and` loses the space before it when it follows a `)` so that
    // `…) and (` becomes `…)and (`. The space before `and` is the only place
    // `and` is special: after a media type it is significant, as in
    // `@media screen and (…)`.
    let mut after_paren = false;
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '(' => {
                out.push('(');
                after_paren = false;
                // Drop the spaces that follow an opening parenthesis.
                while chars.peek().is_some_and(|n| *n == ' ') {
                    chars.next();
                }
            }
            ')' => {
                // A space before `)` goes, mirroring `normalise_value`. Parsoid
                // closes every condition tightly: the cached output holds
                // `(max-width:500px)` and `(prefers-color-scheme:dark)`, and not
                // a single space before a `)` in any at-rule prelude.
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(')');
                after_paren = true;
            }
            ':' | ',' => {
                out.push(c);
                while chars.peek().is_some_and(|n| *n == ' ') {
                    chars.next();
                }
            }
            ' ' => {
                // A space after `)` is dropped only when `and` follows.
                let mut lookahead = chars.clone();
                let is_and = lookahead.by_ref().take(3).collect::<String>() == "and";
                if !(after_paren && is_and) {
                    out.push(' ');
                }
            }
            _ => {
                after_paren = false;
                out.push(c);
            }
        }
    }
    out.trim_end().to_string()
}

/// Tidy a declaration's value: drop whitespace around `,`, `/`, `%` and `)`,
/// and prefer double quotes.
///
/// None of this is cosmetic — the comparison is byte-for-byte — and the rules
/// were read off Parsoid's own output rather than assumed:
///
/// ```text
/// rect(0, 0, 0, 0)         -> rect(0,0,0,0)
/// invert(1) brightness(…)  -> invert(1)brightness(…)
/// 0 100% 100% 0 / 50%      -> 0 100%100%0/50%
/// ```
///
/// A space that separates two *values* is meaningful and is kept:
/// `0.5em 0 1em`, `1px solid #aaa`, `0 auto`. The distinction is that `,`, `/`,
/// `%` and `)` end or join a component, while a space between two values is the
/// component separator itself.
///
/// `calc()` is the exception, and it is not a special case bolted on: its
/// arguments are an arithmetic expression in which the spaces *are* the
/// operators, so `calc(100% - 0.7em)` cannot lose them. It is the only function
/// with that property, and every space that survives next to a `%` in the
/// corpus is inside one.
fn normalise_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    // Nesting depth of `calc(`, whose contents are passed through untouched.
    let mut calc_depth = 0usize;

    while let Some(c) = chars.next() {
        if calc_depth > 0 {
            match c {
                '(' => calc_depth += 1,
                ')' => calc_depth -= 1,
                _ => {}
            }
            out.push(c);
            continue;
        }

        match c {
            '\'' => out.push('"'),
            ',' | '/' | '%' => {
                // The spaces on either side of a joining punctuation mark go.
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(c);
                while chars.peek().is_some_and(|n| *n == ' ') {
                    chars.next();
                }
            }
            ')' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(')');
                // A function chained to the next one loses the separating space.
                let mut lookahead = chars.clone();
                let spaces = lookahead.by_ref().take_while(|n| *n == ' ').count();
                let next = lookahead.next();
                if spaces > 0 && next.is_some_and(|n| n.is_ascii_alphabetic() || n == '-') {
                    for _ in 0..spaces {
                        chars.next();
                    }
                }
            }
            _ => out.push(c),
        }

        // `calc(` opens a pass-through region. Detected after the character is
        // emitted so the `(` itself is handled by the branch above.
        if c == '(' && out.ends_with("calc(") {
            calc_depth = 1;
        }
    }
    out
}

/// Prefix a selector list with the scope, following TemplateStyles' rules.
///
/// Every simple selector is scoped, *except* one that begins with `html` or
/// `body` followed by a descendant combinator. That exception is documented in
/// Extension:TemplateStyles ("to target styles based on skins, use a selector
/// such as `body.skin-vector .myClass`; specification of the `body` element is
/// required") and is what lets a skin-dependent rule escape the content scope.
///
/// Within an at-rule the selector is still scoped — `@media print{body.ns-0
/// .mw-parser-output .hatnote{…}}` — so this is applied at every depth.
fn scope_selector(selector: &str, scope: &str) -> String {
    selector
        .split(',')
        .map(|part| scope_one(part.trim(), scope))
        .collect::<Vec<_>>()
        .join(",")
}

/// Scope a single selector.
///
/// The scope goes on the *subject* of the selector — the last compound — which
/// is where it lands for an ordinary rule. For a document selector
/// (`body.ns-0 .hatnote`) the `body` prefix is left alone and the scope is
/// inserted after it, giving `body.ns-0 .mw-parser-output .hatnote`: the escape
/// hatch is for the `body`/`html` element, not for everything downstream of it.
fn scope_one(selector: &str, scope: &str) -> String {
    if selector.is_empty() || selector.starts_with('@') {
        return selector.to_string();
    }
    let normalised = normalise_combinators(selector);
    match document_prefix(&normalised) {
        // `body.ns-0 .hatnote` → `body.ns-0 .mw-parser-output .hatnote`
        Some(prefix_len) => {
            let (prefix, rest) = normalised.split_at(prefix_len);
            let rest = rest.trim_start();
            if rest.is_empty() {
                return prefix.trim_end().to_string();
            }
            format!("{prefix}{scope} {rest}")
        }
        None => format!("{scope} {normalised}"),
    }
}

/// If `selector` opens with the documented `html`/`body` escape hatch, return
/// the byte length of the prefix up to and including its descendant combinator.
///
/// The requirement is a `html` or `body` element followed by a *descendant*
/// combinator, so `body.ns-0 .hatnote` matches while a bare `body` or
/// `body > .x` does not. Qualifiers before the space are part of the prefix
/// (`body:not(.skin-minerva) .infobox` is in the cached output), which is why
/// the search skips over balanced parentheses.
fn document_prefix(selector: &str) -> Option<usize> {
    let rest = selector
        .strip_prefix("html")
        .or_else(|| selector.strip_prefix("body"))?;
    let head = selector.len() - rest.len();
    let mut depth = 0usize;
    for (offset, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ' ' if depth == 0 => return Some(head + offset + 1),
            _ => {}
        }
    }
    None
}

/// Tighten the combinators: `.a + .b` → `.a+.b`.
///
/// Parsoid's output is `.mw-parser-output .hatnote+.mw-empty-elt+.hatnote` for
/// a source of `.hatnote + .mw-empty-elt + .hatnote`, so the whitespace around
/// `+` and `~` goes. A descendant combinator is a single space and is kept,
/// since removing it would change the selector's meaning.
fn normalise_combinators(selector: &str) -> String {
    let mut out = String::with_capacity(selector.len());
    let mut pending_space = false;
    let mut chars = selector.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        let is_compound_combinator = matches!(c, '+' | '~' | '>');
        if is_compound_combinator {
            // Drop any space before it, then skip the spaces after it.
            while out.ends_with(' ') {
                out.pop();
            }
            out.push(c);
            while chars.peek().is_some_and(|n| n.is_whitespace()) {
                chars.next();
            }
            pending_space = false;
            continue;
        }
        if pending_space && !out.ends_with(['+', '~', '>']) {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    out
}

/// If `selector` opens with the documented `html`/`body` escape hatch, return
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
/// `body` is always `{"extsrc":""}` in the output: the stylesheet is not
/// round-trippable source, so Parsoid records it as empty and the CSS lives in
/// the element's text.
///
/// The `about` id is taken by the caller, which knows where in the document
/// sequence the stylesheet belongs: a `<templatestyles>` is resolved while
/// expansion runs, so its id follows the transclusions around it rather than
/// trailing the whole page.
pub fn style_node(css: &str, revid: u64, src: &str, about: &str) -> crate::dom::node::Node {
    use crate::dom::node::{ElementKind, Node};

    let mut style = Node::element(ElementKind::Other("style".to_string()));
    // Attribute order matters for a byte comparison, and this is the order the
    // cached Parsoid output uses.
    style.set_attr("data-mw-deduplicate", format!("TemplateStyles:r{revid}"));
    style.set_attr("typeof", "mw:Extension/templatestyles");
    style.set_attr("about", about);
    style.set_attr(
        "data-mw",
        format!(r#"{{"name":"templatestyles","attrs":{{"src":"{src}"}},"body":{{"extsrc":""}}}}"#),
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

    /// The `wrapper` attribute replaces the scope, which is how a sandbox copy
    /// of a stylesheet is kept separate from the live one.
    #[test]
    fn a_wrapper_replaces_the_scope() {
        assert_eq!(render(".a{b:1}", Some(".sandbox")), ".sandbox .a{b:1}");
        // The `html`/`body` prefix is left alone and the scope follows it: the
        // exception is for the document element, not for what comes after it.
        assert_eq!(
            render("body .a{b:1}", Some(".sandbox")),
            "body .sandbox .a{b:1}"
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

    /// `calc()` keeps its spaces: inside it they are the operators, so dropping
    /// them changes the value. It is the one place a space next to `%` survives.
    #[test]
    fn calc_keeps_its_spaces() {
        assert_eq!(
            render(".a{width:calc(100% - 0.7em)}", None),
            ".mw-parser-output .a{width:calc(100% - 0.7em)}"
        );
        assert_eq!(
            render(".a{width:calc( 1px + 2px )}", None),
            ".mw-parser-output .a{width:calc( 1px + 2px )}"
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
