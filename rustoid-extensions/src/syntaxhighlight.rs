//! The SyntaxHighlight extension (`<syntaxhighlight>`).
//!
//! Parsoid renders the tag through `SyntaxHighlight::sourceToDom`: a wrapper
//! (`<div>` for a block, `<code>` inline) carrying the extension attributes and
//! the highlighted body. The highlighting itself is Pygments in the PHP
//! extension.
//!
//! Rustoid has no Pygments. This module produces the wrapper, and the body as
//! escaped text: that is exactly what Pygments' `TextLexer` produces for
//! `lang=text` and for a language it does not know, and it is the correct
//! structure for a highlighted block (the token spans inside are a recorded
//! gap; see ONLINE-PARITY.md).

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

/// Render one `<syntaxhighlight>` tag. `None` when the tag names no language,
/// which the extension leaves to its error path.
pub fn render(request: &Request) -> Option<Node> {
    let lang = attribute(request.attrs, "lang")
        .or_else(|| attribute(request.attrs, "language"))
        .unwrap_or("text");
    let inline = request
        .attrs
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("inline"));

    let mut node = Node::element(ElementKind::Other(if inline {
        "code".to_string()
    } else {
        "div".to_string()
    }));

    // `sourceToDom` copies the tag attributes it does not consume, in source
    // order. `class` and `style` are the observable ones: `class` keeps its
    // source position when written (and receives the generated classes), and an
    // empty `style=""` is still serialized. A `class` the source omitted is
    // added after the copied attributes.
    let mut classes = vec![
        "mw-highlight".to_string(),
        format!("mw-highlight-lang-{lang}"),
        "mw-content-ltr".to_string(),
    ];
    let mut has_class = false;
    let mut has_dir = false;
    for (key, value) in request.attrs {
        match key.to_ascii_lowercase().as_str() {
            "lang" | "language" | "inline" | "line" | "start" | "highlight" | "linenos"
            | "linenostart" | "enclose" | "title" | "page" | "filename" | "debug" => {}
            "class" => {
                has_class = true;
                let user = value.trim();
                if !user.is_empty() {
                    classes.push(user.to_string());
                }
                node.set_attr("class", classes.join(" "));
            }
            "style" => node.set_attr("style", value.clone()),
            "dir" => {
                has_dir = true;
                node.set_attr("dir", value.clone());
            }
            _ => node.set_attr(key.clone(), value.clone()),
        }
    }
    if !has_class {
        node.set_attr("class", classes.join(" "));
    }
    if !has_dir {
        node.set_attr("dir", if is_rtl(lang) { "rtl" } else { "ltr" });
    }
    node.set_attr("typeof", "mw:Extension/syntaxhighlight");
    node.set_attr("about", request.about.clone());
    node.set_attr("data-mw", request.data_mw.clone());

    let body = request.body.unwrap_or("");
    if inline {
        if !body.is_empty() {
            node.push_child(Node::text(body));
        }
    } else {
        let mut pre = Node::element(ElementKind::Other("pre".to_string()));
        let mut text = body.to_string();
        // Pygments' `ensurenl` guarantees a trailing newline.
        if !text.ends_with('\n') {
            text.push('\n');
        }
        pre.push_child(Node::text(text));
        node.push_child(pre);
    }
    Some(node)
}

fn attribute<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The languages whose text direction is right-to-left, as
/// `Language::getDir` reports it for the codes the extension accepts.
fn is_rtl(lang: &str) -> bool {
    let base = lang.split(['-', '_']).next().unwrap_or(lang);
    matches!(
        base,
        "ar" | "arc" | "dv" | "fa" | "he" | "khw" | "ks" | "ku" | "ps" | "sd" | "ur" | "yi"
    )
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

    #[test]
    fn an_empty_inline_tag_is_one_code_wrapper() {
        let attrs = vec![
            ("lang".to_string(), "wikitext".to_string()),
            ("inline".to_string(), String::new()),
            ("style".to_string(), "display: none;".to_string()),
        ];
        let node = render(&request(&attrs, None)).expect("a language is named");
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
        let node = render(&request(&attrs, Some("trace"))).expect("a language is named");
        assert!(
            matches!(node.children.as_slice(), [child] if matches!(&child.kind, NodeKind::Text(t) if t == "trace"))
        );
    }

    #[test]
    fn a_block_body_is_a_pre_with_a_trailing_newline() {
        let attrs = vec![("lang".to_string(), "wikitext".to_string())];
        let node = render(&request(&attrs, Some("| name = value"))).expect("a language is named");
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
        assert!(
            matches!(pre.children.as_slice(), [child] if matches!(&child.kind, NodeKind::Text(t) if t == "| name = value\n"))
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
        let node = render(&request(&attrs, Some("trace"))).expect("a language is named");
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
        let node = render(&request(&attrs, None)).expect("a language is named");
        let keys: Vec<&str> = node.attrs.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["style", "class", "dir", "typeof", "about", "data-mw"]
        );
    }

    #[test]
    fn an_rtl_language_sets_dir() {
        let attrs = vec![("lang".to_string(), "ar".to_string())];
        let node = render(&request(&attrs, None)).expect("a language is named");
        assert_eq!(node.get_attr("dir"), Some("rtl"));
    }
}
