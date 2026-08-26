//! SanitizerHandler — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/SanitizerHandler.php`.
//!
//! General token sanitizer that strips out (or converts to text) disallowed
//! HTML tags and attributes. Runs last in the TokenTransform3 stage.

use crate::wikitext::token_utils;
use crate::wikitext::tokens_v2::{Item, ParsoidToken};

/// The set of HTML tags with no end tag (e.g. `<br>`).
fn no_end_tag_set(name: &str) -> bool {
    name == "br"
}

/// The set of allowed literal HTML tags (from Consts::$Sanitizer['AllowedLiteralTags']).
fn allowed_literal_tags() -> &'static [&'static str] {
    &[
        "abbr",
        "b",
        "bdi",
        "bdo",
        "big",
        "blockquote",
        "br",
        "caption",
        "center",
        "cite",
        "code",
        "data",
        "dd",
        "del",
        "dfn",
        "div",
        "dl",
        "dt",
        "em",
        "font",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "i",
        "ins",
        "kbd",
        "li",
        "link",
        "mark",
        "meta",
        "ol",
        "p",
        "pre",
        "q",
        "rb",
        "rp",
        "rt",
        "rtc",
        "ruby",
        "s",
        "samp",
        "small",
        "span",
        "strike",
        "strong",
        "sub",
        "sup",
        "table",
        "td",
        "th",
        "time",
        "tr",
        "tt",
        "u",
        "ul",
        "var",
        "wbr",
    ]
}

fn is_allowed_literal_tag(name: &str) -> bool {
    allowed_literal_tags().contains(&name)
}

/// The SanitizerHandler.
pub struct SanitizerHandler {
    /// Whether we're processing template content (affects source-text
    /// retrieval for disallowed tags; the source-text path is not yet wired
    /// because the token layer doesn't carry source text).
    #[allow(dead_code)]
    in_template: bool,
}

impl SanitizerHandler {
    pub fn new(in_template: bool) -> Self {
        Self { in_template }
    }

    /// Run the sanitizer over a token stream.
    pub fn run(&mut self, tokens: Vec<Item>) -> Vec<Item> {
        let mut output = Vec::new();
        for token in tokens {
            let res = self.on_any(token);
            match res {
                Some(mut items) => output.append(&mut items),
                None => {
                    // Null means pass through; but on_any returns the token in the vec.
                    // (Handled inside on_any.)
                }
            }
        }
        output
    }

    /// Handle a single token.
    fn on_any(&mut self, token: Item) -> Option<Vec<Item>> {
        if let Item::Str(_) = &token {
            return Some(vec![token]);
        }

        let new_token = self.sanitize_token(&token);
        match new_token {
            Some(new) if new != token => Some(vec![new]),
            _ => Some(vec![token]),
        }
    }

    /// Sanitize a single token. Returns None if unchanged.
    fn sanitize_token(&mut self, token: &Item) -> Option<Item> {
        let Item::Tok(tok) = token else {
            return None;
        };

        // Only literal HTML tags (`stx: "html"`) are sanitized. Parsoid-inserted
        // meta/link/behavior-switch tokens (with `typeof="mw:*"` etc.) bypass
        // the sanitizer exactly as in PHP (`SanitizerHandler::onAny` only acts on
        // `isHTMLTag` tokens).
        if !token_utils::is_html_tag(tok) {
            return None;
        }

        let name = tok.get_name();
        let attribs = tok.get_attribs().to_vec();

        // Unknown/disallowed HTML tag → convert to plain text.
        let disallowed = !is_allowed_literal_tag(name)
            || (matches!(tok, ParsoidToken::EndTag(_)) && no_end_tag_set(name));

        if disallowed {
            // Prefer the original source (preserves the author's original tag
            // case and spacing), mirroring PHP's `getWTSource` fallback when a
            // TSR is available outside template context.
            if let Some(src) = tok.data_parsoid().and_then(|dp| dp.src.as_deref())
                && !src.is_empty()
            {
                return Some(Item::Str(src.to_string()));
            }
            return Some(Item::Str(Self::tag_to_text(name, &attribs, tok)));
        }

        // Sanitize attributes (faithful `sanitizeTagAttrs` whitelist).
        if !attribs.is_empty() {
            if matches!(tok, ParsoidToken::Tag(_) | ParsoidToken::SelfclosingTag(_)) {
                let sanitized = crate::sanitizer::sanitize_tag_attrs(name, attribs, |_proto| true);
                let mut new_tok = tok.clone();
                new_tok.set_attribs(sanitized);
                return Some(Item::Tok(new_tok));
            } else {
                // EndTagTk: drop attributes.
                let mut new_tok = tok.clone();
                new_tok.set_attribs(vec![]);
                return Some(Item::Tok(new_tok));
            }
        }

        None
    }

    /// Convert a disallowed tag token back to its textual source.
    fn tag_to_text(
        name: &str,
        attribs: &[crate::wikitext::tokens_v2::KV],
        tok: &ParsoidToken,
    ) -> String {
        match tok {
            ParsoidToken::EndTag(_) => format!("</{name}>"),
            ParsoidToken::SelfclosingTag(_) => {
                let mut buf = format!("<{name}");
                for kv in attribs {
                    buf.push(' ');
                    buf.push_str(&token_utils::key_value_to_string(&kv.key));
                    buf.push_str("='");
                    buf.push_str(&token_utils::key_value_to_string(&kv.value));
                    buf.push('\'');
                }
                buf.push_str(" />");
                buf
            }
            _ => {
                let mut buf = format!("<{name}");
                for kv in attribs {
                    buf.push(' ');
                    buf.push_str(&token_utils::key_value_to_string(&kv.key));
                    buf.push_str("='");
                    buf.push_str(&token_utils::key_value_to_string(&kv.value));
                    buf.push('\'');
                }
                buf.push('>');
                buf
            }
        }
    }
}

impl Default for SanitizerHandler {
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::{DataParsoid, TagTk};

    fn text(s: &str) -> Item {
        Item::Str(s.to_string())
    }

    #[test]
    fn test_plain_text_passthrough() {
        let mut handler = SanitizerHandler::new(false);
        let out = handler.run(vec![text("hello")]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Item::Str(s) if s == "hello"));
    }

    #[test]
    fn test_disallowed_tag_to_text() {
        // An unknown HTML tag should be converted to text.
        let mut tk = TagTk::new("unknown", vec![], DataParsoid::default());
        tk.data_parsoid.stx = Some("html".to_string());
        let token = Item::Tok(ParsoidToken::Tag(tk));

        let mut handler = SanitizerHandler::new(false);
        let out = handler.run(vec![token]);

        // Should have been converted to "<unknown>" text.
        let has_text = out
            .iter()
            .any(|it| matches!(it, Item::Str(s) if s == "<unknown>"));
        assert!(has_text, "expected '<unknown>' text, got {:?}", out);
    }

    #[test]
    fn test_allowed_tag_passthrough() {
        // A <b> tag (allowed) should pass through unchanged.
        let mut tk = TagTk::new("b", vec![], DataParsoid::default());
        tk.data_parsoid.stx = Some("html".to_string());
        let token = Item::Tok(ParsoidToken::Tag(tk.clone()));

        let mut handler = SanitizerHandler::new(false);
        let out = handler.run(vec![token]);

        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "b"));
    }

    #[test]
    fn test_disallowed_tag_preserves_src_case() {
        // A disallowed <President> tag is converted back to text using its
        // original source (preserving case), mirroring PHP's `getWTSource`
        // fallback in `SanitizerHandler::sanitizeToken`.
        let mut tk = TagTk::new("president", vec![], DataParsoid::default());
        tk.data_parsoid.stx = Some("html".to_string());
        tk.data_parsoid.src = Some("<President>".to_string());
        let token = Item::Tok(ParsoidToken::Tag(tk));

        let mut handler = SanitizerHandler::new(false);
        let out = handler.run(vec![token]);

        let has_text = out
            .iter()
            .any(|it| matches!(it, Item::Str(s) if s == "<President>"));
        assert!(has_text, "expected '<President>' text, got {:?}", out);
    }
}
