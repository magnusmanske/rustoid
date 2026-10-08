//! MediaWiki core's section-anchor encoding, `CoreParserFunctions::anchorencode`.
//!
//! This is the *integrated-mode* implementation of `{{anchorencode:…}}` and the
//! exact function `mw.uri.anchorEncode` delegates to (Scribunto's
//! `UriLibrary::anchorEncode` calls `CoreParserFunctions::anchorencode`). It is
//! a different function from Parsoid's standalone `ParserFunctions::pf_anchorencode`
//! — which rustoid ports as
//! [`crate::pipeline::parser_functions::pf_anchorencode`] and which the fixture
//! suite pins — in one decisive way: core runs the text through
//! `Parser::stripSectionName` first, so wikitext links and HTML tags in the
//! argument are *removed* (`[[Perissodactyla|X <small>y</small>]]` → `X y`),
//! while standalone Parsoid leaves them and protects the delimiters with
//! `mw:Entity` spans (T179544).
//!
//! ```text
//! CoreParserFunctions::anchorencode( $parser, $text ):
//!   $text = $parser->killMarkers( $text );
//!   $section = substr( $parser->guessSectionNameFromWikiText( $text ), 1 );
//!   $encodedSection = Sanitizer::safeEncodeAttribute( $section );
//!   return str_replace( '&#95;', '_', $encodedSection );
//! ```
//!
//! with `guessSectionNameFromWikiText( $text )` =
//! `'#' . Sanitizer::escapeIdForLink( getSectionNameFromStrippedText(
//! stripSectionName( $text ) ) )`.

use std::sync::OnceLock;

/// The site's protocol-derived patterns, compiled once.
///
/// `mw.uri.anchorEncode` is hot: `Module:Citation/CS1` calls it once per citation
/// to build each `CITEREF…` anchor, which is ~150 000 calls on a page like
/// `World War II`. Compiling the two protocol alternations per call pushed such
/// a page past the render budget, so they are built here once per engine and
/// reused.
pub struct AnchorEncoder {
    /// `stripSectionName`'s external-link pattern, or `None` when the site has no
    /// protocols (an empty alternation would match everywhere).
    external: Option<regex::Regex>,
    /// `safeEncodeAttribute`'s protocol-colon pattern.
    protocol: Option<regex::Regex>,
}

impl AnchorEncoder {
    pub fn new(protocols: &[String]) -> Self {
        let alternation = protocols
            .iter()
            .map(|p| regex::escape(p))
            .collect::<Vec<_>>()
            .join("|");
        let build = |pattern: &str, case_insensitive: bool| {
            regex::RegexBuilder::new(pattern)
                .case_insensitive(case_insensitive)
                .build()
                .ok()
        };
        if alternation.is_empty() {
            // No protocols: the external-link pattern would match every `[…]`,
            // and the protocol-colon pattern every position. Leave both unset.
            return Self {
                external: None,
                protocol: None,
            };
        }
        Self {
            // `preg_replace( '/\[(?i:PROTOCOLS)([^ ]+?) ([^[]+)\]/', '$2', … )`.
            external: build(&format!(r"\[(?i:{alternation})([^ ]+?) ([^\[]+)\]"), false),
            // `preg_replace_callback( '/((?i)PROTOCOLS)/', … )`.
            protocol: build(&alternation, true),
        }
    }

    /// `CoreParserFunctions::anchorencode`.
    pub fn anchorencode(&self, text: &str) -> String {
        // `$parser->killMarkers` removes strip markers (`UNIQ…QINU`). This path
        // takes already-expanded text, where markers are a wikitext-serialization
        // artifact that cannot appear, so there is nothing to kill.
        let stripped = strip_section_name(text, self.external.as_ref());
        let section = get_section_name_from_stripped_text(&stripped);
        let anchor = crate::sanitizer::escape_id_for_link(&section);
        let encoded = safe_encode_attribute(&anchor, self.protocol.as_ref());
        // `str_replace( '&#95;', '_', … )` — undo the `_` escaping (T407131) so a
        // template that reads the result still sees underscores.
        encoded.replace("&#95;", "_")
    }
}

/// `Parser::stripSectionName` — remove wikitext links, quote markup, and HTML
/// tags, leaving the text a heading anchor would be built from.
fn strip_section_name(text: &str, external: Option<&regex::Regex>) -> String {
    // `[[target|label]]` → `label`.
    let text = internal_link_labelled()
        .replace_all(text, "$2")
        .into_owned();
    // `[[target]]` → `target`.
    let text = internal_link().replace_all(&text, "$1").into_owned();
    // `[proto://… label]` → `label`.
    let text = match external {
        Some(re) => re.replace_all(&text, "$2").into_owned(),
        None => text,
    };
    // Wikitext quotes become `<i>`/`<b>`, which the tag strip below removes.
    let text = do_quotes(&text);
    // `StringUtils::delimiterReplace( '<', '>', '', $text )`.
    remove_between(&text, '<', '>')
}

fn internal_link_labelled() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // `/\[\[:?([^[|]+)\|([^[]+)\]\]/`
    RE.get_or_init(|| regex::Regex::new(r"\[\[:?([^\[|]+)\|([^\[]+)\]\]").unwrap())
}

fn internal_link() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // `/\[\[:?([^[]+)\|?\]\]/`
    RE.get_or_init(|| regex::Regex::new(r"\[\[:?([^\[]+)\|?\]\]").unwrap())
}

/// `Parser::doQuotes` — turn runs of two or more apostrophes into `<i>`/`<b>`
/// tags (which [`strip_section_name`] then removes). Ported verbatim; the
/// `Html5TreeBuilder`'s quote handling is the token-level twin of this.
fn do_quotes(text: &str) -> String {
    // `preg_split( "/(''+)/", $text, -1, PREG_SPLIT_DELIM_CAPTURE )`: the text
    // with each run of 2+ apostrophes as its own element.
    let mut arr: Vec<String> = Vec::new();
    {
        let bytes = text.as_bytes();
        let mut start = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'\'' {
                let run_start = i;
                while i < bytes.len() && bytes[i] == b'\'' {
                    i += 1;
                }
                if i - run_start >= 2 {
                    arr.push(text[start..run_start].to_string());
                    arr.push(text[run_start..i].to_string());
                    start = i;
                    continue;
                }
            }
            i += 1;
        }
        arr.push(text[start..].to_string());
    }
    let countarr = arr.len();
    if countarr == 1 {
        return text.to_string();
    }

    // Preliminary pass: shift apostrophes from markup to text, count the markup.
    let mut numbold = 0usize;
    let mut numitalics = 0usize;
    let mut i = 1usize;
    while i < countarr {
        let mut thislen = arr[i].len();
        if thislen == 4 {
            // `''''` → a literal apostrophe plus `'''`.
            arr[i - 1].push('\'');
            arr[i] = "'''".to_string();
            thislen = 3;
        } else if thislen > 5 {
            // More than five apostrophes: all but the last five are text.
            for _ in 0..(thislen - 5) {
                arr[i - 1].push('\'');
            }
            arr[i] = "'''''".to_string();
            thislen = 5;
        }
        match thislen {
            2 => numitalics += 1,
            3 => numbold += 1,
            5 => {
                numitalics += 1;
                numbold += 1;
            }
            _ => {}
        }
        i += 2;
    }

    // An odd bold *and* italic count likely means one `'''` was an apostrophe
    // before italics; prefer a single-letter word, then a multi-letter one.
    if numbold % 2 == 1 && numitalics % 2 == 1 {
        let mut firstsingleletterword: Option<usize> = None;
        let mut firstmultiletterword: Option<usize> = None;
        let mut firstspace: Option<usize> = None;
        let mut i = 1usize;
        while i < countarr {
            if arr[i].len() == 3 {
                let prev = arr[i - 1].as_bytes();
                let x1 = prev.last().copied();
                let x2 = (prev.len() >= 2).then(|| prev[prev.len() - 2]);
                if x1 == Some(b' ') {
                    firstspace.get_or_insert(i);
                } else if x2 == Some(b' ') {
                    firstsingleletterword = Some(i);
                    break;
                } else if firstmultiletterword.is_none() {
                    firstmultiletterword = Some(i);
                }
            }
            i += 2;
        }
        if let Some(t) = firstsingleletterword
            .or(firstmultiletterword)
            .or(firstspace)
        {
            arr[t] = "''".to_string();
            arr[t - 1].push('\'');
        }
    }

    // Convert the apostrophic mush to HTML.
    let mut output = String::new();
    let mut buffer = String::new();
    let mut state = String::new();
    for (i, r) in arr.iter().enumerate() {
        if i % 2 == 0 {
            if state == "both" {
                buffer.push_str(r);
            } else {
                output.push_str(r);
            }
            continue;
        }
        match r.len() {
            2 => match state.as_str() {
                "i" => {
                    output.push_str("</i>");
                    state.clear();
                }
                "bi" => {
                    output.push_str("</i>");
                    state = "b".to_string();
                }
                "ib" => {
                    output.push_str("</b></i><b>");
                    state = "b".to_string();
                }
                "both" => {
                    output.push_str("<b><i>");
                    output.push_str(&buffer);
                    output.push_str("</i>");
                    state = "b".to_string();
                }
                _ => {
                    output.push_str("<i>");
                    state.push('i');
                }
            },
            3 => match state.as_str() {
                "b" => {
                    output.push_str("</b>");
                    state.clear();
                }
                "bi" => {
                    output.push_str("</i></b><i>");
                    state = "i".to_string();
                }
                "ib" => {
                    output.push_str("</b>");
                    state = "i".to_string();
                }
                "both" => {
                    output.push_str("<i><b>");
                    output.push_str(&buffer);
                    output.push_str("</b>");
                    state = "i".to_string();
                }
                _ => {
                    output.push_str("<b>");
                    state.push('b');
                }
            },
            5 => match state.as_str() {
                "b" => {
                    output.push_str("</b><i>");
                    state = "i".to_string();
                }
                "i" => {
                    output.push_str("</i><b>");
                    state = "b".to_string();
                }
                "bi" => {
                    output.push_str("</i></b>");
                    state.clear();
                }
                "ib" => {
                    output.push_str("</b></i>");
                    state.clear();
                }
                "both" => {
                    output.push_str("<i><b>");
                    output.push_str(&buffer);
                    output.push_str("</b></i>");
                    state.clear();
                }
                _ => {
                    buffer.clear();
                    state = "both".to_string();
                }
            },
            _ => {}
        }
    }
    // Close any remaining tags; the order is significant.
    if state == "b" || state == "ib" {
        output.push_str("</b>");
    }
    if state == "i" || state == "bi" || state == "ib" {
        output.push_str("</i>");
    }
    if state == "bi" {
        output.push_str("</b>");
    }
    if state == "both" && !buffer.is_empty() {
        output.push_str("<b><i>");
        output.push_str(&buffer);
        output.push_str("</i></b>");
    }
    output
}

/// `Parser::getSectionNameFromStrippedText` — normalize whitespace, decode
/// character references, then apply the title's fragment normalization.
fn get_section_name_from_stripped_text(text: &str) -> String {
    let text = crate::sanitizer::normalize_section_name_whitespace(text);
    let text = crate::html::wts_utils::decode_wt_entities_all(&text);
    normalize_section_name(&text)
}

/// `Parser::normalizeSectionName` reduced to the case `anchorencode` hits.
///
/// Core passes `#$text` to `MediaWikiTitleCodec::splitTitleString`, which makes
/// the whole input the fragment: the leading `#` blocks the namespace/interwiki
/// split, and the part before it is empty, so the illegal-character and `~~~`
/// checks (which run on that empty part) never fire. What is left is the title
/// whitespace collapse and the fragment's `_` → ` ` swap — i.e. every run of
/// title whitespace (including the no-break space an `&nbsp;` decodes to)
/// collapses to a single space, with Unicode bidi overrides removed.
fn normalize_section_name(text: &str) -> String {
    let cleaned: String = text.chars().filter(|c| !is_bidi_override(*c)).collect();
    let collapsed = collapse_title_whitespace(&cleaned);
    collapsed.trim_matches('_').replace('_', " ")
}

/// The whitespace `splitTitleString` collapses (`/[ _\xA0\x{1680}\x{180E}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}]+/`),
/// with runs collapsed to a single `_`.
fn collapse_title_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if is_title_whitespace(c) {
            if !prev_space {
                out.push('_');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

fn is_title_whitespace(c: char) -> bool {
    matches!(
        c,
        ' ' | '_' | '\u{00A0}' | '\u{1680}' | '\u{180E}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

fn is_bidi_override(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}')
}

/// `Sanitizer::safeEncodeAttribute` — `encodeAttribute` plus the extra armoring
/// against later wiki processing, then the protocol-colon escape.
fn safe_encode_attribute(text: &str, protocol: Option<&regex::Regex>) -> String {
    // `Sanitizer::encodeAttribute`: `htmlspecialchars( ENT_QUOTES )`, then the
    // control whitespace that attribute decoding would otherwise swallow.
    let mut encoded = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => encoded.push_str("&amp;"),
            '<' => encoded.push_str("&lt;"),
            '>' => encoded.push_str("&gt;"),
            '"' => encoded.push_str("&quot;"),
            '\'' => encoded.push_str("&#039;"),
            '\n' => encoded.push_str("&#10;"),
            '\r' => encoded.push_str("&#13;"),
            '\t' => encoded.push_str("&#9;"),
            _ => encoded.push(c),
        }
    }

    // The `strtr` table, longest key first (PHP replaces the longest key at each
    // position). `<`, `>`, `"` and `''` are already absent after
    // `encodeAttribute`, so only these can match.
    const REPLACEMENTS: &[(&str, &str)] = &[
        ("ISBN", "&#73;SBN"),
        ("PMID", "&#80;MID"),
        ("RFC", "&#82;FC"),
        ("\u{FF3F}", "&#xFF3F;"),
        ("{", "&#123;"),
        ("}", "&#125;"),
        ("[", "&#91;"),
        ("]", "&#93;"),
        ("|", "&#124;"),
        ("_", "&#95;"),
    ];
    let mut out = String::with_capacity(encoded.len());
    let mut rest = encoded.as_str();
    'scan: while !rest.is_empty() {
        for (key, value) in REPLACEMENTS {
            if rest.starts_with(key) {
                out.push_str(value);
                rest = &rest[key.len()..];
                continue 'scan;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }

    // `preg_replace_callback( '/((?i)PROTOCOLS)/', fn($m) => str_replace( ':', '&#58;', $m[1] ) )`.
    match protocol {
        Some(re) => re
            .replace_all(&out, |caps: &regex::Captures| caps[0].replace(':', "&#58;"))
            .into_owned(),
        None => out,
    }
}

/// `StringUtils::delimiterReplace( start, end, "", text )`: remove every
/// `start`…`end` span (non-greedy, including the delimiters). An unterminated
/// `start` is left as literal text.
fn remove_between(text: &str, start: char, end: char) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(si) = rest.find(start) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..si]);
        let after = &rest[si + start.len_utf8()..];
        match after.find(end) {
            Some(ei) => rest = &after[ei + end.len_utf8()..],
            None => {
                out.push_str(&rest[si..]);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocols() -> Vec<String> {
        ["//", "http://", "https://", "ftp://", "mailto:", "news:"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn encode(text: &str) -> String {
        AnchorEncoder::new(&protocols()).anchorencode(text)
    }

    /// The battery below is the live wiki's `{{anchorencode:…}}` output,
    /// captured from `transform/wikitext/to/html` on en.wikipedia.org.
    #[test]
    fn matches_the_live_wikis_integrated_anchorencode() {
        assert_eq!(encode("Hello World?"), "Hello_World?");
        assert_eq!(encode("a b#c"), "a_b#c");
        assert_eq!(encode("[[Foo|Bar]]"), "Bar");
        assert_eq!(encode("[[Foo]]"), "Foo");
        assert_eq!(encode("<small>X</small>"), "X");
        assert_eq!(encode("''Foo''"), "Foo");
        assert_eq!(encode("'''Foo'''"), "Foo");
        // No first-letter capitalization for a fragment.
        assert_eq!(encode("foo"), "foo");
        // `&nbsp;` decodes to a no-break space, which the title collapse folds.
        assert_eq!(encode("a&nbsp;b"), "a_b");
        assert_eq!(encode("[http://x.com Foo]"), "Foo");
        assert_eq!(encode("A_B"), "A_B");
        // `escapeIdForLink` double-encodes an existing percent sequence.
        assert_eq!(encode("a%20b"), "a%2520b");
        // `safeEncodeAttribute` entity-encodes the brace characters.
        assert_eq!(encode("a{b}c"), "a&#123;b&#125;c");
        assert_eq!(encode("x_y z"), "x_y_z");
    }

    /// The `Zebra` navbox title, reduced: `Template:Perissodactyla`'s
    /// `title = Extant [[Perissodactyla|…]] species by suborder`.
    #[test]
    fn strips_a_piped_link_with_nested_tags() {
        assert_eq!(
            encode(
                "Extant [[Perissodactyla|Perissodactyla <small>(Odd-toed ungulates)</small>]] species by suborder"
            ),
            "Extant_Perissodactyla_(Odd-toed_ungulates)_species_by_suborder"
        );
    }

    #[test]
    fn a_single_apostrophe_is_text_not_markup() {
        // `encodeAttribute` turns it into `&#039;`.
        assert_eq!(encode("a'b"), "a&#039;b");
    }

    #[test]
    fn external_links_need_a_configured_protocol() {
        // `gopher://` is absent from the test protocol list, so the bracket is
        // left as ordinary text — it is escaped, not resolved to a label (the
        // external-link pattern requires a *valid* protocol).
        assert_eq!(
            encode("[gopher://x.org Foo]"),
            "&#91;gopher://x.org_Foo&#93;"
        );
    }

    #[test]
    fn an_unterminated_angle_bracket_is_kept_but_escaped() {
        assert_eq!(encode("a<b"), "a&lt;b");
        assert_eq!(encode("a<b>c"), "ac");
    }
}
