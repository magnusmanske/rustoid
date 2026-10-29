//! The SyntaxHighlight extension (`<syntaxhighlight>`).
//!
//! Parsoid renders the tag through `SyntaxHighlight::sourceToDom`: a wrapper
//! (`<div>` for a block, `<code>` inline) carrying the extension attributes and
//! the highlighted body. The highlighting is Pygments in the PHP extension;
//! [`wikitext`] and [`highlight`] port it for the wikitext lexer, and `text`
//! needs no lexing at all.
//!
//! The wrapper's own rules are observable too, so they follow the PHP:
//!
//! - the body is `rtrim(trim($text, "\n"))` (leading/trailing linefeeds, then
//!   trailing whitespace), and an inline body is `trim`med again;
//! - an empty body produces no `<pre>` at all;
//! - a language Pygments knows gets `mw-highlight-lang-{lang}`, an empty
//!   `<span></span>` before the first line (Pygments' `_wrap_pre`) and exactly
//!   one trailing newline; a language it does not know is plain `<pre>` text with
//!   no language class and no added newline;
//! - only `class`, `style` and `id` are copied from the tag, and `dir` is
//!   computed from the `dir` argument.

mod highlight;
mod lexer;
mod wikitext;

use rustoid_ast::dom::node::{ElementKind, Node};

/// Everything the renderer needs from the parser.
pub struct Request<'a> {
    /// The tag's attributes in source order (the `data-mw.attrs` echo).
    pub attrs: &'a [(String, String)],
    /// The tag body; `None` for a self-closing tag.
    pub body: Option<&'a str>,
    /// The `about` id the extension encapsulation took.
    pub about: String,
    /// The serialized `data-mw`, already built by the parser.
    pub data_mw: String,
}

/// The languages this port lexes. A language Pygments knows but this port does
/// not is rendered as plain code (see the module docs).
fn is_lexed(lang: &str) -> bool {
    matches!(lang, "wikitext" | "mediawiki" | "text")
}

/// Render one `<syntaxhighlight>` tag.
pub fn render(request: &Request) -> Option<Node> {
    let lang = attribute(request.attrs, "lang")
        .or_else(|| attribute(request.attrs, "language"))
        .unwrap_or("")
        .to_ascii_lowercase();
    let inline = request
        .attrs
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("inline"));

    // `SyntaxHighlight::processContent`: `rtrim(trim($text, "\n"))`, then
    // `trim` again for an inline tag.
    let mut code = request.body.unwrap_or("").trim_matches('\n').trim_end();
    if inline {
        code = code.trim();
    }

    let mut node = Node::element(ElementKind::Other(if inline {
        "code".to_string()
    } else {
        "div".to_string()
    }));

    // Only these three survive `Sanitizer::validateAttributes`, and they keep
    // their source order; `class` is replaced in place by the generated list.
    let copied: Vec<&(String, String)> = request
        .attrs
        .iter()
        .filter(|(key, _)| matches!(key.to_ascii_lowercase().as_str(), "class" | "style" | "id"))
        .collect();
    let mut class_list: Vec<String> = copied
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("class"))
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    class_list.push("mw-highlight".to_string());
    if is_lexed(&lang) {
        class_list.push(format!("mw-highlight-lang-{lang}"));
    }
    let dir = if attribute(request.attrs, "dir") == Some("rtl") {
        "rtl"
    } else {
        "ltr"
    };
    class_list.push(format!("mw-content-{dir}"));
    let class = class_list.join(" ");
    let mut has_class = false;
    for (key, value) in copied.iter().map(|(key, value)| (key, value)) {
        if key.eq_ignore_ascii_case("class") {
            node.set_attr("class", class.clone());
            has_class = true;
        } else {
            node.set_attr((*key).clone(), (*value).clone());
        }
    }
    if !has_class {
        node.set_attr("class", class);
    }
    node.set_attr("dir", dir);
    node.set_attr("typeof", "mw:Extension/syntaxhighlight");
    node.set_attr("about", request.about.clone());
    node.set_attr("data-mw", request.data_mw.clone());

    if !code.is_empty() {
        if inline {
            for child in body_nodes(&lang, code, true) {
                node.push_child(child);
            }
        } else {
            let mut pre = Node::element(ElementKind::Other("pre".to_string()));
            if is_lexed(&lang) {
                for child in body_nodes(&lang, code, false) {
                    pre.push_child(child);
                }
            } else {
                // `plainCodeWrap`: escaped text, no trailing newline.
                pre.push_child(Node::text(code.to_string()));
            }
            node.push_child(pre);
        }
    }
    Some(node)
}

/// The body's nodes, already highlighted and (for the block case) prefixed with
/// Pygments' empty anchor span.
fn body_nodes(lang: &str, code: &str, inline: bool) -> Vec<Node> {
    // Pygments' `ensurenl`: the lexer's input always ends with a newline.
    let source = format!("{code}\n");
    let tokens: Vec<(&'static str, &str)> = if lang == "text" {
        vec![("", source.as_str())]
    } else {
        wikitext::lex(&source)
    };
    highlight::nodes(&tokens, inline)
}

fn attribute<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustoid_ast::dom::node::NodeKind;

    fn request<'a>(attrs: &'a [(String, String)], body: Option<&'a str>) -> Request<'a> {
        Request {
            attrs,
            body,
            about: "#mwt10".to_string(),
            data_mw: "{}".to_string(),
        }
    }

    fn text_of(node: &Node) -> String {
        match &node.kind {
            NodeKind::Text(t) => t.clone(),
            _ => node.children.iter().map(text_of).collect(),
        }
    }

    #[test]
    fn an_empty_inline_tag_is_one_code_wrapper() {
        let attrs = vec![
            ("lang".to_string(), "wikitext".to_string()),
            ("inline".to_string(), String::new()),
            ("style".to_string(), "display: none;".to_string()),
        ];
        let node = render(&request(&attrs, None)).expect("always rendered");
        assert!(
            matches!(&node.kind, NodeKind::Element(ElementKind::Other(tag)) if tag == "code"),
            "{:?}",
            node.kind
        );
        assert_eq!(
            node.get_attr("class"),
            Some("mw-highlight mw-highlight-lang-wikitext mw-content-ltr")
        );
        assert_eq!(node.get_attr("style"), Some("display: none;"));
        assert_eq!(node.get_attr("dir"), Some("ltr"));
        assert!(node.children.is_empty());
    }

    #[test]
    fn an_inline_body_is_its_text() {
        let attrs = vec![
            ("lang".to_string(), "text".to_string()),
            ("inline".to_string(), "1".to_string()),
        ];
        let node = render(&request(&attrs, Some("trace"))).expect("always rendered");
        assert!(
            matches!(node.children.as_slice(), [child] if matches!(&child.kind, NodeKind::Text(t) if t == "trace"))
        );
    }

    #[test]
    fn a_block_body_is_a_pre_with_an_anchor_span_and_a_trailing_newline() {
        let attrs = vec![("lang".to_string(), "text".to_string())];
        let node = render(&request(&attrs, Some("| name = value"))).expect("always rendered");
        assert!(
            matches!(&node.kind, NodeKind::Element(ElementKind::Other(tag)) if tag == "div"),
            "{:?}",
            node.kind
        );
        let [pre] = node.children.as_slice() else {
            panic!("expected the <pre>");
        };
        assert!(
            matches!(&pre.kind, NodeKind::Element(ElementKind::Other(tag)) if tag == "pre"),
            "{:?}",
            pre.kind
        );
        let Some(anchor) = pre.children.first() else {
            panic!("expected the anchor span");
        };
        assert!(
            matches!(&anchor.kind, NodeKind::Element(ElementKind::Other(tag)) if tag == "span")
        );
        assert!(anchor.children.is_empty());
        assert_eq!(text_of(pre), "| name = value\n");
    }

    #[test]
    fn an_unknown_language_is_plain_code() {
        let attrs = vec![("lang".to_string(), "nosuchlang".to_string())];
        let node = render(&request(&attrs, Some("a < b"))).expect("always rendered");
        assert_eq!(
            node.get_attr("class"),
            Some("mw-highlight mw-content-ltr"),
            "an unknown language adds no lang class"
        );
        assert_eq!(text_of(&node), "a < b", "no newline is added");
    }

    #[test]
    fn an_empty_block_has_no_pre() {
        let attrs = vec![("lang".to_string(), "wikitext".to_string())];
        let node = render(&request(&attrs, Some("\n\n"))).expect("always rendered");
        assert!(node.children.is_empty());
        assert_eq!(
            node.get_attr("class"),
            Some("mw-highlight mw-highlight-lang-wikitext mw-content-ltr")
        );
    }

    #[test]
    fn an_empty_style_is_kept_and_a_source_class_keeps_its_position() {
        // Source order `lang, class, style, inline`: the copied attributes come
        // out as class, style, dir; the generated class merges into the source
        // one and the empty style is still serialized.
        let attrs = vec![
            ("lang".to_string(), "text".to_string()),
            ("class".to_string(), String::new()),
            ("style".to_string(), String::new()),
            ("inline".to_string(), "1".to_string()),
        ];
        let node = render(&request(&attrs, Some("trace"))).expect("always rendered");
        let keys: Vec<&str> = node.attrs.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["class", "style", "dir", "typeof", "about", "data-mw"]
        );
        assert_eq!(node.get_attr("style"), Some(""));

        // With no source `class`, the generated one follows `style`.
        let attrs = vec![
            ("lang".to_string(), "wikitext".to_string()),
            ("inline".to_string(), String::new()),
            ("style".to_string(), "display: none;".to_string()),
        ];
        let node = render(&request(&attrs, None)).expect("always rendered");
        let keys: Vec<&str> = node.attrs.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["style", "class", "dir", "typeof", "about", "data-mw"]
        );
    }

    #[test]
    fn an_rtl_language_sets_dir() {
        let attrs = vec![("dir".to_string(), "rtl".to_string())];
        let node = render(&request(&attrs, None)).expect("always rendered");
        assert_eq!(node.get_attr("dir"), Some("rtl"));
        assert_eq!(node.get_attr("class"), Some("mw-highlight mw-content-rtl"));
    }
}
