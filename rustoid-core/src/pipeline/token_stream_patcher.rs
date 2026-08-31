//! TokenStreamPatcher — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/TokenStreamPatcher.php`.
//!
//! This is a line-based handler that runs *first* in the TT3 pipeline (before
//! PreHandler). It repairs the token stream in ways the PEG tokenizer cannot:
//!
//!   * It buffers newlines, whitespace, and transclusion marker metas; when a
//!     SOL-transparent link (a `<link rel="mw:PageProp/Category|redirect|Language">`,
//!     i.e. a category/redirect/language link) is then seen, the buffered
//!     newlines are re-wrapped into `EmptyLineTk` tokens so the ParagraphWrapper
//!     treats the preceding lines as empty lines (keeps the category link
//!     attached to its preceding content instead of starting a fresh paragraph).
//!   * It tracks `mw:Transclusion`/`mw:Transclusion/End` marker metas so a nested
//!     empty transclusion doesn't disturb SOL state.
//!
//! NOTE: the `trReparseBuf` table-row attribute reparse (T2529) and
//! `convertNonHTMLTokenToString` table-tag stitching are not yet ported; they
//! are only needed by the "transclusion in discarded table attribute position"
//! cluster (T322557).

use crate::wikitext::tokens_v2::{DataParsoid, EmptyLineTk, Item, ParsoidToken, SelfclosingTagTk};

/// Whether a token is a SOL-transparent *link* tag (a `<link>` whose `rel` is a
/// page-prop category/redirect/language link). Mirrors PHP
/// `TokenUtils::isSolTransparentLinkTag`.
fn is_sol_transparent_link_tag(token: &ParsoidToken) -> bool {
    let ParsoidToken::SelfclosingTag(t) = token else {
        return false;
    };
    if t.name != "link" {
        return false;
    }
    let Some(rel) = t
        .attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("rel"))
        .and_then(|kv| kv.value.as_str())
    else {
        return false;
    };
    rel.split_whitespace().any(|r| {
        r == "mw:PageProp/Category" || r == "mw:PageProp/redirect" || r == "mw:PageProp/Language"
    })
}

fn is_whitespace_string(item: &Item) -> bool {
    matches!(item, Item::Str(s) if !s.is_empty() && s.chars().all(|c| c == ' ' || c == '\t'))
}

/// The TokenStreamPatcher handler.
pub struct TokenStreamPatcher {
    /// Buffered newline/whitespace/metas awaiting a SOL-transparent link (or
    /// flush at the next non-whitespace token). Mirrors PHP's
    /// `$nlWsMetaTokenBuf`.
    nl_ws_meta_token_buf: Vec<Item>,
}

impl TokenStreamPatcher {
    pub fn new() -> Self {
        Self {
            nl_ws_meta_token_buf: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.nl_ws_meta_token_buf.clear();
    }

    /// Emit buffered newlines/whitespace/metas before `ret`. Mirrors PHP
    /// `getResultTokens`.
    fn get_result_tokens(&mut self, ret: Vec<Item>) -> Vec<Item> {
        if !self.nl_ws_meta_token_buf.is_empty() {
            let buf = std::mem::take(&mut self.nl_ws_meta_token_buf);
            let mut out = buf;
            out.extend(ret);
            out
        } else {
            ret
        }
    }

    /// Run the handler over a token stream.
    pub fn run(&mut self, tokens: Vec<Item>) -> Vec<Item> {
        let mut out = Vec::new();
        for token in tokens {
            match &token {
                Item::Tok(ParsoidToken::Nl(_)) => {
                    // onNewline: buffer the newline, stay at SOL.
                    self.nl_ws_meta_token_buf.push(token);
                }
                Item::Tok(ParsoidToken::Eof(_)) => {
                    // onEnd: flush buffered newlines/metas, then emit EOF.
                    out.extend(self.get_result_tokens(vec![token]));
                }
                _ => {
                    if let Some(items) = self.on_any(token) {
                        out.extend(items);
                    }
                }
            }
        }
        // Flush anything still buffered at end-of-stream.
        out.extend(self.get_result_tokens(Vec::new()));
        self.reset();
        out
    }

    /// Handle a non-newline/EOF token. Mirrors PHP `onAnyInternal` (minus the
    /// table-row reparse and `convertNonHTMLTokenToString` paths).
    fn on_any(&mut self, token: Item) -> Option<Vec<Item>> {
        match &token {
            Item::Str(_) => {
                // Whitespace-only strings are buffered with pending newlines;
                // otherwise flush the buffer first.
                if is_whitespace_string(&token) && !self.nl_ws_meta_token_buf.is_empty() {
                    self.nl_ws_meta_token_buf.push(token);
                    return Some(Vec::new());
                }
                Some(self.get_result_tokens(vec![token]))
            }
            Item::Tok(ParsoidToken::Comment(_)) | Item::Tok(ParsoidToken::EmptyLine(_)) => {
                // Comments / EmptyLines don't change SOL state.
                Some(self.get_result_tokens(vec![token]))
            }
            Item::Tok(ParsoidToken::SelfclosingTag(stt)) => self.on_selfclosing(stt.clone(), token),
            _ => Some(self.get_result_tokens(vec![token])),
        }
    }

    /// Handle a self-closing tag. Mirrors PHP's `SelfclosingTagTk` branch of
    /// `onAnyInternal`.
    fn on_selfclosing(&mut self, stt: SelfclosingTagTk, token: Item) -> Option<Vec<Item>> {
        // A sol-transparent link after buffered newlines: re-wrap the buffered
        // newlines/metas into EmptyLineTk tokens (tunnels them through the
        // line-based handlers without affecting them).
        if is_sol_transparent_link_tag(&ParsoidToken::SelfclosingTag(stt.clone())) {
            let n = self.nl_ws_meta_token_buf.len();
            if n > 0 {
                // Split at the first self-closing tag (a transclusion start
                // meta), matching PHP's `while (!$tok instanceof
                // SelfclosingTagTk) $i++`.
                let mut i = 0;
                while i < n
                    && !matches!(
                        self.nl_ws_meta_token_buf[i],
                        Item::Tok(ParsoidToken::SelfclosingTag(_))
                    )
                {
                    i += 1;
                }
                let mut toks: Vec<Item> = Vec::new();
                if i > 0 {
                    let inner: Vec<ParsoidToken> = self.nl_ws_meta_token_buf[..i]
                        .iter()
                        .cloned()
                        .filter_map(|it| match it {
                            Item::Tok(t) => Some(t),
                            Item::Str(_) => None,
                        })
                        .collect();
                    toks.push(Item::Tok(ParsoidToken::EmptyLine(EmptyLineTk::new(
                        inner,
                        DataParsoid::default(),
                    ))));
                }
                if i < n {
                    toks.push(self.nl_ws_meta_token_buf[i].clone());
                    if i + 1 < n {
                        let inner: Vec<ParsoidToken> = self.nl_ws_meta_token_buf[i + 1..]
                            .iter()
                            .cloned()
                            .filter_map(|it| match it {
                                Item::Tok(t) => Some(t),
                                Item::Str(_) => None,
                            })
                            .collect();
                        toks.push(Item::Tok(ParsoidToken::EmptyLine(EmptyLineTk::new(
                            inner,
                            DataParsoid::default(),
                        ))));
                    }
                }
                self.nl_ws_meta_token_buf.clear();
                toks.push(token);
                return Some(toks);
            }
            return Some(vec![token]);
        }

        // A transclusion/param marker meta (non-literal-HTML) buffers alongside
        // any pending newlines so an empty transclusion doesn't disturb a
        // following SOL-transparent link.
        if stt.name == "meta" && stt.data_parsoid.stx.as_deref() != Some("html") {
            let is_transclusion = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str())
                .is_some_and(|ty| {
                    ty.split_whitespace()
                        .any(|x| x == "mw:Transclusion" || x == "mw:Param")
                });
            if is_transclusion && !self.nl_ws_meta_token_buf.is_empty() {
                self.nl_ws_meta_token_buf.push(token);
                return Some(Vec::new());
            }
        }

        // Fall through: flush the buffer, then emit the token.
        Some(self.get_result_tokens(vec![token]))
    }
}

impl Default for TokenStreamPatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::{DataParsoid, NlTk, SelfclosingTagTk, SourceRange};

    fn nl() -> Item {
        Item::Tok(ParsoidToken::Nl(NlTk::new(SourceRange::new(0, 0))))
    }

    fn link(rel: &str) -> Item {
        let mut t = SelfclosingTagTk::new("link", vec![], DataParsoid::default());
        t.add_attribute_str("rel", rel);
        Item::Tok(ParsoidToken::SelfclosingTag(t))
    }

    fn meta(ty: &str) -> Item {
        let mut t = SelfclosingTagTk::new("meta", vec![], DataParsoid::default());
        t.add_attribute_str("typeof", ty);
        Item::Tok(ParsoidToken::SelfclosingTag(t))
    }

    #[test]
    fn test_is_sol_transparent_link_tag() {
        let mut t = SelfclosingTagTk::new("link", vec![], DataParsoid::default());
        t.add_attribute_str("rel", "mw:PageProp/Category");
        assert!(is_sol_transparent_link_tag(&ParsoidToken::SelfclosingTag(
            t
        )));

        let mut t = SelfclosingTagTk::new("link", vec![], DataParsoid::default());
        t.add_attribute_str("rel", "mw:WikiLink");
        assert!(!is_sol_transparent_link_tag(&ParsoidToken::SelfclosingTag(
            t
        )));

        let t = SelfclosingTagTk::new("meta", vec![], DataParsoid::default());
        assert!(!is_sol_transparent_link_tag(&ParsoidToken::SelfclosingTag(
            t
        )));
    }

    #[test]
    fn test_newlines_before_link_wrapped_in_empty_line() {
        // Two newlines before a category link become a single EmptyLineTk.
        let mut tsp = TokenStreamPatcher::new();
        let out = tsp.run(vec![nl(), nl(), link("mw:PageProp/Category")]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(
            matches!(
                &out[0],
                Item::Tok(ParsoidToken::EmptyLine(t)) if t.tokens.len() == 2
            ),
            "{out:?}"
        );
        assert!(
            matches!(
                &out[1],
                Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "link"
            ),
            "{out:?}"
        );
    }

    #[test]
    fn test_plain_text_passes_through() {
        // Text followed by a newline then more text: newlines are buffered and
        // flushed in order, with no reordering.
        let mut tsp = TokenStreamPatcher::new();
        let out = tsp.run(vec![
            Item::Str("a".to_string()),
            nl(),
            Item::Str("b".to_string()),
        ]);
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(matches!(&out[0], Item::Str(s) if s == "a"));
        assert!(matches!(&out[1], Item::Tok(ParsoidToken::Nl(_))));
        assert!(matches!(&out[2], Item::Str(s) if s == "b"));
    }

    #[test]
    fn test_transclusion_meta_buffered() {
        // A newline then a transclusion start meta then a category link: the
        // buffered run is re-wrapped so the meta stays grouped with the link.
        let mut tsp = TokenStreamPatcher::new();
        let out = tsp.run(vec![
            nl(),
            meta("mw:Transclusion"),
            link("mw:PageProp/Category"),
        ]);
        // Expect: EmptyLine[NL] (the preceding newline), then <meta/>, then
        // <link/> (the transclusion start meta is flushed between them).
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(
            matches!(&out[0], Item::Tok(ParsoidToken::EmptyLine(_))),
            "{out:?}"
        );
        assert!(matches!(&out[1], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "meta"));
        assert!(matches!(&out[2], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "link"));
    }
}
