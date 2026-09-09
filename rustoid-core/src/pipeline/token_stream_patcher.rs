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

/// Whether a string begins with list/table syntax that triggers the T2529
/// reparse: `{|` (table start) or a list character `*`/`#`/`;`/`:`. Mirrors
/// PHP's `preg_match( '/^(?:{\\|[:;#*])/', $token )`.
fn starts_table_or_list_syntax(s: &str) -> bool {
    s.starts_with("{|") || s.starts_with(['*', '#', ';', ':'])
}

/// Re-tokenize a fresh SOL string as a list item or table start (mirrors PHP's
/// `reprocessTokens` for the `T2529hack` cases, which re-parse the string with
/// `startRule = 'list_item'` / `'table_start_tag'`).
///
/// Since the string came from a spliced template argument and contains no
/// further templates to expand, a plain `PegTokenizer::tokenize` at SOL is the
/// faithful equivalent (the nested `wikitext-to-expanded-tokens` pipeline only
/// re-expands templates, which are absent here).
fn reprocess_tokens(s: &str) -> Vec<Item> {
    crate::pipeline::template_handler::tokenize_wikitext_to_items(
        s,
        /* in_template */ true,
        &[],
    )
}

/// The TokenStreamPatcher handler.
pub struct TokenStreamPatcher {
    /// Buffered newline/whitespace/metas awaiting a SOL-transparent link (or
    /// flush at the next non-whitespace token). Mirrors PHP's
    /// `$nlWsMetaTokenBuf`.
    nl_ws_meta_token_buf: Vec<Item>,
    /// Nesting depth of `<table>` start/end tags (mirrors `$wikiTableNesting`),
    /// used to decide whether a bare `<td>`/`<th>`/`<tr>`/`<caption>` is outside
    /// a table and must be converted back to pipe wikitext.
    wiki_table_nesting: usize,
    /// Whether we are currently at start-of-line. Mirrors PHP's `$sol`.
    sol: bool,
    /// Whether the previous token was a transclusion *start* meta (so a
    /// following list/table-syntax string triggers the T2529 reparse). Mirrors
    /// PHP's `$tplInfo['atStart']`.
    tpl_info_at_start: bool,
    /// Whether the pipeline is processing independent (top-level / attribute /
    /// ext-tag) content, where list/table reparse is enabled. Mirrors PHP's
    /// `$inIndependentParse`.
    in_independent_parse: bool,
}

impl TokenStreamPatcher {
    pub fn new() -> Self {
        Self {
            nl_ws_meta_token_buf: Vec::new(),
            wiki_table_nesting: 0,
            sol: true,
            tpl_info_at_start: false,
            in_independent_parse: true,
        }
    }

    fn reset(&mut self) {
        self.nl_ws_meta_token_buf.clear();
        self.wiki_table_nesting = 0;
        self.sol = true;
        self.tpl_info_at_start = false;
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
                    self.sol = true;
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
    /// table-row reparse path).
    fn on_any(&mut self, token: Item) -> Option<Vec<Item>> {
        match &token {
            Item::Str(s) => {
                // While buffering newlines (awaiting a SOL-transparent link),
                // buffer intervening whitespace-only strings too.
                if is_whitespace_string(&token) && !self.nl_ws_meta_token_buf.is_empty() {
                    self.nl_ws_meta_token_buf.push(token);
                    return Some(Vec::new());
                }

                // T2529 hack: a fresh string right after a transclusion start
                // meta that begins with list/table syntax is re-tokenized as a
                // list item / table start. This is how `{{1x|*bar}}` — whose
                // `{{{1}}}` spliced to the bare string `*bar` — becomes a
                // `<ul><li>` (the tokenizer never saw `*bar`). When we are not
                // already at SOL, a newline is inserted first to force SOL
                // (mirrors PHP's `T2529hack` newline insertion).
                let t2529hack = self.tpl_info_at_start && starts_table_or_list_syntax(s);
                if t2529hack && !self.sol {
                    self.nl_ws_meta_token_buf.push(Item::Tok(ParsoidToken::Nl(
                        crate::wikitext::tokens_v2::NlTk::new(
                            crate::wikitext::tokens_v2::SourceRange::new(0, 0),
                        ),
                    )));
                    self.sol = true;
                }

                if self.sol && self.in_independent_parse && t2529hack {
                    if s.starts_with("{|") {
                        self.wiki_table_nesting += 1;
                    }
                    let reprocessed = reprocess_tokens(s);
                    // The re-tokenized content is fully expanded (end of TT2);
                    // leaves us not-at-SOL for subsequent content.
                    self.sol = false;
                    self.tpl_info_at_start = false;
                    return Some(self.get_result_tokens(reprocessed));
                }

                if self.sol {
                    // Plain text at SOL: stays SOL only if whitespace, else clears.
                    if is_whitespace_string(&token) {
                        // stays at SOL; falls through to emit below.
                    } else {
                        self.sol = false;
                    }
                } else {
                    self.sol = false;
                }
                self.tpl_info_at_start = false;
                Some(self.get_result_tokens(vec![token]))
            }
            Item::Tok(ParsoidToken::Comment(_)) | Item::Tok(ParsoidToken::EmptyLine(_)) => {
                // Comments / EmptyLines don't change SOL state.
                Some(self.get_result_tokens(vec![token]))
            }
            Item::Tok(ParsoidToken::SelfclosingTag(stt)) => self.on_selfclosing(stt.clone(), token),
            Item::Tok(ParsoidToken::Tag(t)) => self.on_tag(t.clone(), token),
            Item::Tok(ParsoidToken::EndTag(t)) => self.on_end_tag(t.clone(), token),
            _ => Some(self.get_result_tokens(vec![token])),
        }
    }

    /// Handle a start tag. Mirrors PHP's `TagTk` branch of `onAnyInternal`:
    /// track `<table>` nesting, and convert a bare `<td>`/`<th>`/`<tr>`/`<caption>`
    /// outside a table (or a stray `listItem` in attribute context, not yet wired)
    /// back to its pipe wikitext via `convert_non_html_token_to_string`.
    fn on_tag(&mut self, t: crate::wikitext::tokens_v2::TagTk, token: Item) -> Option<Vec<Item>> {
        let is_html = t.data_parsoid.stx.as_deref() == Some("html");
        if !is_html {
            match t.name.as_str() {
                "table" => {
                    self.wiki_table_nesting += 1;
                }
                "td" | "th" | "tr" | "caption" if self.wiki_table_nesting == 0 => {
                    return Some(self.get_result_tokens(convert_non_html_token_to_string(
                        &ParsoidToken::Tag(t),
                    )));
                }
                _ => {}
            }
        }
        Some(self.get_result_tokens(vec![token]))
    }

    /// Handle an end tag. Mirrors PHP's `EndTagTk` branch of `onAnyInternal`:
    /// decrement `<table>` nesting, and convert a stray `</table>`/`</caption>`
    /// outside a table back to `|}`/``.
    fn on_end_tag(
        &mut self,
        t: crate::wikitext::tokens_v2::EndTagTk,
        token: Item,
    ) -> Option<Vec<Item>> {
        let is_html = t.data_parsoid.stx.as_deref() == Some("html");
        if !is_html {
            if self.wiki_table_nesting > 0 {
                if t.name == "table" {
                    self.wiki_table_nesting -= 1;
                }
            } else if t.name == "table" || t.name == "caption" {
                return Some(self.get_result_tokens(convert_non_html_token_to_string(
                    &ParsoidToken::EndTag(t),
                )));
            }
        }
        Some(self.get_result_tokens(vec![token]))
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
            let ty = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str());
            let has_type = |x: &str| ty.is_some_and(|t| t.split_whitespace().any(|c| c == x));
            // Track whether we are right after a transclusion *start* meta
            // (so a following list/table-syntax string re-tokenizes).
            if has_type("mw:Transclusion") {
                self.tpl_info_at_start = true;
            } else if has_type("mw:Transclusion/End") {
                self.tpl_info_at_start = false;
            }
            let is_transclusion = has_type("mw:Transclusion") || has_type("mw:Param");
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

/// Convert a non-HTML table-cell/table token back to its pipe wikitext. Faithful
/// port of PHP `TokenStreamPatcher::convertNonHTMLTokenToString` for the
/// reconstructible cases (no `tsr`/source reparse): a bare `<td>`/`<th>`/`<tr>`/
/// `<caption>`/`<table>` outside a table is re-emitted as `|`/`!`/`|-`/`|+`/`{|`
/// (or `||`/`!!` for `stx === 'row'` cells), appending any cell-attribute source.
fn convert_non_html_token_to_string(token: &ParsoidToken) -> Vec<Item> {
    let (name, dp, is_end) = match token {
        ParsoidToken::Tag(t) => (t.name.as_str(), &t.data_parsoid, false),
        ParsoidToken::EndTag(t) => (t.name.as_str(), &t.data_parsoid, true),
        _ => return vec![Item::Tok(token.clone())],
    };

    // Reconstruct the leading marker (start-tag source variation wins, e.g. a
    // non-default `{|` or a `{{!}}`-style variation).
    let mut buf = if let Some(start_tag_src) = dp.start_tag_src.as_deref() {
        start_tag_src.to_string()
    } else {
        match (name, is_end) {
            ("td", false) if dp.stx.as_deref() == Some("row") => "||".to_string(),
            ("td", false) => "|".to_string(),
            ("th", false) if dp.stx.as_deref() == Some("row") => "!!".to_string(),
            ("th", false) => "!".to_string(),
            ("tr", false) => "|-".to_string(),
            ("caption", false) => "|+".to_string(),
            ("caption", true) => String::new(),
            ("table", false) => "{|".to_string(),
            ("table", true) => "|}".to_string(),
            _ => return vec![Item::Tok(token.clone())],
        }
    };

    // Append the cell-attribute source (if any), then the attribute separator
    // for cells (`|`/`!`), mirroring `TableFixups::convertAttribsToContent`'s
    // re-join. Reparsing (when the attribute source contains `'[{<`) is not yet
    // wired; the bare-pipe reconstruction covers the common stripped-cell case.
    if let Some(cell_attr_src) = dp.tmp.attr_src.as_deref() {
        buf.push_str(cell_attr_src);
        if matches!(name, "caption" | "td" | "th") {
            buf.push_str(dp.attr_sep_src.as_deref().unwrap_or("|"));
        }
    } else if let Some(attr_sep_src) = dp.attr_sep_src.as_deref() {
        buf.push_str(attr_sep_src);
    }

    vec![Item::Str(buf)]
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

    #[test]
    fn test_bare_td_converted_back_to_pipe() {
        // `a\n|b` tokenizes `|b` as a bare `<td>`; the patcher converts it back
        // to `|` text (outside any table) so the paragraph wrapper keeps `a |b`
        // in one paragraph.
        let mut td = crate::wikitext::tokens_v2::TagTk::new("td", vec![], DataParsoid::default());
        td.data_parsoid.tsr = Some(crate::wikitext::tokens_v2::SourceRange::new(0, 1));
        let mut tsp = TokenStreamPatcher::new();
        let out = tsp.run(vec![
            Item::Str("a".to_string()),
            nl(),
            Item::Tok(ParsoidToken::Tag(td)),
            Item::Str("b".to_string()),
        ]);
        // Expect: "a", newline, "|" text, "b".
        assert_eq!(out.len(), 4, "{out:?}");
        assert!(matches!(&out[2], Item::Str(s) if s == "|"), "{out:?}");
    }

    #[test]
    fn test_t2529_list_reparse_after_transclusion() {
        // A transclusion start meta immediately followed by a bare `*bar` string
        // (as produced by `{{1x|*bar}}` token-level arg splicing) re-tokenizes
        // `*bar` into a listItem.
        let mut tsp = TokenStreamPatcher::new();
        let out = tsp.run(vec![meta("mw:Transclusion"), Item::Str("*bar".to_string())]);
        assert!(
            out.iter().any(|it| {
                matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "listItem")
            }),
            "expected a listItem token, got {out:?}"
        );
    }

    #[test]
    fn test_t2529_no_reparse_without_transclusion() {
        // Without a preceding transclusion start meta, `*bar` at SOL is just text
        // (well, in a full pipeline it would be tokenized upstream; here the
        // patcher must leave an untagged string alone).
        let mut tsp = TokenStreamPatcher::new();
        let out = tsp.run(vec![Item::Str("*bar".to_string())]);
        assert!(
            !out.iter().any(|it| {
                matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "listItem")
            }),
            "did not expect a listItem token, got {out:?}"
        );
    }

    #[test]
    fn test_convert_non_html_token_to_string_variants() {
        let mk = |name: &str, stx: Option<&str>| {
            let mut t =
                crate::wikitext::tokens_v2::TagTk::new(name, vec![], DataParsoid::default());
            t.data_parsoid.stx = stx.map(str::to_string);
            t
        };
        assert_eq!(
            convert_non_html_token_to_string(&ParsoidToken::Tag(mk("td", None))),
            vec![Item::Str("|".to_string())]
        );
        assert_eq!(
            convert_non_html_token_to_string(&ParsoidToken::Tag(mk("td", Some("row")))),
            vec![Item::Str("||".to_string())]
        );
        assert_eq!(
            convert_non_html_token_to_string(&ParsoidToken::Tag(mk("th", None))),
            vec![Item::Str("!".to_string())]
        );
        assert_eq!(
            convert_non_html_token_to_string(&ParsoidToken::Tag(mk("tr", None))),
            vec![Item::Str("|-".to_string())]
        );
        assert_eq!(
            convert_non_html_token_to_string(&ParsoidToken::Tag(mk("table", None))),
            vec![Item::Str("{|".to_string())]
        );
        let end_table =
            crate::wikitext::tokens_v2::EndTagTk::new("table", vec![], DataParsoid::default());
        assert_eq!(
            convert_non_html_token_to_string(&ParsoidToken::EndTag(end_table)),
            vec![Item::Str("|}".to_string())]
        );
    }
}
