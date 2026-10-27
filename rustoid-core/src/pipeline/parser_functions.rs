//! ParserFunctions — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/ParserFunctions.php`.
//!
//! Implements the parser functions used by the Parsoid-native template
//! expansion pipeline: conditionals (#if, #ifeq, #switch, #ifexpr, #iferror),
//! expressions (#expr), case conversion (#lc, #uc, ...), padding (#padleft /
//! #padright), #tag, #urlencode, #anchorencode, and a number of magic
//! variables.
//!
//! Functions operate on `Params` (a key/value argument array) and return token
//! arrays (strings and `<a>`/`<span>`/`<meta>` tokens).

use crate::error::RustoidError;
use crate::wikitext::token_utils::tokens_to_string;
use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, Item, KV, KeyValue, ParsoidToken, TagTk};

/// Parameter wrapper (mirrors PHP's `Params`).
#[derive(Debug, Clone, Default)]
pub struct Params {
    pub args: Vec<KV>,
}

impl Params {
    pub fn new(args: Vec<KV>) -> Self {
        Self { args }
    }

    /// Convert args to a key→value dict (mirrors `Params::dict`).
    pub fn dict(&self) -> std::collections::HashMap<String, KeyValue> {
        let mut res = std::collections::HashMap::new();
        for kv in &self.args {
            let key = key_value_to_string(&kv.key);
            let key = key.trim().to_string();
            res.insert(key, kv.value.clone());
        }
        res
    }

    /// Convert args to a named-argument view, mirroring `Params::named`.
    ///
    /// Positional args (empty key) get 1-based indexes; named args are keyed
    /// by their trimmed key.
    pub fn named(&self) -> NamedArgs {
        let mut dict = std::collections::HashMap::new();
        let mut named_args = std::collections::HashMap::new();
        let mut index = 1usize;

        for kv in &self.args {
            let k = key_value_to_string(&kv.key);
            let k = k.trim().to_string();
            if k.is_empty() {
                dict.insert(index.to_string(), kv.value.clone());
                index += 1;
            } else {
                named_args.insert(k.clone(), true);
                dict.insert(k, kv.value.clone());
            }
        }

        NamedArgs { dict, named_args }
    }

    /// Slice args and convert their values to strings (mirrors `Params::getSlice`).
    pub fn get_slice(&self, start: usize, end: usize) -> Vec<KV> {
        self.args[start..start.min(end.saturating_sub(start))].to_vec()
    }
}

/// The result of `Params::named`: a positional/named argument view plus a map
/// indicating which keys are named (mirrors PHP's `namedArgs` + `dict`).
#[derive(Debug, Clone, Default)]
pub struct NamedArgs {
    pub dict: std::collections::HashMap<String, KeyValue>,
    pub named_args: std::collections::HashMap<String, bool>,
}

/// Extract a key value as a trimmed string.
fn key_value_to_string(kv: &KeyValue) -> String {
    match kv {
        KeyValue::Str(s) => s.clone(),
        KeyValue::Tokens(t) => tokens_to_string(t),
    }
}

/// Core's `decodeTrimExpand` minus the entity decoding: the comparison key of a
/// `#switch` entry is its trimmed expansion.
fn args_to_test(kv: &KeyValue) -> String {
    key_value_to_string(kv).trim().to_string()
}

/// Does this argument carry the error box `#iferror` looks for?
///
/// The extension's test is a regex over the expanded string,
/// `/<(?:strong|span|p|div)\s…class="…error…"/`. rustoid may already have
/// tokenized that markup (a text-returning branch is re-tokenized), so a literal
/// string and an element token with an `error` class both count.
fn key_value_has_error_markup(kv: &KeyValue) -> bool {
    fn item_has_error(item: &Item) -> bool {
        match item {
            Item::Str(s) => s.contains("class=\"error\"") || s.contains("<strong class=\"error\">"),
            Item::Tok(tok) => {
                matches!(tok.get_name(), "strong" | "span" | "p" | "div")
                    && tok.get_attribs().iter().any(|attr| {
                        attr.key.as_str() == Some("class")
                            && attr
                                .value
                                .as_str()
                                .is_some_and(|v| v.split_whitespace().any(|w| w == "error"))
                    })
            }
        }
    }
    match kv {
        KeyValue::Str(s) => s.contains("class=\"error\"") || s.contains("<strong class=\"error\">"),
        KeyValue::Tokens(t) => t.iter().any(item_has_error),
    }
}

/// Does this argument carry an explicit `=` — i.e. is it `name=value`?
///
/// The tokenizer records a *positional* argument's key range as ending where its
/// value begins, so `|=value` (empty name, real `=`) is distinguishable from
/// `|value` even though both render an empty key. `#switch`'s fall-through groups
/// depend on the difference.
fn arg_is_named(kv: &KV) -> bool {
    match &kv.src_offsets {
        Some(so) => so.key_end != so.value_start,
        None => !key_value_to_string(&kv.key).trim().is_empty(),
    }
}

/// Core's `MagicWord::matchStartToEnd` for `default`.
fn is_default_word(test: &str) -> bool {
    test == "#default" || test == "default"
}

/// Serialize a `#tag` attribute list to a ` name="value"…` source fragment
/// (mirrors the attribute serialization in core `tagObj`'s non-extension branch,
/// used to reconstruct the opening tag source for an `extension` token).
fn serialize_tag_attribs(_display_target: &str, tag_attribs: &[KV]) -> String {
    let mut out = String::new();
    for kv in tag_attribs {
        let name = key_value_to_string(&kv.key);
        let value = key_value_to_string(&kv.value);
        out.push(' ');
        out.push_str(&name);
        out.push_str("=\"");
        // The source is re-parsed (Cite reads a ref's start tag from it), so a
        // value holding a quote must be escaped or it ends the attribute:
        // `{{#tag:ref|body|name=a "b", p. xix}}` otherwise served four bogus
        // attributes and a note named `a`.
        out.push_str(&value.replace('"', "&quot;"));
        out.push('"');
    }
    out
}

/// Strip a single pair of surrounding single or double quotes from a `#tag`
/// attribute value, mirroring core `tagObj`'s quote-stripping regexp.
fn strip_attr_value_quotes(value: &str) -> String {
    let b = value.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        // Surrounded by matching quotes: strip them (empty `""`/`''` → empty).
        if b.len() == 2 {
            String::new()
        } else {
            value[1..value.len() - 1].to_string()
        }
    } else {
        value.to_string()
    }
}

/// Extract a value as a string.
fn value_to_string(kv: &KeyValue) -> String {
    match kv {
        KeyValue::Str(s) => s.clone(),
        KeyValue::Tokens(t) => tokens_to_string(t),
    }
}

/// `Language::formatNum` for English — see [`ParserFunctions::pf_formatnum`].
///
/// Not numeric-accurate above 2^53: MediaWiki formats through ICU's
/// `NumberFormatter`, which reads the value as a double, so a longer integer
/// would round; English number formatting has no such input in practice.
fn format_num_english(number: &str) -> String {
    if number.is_empty() {
        return String::new();
    }
    match number {
        // English defines no `formatnum-nan` message, so the key stands in.
        "NAN" => return "<formatnum-nan>".to_string(),
        "INF" => return "\u{221e}".to_string(),
        "-INF" => return "\u{2212}\u{221e}".to_string(),
        _ => {}
    }
    if !is_numeric(number) {
        return format_non_numeric(number, format_num_english);
    }
    let (sign, body) = match number.strip_prefix('-') {
        Some(rest) => ('\u{2212}', rest),
        None => ('\0', number),
    };
    let mut out = String::new();
    if sign != '\0' {
        out.push(sign);
    }
    out.push_str(&group_thousands_preserving_decimals(body));
    out
}

/// `Language::formatNumNoSeparators` for English: the same input handling as
/// [`format_num_english`], but the grouping step is skipped, so only the minus
/// sign is rewritten.
fn format_num_no_separators(number: &str) -> String {
    if number.is_empty() {
        return String::new();
    }
    match number {
        "NAN" => return "<formatnum-nan>".to_string(),
        "INF" => return "\u{221e}".to_string(),
        "-INF" => return "\u{2212}\u{221e}".to_string(),
        _ => {}
    }
    if !is_numeric(number) {
        return format_non_numeric(number, format_num_no_separators);
    }
    number
        .strip_prefix('-')
        .map_or_else(|| number.to_string(), |rest| format!("\u{2212}{rest}"))
}

/// `Language::parseFormattedNumber` for English: undo [`format_num_english`].
fn parse_formatted_number(number: &str) -> String {
    match number {
        "<formatnum-nan>" => return "NAN".to_string(),
        "\u{221e}" => return "INF".to_string(),
        _ => {}
    }
    let number = number.replace('\u{2212}', "-");
    if number == "-\u{221e}" {
        return "-INF".to_string();
    }
    number.replace(',', "")
}

/// Group the integer part in threes, keeping any fraction verbatim.
fn group_thousands_preserving_decimals(body: &str) -> String {
    match body.split_once('.') {
        Some((int_part, frac)) => format!("{}.{frac}", group_thousands(int_part)),
        None => group_thousands(body),
    }
}

/// Insert `,` every three digits, counted from the right, over the whole run
/// (leading zeros included — ICU's `MIN_INTEGER_DIGITS` keeps them).
fn group_thousands(digits: &str) -> String {
    let n = digits.len();
    if n <= 3 {
        return digits.to_string();
    }
    let mut out = String::with_capacity(n + n / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Rewrite every number-like run through `f` — `Language::formatNumInternal`'s
/// legacy pass for non-numeric input.
fn format_non_numeric(s: &str, f: fn(&str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match match_number_span(s, i) {
            Some(end) => {
                out.push_str(&f(&s[i..end]));
                i = end;
            }
            None => {
                let c = s[i..].chars().next().expect("index is a char boundary");
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// The span length of the number regex
/// `(-(?=[\d.]))?(\d+|(?=\.\d))(\.\d*)?([Ee][-+]?\d+)?` at `start`, or
/// `None` if it does not match there. The pattern cannot match empty, so a
/// caller may advance by the returned length unconditionally.
fn match_number_span(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    let digit = |i: usize| matches!(b.get(i), Some(c) if c.is_ascii_digit());
    let mut i = start;

    // `(-(?=[\d.]))?` — a minus sign only when a number follows.
    if b.get(i) == Some(&b'-') {
        if digit(i + 1) || b.get(i + 1) == Some(&b'.') {
            i += 1;
        } else {
            return None;
        }
    }
    // `(\d+|(?=\.\d))` — a digit run, or a zero-width lookahead at `.digit`.
    let mut j = i;
    while digit(j) {
        j += 1;
    }
    if j > i {
        i = j;
    } else if !(b.get(i) == Some(&b'.') && digit(i + 1)) {
        return None;
    }
    // `(\.\d*)?`
    if b.get(i) == Some(&b'.') {
        i += 1;
        while digit(i) {
            i += 1;
        }
    }
    // `([Ee][-+]?\d+)?`
    if matches!(b.get(i), Some(&c) if c == b'e' || c == b'E') {
        let mut k = i + 1;
        if matches!(b.get(k), Some(&c) if c == b'+' || c == b'-') {
            k += 1;
        }
        let digits_start = k;
        while digit(k) {
            k += 1;
        }
        if k > digits_start {
            i = k;
        }
    }
    Some(i)
}

/// PHP `is_numeric` for the shapes `formatnum` sees: optional sign, decimal or
/// exponent notation, surrounding whitespace.
fn is_numeric(s: &str) -> bool {
    let t = s.trim();
    let t = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (t, None),
    };
    if let Some(e) = exponent {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        if e.is_empty() || !e.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    let (int_part, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let int_ok = !int_part.is_empty() && int_part.bytes().all(|c| c.is_ascii_digit());
    match frac {
        Some(f) => f.bytes().all(|c| c.is_ascii_digit()) && (int_ok || !f.is_empty()),
        None => int_ok,
    }
}

/// A [`KeyValue`] read as a bare value (empty key), so `expand_kv` answers the
/// value alone rather than its `key=value` rejoining.
fn value_only(value: &KeyValue) -> KV {
    KV {
        key: KeyValue::Str(String::new()),
        value: value.clone(),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    }
}

/// `preg_match('/\d+=/i', $form)`: a digit immediately followed by `=`, which
/// marks an explicit `n=form` entry in `{{PLURAL:…}}`.
fn has_explicit_plural_number(form: &str) -> bool {
    form.as_bytes()
        .windows(2)
        .any(|w| w[0].is_ascii_digit() && w[1] == b'=')
}

/// MediaWiki's `NS_MEDIA` (the `Media:` pseudo-namespace), which
/// `CoreParserFunctions::urlFunction` converts to `NS_FILE`.
const MEDIA_NAMESPACE_ID: i32 = -2;
/// MediaWiki's `NS_FILE` (the `File:` namespace).
const FILE_NAMESPACE_ID: i32 = -6;

/// `CoreParserFunctions::urlFunction` — a title's URL in the `local`, `full` or
/// `canonical` flavour chosen by `name`, or `None` for a title the wiki rejects
/// (a name it cannot build).
fn url_function(
    config: &dyn crate::traits::SiteConfig,
    s: &str,
    query: Option<&str>,
    name: &str,
) -> Option<String> {
    let mut title = crate::title::TitleParser::try_parse(s, config)?;
    // `urlFunction` converts NS_MEDIA -> NS_FILE.
    if title.namespace_id == MEDIA_NAMESPACE_ID {
        title = crate::title::Title::new(FILE_NAMESPACE_ID, title.text.clone());
    }

    // `getLocalURL`: an interwiki title reaches the remote wiki's URL; a local
    // one is the article path or the script with `?title=`.
    let local = match title
        .interwiki
        .as_ref()
        .and_then(|p| config.interwiki_map().get(p))
    {
        Some(iw) => {
            let page = wf_urlencode(&title.get_dbkey());
            append_query(&iw.url.replace("$1", &page), query)
        }
        None => local_url(config, &title, query),
    };

    let url = if name.starts_with("localurl") {
        local
    } else if name.starts_with("fullurl") {
        // `getFullURL`: expand to `PROTO_RELATIVE` (protocol-relative) and add
        // the fragment.
        format!(
            "{}{}",
            expand_protocol(config, &local, Proto::Relative),
            fragment_for_url(&title)
        )
    } else {
        format!(
            "{}{}",
            expand_protocol(config, &local, Proto::Canonical),
            fragment_for_url(&title)
        )
    };
    Some(url)
}

/// The protocol expansion `UrlUtils::expand` performs: a relative URL is
/// qualified with the wiki's server, a protocol-relative one gets the protocol,
/// and an absolute URL is left as-is.
enum Proto {
    Relative,
    Canonical,
}

fn expand_protocol(config: &dyn crate::traits::SiteConfig, url: &str, proto: Proto) -> String {
    if url.starts_with("//") {
        return match proto {
            Proto::Relative => url.to_string(),
            Proto::Canonical => format!("{protocol}:{url}", protocol = site_protocol(config)),
        };
    }
    if !url.starts_with('/') {
        return url.to_string();
    }
    match proto {
        Proto::Relative => format!("//{}{url}", server_authority(config)),
        Proto::Canonical => format!(
            "{protocol}://{authority}{url}",
            protocol = site_protocol(config),
            authority = server_authority(config)
        ),
    }
}

/// `Title::getLocalURL`: the article path for a bare title, else the script with
/// `?title=` and the query (`$wgScript`, the `-` query meaning empty).
fn local_url(
    config: &dyn crate::traits::SiteConfig,
    title: &crate::title::Title,
    query: Option<&str>,
) -> String {
    let dbkey = wf_urlencode(&title.get_prefixed_db_key());
    match query {
        None => config.article_path().replace("$1", &dbkey),
        Some(q) => {
            let q = if q == "-" { "" } else { q };
            format!("{}/index.php?title={dbkey}&{q}", config.script_path())
        }
    }
}

/// `getFragmentForURL`: `#` and the fragment, or nothing.
fn fragment_for_url(title: &crate::title::Title) -> String {
    title
        .fragment
        .as_ref()
        .map(|f| format!("#{}", wf_urlencode(f)))
        .unwrap_or_default()
}

/// `wfAppendQuery` for the interwiki case: `&`-joined when the URL already has
/// a query, `?`-joined otherwise. A `null` query adds nothing.
fn append_query(url: &str, query: Option<&str>) -> String {
    match query {
        None => url.to_string(),
        Some(q) => {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}{q}")
        }
    }
}

/// The wiki's own protocol (`https` on enwiki). `server_url` may be
/// protocol-relative (the wiki's `$wgServer` is `//en.wikipedia.org`), in which
/// case the request protocol — https on the Wikimedia wikis — is assumed.
fn site_protocol(config: &dyn crate::traits::SiteConfig) -> &str {
    config
        .server_url()
        .split_once("://")
        .map(|(protocol, _)| protocol)
        .unwrap_or("https")
}

/// The wiki's host, without protocol or path.
fn server_authority(config: &dyn crate::traits::SiteConfig) -> &str {
    config
        .server_url()
        .split("://")
        .last()
        .unwrap_or(config.server_url())
        .trim_start_matches('/')
}

/// MediaWiki's `wfUrlencode`: `rawurlencode` (unreserved `A-Za-z0-9-_.~` kept),
/// then a handful of sub-delims and `; @ : / '` unescaped.
fn wf_urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b';'
            | b'@'
            | b'$'
            | b','
            | b'/'
            | b'!'
            | b'*'
            | b'('
            | b')'
            | b'\''
            | b':' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// PHP's `rawurlencode`: keep the RFC 3986 unreserved set, percent-encode the
/// rest byte-wise.
fn rawurlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// PHP's `urlencode`: `rawurlencode` with a space as `+`. PHP encodes `~` here
/// (its `urlencode` predates the unreserved-set change), so `rawurlencode` and
/// `urlencode` differ on that one character.
fn php_urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The count `{{PLURAL:count|…}}` is evaluated against: core applies
/// `parseFormattedNumber` (grouping commas dropped, U+2212 read as `-`) and
/// then PHP's `settype`, which keeps an all-digit string an integer and makes
/// everything else a float.
struct PluralCount {
    /// Whether the argument was an integer (`settype` → `int`).
    is_int: bool,
    value: f64,
    /// PHP's `(string)$count`, used to match an explicit `n=form` entry.
    display: String,
}

impl PluralCount {
    fn parse(text: &str) -> Self {
        let cleaned: String = text
            .chars()
            .filter(|c| *c != ',')
            .map(|c| if c == '\u{2212}' { '-' } else { c })
            .collect();
        let trimmed = cleaned.trim();
        let is_int = !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit());
        let value = trimmed.parse::<f64>().unwrap_or(f64::NAN);
        let display = if is_int {
            // `(string)(int)` drops leading zeros.
            let stripped = trimmed.trim_start_matches('0');
            if stripped.is_empty() {
                "0".to_string()
            } else {
                stripped.to_string()
            }
        } else if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
            // PHP renders a float with no fraction as its integer form.
            format!("{}", value as i64)
        } else {
            format!("{value}")
        };
        PluralCount {
            is_int,
            value,
            display,
        }
    }

    /// English's CLDR rule `i = 1 and v = 0` → form 0 (`one`), else form 1.
    /// An integer has no fraction (`v = 0`) and `i` is the value; a float is
    /// `one` only when it is integral and its absolute value is 1.
    fn plural_index(&self) -> usize {
        let one = if self.is_int {
            self.value == 1.0
        } else {
            self.value.abs() == 1.0 && self.value.fract() == 0.0
        };
        if one { 0 } else { 1 }
    }
}

/// PHP's `(int)` cast applied to a string: trim leading whitespace, take an
/// optional sign and the leading run of ASCII digits, and answer `0` when there
/// are none. `(int)"2abc"` is `2`, `(int)"abc"` is `0`.
fn php_int_cast(s: &str) -> i64 {
    let t = s.trim_start();
    let (negative, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    match digits.parse::<i64>() {
        Ok(v) if negative => -v,
        Ok(v) => v,
        // `0` for no digits; saturate rather than wrap on overflow, as PHP does.
        Err(_) if digits.is_empty() => 0,
        Err(_) => i64::MAX,
    }
}

/// PHP's default `trim()` character list — `" \t\n\r\0\x0B"`.
///
/// Rust's `str::trim` is Unicode-aware and also strips e.g. U+00A0 (NBSP), which
/// PHP does *not*. That difference is observable: `{{#if:&nbsp;|then|else}}`
/// takes the *then* branch on the service (the condition is non-empty) but was
/// read as an empty condition here — Earth's `|temp_name1 = &nbsp;` lost the
/// whole surface-temperature table from its infobox.
fn php_trim_char(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\0' | '\u{0B}')
}

fn php_trim(s: &str) -> &str {
    s.trim_matches(php_trim_char)
}

/// The ParserFunctions handler.
pub struct ParserFunctions;

/// Whether a KV side has a faithful *text* spelling — i.e. stringifying it and
/// tokenizing the result reproduces the same tokens.
///
/// `Str` always does; a `urllink` does (its `href`), and so does a comment
/// (dropped either way). A `template`/marker/extension token does not: its
/// `about`, `data-mw` and fragment id are not in its text.
fn has_text_spelling(value: &KeyValue) -> bool {
    match value {
        KeyValue::Str(_) => true,
        KeyValue::Tokens(items) => items.iter().all(|item| match item {
            Item::Str(_) => true,
            Item::Tok(ParsoidToken::Comment(_)) => true,
            Item::Tok(ParsoidToken::SelfclosingTag(t)) => t.name == "urllink",
            _ => false,
        }),
    }
}

impl ParserFunctions {
    /// `#if` — a port of core `ParserFunctions::if`, which answers
    /// `trim($frame->expand($args[n]))`.
    ///
    /// Parsoid's *native* `pf_if` does not trim (`expandKV` runs with its
    /// `$trim` default of false), so the two disagree on a branch with surrounding
    /// blanks — `{{#if:1|Y }}` is `Y` on the service and `Y ` natively. The served
    /// HTML is the target, so this trims.
    pub fn pf_if(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let condition = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        if !php_trim(&condition).is_empty() {
            Self::trimmed_branch(args.get(1))
        } else {
            Self::trimmed_branch(args.get(2))
        }
    }

    /// `#ifeq` — a port of core `ParserFunctions::ifeq`.
    pub fn pf_ifeq(params: &Params) -> Vec<Item> {
        let args = &params.args;
        if args.len() < 3 {
            return vec![];
        }
        let a = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        let b = args
            .get(1)
            .map(|kv| key_value_to_string(&kv.value))
            .unwrap_or_default();
        if php_trim(&a) == php_trim(&b) {
            Self::trimmed_branch(args.get(2))
        } else {
            Self::trimmed_branch(args.get(3))
        }
    }

    /// One of a conditional's branches, trimmed. Core's `if`, `ifeq`, `ifexpr`
    /// and `iferror` all answer `trim($frame->expand(…))`, and the trim happens
    /// *after* expansion — so a branch that is a bare newline, or whose blanks
    /// come from the source, loses them. The ParserFunctions extension's
    /// `#ifexist` does the same (`trim( $frame->expand( $result ) )`), so it
    /// shares this path.
    pub fn trimmed_branch(kv: Option<&KV>) -> Vec<Item> {
        let mut items = Self::expand_kv(kv, None);
        trim_item_edges(&mut items);
        items
    }

    /// `{{PLURAL:count|sing|plur|…}}` — core `CoreParserFunctions::plural`
    /// feeding `Language::convertPlural`. English has the CLDR rule
    /// `i = 1 and v = 0`, so a count of one answers the first form and any
    /// other count the last; an explicit `n=form` entry wins when `n` equals
    /// the count, and is otherwise dropped (`handleExplicitPluralForms`).
    pub fn pf_plural(params: &Params) -> Vec<Item> {
        // The count is the colon argument, expanded like any other.
        let count_kv = params.args.first().map(|kv| value_only(&kv.key));
        let count = tokens_to_string(&Self::expand_kv(count_kv.as_ref(), None));
        let count = PluralCount::parse(&count);

        // Each form as MediaWiki hands it to `convertPlural`: `key=value` when
        // the argument was named (`1=one`), the value alone when positional.
        let forms: Vec<KV> = params.args.iter().skip(1).cloned().collect();
        let strings: Vec<String> = forms
            .iter()
            .map(|kv| tokens_to_string(&Self::expand_kv(Some(kv), None)))
            .collect();

        let mut kept: Vec<&KV> = Vec::new();
        for (kv, form) in forms.iter().zip(&strings) {
            if has_explicit_plural_number(form) {
                let pos = form.find('=').unwrap_or(form.len());
                if form[..pos] == count.display {
                    return Self::expand_kv(Some(&value_only(&kv.value)), None);
                }
                continue;
            }
            kept.push(kv);
        }
        if kept.is_empty() {
            return Vec::new();
        }
        let index = count.plural_index().min(kept.len() - 1);
        Self::expand_kv(Some(kept[index]), None)
    }

    /// `{{fullurl:title|query}}` and its siblings — core
    /// `CoreParserFunctions::urlFunction` over `Title::getLocalURL`,
    /// `getFullURL` and `getCanonicalURL`. The names are `localurl`/`localurle`,
    /// `fullurl`/`fullurle` and `canonicalurl`/`canonicalurle`; the `…e`
    /// spellings only `htmlspecialchars` the answer in the legacy parser, while
    /// Parsoid inserts the string as text and the serializer escapes it, so both
    /// spellings answer the same bytes here.
    pub fn pf_url(
        config: &dyn crate::traits::SiteConfig,
        name: &str,
        params: &Params,
    ) -> Vec<Item> {
        let raw = params.args.first().map(|kv| value_only(&kv.key));
        let s = tokens_to_string(&Self::expand_kv(raw.as_ref(), None));
        let query = params
            .args
            .get(1)
            .map(|kv| tokens_to_string(&Self::expand_kv(Some(kv), None)));
        let url = url_function(config, &s, query.as_deref(), name).unwrap_or_default();
        vec![Item::Str(url)]
    }

    /// `#switch` — a port of core `ParserFunctions::switch`, which is what the
    /// served HTML shows. (Parsoid's *native* `pf_switch` is a different,
    /// simplified algorithm; the online target is core's.)
    ///
    /// The shape worth naming is how a *fall-through group* is written. Core
    /// reads `|a|b|c=result` as "cases a and b fall through to c's result", and
    /// it does that by remembering whether a positional entry matched and then
    /// returning the next `=value` it sees. A `=value` with nothing before the
    /// `=` is therefore a *result*, not a case — which is what
    /// `|2|3|12|=exclude` in `Template:Short description` needs, and what an
    /// earlier reading got wrong by treating every empty-key entry as a case.
    pub fn pf_switch(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let Some(primary) = args.first().map(|kv| args_to_test(&kv.key)) else {
            return vec![];
        };

        let mut found = false;
        let mut default_found = false;
        let mut default: Option<KeyValue> = None;
        let mut last_item_had_no_equals = false;
        let mut last_item: Vec<Item> = Vec::new();

        for kv in args.iter().skip(1) {
            if arg_is_named(kv) {
                // `name=value`, including the empty name of `=value`.
                last_item_had_no_equals = false;
                if found {
                    // A case matched earlier; this is its result.
                    return Self::branch_items(&kv.value);
                }
                let test = args_to_test(&kv.key);
                if test == primary {
                    return Self::branch_items(&kv.value);
                }
                if default_found || is_default_word(&test) {
                    default = Some(kv.value.clone());
                    default_found = false;
                }
            } else {
                // A bare value: a case, compared against the target.
                last_item_had_no_equals = true;
                let test = value_to_string(&kv.value).trim().to_string();
                if is_default_word(&test) {
                    default_found = true;
                }
                if test == primary {
                    found = true;
                }
                last_item = Self::branch_items(&kv.value);
            }
        }

        // A trailing case with no `=` is the default, written the other way.
        if last_item_had_no_equals {
            return last_item;
        }
        match default {
            Some(default) => Self::branch_items(&default),
            None => vec![],
        }
    }

    /// The items a matched `#switch` branch yields, with surrounding whitespace
    /// trimmed. Mirrors `expandKV`'s token-preserving branch: a value holding an
    /// HTML tag must stay *tokens*, or `{{#switch:x|x=<div>a</div>}}` renders
    /// the tag as literal text. Only the outer string items are trimmed, so a
    /// tag's own tokens pass through untouched.
    fn branch_items(value: &KeyValue) -> Vec<Item> {
        let mut items = Self::expand_kv(
            Some(&KV {
                key: KeyValue::Str(String::new()),
                value: value.clone(),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            }),
            None,
        );
        trim_item_edges(&mut items);
        items
    }

    /// `#expr` — mirrors `pf_expr`.
    pub fn pf_expr(params: &Params) -> Vec<Item> {
        let target = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        vec![Item::Str(evaluate_expression(&target))]
    }

    /// `#ifexpr` — mirrors `pf_ifexpr`.
    ///
    /// Core catches the expression error and answers the error markup *itself*,
    /// rather than picking a branch: the `try` around `doExpression` returns
    /// `<strong class="error">…</strong>`. That propagation is load-bearing —
    /// `{{#iferror:{{#ifexpr: 30em>1}}|ERR|OK}}` is `ERR` on the service — but it
    /// only became safe once a parser function's arguments expand in the calling
    /// frame (see [`crate::pipeline::parser`]'s `expand_parser_function_args`):
    /// shipping it earlier made a `#time` inside an `#ifexpr` condition error and
    /// lose `Quicksilver (film)`'s infobox cell id.
    pub fn pf_ifexpr(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let target = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        let res = evaluate_expression(&target);
        // The only non-numeric answer `evaluate_expression` can give is the error
        // markup, and core returns it unchanged instead of a branch.
        if res.contains("class=\"error\"") {
            return vec![Item::Str(res)];
        }
        if res != "0" && !res.is_empty() {
            Self::trimmed_branch(args.get(1))
        } else {
            Self::trimmed_branch(args.get(2))
        }
    }

    /// `#iferror` — mirrors `pf_iferror`.
    ///
    /// Core's `#iferror` has a third shape: with no `else` argument it answers
    /// the *test* itself (already expanded and trimmed), not the empty string.
    /// That is what makes `{{#ifexpr:{{#iferror:{{YEAR|1919}}|{{#time:Y}}}} >= …}}`
    /// compare against the year rather than against nothing, which is the branch
    /// `Template:Marriage` depends on.
    pub fn pf_iferror(params: &Params) -> Vec<Item> {
        let args = &params.args;
        // Core tests the expanded *string* for an error box. rustoid's test may
        // already be re-tokenized (`<strong class="error">` is a Tag token), so
        // look for the markup in both forms: a literal string, or an element
        // whose `class` contains `error`.
        let has_error = args
            .first()
            .is_some_and(|kv| key_value_has_error_markup(&kv.key));
        if has_error {
            Self::trimmed_branch(args.get(1))
        } else if args.len() > 2 {
            Self::trimmed_branch(args.get(2))
        } else {
            // No `else`: `$result = $test`, then `trim($frame->expand($result))`.
            // The test reaches here already expanded, and core re-expands the
            // *string* — so extension tokens (`<templatestyles>`) and markup it
            // carried survive the round trip. Stringifying it instead dropped
            // the extension (the string form of a placeholder is empty) and made
            // the surviving markup literal text: `Template:Infobox country`'s
            // Gini row lost the `Template:Color` stylesheet and rendered
            // `&lt;span class="tmp-color"`.
            let mut items = match args.first().map(|kv| &kv.key) {
                Some(KeyValue::Str(s)) => vec![Item::Str(s.clone())],
                Some(KeyValue::Tokens(t)) => t
                    .iter()
                    .filter(|it| !matches!(it, Item::Tok(ParsoidToken::Comment(_))))
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            trim_item_edges(&mut items);
            items
        }
    }

    /// `#lc` — mirrors `pf_lc`.
    pub fn pf_lc(params: &Params) -> Vec<Item> {
        let target = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        vec![Item::Str(target.to_lowercase())]
    }

    /// `#uc` — mirrors `pf_uc`.
    pub fn pf_uc(params: &Params) -> Vec<Item> {
        let target = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        vec![Item::Str(target.to_uppercase())]
    }

    /// `#ucfirst` — mirrors `pf_ucfirst`.
    pub fn pf_ucfirst(params: &Params) -> Vec<Item> {
        let target = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        if target.is_empty() {
            return vec![];
        }
        let mut chars = target.chars();
        let first = chars.next().unwrap().to_uppercase().collect::<String>();
        vec![Item::Str(format!("{first}{}", chars.collect::<String>()))]
    }

    /// `#lcfirst` — mirrors `pf_lcfirst`.
    pub fn pf_lcfirst(params: &Params) -> Vec<Item> {
        let target = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        if target.is_empty() {
            return vec![];
        }
        let mut chars = target.chars();
        let first = chars.next().unwrap().to_lowercase().collect::<String>();
        vec![Item::Str(format!("{first}{}", chars.collect::<String>()))]
    }

    /// `#padleft` — mirrors `pf_padleft`.
    pub fn pf_padleft(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let target = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        if args.len() < 2 {
            return vec![];
        }
        let n: i64 = args
            .get(1)
            .map(|kv| value_to_string(&kv.value).trim().parse().unwrap_or(0))
            .unwrap_or(0);
        if n <= 0 {
            return vec![];
        }
        let pad = args
            .get(2)
            .map(|kv| value_to_string(&kv.value))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "0".to_string());

        let target_len = target.chars().count() as i64;
        let pad_len = pad.chars().count() as i64;
        let mut extra = String::new();
        while target_len + (extra.chars().count() as i64) + pad_len < n {
            extra.push_str(&pad);
        }
        if target_len + (extra.chars().count() as i64) < n {
            let remaining = (n - target_len - extra.chars().count() as i64) as usize;
            extra.push_str(&pad.chars().take(remaining).collect::<String>());
        }
        vec![Item::Str(format!("{extra}{target}"))]
    }

    /// `#padright` — mirrors `pf_padright`.
    pub fn pf_padright(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let target = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        if args.len() < 2 {
            return vec![];
        }
        let n: i64 = args
            .get(1)
            .map(|kv| value_to_string(&kv.value).trim().parse().unwrap_or(0))
            .unwrap_or(0);
        if n <= 0 {
            return vec![];
        }
        let pad = args
            .get(2)
            .map(|kv| value_to_string(&kv.value))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "0".to_string());

        let mut result = target;
        let pad_len = pad.chars().count() as i64;
        while (result.chars().count() as i64) + pad_len < n {
            result.push_str(&pad);
        }
        if (result.chars().count() as i64) < n {
            let remaining = (n - result.chars().count() as i64) as usize;
            result.push_str(&pad.chars().take(remaining).collect::<String>());
        }
        vec![Item::Str(result)]
    }

    /// `{{formatnum:…}}` — MediaWiki core's `CoreParserFunctions::formatnum`
    /// (the production path; Parsoid's own `pf_formatnum` is a stub that returns
    /// its argument unchanged, so core is the reference for byte parity).
    ///
    /// The number is formatted by `Language::formatNum`, which for English — a
    /// language with neither a digit- nor a separator-transform table — reduces
    /// to: group the integer part in threes with `,`, keep the decimal point and
    /// every fraction digit exactly as written (`1234.500` → `1,234.500`), and
    /// turn a leading `-` into the Unicode minus `−` (U+2212). Non-numeric input
    /// is rewritten piecewise, one number-like run at a time.
    ///
    /// The colon argument is the number; up to two further arguments are
    /// modifiers matched against the `rawsuffix`/`nocommafysuffix`/`lossless`
    /// magic words: `R` parses a formatted number back to a bare one, `NOSEP`
    /// formats without separators, and `LOSSLESS` keeps the formatted value only
    /// when it round-trips through `R`.
    pub fn pf_formatnum(params: &Params) -> Vec<Item> {
        let args = &params.args;
        // `Parser::callParserFunction` trims every plain-text argument; without
        // it `{{formatnum: 190 }}` grouped the leading space as if it were a
        // digit (` ,190`).
        let number = args
            .first()
            .map(|kv| key_value_to_string(&kv.key).trim().to_string())
            .unwrap_or_default();
        // A modifier is a positional `|` argument; the tokenizer stores those in
        // the value side of the `KV`.
        let modifier = |i: usize| -> String {
            args.get(i)
                .map(|kv| {
                    let value = value_to_string(&kv.value);
                    if value.is_empty() {
                        key_value_to_string(&kv.key).trim().to_string()
                    } else {
                        value.trim().to_string()
                    }
                })
                .unwrap_or_default()
        };
        let (m1, m2) = (modifier(1), modifier(2));
        // `rawsuffix` (`R`) is case-sensitive; `nocommafysuffix` (`NOSEP`) and
        // `lossless` are not. `MagicWordArray::matchStartToEnd` requires the
        // modifier to be the whole argument.
        let word = |canonical: &str, text: &str| canonical.eq_ignore_ascii_case(text);
        let raw = m1 == "R" || m2 == "R";
        let no_separators = word("NOSEP", &m1) || word("NOSEP", &m2);
        let lossless = word("LOSSLESS", &m1) || word("LOSSLESS", &m2);

        let value = if raw {
            parse_formatted_number(&number)
        } else {
            let format = |s: &str| {
                if no_separators {
                    format_num_no_separators(s)
                } else {
                    format_num_english(s)
                }
            };
            if lossless {
                let formatted = format(&number);
                if parse_formatted_number(&formatted) == number {
                    formatted
                } else {
                    number.clone()
                }
            } else {
                format(&number)
            }
        };
        // An input that passes through unchanged keeps its markup. Core answers
        // the expanded *string* and the parser re-parses it, so an entity span
        // (`&nbsp;`) survives; rendering the decoded text instead emitted a bare
        // NBSP character. `Template:US$` formats `2.221&nbsp;trillion`, and
        // `Canada`'s GDP row lost the `<span typeof="mw:Entity">` around it.
        if value == number
            && let Some(kv) = args.first()
        {
            let mut items: Vec<Item> = match &kv.key {
                KeyValue::Str(s) => vec![Item::Str(s.clone())],
                KeyValue::Tokens(t) => t
                    .iter()
                    .filter(|it| !matches!(it, Item::Tok(ParsoidToken::Comment(_))))
                    .cloned()
                    .collect(),
            };
            trim_item_edges(&mut items);
            if !items.is_empty() {
                return items;
            }
        }
        vec![Item::Str(value)]
    }

    /// `#titleparts` — a port of the ParserFunctions extension's
    /// `ParserFunctions::titleparts`. `{{#titleparts:Hello/World|1}}` is
    /// `Hello`.
    ///
    /// ```php
    /// $bits = explode( '/', $ntitle->getPrefixedText(), 25 );
    /// if ( $offset > 0 ) { --$offset; }
    /// return implode( '/', array_slice( $bits, $offset, $parts ?: null ) );
    /// ```
    ///
    /// The title is split into at most 25 slash-separated parts (the 25th holds
    /// the remainder). `$parts` is how many to keep (`0` = all; negative drops
    /// that many from the end) and `$offset` the 1-based index of the first part
    /// (`0` and `1` both mean the first; negative counts from the end). A title
    /// `Title::newFromText` rejects is returned verbatim.
    pub fn pf_titleparts(config: &dyn crate::traits::SiteConfig, params: &Params) -> Vec<Item> {
        let args = &params.args;
        let title = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        let parts = args
            .get(1)
            .map(|kv| php_int_cast(&value_to_string(&kv.value)))
            .unwrap_or(0);
        let offset = args
            .get(2)
            .map(|kv| php_int_cast(&value_to_string(&kv.value)))
            .unwrap_or(0);
        // `if ( $offset > 0 ) { --$offset; }` — the function's offset is 1-based,
        // `array_slice`'s 0-based. `0` and negative offsets are left alone.
        let offset = if offset > 0 { offset - 1 } else { offset };

        let Some(parsed) = crate::title::TitleParser::try_parse(&title, config) else {
            return vec![Item::Str(title)];
        };
        let prefixed = parsed.get_prefixed_text();

        // `explode( '/', $text, 25 )`: up to 24 splits, the rest in the last part.
        let mut bits: Vec<&str> = Vec::new();
        let mut rest = prefixed.as_str();
        while bits.len() < 24 {
            match rest.find('/') {
                Some(i) => {
                    bits.push(&rest[..i]);
                    rest = &rest[i + 1..];
                }
                None => break,
            }
        }
        bits.push(rest);

        // `array_slice( $bits, $offset, $parts ?: null )`.
        let n = bits.len() as i64;
        let start = if offset >= 0 {
            offset.min(n)
        } else if -offset > n {
            0
        } else {
            n + offset
        };
        let end = if parts == 0 {
            n
        } else if parts > 0 {
            (start + parts).min(n)
        } else {
            (n + parts).max(start)
        };
        vec![Item::Str(bits[start as usize..end as usize].join("/"))]
    }

    /// `#tag` — mirrors `pf_tag` / `tag_worker`, plus the extension-tag branch
    /// of MediaWiki core's `tagObj` (which routes registered extension tags
    /// through `extensionSubstitution` rather than emitting a plain tag).
    pub fn pf_tag(config: &dyn crate::traits::SiteConfig, params: &Params) -> Vec<Item> {
        let args = &params.args;
        let target = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        if target.is_empty() {
            return vec![];
        }

        // Collect the tag attributes (named args) and content (the single
        // positional-but-one arg), losing the attribute order like the legacy
        // `tagObj`/`tag_worker` do. Attribute values have any surrounding
        // single/double quotes stripped, mirroring `tagObj`'s
        // `preg_match('/^(?:["'](.+)["']|""|\'\')$/s', …)`.
        //
        // The tag name and the inner content are taken *positionally*: the first
        // argument after the tag name is the content whatever it contains, and
        // only the remaining arguments are attribute candidates (`name=value`;
        // a bare positional extra is dropped). Mirrors `tagObj`'s `array_shift`
        // for the tag and for `$inner`, and Parsoid's `tag_worker`. Splitting the
        // content arg on `=` as a generic parser function would misreads
        // `…|80px|alt=…` in an `<imagemap>` body as an attribute and drops the
        // line breaks (`{{#tag:imagemap|…}}`).
        let mut content: Vec<Item> = Vec::new();
        let mut tag_attribs: Vec<KV> = Vec::new();
        let lc_target = target.to_lowercase();
        let is_ext = config
            .extension_tags()
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&lc_target));
        // A `{{#tag:<extension>}}` call is a fragment handler: Parsoid prepares
        // its result as one atomic fragment before expanding it, so the call
        // spends the first pFragment number ahead of its body's pieces. A call
        // reached from another such call's argument is expanded lazily and has
        // no preparation of its own (see `pfragment::set_lazy`).
        if is_ext {
            crate::pipeline::pfragment::allocate_tag();
        }
        if let Some(kv) = args.get(1) {
            if key_value_to_string(&kv.key).is_empty() {
                content.extend(key_value_to_items(&kv.value));
            } else {
                // A split `key=value` arg is reassembled with the `=` in place.
                content.extend(key_value_to_items(&kv.key));
                content.push(Item::Str("=".to_string()));
                content.extend(key_value_to_items(&kv.value));
            }
        }
        for kv in args.get(2..).unwrap_or_default() {
            if key_value_to_string(&kv.key).is_empty() {
                continue;
            }
            let name = key_value_to_string(&kv.key).trim().to_string();
            // An attribute value can carry HTML markup. Parsoid keeps it as a
            // token array and stringifies it with the *source* of each token, so
            // `text=<div title="…">&nbsp;</div>` reaches the extension as that
            // markup (`Module:OSM Location map` passes exactly that to
            // `frame:extensionTag`). `tokens_to_string` drops plain HTML tags,
            // which emptied the value.
            let value = match &kv.value {
                KeyValue::Str(s) => s.clone(),
                KeyValue::Tokens(items) => {
                    crate::pipeline::parser::tag_source_with_fragments(items)
                }
            };
            let value = strip_attr_value_quotes(value.trim());
            let mut kv = kv.clone();
            kv.key = KeyValue::Str(name);
            kv.value = KeyValue::Str(value);
            tag_attribs.push(kv);
        }

        if is_ext {
            return Self::tag_extension_token(&lc_target, &target, &tag_attribs, &content);
        }

        let mut tag = crate::wikitext::tokens_v2::TagTk::new(&target, vec![], Default::default());
        tag.attribs = tag_attribs;
        let mut out = vec![Item::Tok(ParsoidToken::Tag(tag))];
        out.extend(content);
        out.push(Item::Tok(ParsoidToken::EndTag(
            crate::wikitext::tokens_v2::EndTagTk::new(&target, vec![], Default::default()),
        )));
        out
    }

    /// Build an `extension` token for a `#tag` of a registered extension tag,
    /// mirroring the tokenizer's `maybe_extension_tag` output (with `name`,
    /// `source`, and parsed attributes stored as rich `data-mw` attribs). This
    /// lets the extension handler (`extension_handler::run`) expand it into the
    /// `mw:Extension/{name}` DOM shape, exactly as a literal `<name>` tag would.
    fn tag_extension_token(
        lc_target: &str,
        display_target: &str,
        tag_attribs: &[KV],
        content: &[Item],
    ) -> Vec<Item> {
        use crate::wikitext::tokens_v2::{
            DataMw, DataMwAttrib, DataMwValue, DomSourceRange, SelfclosingTagTk,
        };

        // Reconstruct the literal `<name attrs>content</name>` source so that
        // `extract_ext_body` can recover the raw body via the open/close widths.
        // Magic pipe words in the content (`{{!}}` → `|`, `{{{!}}` → `{|`) are
        // expanded here, *after* the `#tag` arguments have been split, so the
        // pipes they produce aren't consumed as argument separators (mirrors the
        // token-level `processSpecialMagicWord`/`!` magic-variable handling).
        //
        // Core strips comments from a parser function's arguments before `#tag`
        // builds the extension. That happens in the source builders
        // (`tag_content_source` and the fallback), which also apply the
        // preprocessor's whole-line rule, so the tokens are passed through
        // unmodified here.
        let content_src: String = crate::expand::tpl_args::replace_magic_pipe(
            &crate::pipeline::parser::tag_source_with_fragments(content),
        );
        let attr_src = serialize_tag_attribs(display_target, tag_attribs);
        let open_tag = format!("<{}{attr_src}>", display_target.to_lowercase());
        let close_tag = format!("</{lc_target}>");
        let source = format!("{open_tag}{content_src}{close_tag}");

        let dp = crate::wikitext::tokens_v2::DataParsoid {
            tsr: None,
            src: Some(source.clone()),
            ext_tag_offsets: Some(DomSourceRange {
                start: Some(0),
                end: Some(source.len()),
                open_width: Some(open_tag.len()),
                close_width: Some(close_tag.len()),
                ..Default::default()
            }),
            ..Default::default()
        };
        // The content is this call's argument value, expanded with `inTemplate`;
        // a nested template therefore is not `mw:Transclusion`-wrapped. Carried so
        // the ref-body expansion picks the same context. See
        // `Parser::number_extension_token`.
        let mut dp = dp;
        dp.tmp.in_arg_value = true;
        // Keep the content tokens for [`Parser::tag_body_source`], which
        // re-serializes them with a marker for every token that has no wikitext
        // form (`source` above drops those).
        dp.tmp.tag_content = Some(content.to_vec());

        let mut stt = SelfclosingTagTk::new("extension", vec![], dp);
        stt.add_attribute_str("typeof", "mw:Extension");
        stt.add_attribute_str("name", lc_target);
        stt.add_attribute_str("source", &source);
        stt.data_mw = Some(DataMw {
            parts: Vec::new(),
            attribs: tag_attribs
                .iter()
                .map(|kv| DataMwAttrib {
                    key: DataMwValue::Str(key_value_to_string(&kv.key)),
                    value: DataMwValue::Str(key_value_to_string(&kv.value)),
                })
                .collect(),
            src: None,
        });

        vec![Item::Tok(ParsoidToken::SelfclosingTag(stt))]
    }

    /// `#urlencode` — core `CoreParserFunctions::urlencode`, whose mode argument
    /// picks the encoder: `url_wiki` (`wfUrlencode`, spaces as `_`), `url_path`
    /// (`rawurlencode`), and `url_query`/absent (PHP's `urlencode`, which is
    /// `rawurlencode` with `+` for a space). The old blanket `rawurlencode`-ish
    /// encoding answered `a%20b` where the wiki answers `a+b`, left `?&=/` alone,
    /// and ignored the mode entirely.
    pub fn pf_urlencode(params: &Params) -> Vec<Item> {
        let target = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        let mode = params
            .args
            .get(1)
            .map(|kv| tokens_to_string(&Self::expand_kv(Some(kv), None)))
            .unwrap_or_default();
        let encoded = match mode.to_ascii_uppercase().as_str() {
            "WIKI" => wf_urlencode(&target.replace(' ', "_")),
            "PATH" => rawurlencode(&target),
            _ => php_urlencode(&target),
        };
        vec![Item::Str(encoded)]
    }

    /// `#anchorencode` — encode a string so it can be used as a link fragment.
    /// Mirrors `ParserFunctions::pf_anchorencode` (which in turn mirrors
    /// `Parser::guessSectionNameFromWikiText`):
    ///
    /// - collapse runs of spaces/underscores and trim,
    /// - decode character references,
    /// - escape as an HTML5 fragment id (`Sanitizer::escapeIdForLink`),
    /// - then split on the characters that would otherwise be re-interpreted as
    ///   wikitext (`{}[]|`, `''`, `ISBN`, `RFC`, `PMID`, `__`), wrapping each such
    ///   delimiter in an `mw:Entity` span so it survives as literal text (T179544).
    pub fn pf_anchorencode(params: &Params) -> Vec<Item> {
        let target = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();

        let normalized = crate::sanitizer::normalize_section_name_whitespace(&target);
        let decoded = crate::html::wts_utils::decode_wt_entities_all(&normalized);
        let escaped = crate::sanitizer::escape_id_for_link(&decoded);

        // Split on the delimiter alternation, keeping the delimiters
        // (`PREG_SPLIT_DELIM_CAPTURE`): `pieces` holds the literal runs and
        // `delims[i]` the delimiter that followed `pieces[i]`.
        let (pieces, delims) = split_keep_delimiters(&escaped);

        let mut out: Vec<Item> = Vec::new();
        for (i, delim) in delims.iter().enumerate() {
            if i < pieces.len() {
                out.push(Item::Str(pieces[i].to_string()));
            }
            // `''` is two separate entities; anything else is one entity for the
            // first char plus the remainder as literal text.
            let mut chars = delim.chars();
            if let Some(first) = chars.next() {
                encode_char_entity(first, &mut out);
            }
            if *delim != "''" {
                let rest: String = chars.collect();
                if !rest.is_empty() {
                    out.push(Item::Str(rest));
                }
            } else if let Some(second) = chars.next() {
                encode_char_entity(second, &mut out);
            }
        }
        if let Some(last) = pieces.last() {
            out.push(Item::Str((*last).to_string()));
        }
        out
    }

    /// `#dir` — the directionality of a language code: `ltr`, `rtl`, or `auto`
    /// (the direction of the content language when no code is given).
    /// Mirrors MediaWiki core's `CoreParserFunctions::dir`.
    ///
    /// Core gets the direction from `Language::getDir()`, which consults the
    /// site's per-language `rtl` flag. rustoid's `SiteConfig` carries only the
    /// single content-language code, not a language table, and the direction is
    /// not decidable from the code alone (`ar` is RTL and `en` LTR; both are
    /// plain two-letter codes). So only the cases decidable from the input are
    /// modelled: an explicit `-x-rtl`/`-x-ltr` BCP-47 bidi override, and an
    /// omitted code. Any other code is assumed LTR, which is the overwhelmingly
    /// common case and is what the site config would report for its own
    /// language — but it is an assumption, not a lookup.
    ///
    /// TODO: once `SiteConfig` exposes per-language direction data (PHP's
    /// `languagevariants`/`languages` blocks), look the code up instead of
    /// assuming.
    pub fn pf_dir(params: &Params) -> Vec<Item> {
        let code = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        let code = php_trim(&code).to_lowercase();

        let dir = if code.is_empty() {
            "auto"
        } else if code.ends_with("-x-rtl") {
            "rtl"
        } else {
            "ltr"
        };
        vec![Item::Str(dir.to_string())]
    }

    /// `{{#language:code|inLanguage}}` — core `CoreParserFunctions::language`
    /// over `Language::fetchLanguageName`.
    ///
    /// An omitted `inLanguage` answers the language's *autonym* (`ang` →
    /// `Ænglisc`); `en` answers the English name (`ang` → `Old English`). An
    /// unknown code answers itself, and an empty code falls back to the content
    /// language. A non-English `inLanguage` would need MediaWiki's translated
    /// name tables, which rustoid does not carry; it answers the English name,
    /// which is also what core falls back to for an unknown in-language.
    pub fn pf_language(config: &dyn crate::traits::SiteConfig, params: &Params) -> Vec<Item> {
        let code = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        let in_language = params
            .args
            .get(1)
            .map(|kv| key_value_to_string(&kv.value))
            .unwrap_or_default();
        let code = php_trim(&code);
        let in_language = php_trim(&in_language);
        let code = if code.is_empty() {
            config.language_code()
        } else {
            code
        };

        let lookup = |table: &[(&str, &str)]| {
            table
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(code))
                .map(|(_, name)| name.to_string())
        };
        let name = if in_language.is_empty() {
            lookup(crate::lua::LANGUAGE_AUTONYMS)
        } else {
            lookup(crate::lua::LANGUAGE_NAMES)
        };
        vec![Item::Str(name.unwrap_or_else(|| code.to_string()))]
    }

    /// Expand a KV into items (mirrors `expandKV`).
    ///
    /// A token value is returned as *tokens*, not stringified, so an HTML tag in
    /// the branch is parsed instead of escaped. `{{#ifeq:1|0|y|<div>hi</div>}}`
    /// must render the element: Parsoid produces `<p>y</p><div>hi</div>`, and
    /// `tokens_to_string` would have turned the tag into literal text.
    ///
    /// A `Str` value still becomes `Item::Str`, which is what an ordinary text
    /// branch needs.
    ///
    /// When either half of a named argument holds tokens, the two are rejoined as
    /// *tokens* (Parsoid's `rejoinKV`), never stringified: a `{{!}}` inside a
    /// `#if` branch sits in the **key** half of a `k=v` split
    /// (`{{!}} colspan=…`), and stringifying dropped it — so
    /// `Template:Taxobox/core`'s image row lost its cell.
    fn expand_kv(kv: Option<&KV>, default: Option<&str>) -> Vec<Item> {
        // `$frame->expand` removes HTML comments wherever they appear
        // (`PPFrame_Hash::expand`'s `comment` arm appends the empty string in
        // HTML output mode), so a comment in a branch value is gone *before* the
        // caller trims. Dropping it here is not cosmetic: `trim_item_edges`
        // stops at any token that is not a newline, so a comment keeps the
        // blanks around it alive — and that leftover whitespace is the stray
        // `<span about="#mwtN"> </span>` that `Template:Redirect-several`
        // leaves on `Polio vaccine` (its `#switch` default is
        // `<!-- … -->\n     {{#switch:…}}`).
        let no_comments = |items: Vec<Item>| -> Vec<Item> {
            items
                .into_iter()
                .filter(|it| !matches!(it, Item::Tok(ParsoidToken::Comment(_))))
                .collect()
        };
        let value_items = |value: &KeyValue| -> Vec<Item> {
            no_comments(match value {
                KeyValue::Str(v) => vec![Item::Str(v.clone())],
                KeyValue::Tokens(t) => t.clone(),
            })
        };

        let Some(kv) = kv else {
            return vec![Item::Str(default.unwrap_or("").to_string())];
        };
        // `expandKV`: both sides plain strings → the `k=v` (or `v`) string.
        if let (KeyValue::Str(k), KeyValue::Str(v)) = (&kv.key, &kv.value) {
            return if k.is_empty() {
                vec![Item::Str(v.clone())]
            } else {
                vec![Item::Str(format!("{k}={v}"))]
            };
        }
        // A *name* that had to be tokenized (it holds a URL, so the
        // name/value split took the `=` out of it) must not keep its tokens:
        // Parsoid stringifies the whole KV and expands it again
        // (`TemplateHandlerArguments::getOrderedArgs`), which reads the
        // reassembled `name=value` as one wikitext run. Keeping the name's
        // tokens, its autolink ended at the `=` and the value fell outside it —
        // `{{#if:1|http://x/?a=b}}` rendered `http://x/?a` plus `=b`
        // (World War II's `GBurl` reference).
        //
        // Only a KV that has a full *text* spelling is re-read this way: a
        // token value may carry transclusion markers or extension placeholders,
        // and plain text would lose them. A `urllink` is text (its `href`).
        if matches!(kv.key, KeyValue::Tokens(_))
            && has_text_spelling(&kv.key)
            && has_text_spelling(&kv.value)
        {
            let value = key_value_to_string(&kv.value);
            let text = match key_value_to_string(&kv.key) {
                k if k.is_empty() => value,
                k => format!("{k}={value}"),
            };
            return crate::pipeline::template_handler::tokenize_wikitext_to_items(
                &text,
                /* in_template */ false,
                &[],
            );
        }
        // Otherwise `rejoinKV`: key + `=` + value, all as tokens.
        let mut items = match &kv.key {
            KeyValue::Str(k) if k.is_empty() => Vec::new(),
            KeyValue::Str(k) => vec![Item::Str(k.clone()), Item::Str("=".to_string())],
            KeyValue::Tokens(t) if t.is_empty() => Vec::new(),
            KeyValue::Tokens(t) => {
                let mut items = no_comments(t.clone());
                items.push(Item::Str("=".to_string()));
                items
            }
        };
        if items.is_empty() {
            return value_items(&kv.value);
        }
        items.extend(value_items(&kv.value));
        items
    }
}

/// Trim leading and trailing whitespace from an item list, mirroring
/// `TokenUtils::tokenTrim`.
///
/// That function walks from each end until it reaches a non-empty string or a
/// token that is not a newline: a leading or trailing `NlTk` is replaced by the
/// empty string and the walk *continues*, and a string is stripped of its
/// whitespace. Only strings and newline tokens are touched, so a markup token at
/// either end stops the walk and its neighbouring blanks are left alone.
///
/// `#switch` trims its matched branch, but a branch may hold markup whose tokens
/// must survive: `{{#switch:x|x=<div>a</div>}}` keeps the `<div>` and loses only
/// the surrounding blanks. Trimming the *stringified* form instead would discard
/// the tags along with the whitespace.
///
/// Handling the newline token is load-bearing rather than tidy: a branch that is
/// a bare `\n` is a newline *token*, not a whitespace string, so
/// `{{#switch:other|other|#default=\n}}` used to answer a newline where the
/// service answers nothing — and inside `Template:Main other` that newline is
/// what left a stray `<span about="#mwtN"> </span>` on `Help:Introduction`.
pub(crate) fn trim_item_edges(items: &mut Vec<Item>) {
    let mut start = 0;
    while start < items.len() {
        match &mut items[start] {
            Item::Str(s) => {
                let trimmed = s.trim_start_matches(php_trim_char);
                if trimmed.len() != s.len() {
                    *s = trimmed.to_string();
                }
                if !s.is_empty() {
                    break;
                }
                start += 1;
            }
            // A comment never reaches the output, so it cannot stop the trim.
            // Core trims the *expanded string*, where `<!--…-->` is already
            // gone: a branch ending `text <!--c-->` is `text`, not `text `.
            // `Template:Unichar/main`'s else branch ends in exactly that, after
            // an empty `{{Unichar/notes}}`, and the surviving space gave
            // `{{unichar|…}})` a space before the bracket on `Bitcoin`.
            Item::Tok(ParsoidToken::Nl(_) | ParsoidToken::Comment(_)) => start += 1,
            _ => break,
        }
    }
    items.drain(..start);

    while let Some(last) = items.last_mut() {
        match last {
            Item::Str(s) => {
                let trimmed = s.trim_end_matches(php_trim_char);
                if trimmed.len() != s.len() {
                    *s = trimmed.to_string();
                }
                if !s.is_empty() {
                    break;
                }
                items.pop();
            }
            Item::Tok(ParsoidToken::Nl(_) | ParsoidToken::Comment(_)) => {
                items.pop();
            }
            _ => break,
        }
    }
}

/// The trailing half of [`trim_item_edges`] for a branch that has already been
/// expanded.
///
/// Core trims the expanded string, so whitespace that only reaches the edge
/// because a nested call answered nothing is trimmed too: `Template:Unichar/main`
/// ends its `#ifexpr` branch with ` {{Unichar/notes}}`, and the space before the
/// empty notes call must not survive.
///
/// Unlike the pre-expansion trim this leaves a newline token alone. rustoid
/// expands a branch in more than one stage (an argument value's own templates
/// are expanded later), so a newline at *this* stage's edge may still be
/// followed by content in the finished branch; dropping it removed the line
/// break the list handler needs (`List of sovereign states`' `{{Compact ToC}}`).
pub(crate) fn trim_item_edges_str(items: &mut Vec<Item>) {
    while let Some(last) = items.last_mut() {
        match last {
            Item::Str(s) => {
                let trimmed = s.trim_end_matches(php_trim_char);
                if trimmed.len() != s.len() {
                    *s = trimmed.to_string();
                }
                if !s.is_empty() {
                    break;
                }
                items.pop();
            }
            Item::Tok(ParsoidToken::Comment(_)) => {
                items.pop();
            }
            _ => break,
        }
    }
}

/// Append a single character to `out` wrapped in an `mw:Entity` span, so it
/// survives as literal text instead of being re-interpreted as wikitext.
fn encode_char_entity(c: char, out: &mut Vec<Item>) {
    let enc = entity_encode_all(c);
    let dp = DataParsoid {
        src: Some(enc),
        src_content: Some(c.to_string()),
        ..DataParsoid::default()
    };
    let mut span = TagTk::new("span", vec![], dp);
    span.add_attribute_str("typeof", "mw:Entity");
    out.push(Item::Tok(ParsoidToken::Tag(span)));
    out.push(Item::Str(c.to_string()));
    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        "span",
        vec![],
        DataParsoid::default(),
    ))));
}

/// `Utils::entityEncodeAll` — encode `s` as a numeric character reference.
///
/// PHP uses `mb_encode_numericentity($s, [0, 0x10ffff, 0, ~0], 'utf-8', true)`
/// (hex form), which encodes each *codepoint* (not each UTF-8 byte) and pads to
/// at least two hex digits, then maps a few conventions over the result. The
/// only convention that matters here is `&nbsp;` for U+00A0.
fn entity_encode_all(s: char) -> String {
    if s == '\u{A0}' {
        return "&nbsp;".to_string();
    }
    format!("&#x{:02X};", s as u32)
}

/// Split `s` on the `#anchorencode` delimiter alternation
/// `([\{\}\[\]|]|''|ISBN|RFC|PMID|__)`, returning the pieces and the captured
/// delimiters (mirroring `preg_split` with `PREG_SPLIT_DELIM_CAPTURE`).
/// `pieces.len() == delims.len() * 2 + 1`.
fn split_keep_delimiters(s: &str) -> (Vec<&str>, Vec<&str>) {
    const DELIMS: [&str; 8] = ["{", "}", "[", "]", "|", "''", "ISBN", "RFC"];
    const DELIMS2: [&str; 2] = ["PMID", "__"];

    let mut pieces = Vec::new();
    let mut delims = Vec::new();
    let rest = s;
    let mut start = 0;

    while start < rest.len() {
        let at = DELIMS
            .iter()
            .chain(DELIMS2.iter())
            .filter_map(|d| rest[start..].find(d).map(|i| (start + i, *d)))
            // Prefer the earliest match; on a tie the alternation order decides,
            // which `min_by_key` on the index alone would not respect, so compare
            // the delimiter's position in `DELIMS` as a tiebreak.
            .min_by_key(|(i, d)| {
                (
                    *i,
                    DELIMS.iter().position(|x| x == d).unwrap_or(DELIMS.len()),
                )
            });
        let Some((idx, delim)) = at else {
            break;
        };
        pieces.push(&rest[start..idx]);
        delims.push(delim);
        start = idx + delim.len();
    }
    pieces.push(&rest[start..]);
    (pieces, delims)
}

/// Convert a `KeyValue` into a flat token chunk, splicing every token (a whole
/// `Tokens` list expands to multiple `Item`s). Mirrors PHP's `tag_worker` which
/// does `PHPUtils::pushArray($toks, $kv->v)` for a non-string argument value,
/// so `mw-quote`/`wikilink`/etc. tokens flow into the tag body instead of being
/// collapsed to a string via `tokensToString`.
fn key_value_to_items(v: &KeyValue) -> Vec<Item> {
    match v {
        KeyValue::Str(s) => vec![Item::Str(s.clone())],
        KeyValue::Tokens(t) => t.clone(),
    }
}

/// Reconstruct a single token's *wikitext source*, mirroring how PHP's token
/// stream round-trips inline tokens (bash from `data_parsoid->src`, or the
/// `value` attribute for `mw-quote`, etc.). Used when an extension body must be
/// re-serialized for `format="wikitext"` re-tokenization. Returns empty for
/// tokens with no recoverable source.
fn token_to_source(t: &ParsoidToken) -> String {
    let dp_src = t.data_parsoid().and_then(|dp| dp.src.clone());
    if let Some(src) = dp_src {
        return src;
    }
    match t {
        // `mw-quote` carries its source in the `value` attribute (e.g. `'''`).
        ParsoidToken::SelfclosingTag(tk) if tk.name == "mw-quote" => tk
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("value"))
            .and_then(|kv| kv.value.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Serialize a `#tag` extension's content back to wikitext, for the token
/// `tag_content_source` could not rebuild as a whole.
///
/// Each token falls back to its own `src`. The decoded text after an `mw:Entity`
/// span is skipped: the span's `src` already spells the entity, and emitting both
/// doubles it (`&nbsp;` + a literal NBSP).
///
/// A token with **no** source of its own serializes to nothing, which is
/// deliberately not what [`Parser::tag_body_source`] does: that one markerizes
/// such tokens so they survive the round-trip.
pub(crate) fn tag_content_fallback_source(content: &[Item]) -> String {
    let mut s = String::new();
    let mut i = 0;
    while i < content.len() {
        match &content[i] {
            Item::Str(text) => s.push_str(text),
            // A comment is removed with its whole line when it has one of its
            // own; [`crate::pipeline::parser::strip_html_comments`] applies the
            // rule to the assembled string.
            Item::Tok(ParsoidToken::Comment(_)) => s.push_str("<!--c-->"),
            Item::Tok(t) => {
                let source = token_to_source(t);
                if source.is_empty() {
                    // A token with no `src` of its own can still have a known
                    // wikitext form. A module's `<span>[[link]]</span>` reaches
                    // here as a wikilink token without one, and dropping it left
                    // an empty span in the note body (`Module:IPA`).
                    if let Some(rendered) = crate::pipeline::parser::tag_content_source(
                        std::slice::from_ref(&content[i]),
                    ) {
                        s.push_str(&rendered);
                    }
                } else {
                    s.push_str(&source);
                }
                if crate::wikitext::token_utils::skip_entity_decoded_text(t, content.get(i + 1)) {
                    i += 1;
                }
            }
        }
        i += 1;
    }
    crate::pipeline::parser::strip_html_comments(&s)
}

/// Very basic expression evaluator for `#expr` and `#ifexpr`.
/// Supports +, -, *, /, % and parentheses. Returns result as string.
pub(crate) fn evaluate_expression(expr: &str) -> String {
    let expr = expr.trim();
    if expr.is_empty() {
        return String::new();
    }

    // Try to parse as a simple integer expression.
    let tokens = match tokenize_expr(expr) {
        Ok(tokens) => tokens,
        // MediaWiki's `ExprParser` errors on a word it does not know, and the
        // error is load-bearing: `{{#iferror:{{#expr:…}}|…}}` and templates that
        // test their own output for `[Ee]rror` (`Template:Period start`) rely on
        // it. Silently dropping the word answered a number instead.
        Err(message) => {
            return format!("<strong class=\"error\">Expression error: {message}</strong>");
        }
    };
    match eval_simple(&tokens) {
        Ok(val) => format_expr_number(val),
        Err(RustoidError::Parse(message)) => {
            format!("<strong class=\"error\">Expression error: {message}</strong>")
        }
        Err(_) => format!("<strong class=\"error\">Expression error: {expr}</strong>"),
    }
}

/// Format an `#expr` result the way MediaWiki does: `sprintf( '%.14G', $result )`
/// — 14 significant digits, exponential when the decimal exponent is below `-4`
/// or at least `14`, fixed otherwise, trailing zeros dropped. MediaWiki's PHP
/// `%G` keeps one fractional digit in the exponential form (`1.0E-5`) and does
/// not zero-pad the exponent, so this is not C's `%G`.
fn format_expr_number(val: f64) -> String {
    if val == 0.0 {
        return "0".to_string();
    }
    if !val.is_finite() {
        // A non-finite result is not something `%.14G` renders; keep the value
        // legible rather than panicking.
        return val.to_string();
    }
    // `{:.13e}` is exactly 14 significant digits, correctly rounded.
    let sci = format!("{:.13e}", val);
    let Some((mant, exp)) = sci.split_once('e') else {
        return val.to_string();
    };
    let Ok(e10) = exp.parse::<i32>() else {
        return val.to_string();
    };
    let neg = mant.starts_with('-');
    let mant = mant.trim_start_matches('-');
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let sign = if neg { "-" } else { "" };

    if (-4..14).contains(&e10) {
        // Fixed notation. `digits[0]` is the `10^e10` place.
        let mut s = String::from(sign);
        if e10 >= 0 {
            let int_len = (e10 + 1) as usize;
            s.push_str(&digits[..int_len]);
            let frac = &digits[int_len..];
            let frac = frac.trim_end_matches('0');
            if !frac.is_empty() {
                s.push('.');
                s.push_str(frac);
            }
        } else {
            s.push_str("0.");
            for _ in 0..(-e10 - 1) {
                s.push('0');
            }
            let frac = digits.trim_end_matches('0');
            s.push_str(frac);
        }
        s
    } else {
        // Exponential notation: one digit, then a trimmed fractional part that
        // keeps at least one digit.
        let frac = mant.split_once('.').map(|(_, f)| f).unwrap_or("");
        let frac = frac.trim_end_matches('0');
        let mut s = String::from(sign);
        s.push_str(&mant[..1]);
        s.push('.');
        if frac.is_empty() {
            s.push('0');
        } else {
            s.push_str(frac);
        }
        s.push('E');
        s.push(if e10 < 0 { '-' } else { '+' });
        s.push_str(&e10.abs().to_string());
        s
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ExprToken {
    Num(f64),
    /// Arithmetic: `+ - * / %`, plus `D` for `div` (integer division).
    Op(char),
    /// A comparison, spelled `=`, `!=`, `<`, `>`, `<=` or `>=`. `<>` is
    /// normalised to `!=` because that is what it means.
    Cmp(&'static str),
    And,
    Or,
    Not,
    /// `^` — exponentiation, right-associative.
    Caret,
    /// `round` — `lhs round rhs`, rounding `lhs` to `rhs` decimal places. A
    /// *binary* operator (lower precedence than `+ -`), so a leading `round`
    /// with no left operand is the parser's error, not a dropped word.
    Round,
    /// `e` — Euler's number where a value belongs, and the scientific-notation
    /// operator `lhs * 10^rhs` where an operator belongs. MediaWiki's parser
    /// decides by position, which is why `1E(-14)` is `1e-14` while `e` alone
    /// is 2.718….
    Exp,
    /// A unary math function (`abs`, `floor`, `ln`, …). It applies to the next
    /// *primary*, so `floor2.5^2` is `(floor 2.5)^2`.
    Func(&'static str),
    LParen,
    RParen,
}

fn tokenize_expr(expr: &str) -> std::result::Result<Vec<ExprToken>, String> {
    let mut tokens = Vec::new();
    let bytes = expr.as_bytes();
    let lower = expr.to_ascii_lowercase();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Word operators, functions and constants. A bare word is not a value,
        // so anything not in this set is the parser's "Unrecognized word" error
        // (mirrors `ExprParser::doExpression`). `round` is a binary operator
        // and stays a token; the other functions/constants are recognised so
        // they do not read as garbage, but the evaluator does not implement
        // them, so they are dropped.
        if b.is_ascii_alphabetic() {
            let start = i;
            // MediaWiki's expression lexer reads a *word* as letters only, so
            // `floor4` is the function `floor` applied to `4`, not the unknown
            // word `floor4`. `Template:Rating` relies on exactly that spelling
            // (`{{#expr:floor{{{1}}}-1}}`), and a digits-in-words scan turned it
            // into `Expression error: Unrecognized word "floor4"`.
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            match &lower[start..i] {
                "and" => tokens.push(ExprToken::And),
                "or" => tokens.push(ExprToken::Or),
                "not" => tokens.push(ExprToken::Not),
                "div" => tokens.push(ExprToken::Op('D')),
                "mod" => tokens.push(ExprToken::Op('%')),
                // `round` is a binary operator, not a value: keep it as a token
                // so a missing left operand becomes the parser's error.
                "round" => tokens.push(ExprToken::Round),
                // Constants are values, not functions.
                "e" => tokens.push(ExprToken::Exp),
                "pi" => tokens.push(ExprToken::Num(std::f64::consts::PI)),
                // `log` is deliberately absent: the live wiki reports it as an
                // unrecognized word (verified against the transform endpoint).
                "abs" => tokens.push(ExprToken::Func("abs")),
                "ceil" => tokens.push(ExprToken::Func("ceil")),
                "floor" => tokens.push(ExprToken::Func("floor")),
                "trunc" => tokens.push(ExprToken::Func("trunc")),
                "sqrt" => tokens.push(ExprToken::Func("sqrt")),
                "exp" => tokens.push(ExprToken::Func("exp")),
                "ln" => tokens.push(ExprToken::Func("ln")),
                "sin" => tokens.push(ExprToken::Func("sin")),
                "cos" => tokens.push(ExprToken::Func("cos")),
                "tan" => tokens.push(ExprToken::Func("tan")),
                "asin" => tokens.push(ExprToken::Func("asin")),
                "acos" => tokens.push(ExprToken::Func("acos")),
                "atan" => tokens.push(ExprToken::Func("atan")),
                _ => return Err(format!("Unrecognized word \"{}\".", &expr[start..i])),
            }
            continue;
        }
        match b {
            b'+' | b'-' | b'*' | b'/' | b'%' => {
                tokens.push(ExprToken::Op(b as char));
                i += 1;
            }
            b'^' => {
                tokens.push(ExprToken::Caret);
                i += 1;
            }
            b'(' => {
                tokens.push(ExprToken::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(ExprToken::RParen);
                i += 1;
            }
            // `=`, `==`, `!=`, `<>`, `<`, `<=`, `>`, `>=`. MediaWiki compares
            // numbers here, and every comparison answers `1` or `0` — which is
            // what makes `{{#ifexpr: … > 100 | … }}` work at all.
            b'=' => {
                i += 1;
                if bytes.get(i) == Some(&b'=') {
                    i += 1;
                }
                tokens.push(ExprToken::Cmp("="));
            }
            b'!' => {
                i += 1;
                if bytes.get(i) == Some(&b'=') {
                    i += 1;
                    tokens.push(ExprToken::Cmp("!="));
                }
            }
            b'<' => {
                i += 1;
                match bytes.get(i) {
                    Some(b'=') => {
                        i += 1;
                        tokens.push(ExprToken::Cmp("<="));
                    }
                    Some(b'>') => {
                        i += 1;
                        tokens.push(ExprToken::Cmp("!="));
                    }
                    _ => tokens.push(ExprToken::Cmp("<")),
                }
            }
            b'>' => {
                i += 1;
                if bytes.get(i) == Some(&b'=') {
                    i += 1;
                    tokens.push(ExprToken::Cmp(">="));
                } else {
                    tokens.push(ExprToken::Cmp(">"));
                }
            }
            b'0'..=b'9' | b'.' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                // Scientific notation: `e`/`E`, an optional sign, then digits.
                // A standalone `e` (the constant) starts with a letter and is
                // handled above, so this only ever consumes an exponent that
                // hangs off a numeric literal.
                if let Some(&e) = bytes.get(i)
                    && (e == b'e' || e == b'E')
                {
                    let mut j = i + 1;
                    if matches!(bytes.get(j), Some(b'+' | b'-')) {
                        j += 1;
                    }
                    if bytes.get(j).is_some_and(u8::is_ascii_digit) {
                        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                            j += 1;
                        }
                        i = j;
                    }
                }
                if let Ok(num) = expr[start..i].parse::<f64>() {
                    tokens.push(ExprToken::Num(num));
                }
            }
            _ => {
                i += 1;
            } // Skip unknown chars
        }
    }
    Ok(tokens)
}

/// Simple precedence-climbing expression evaluator.
fn eval_simple(tokens: &[ExprToken]) -> std::result::Result<f64, RustoidError> {
    let mut pos = 0;
    expr_parse(tokens, &mut pos, 0)
}

fn expr_parse(
    tokens: &[ExprToken],
    pos: &mut usize,
    min_prec: u8,
) -> std::result::Result<f64, RustoidError> {
    let mut lhs = expr_primary(tokens, pos)?;
    while *pos < tokens.len() {
        let op = tokens[*pos].clone();
        let prec = precedence(&op);
        // `precedence` is 0 for everything that cannot appear between two
        // operands — a `)`, a number, a parenthesis — and those must *end* the
        // loop rather than be consumed as an operator with no effect. Treating
        // them as operators swallowed the rest of the expression after a
        // parenthesised group: `(650-590)/650*250` answered `60`.
        if prec == 0 || prec < min_prec {
            break;
        }
        // `^` binds to the right, so the same precedence is allowed on the
        // right-hand side; everything else is left-associative.
        let next_min = if op == ExprToken::Caret {
            prec
        } else {
            prec + 1
        };
        *pos += 1;
        let rhs = expr_parse(tokens, pos, next_min)?;
        lhs = match op {
            ExprToken::Op('+') => lhs + rhs,
            ExprToken::Op('-') => lhs - rhs,
            ExprToken::Op('*') => lhs * rhs,
            ExprToken::Op('/') => {
                if rhs == 0.0 {
                    return Err(RustoidError::Parse("Division by zero".to_string()));
                }
                lhs / rhs
            }
            ExprToken::Op('D') => {
                if rhs == 0.0 {
                    return Err(RustoidError::Parse("Division by zero".to_string()));
                }
                (lhs / rhs).trunc()
            }
            ExprToken::Op('%') => {
                if rhs == 0.0 {
                    return Err(RustoidError::Parse("Division by zero".to_string()));
                }
                lhs - rhs * (lhs / rhs).trunc()
            }
            ExprToken::Caret => {
                if rhs == 0.0 {
                    1.0
                } else {
                    lhs.powf(rhs)
                }
            }
            // `e` between two operands is scientific notation: `1E(-14)`, as
            // templates such as `Template:Order of magnitude` write it.
            ExprToken::Exp => lhs * 10f64.powf(rhs),
            ExprToken::And => bool_num(truthy(lhs) && truthy(rhs)),
            ExprToken::Or => bool_num(truthy(lhs) || truthy(rhs)),
            ExprToken::Cmp(cmp) => compare(cmp, lhs, rhs),
            ExprToken::Round => round_to(lhs, rhs),
            _ => lhs,
        };
    }
    Ok(lhs)
}

/// MediaWiki's `#expr` answers `1`/`0` for a comparison, so a boolean is a
/// number here too.
fn truthy(v: f64) -> bool {
    v != 0.0
}

fn bool_num(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

fn compare(cmp: &str, lhs: f64, rhs: f64) -> f64 {
    bool_num(match cmp {
        "=" => lhs == rhs,
        "!=" => lhs != rhs,
        "<" => lhs < rhs,
        ">" => lhs > rhs,
        "<=" => lhs <= rhs,
        ">=" => lhs >= rhs,
        _ => false,
    })
}

/// `value round digits` — MediaWiki's `round`, which rounds to `digits` decimal
/// places (a negative `digits` rounds to tens, hundreds, …), half away from zero
/// as PHP's `round` does. `{{#expr: 1234.5678 round 2}}` is `1234.57` and
/// `{{#expr: 1234 round -2}}` is `1200`.
fn round_to(value: f64, digits: f64) -> f64 {
    let factor = 10f64.powi(digits as i32);
    let rounded = (value * factor).round() / factor;
    if rounded.is_finite() { rounded } else { value }
}

fn expr_primary(tokens: &[ExprToken], pos: &mut usize) -> std::result::Result<f64, RustoidError> {
    if *pos >= tokens.len() {
        return Ok(0.0);
    }
    match tokens[*pos] {
        ExprToken::Num(n) => {
            *pos += 1;
            Ok(n)
        }
        ExprToken::Exp => {
            *pos += 1;
            Ok(std::f64::consts::E)
        }
        ExprToken::Op('-') => {
            *pos += 1;
            let val = expr_primary(tokens, pos)?;
            Ok(-val)
        }
        ExprToken::Op('+') => {
            *pos += 1;
            expr_primary(tokens, pos)
        }
        ExprToken::Not => {
            *pos += 1;
            let val = expr_primary(tokens, pos)?;
            Ok(if truthy(val) { 0.0 } else { 1.0 })
        }
        ExprToken::LParen => {
            *pos += 1;
            let val = expr_parse(tokens, pos, 0)?;
            if *pos < tokens.len() && tokens[*pos] == ExprToken::RParen {
                *pos += 1;
            }
            Ok(val)
        }
        // `round` needs a left operand; where a value is expected it is the
        // parser's error (`{{#expr: round 5}}`).
        ExprToken::Round => Err(RustoidError::Parse("Unexpected round operator".to_string())),
        // MediaWiki's parser tracks whether it expects a value or an operator and
        // rejects a *binary* operator where a value belongs: `{{#ifexpr: > 1}}` is
        // "Expression error: Unexpected > operator", not false. Quietly answering
        // zero made `{{#iferror:{{#ifexpr: > 1}}|ERR|OK}}` answer `OK`, which
        // `Template:Reflist` observes: its class falls to the `reflist-columns-3`
        // default instead of `mw-references-columns`.
        ExprToken::Cmp(name) => Err(RustoidError::Parse(format!("Unexpected {name} operator"))),
        ExprToken::Op(op) => Err(RustoidError::Parse(format!(
            "Unexpected {} operator",
            expr_operator_name(op)
        ))),
        ExprToken::Caret => Err(RustoidError::Parse("Unexpected ^ operator".to_string())),
        ExprToken::And => Err(RustoidError::Parse("Unexpected and operator".to_string())),
        ExprToken::Or => Err(RustoidError::Parse("Unexpected or operator".to_string())),
        ExprToken::Func(name) => {
            *pos += 1;
            // The argument is the next primary — a number, a constant, a
            // parenthesised expression, a signed value, or another function.
            // That binding is what makes `floor2.5^2` answer `4` (i.e. `(floor
            // 2.5)^2`), matching the live wiki.
            let arg = expr_primary(tokens, pos)?;
            Ok(apply_expr_func(name, arg))
        }
        ExprToken::RParen => Ok(0.0),
    }
}

/// The name MediaWiki's expression parser uses for an operator in its error
/// messages (`unexpected_operator` / `missing_operand`).
fn expr_operator_name(op: char) -> &'static str {
    match op {
        'D' => "div",
        '%' => "mod",
        '+' => "+",
        '-' => "-",
        '*' => "*",
        '/' => "/",
        _ => "?",
    }
}

/// Evaluate one `#expr` math function. MediaWiki's `ExprParser` uses PHP's
/// counterparts; the names the lexer accepts are exactly the ones the wiki
/// evaluates.
fn apply_expr_func(name: &str, value: f64) -> f64 {
    match name {
        "abs" => value.abs(),
        "ceil" => value.ceil(),
        "floor" => value.floor(),
        "trunc" => value.trunc(),
        "sqrt" => value.sqrt(),
        "exp" => value.exp(),
        "ln" => value.ln(),
        "sin" => value.sin(),
        "cos" => value.cos(),
        "tan" => value.tan(),
        "asin" => value.asin(),
        "acos" => value.acos(),
        "atan" => value.atan(),
        _ => value,
    }
}

/// MediaWiki's operator precedence, loosest first: `or`, `and`, comparison,
/// additive, multiplicative, then `^`.
fn precedence(t: &ExprToken) -> u8 {
    match t {
        ExprToken::Or => 1,
        ExprToken::And => 2,
        ExprToken::Cmp(_) => 3,
        // `round` binds looser than `+ -` (its second operand is a plain
        // number) but tighter than a comparison: `1.234 + 1 round 1` is
        // `(1.234 + 1) round 1`.
        ExprToken::Round => 4,
        ExprToken::Op('+' | '-') => 5,
        ExprToken::Op('*' | '/' | '%' | 'D') => 6,
        ExprToken::Caret => 7,
        // `1E-14` binds like `^`: tighter than `*`.
        ExprToken::Exp => 7,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(k: &str, v: &str) -> KV {
        KV {
            key: KeyValue::Str(k.to_string()),
            value: KeyValue::Str(v.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        }
    }

    fn params(args: Vec<(&str, &str)>) -> Params {
        Params::new(args.into_iter().map(|(k, v)| kv(k, v)).collect())
    }

    /// `expandKV`: when either half of a named argument holds tokens, the two are
    /// rejoined as *tokens* (Parsoid's `rejoinKV`), never stringified. A `{{!}}`
    /// inside a `#if` branch sits in the **key** half of a `k=v` split, so
    /// stringifying dropped it and `Template:Taxobox/core`'s image row lost its
    /// cell.
    /// A *name* tokenized as a URL (`{{#if:1|http://x/?a=b}}`): the `=` split the
    /// KV, so the name's autolink stops at the `=` and the value would fall
    /// outside it. Parsoid stringifies the KV and expands it again, reading the
    /// reassembled `name=value` as one wikitext run, so the URL is one link.
    #[test]
    fn test_expand_kv_re_reads_a_url_name_with_its_value() {
        use crate::wikitext::tokens_v2::{DataParsoid, SelfclosingTagTk};
        let mut url = SelfclosingTagTk::new("urllink", vec![], DataParsoid::default());
        url.add_attribute_str("href", "http://x/?a");
        let kv = KV {
            key: KeyValue::Tokens(vec![Item::Tok(ParsoidToken::SelfclosingTag(url))]),
            value: KeyValue::Str("b".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        let items = ParserFunctions::expand_kv(Some(&kv), None);
        assert_eq!(items.len(), 1, "{items:?}");
        let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &items[0] else {
            panic!("expected a urllink, got {items:?}");
        };
        assert_eq!(t.name, "urllink");
        assert_eq!(
            t.attribs
                .iter()
                .find(|a| a.key.as_str() == Some("href"))
                .and_then(|a| a.value.as_str()),
            Some("http://x/?a=b")
        );
    }

    #[test]
    fn test_expand_kv_rejoins_a_tokenized_named_argument() {
        use crate::wikitext::tokens_v2::{DataParsoid, SelfclosingTagTk};
        let tok = || {
            Item::Tok(ParsoidToken::SelfclosingTag(SelfclosingTagTk::new(
                "template",
                vec![],
                DataParsoid::default(),
            )))
        };
        let kv = KV {
            key: KeyValue::Tokens(vec![tok(), Item::Str(" colspan".to_string())]),
            value: KeyValue::Tokens(vec![Item::Str("2".to_string()), tok()]),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        let items = ParserFunctions::expand_kv(Some(&kv), None);
        assert_eq!(items.len(), 5, "{items:?}");
        assert!(matches!(items[0], Item::Tok(_)));
        assert_eq!(items[1], Item::Str(" colspan".to_string()));
        assert_eq!(items[2], Item::Str("=".to_string()));
        assert_eq!(items[3], Item::Str("2".to_string()));
        assert!(matches!(items[4], Item::Tok(_)));
    }

    #[test]
    fn test_pf_titleparts_matches_the_extension() {
        // Values probed from the live wiki (the ParserFunctions extension's
        // `titleparts`), so a rewrite that invents its own `array_slice`
        // semantics fails loudly rather than subtly.
        let config = crate::mock::MockSiteConfig::new();
        let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
            (vec![("A/B/C/D", ""), ("", "2"), ("", "2")], "B/C"),
            (vec![("A/B/C/D", "")], "A/B/C/D"),
            (vec![("A/B/C/D", ""), ("", "-1")], "A/B/C"),
            (vec![("A/B/C/D", ""), ("", "2"), ("", "-2")], "C/D"),
            (vec![("foo/bar/baz", ""), ("", "0"), ("", "2")], "bar/baz"),
            (
                vec![("foo/bar/baz", ""), ("", "0"), ("", "0")],
                "Foo/bar/baz",
            ),
            (vec![("foo/bar/baz", ""), ("", "2"), ("", "5")], ""),
            (vec![("foo/bar/baz", ""), ("", "-2")], "Foo"),
            (vec![("foo/bar/baz", ""), ("", "2"), ("", "-1")], "baz"),
            (vec![("Template:Foo/Bar", ""), ("", "1"), ("", "2")], "Bar"),
            (vec![("a/b/c", ""), ("", "1"), ("", "-9")], "A"),
            (vec![("a/b/c", ""), ("", "-1"), ("", "-1")], ""),
            (vec![("foo/bar baz", ""), ("", "1"), ("", "1")], "Foo"),
        ];
        for (args, want) in cases {
            let out = ParserFunctions::pf_titleparts(&config, &params(args.clone()));
            assert_eq!(out, vec![Item::Str(want.to_string())], "{args:?}");
        }
    }

    #[test]
    fn test_pf_expr_errors_on_an_unrecognized_word() {
        // MediaWiki's `ExprParser` errors on a word it does not know, and the
        // error is load-bearing: `{{#iferror:{{#expr:…}}}}` and templates that
        // test their own output for `[Ee]rror` (`Template:Period start`)
        // depend on it. Silently dropping the word answered a number instead.
        let out = ParserFunctions::pf_expr(&params(vec![("abc", "")]));
        let Item::Str(s) = &out[0] else {
            panic!("expected a string item: {out:?}")
        };
        assert!(s.contains("class=\"error\""), "{s}");
        assert!(s.contains("Unrecognized word \"abc\""), "{s}");

        // A recognized function word must not error, and arithmetic still works.
        assert_eq!(
            ParserFunctions::pf_expr(&params(vec![("1234", "")])),
            vec![Item::Str("1234".to_string())]
        );
        assert_eq!(
            ParserFunctions::pf_expr(&params(vec![("1+2", "")])),
            vec![Item::Str("3".to_string())]
        );
    }

    #[test]
    fn test_pf_expr_math_functions() {
        // The live wiki lexes a word as letters only, so `floor4` is `floor(4)`,
        // and evaluates the math functions applied to the next primary (which is
        // why `floor2.5^2` is `(floor 2.5)^2 = 4`). Each expectation was taken
        // from the transform endpoint.
        for (expr, want) in [
            ("floor4", "4"),
            ("floor4.7", "4"),
            ("floor 4.7", "4"),
            ("ceil4.2", "5"),
            ("trunc-4.7", "-4"),
            ("abs-4", "4"),
            ("sqrt9", "3"),
            ("exp0", "1"),
            ("ln(abs-4)", "1.3862943611199"),
            ("floor2.5^2", "4"),
            ("floor4-1", "3"),
            ("sin0", "0"),
            ("pi", "3.1415926535898"),
        ] {
            assert_eq!(
                ParserFunctions::pf_expr(&params(vec![(expr, "")])),
                vec![Item::Str(want.to_string())],
                "{expr}"
            );
        }
        // `log` is not one of the names the wiki evaluates.
        let out = ParserFunctions::pf_expr(&params(vec![("log100", "")]));
        let Item::Str(s) = &out[0] else {
            panic!("expected a string item: {out:?}")
        };
        assert!(s.contains("Unrecognized word \"log\""), "{s}");
    }

    /// Core's `#ifexpr` catches the expression error and answers the error
    /// markup itself rather than a branch, which is what makes
    /// `{{#iferror:{{#ifexpr: 30em>1}}|ERR|OK}}` answer `ERR`.
    /// A branch's edges are trimmed through comments, which never reach the
    /// output: `text <!--c-->` is `text`, as core trims the expanded *string*.
    #[test]
    fn a_branch_trims_through_a_trailing_comment() {
        use crate::wikitext::tokens_v2::{CommentTk, DataParsoid, ParsoidToken};

        let mut items = vec![
            Item::Str(" x ".to_string()),
            Item::Tok(ParsoidToken::Comment(CommentTk::new(
                "c",
                DataParsoid::default(),
            ))),
        ];
        trim_item_edges(&mut items);
        assert_eq!(items, vec![Item::Str("x".to_string())]);

        let mut items = vec![
            Item::Tok(ParsoidToken::Comment(CommentTk::new(
                "c",
                DataParsoid::default(),
            ))),
            Item::Str(" y".to_string()),
        ];
        trim_item_edges(&mut items);
        assert_eq!(items, vec![Item::Str("y".to_string())]);
    }

    #[test]
    fn test_pf_ifexpr_propagates_an_expression_error() {
        let out =
            ParserFunctions::pf_ifexpr(&params(vec![("30em>1", ""), ("then", ""), ("else", "")]));
        let Item::Str(s) = &out[0] else {
            panic!("expected a string item: {out:?}")
        };
        assert!(s.contains("class=\"error\""), "{s}");

        // A valid expression still selects a branch.
        assert_eq!(
            ParserFunctions::pf_ifexpr(&params(vec![("1", ""), ("", "yes"), ("", "no")])),
            vec![Item::Str("yes".to_string())]
        );

        // A binary operator with no left operand is an error, not `false`:
        // `Template:Reflist` tests exactly `{{#ifexpr: {{{1|}}} > 1 }}` with an
        // empty `1` and relies on the `#iferror` branch that follows from it.
        let out = ParserFunctions::pf_ifexpr(&params(vec![(" > 1", ""), ("", "yes"), ("", "no")]));
        let Item::Str(s) = &out[0] else {
            panic!("expected a string item: {out:?}")
        };
        assert!(s.contains("Unexpected > operator"), "{s}");
    }

    #[test]
    fn test_pf_if_true() {
        // #if:x|yes|no → args = [(k=x, v=), (k=, v=yes), (k=, v=no)]
        let p = params(vec![("x", ""), ("", "yes"), ("", "no")]);
        let out = ParserFunctions::pf_if(&p);
        assert_eq!(out, vec![Item::Str("yes".to_string())]);
    }

    #[test]
    fn test_pf_if_empty() {
        // #if:|yes|no → args = [(k=, v=), (k=, v=yes), (k=, v=no)]
        let p = params(vec![("", ""), ("", "yes"), ("", "no")]);
        let out = ParserFunctions::pf_if(&p);
        assert_eq!(out, vec![Item::Str("no".to_string())]);
    }

    #[test]
    fn test_pf_ifeq_match() {
        // #ifeq:a|a|yes|no
        let p = params(vec![("a", ""), ("", "a"), ("", "yes"), ("", "no")]);
        let out = ParserFunctions::pf_ifeq(&p);
        assert_eq!(out, vec![Item::Str("yes".to_string())]);
    }

    /// A taken branch that is tokenized markup must stay tokens, so the tag is
    /// parsed rather than escaped. Parsoid renders
    /// `{{#ifeq:1|0|y|<div>hi</div>}}` as a real `<div>` element.
    #[test]
    fn test_pf_ifeq_branch_keeps_markup_tokens() {
        use crate::wikitext::tokens_v2::{DataParsoid, TagTk};

        let mut div = TagTk::new("div", vec![], DataParsoid::default());
        div.data_parsoid.src = Some("<div>".to_string());
        let tokens = vec![Item::Tok(ParsoidToken::Tag(div))];

        let p = Params::new(vec![
            kv("1", ""),
            kv("0", ""),
            kv("", "y"),
            KV {
                key: KeyValue::Str(String::new()),
                value: KeyValue::Tokens(tokens),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ]);
        let out = ParserFunctions::pf_ifeq(&p);
        // The div token survives; stringifying would have given `Item::Str`.
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "div"),
            "expected the div token, got {out:?}"
        );
    }

    #[test]
    fn test_pf_expr() {
        let p = params(vec![("2+3*4", "")]);
        let out = ParserFunctions::pf_expr(&p);
        assert_eq!(out, vec![Item::Str("14".to_string())]);
    }

    /// A comparison answers `1`/`0`, which is what makes `#ifexpr` work.
    /// Without them the evaluator returned the left operand, so
    /// `{{#ifexpr: 39>100 | yes | no}}` took the `yes` branch.
    #[test]
    fn test_expr_comparisons_and_booleans() {
        for (expr, want) in [
            ("39>100", "0"),
            ("139>100", "1"),
            ("1=1", "1"),
            ("1<>2", "1"),
            ("1!=1", "0"),
            ("2<=2", "1"),
            ("not 0", "1"),
            ("1 and 0", "0"),
            ("2>1 and 3>2", "1"),
            ("1 or 0", "1"),
            ("2^10", "1024"),
            ("7 div 2", "3"),
        ] {
            assert_eq!(evaluate_expression(expr), want, "expr {expr:?}");
        }
    }

    /// `round` is a *binary* operator (rounding to N decimal places, negatives
    /// rounding left), so a leading `round` with no left operand is an error and
    /// not a dropped word — which is load-bearing for `Template:Period start`'s
    /// guard. The result is formatted as MediaWiki's `sprintf('%.14G')`: 14
    /// significant digits, exponential outside `[-4, 14)`, no zero-padded
    /// exponent and at least one fractional digit in the exponential form.
    #[test]
    fn test_expr_round_and_number_formatting() {
        for (expr, want) in [
            ("5 round 2", "5"),
            ("1234.5678 round 2", "1234.57"),
            ("1234 round -2", "1200"),
            ("1.234 + 1 round 1", "2.2"),
            ("538.8/650*250", "207.23076923077"),
            ("1/3", "0.33333333333333"),
            ("1e13", "10000000000000"),
            ("1e14", "1.0E+14"),
            ("1e-5", "1.0E-5"),
            // `E` between two operands is the scientific-notation operator, so a
            // parenthesised exponent works as well: `Template:Order of magnitude`
            // writes `1E((…)-14)`.
            ("1E(-14)", "1.0E-14"),
            ("2E(2)", "200"),
            ("e", "2.718281828459"),
            ("0.0001", "0.0001"),
            ("1.0", "1"),
            ("0.1+0.2", "0.3"),
        ] {
            assert_eq!(evaluate_expression(expr), want, "expr {expr:?}");
        }
        assert!(
            evaluate_expression("round 5").contains("Unexpected round operator"),
            "a prefix `round` is the parser's error"
        );
        assert!(
            evaluate_expression("1/0").contains("Division by zero"),
            "division by zero is MediaWiki's message"
        );
    }

    /// `|2|3|12|=exclude` is a fall-through group: the cases 2, 3 and 12 all
    /// answer `exclude`. The `=exclude` is a *result* for the group, which is why
    /// the tokenizer has to record whether a part had an `=` — both entries
    /// render an empty key.
    #[test]
    fn test_pf_switch_fall_through_group() {
        // `#switch:12|2|3|12|=exclude|#default=DEF`. The `=exclude` and
        // `#default=DEF` parts are named (their key range does not end where
        // their value begins); the bare cases are positional.
        let positional = |v: &str| KV {
            key: KeyValue::Str(String::new()),
            value: KeyValue::Str(v.to_string()),
            src_offsets: Some(crate::wikitext::tokens_v2::KVSourceRange {
                key_start: 0,
                key_end: 0,
                value_start: 0,
                value_end: v.len(),
                source: None,
            }),
            ksrc: None,
            vsrc: None,
        };
        let named = |k: &str, v: &str| KV {
            key: KeyValue::Str(k.to_string()),
            value: KeyValue::Str(v.to_string()),
            src_offsets: Some(crate::wikitext::tokens_v2::KVSourceRange {
                key_start: 0,
                key_end: k.len(),
                value_start: k.len() + 1,
                value_end: k.len() + 1 + v.len(),
                source: None,
            }),
            ksrc: None,
            vsrc: None,
        };
        let p = Params::new(vec![
            kv("12", ""),
            positional("2"),
            positional("3"),
            positional("12"),
            named("", "exclude"),
            named("#default", "DEF"),
        ]);
        assert_eq!(
            ParserFunctions::pf_switch(&p),
            vec![Item::Str("exclude".to_string())]
        );

        let p = Params::new(vec![
            kv("99", ""),
            positional("2"),
            positional("3"),
            positional("12"),
            named("", "exclude"),
            named("#default", "DEF"),
        ]);
        assert_eq!(
            ParserFunctions::pf_switch(&p),
            vec![Item::Str("DEF".to_string())]
        );
    }

    /// A branch that is a bare newline is a newline *token*, not a whitespace
    /// string, and `TokenUtils::tokenTrim` walks past it. Missing that left a
    /// stray `<span about="#mwtN"> </span>` inside `Template:Short description`
    /// on `Help:Introduction`: `Template:Main other`'s body ends with
    /// `| #default = {{{2|}}}\n}}`, so the matched value was a newline.
    #[test]
    fn test_token_trim_walks_past_newline_tokens() {
        use crate::wikitext::tokens_v2::{NlTk, SourceRange};

        let named = |v: &str| KV {
            key: KeyValue::Str("#default".to_string()),
            value: KeyValue::Tokens(vec![
                Item::Tok(ParsoidToken::Nl(NlTk::new(SourceRange::new(0, 1)))),
                Item::Str(v.to_string()),
            ]),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        // `#default` whose value is only a newline: nothing survives.
        let p = Params::new(vec![kv("other", ""), named("")]);
        assert_eq!(ParserFunctions::pf_switch(&p), vec![]);

        // A newline *inside* the value stops the walk: the text is kept.
        let p = Params::new(vec![kv("other", ""), named("x")]);
        assert_eq!(
            ParserFunctions::pf_switch(&p),
            vec![Item::Str("x".to_string())]
        );
    }

    /// Core trims a conditional's taken branch after expanding it, so a branch's
    /// surrounding blanks are not part of the answer. Parsoid's native versions
    /// do not trim, which is why the fixture expectations can disagree.
    #[test]
    fn test_conditionals_trim_their_branch() {
        let p = params(vec![("1", ""), ("", "Y "), ("", "N")]);
        assert_eq!(ParserFunctions::pf_if(&p), vec![Item::Str("Y".to_string())]);

        let p = params(vec![("1", ""), ("", "1"), ("", " Y "), ("", "N")]);
        assert_eq!(
            ParserFunctions::pf_ifeq(&p),
            vec![Item::Str("Y".to_string())]
        );

        let p = params(vec![("1", ""), ("", " Y "), ("", "N")]);
        assert_eq!(
            ParserFunctions::pf_ifexpr(&p),
            vec![Item::Str("Y".to_string())]
        );
    }

    #[test]
    fn test_pf_lc_uc() {
        let p = params(vec![("HeLLo", "")]);
        assert_eq!(
            ParserFunctions::pf_lc(&p),
            vec![Item::Str("hello".to_string())]
        );
        assert_eq!(
            ParserFunctions::pf_uc(&p),
            vec![Item::Str("HELLO".to_string())]
        );
    }

    #[test]
    fn test_pf_plural() {
        // English: a count of one takes the first form, any other the last.
        let two = params(vec![("2", ""), ("", "Shortcut"), ("", "Shortcuts")]);
        assert_eq!(
            ParserFunctions::pf_plural(&two),
            vec![Item::Str("Shortcuts".to_string())]
        );
        let one = params(vec![("1", ""), ("", "Shortcut"), ("", "Shortcuts")]);
        assert_eq!(
            ParserFunctions::pf_plural(&one),
            vec![Item::Str("Shortcut".to_string())]
        );
        // An explicit `n=form` wins when `n` matches, and is dropped otherwise.
        let explicit_hit = params(vec![("1", ""), ("1", "one"), ("", "many")]);
        assert_eq!(
            ParserFunctions::pf_plural(&explicit_hit),
            vec![Item::Str("one".to_string())]
        );
        let explicit_miss = params(vec![("5", ""), ("1", "one"), ("", "many")]);
        assert_eq!(
            ParserFunctions::pf_plural(&explicit_miss),
            vec![Item::Str("many".to_string())]
        );
    }

    #[test]
    fn test_pf_url() {
        // Mock site: server `https://en.wikipedia.org`, article path `/wiki/$1`,
        // script path `/w`.
        let config = crate::mock::MockSiteConfig::new();
        let title = params(vec![("WP:X", "")]);
        assert_eq!(
            ParserFunctions::pf_url(&config, "fullurl", &title),
            vec![Item::Str("//en.wikipedia.org/wiki/WP:X".to_string())]
        );
        let query = params(vec![("WP:X", ""), ("", "action=edit")]);
        assert_eq!(
            ParserFunctions::pf_url(&config, "localurl", &query),
            vec![Item::Str("/w/index.php?title=WP:X&action=edit".to_string())]
        );
        let bare = params(vec![("WP:X", "")]);
        assert_eq!(
            ParserFunctions::pf_url(&config, "canonicalurl", &bare),
            vec![Item::Str("https://en.wikipedia.org/wiki/WP:X".to_string())]
        );
    }

    #[test]
    fn test_pf_urlencode() {
        // Probed on the live wiki. The default is PHP's `urlencode` (`+` for a
        // space, everything but `A-Za-z0-9-_.` percent-encoded), `|wiki` is
        // `wfUrlencode`, `|path` is `rawurlencode`.
        let default = params(vec![("a b|c", "")]);
        assert_eq!(
            ParserFunctions::pf_urlencode(&default),
            vec![Item::Str("a+b%7Cc".to_string())]
        );
        let query = params(vec![("x/y?z=1&w", "")]);
        assert_eq!(
            ParserFunctions::pf_urlencode(&query),
            vec![Item::Str("x%2Fy%3Fz%3D1%26w".to_string())]
        );
        let wiki = params(vec![("a b", ""), ("", "wiki")]);
        assert_eq!(
            ParserFunctions::pf_urlencode(&wiki),
            vec![Item::Str("a_b".to_string())]
        );
        let path = params(vec![("a b", ""), ("", "path")]);
        assert_eq!(
            ParserFunctions::pf_urlencode(&path),
            vec![Item::Str("a%20b".to_string())]
        );
    }

    #[test]
    fn test_pf_padleft() {
        // #padleft:7|3|0 → target=7, n=3, pad=0
        let p = params(vec![("7", ""), ("", "3"), ("", "0")]);
        let out = ParserFunctions::pf_padleft(&p);
        assert_eq!(out, vec![Item::Str("007".to_string())]);
    }

    #[test]
    fn test_pf_formatnum_english() {
        // The `Language::formatNum` behaviours the corpus depends on, each
        // checked against the live Parsoid oracle: thousands grouping, fraction
        // digits kept verbatim, leading zeros kept, the Unicode minus, and the
        // empty input answered with the empty string.
        let cases = [
            ("1035000", "1,035,000"),
            ("40", "40"),
            ("1234567.89", "1,234,567.89"),
            ("007", "007"),
            ("-1234", "\u{2212}1,234"),
            ("1234.500", "1,234.500"),
            ("-0", "\u{2212}0"),
            ("abc123def", "abc123def"),
            // Parser-function arguments are trimmed before the handler runs.
            ("  190  ", "190"),
            ("", ""),
        ];
        for (input, expected) in cases {
            let p = params(vec![(input, "")]);
            assert_eq!(
                ParserFunctions::pf_formatnum(&p),
                vec![Item::Str(expected.to_string())],
                "formatnum({input:?})"
            );
        }
    }

    #[test]
    fn test_pf_formatnum_modifiers() {
        // `|R` parses a formatted number back to a bare one; `|NOSEP` formats
        // without separators. Both spellings reach a page in practice.
        let raw = params(vec![("1,035,000", ""), ("", "R")]);
        assert_eq!(
            ParserFunctions::pf_formatnum(&raw),
            vec![Item::Str("1035000".to_string())]
        );
        let nosep = params(vec![("1035000", ""), ("", "NOSEP")]);
        assert_eq!(
            ParserFunctions::pf_formatnum(&nosep),
            vec![Item::Str("1035000".to_string())]
        );
    }

    #[test]
    fn test_pf_tag() {
        // #tag:b|hello|class=foo — `b` is not a registered extension tag, so it
        // falls through to the plain `tag_worker` path.
        let config = crate::mock::MockSiteConfig::new();
        let p = params(vec![("b", ""), ("", "hello"), ("class", "foo")]);
        let out = ParserFunctions::pf_tag(&config, &p);
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "b"));
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "hello"))
        );
    }

    /// Core's `tagObj` trims a named attribute's name and value, then strips a
    /// surrounding pair of quotes. `class = ' foo '` is therefore the attribute
    /// `class`, value `foo` — not the value ` foo ` in a `class ` attribute.
    #[test]
    fn test_pf_tag_trims_attribute_name_and_value() {
        let config = crate::mock::MockSiteConfig::new();
        // `b` is not a registered extension tag, so the plain path keeps the
        // attributes on the tag token where they can be read directly.
        let p = params(vec![("b", ""), ("", "hello"), ("  class  ", " 'foo' ")]);
        let out = ParserFunctions::pf_tag(&config, &p);
        let Item::Tok(ParsoidToken::Tag(t)) = &out[0] else {
            panic!("expected a tag token, got {out:?}");
        };
        assert_eq!(t.attribs.len(), 1);
        assert_eq!(t.attribs[0].key.as_str(), Some("class"));
        // Trim outermost, then strip the quotes (core's order).
        assert_eq!(t.attribs[0].value.as_str(), Some("foo"));
    }

    #[test]
    fn test_pf_tag_extension_routing() {
        // `pre` is a registered extension tag, so `#tag:pre` must produce an
        // `extension` token (not a plain `<pre>`), letting the extension handler
        // emit `mw:Extension/pre` and sanitize `format` away.
        let config = crate::mock::MockSiteConfig::new();
        let p = params(vec![("pre", ""), ("", "123"), ("format", "\"wikitext\"")]);
        let out = ParserFunctions::pf_tag(&config, &p);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Item::Tok(ParsoidToken::SelfclosingTag(t)) => {
                assert_eq!(t.name, "extension");
                let name = t
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("name"))
                    .and_then(|kv| kv.value.as_str());
                assert_eq!(name, Some("pre"));
            }
            other => panic!("expected extension token, got {other:?}"),
        }
    }

    /// A `#tag:<extension>` call spends the parse's pFragment numbers in order:
    /// one for its own atomic result, then one per piece of its body. Parsoid
    /// prepares the call before expanding the body, so the body's first piece is
    /// numbered after the call's own. The marker in `source` is what the served
    /// `extsrc` carries.
    #[test]
    fn test_pf_tag_numbers_its_fragment_before_the_body_pieces() {
        use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, TagTk};

        crate::pipeline::pfragment::reset();
        let placeholder = |name: &str, ty: &str| {
            let mut tag = TagTk::new(name, vec![], DataParsoid::default());
            tag.add_attribute_str("typeof", ty);
            tag.add_attribute_str("data-fragment-id", "7");
            Item::Tok(ParsoidToken::Tag(tag))
        };
        let content = vec![
            Item::Str("a".to_string()),
            placeholder("style", "mw:DOMFragment"),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "style",
                vec![],
                DataParsoid::default(),
            ))),
            Item::Str("b".to_string()),
        ];
        let p = Params::new(vec![
            kv("pre", ""),
            KV {
                key: KeyValue::Str(String::new()),
                value: KeyValue::Tokens(content),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ]);
        let out = ParserFunctions::pf_tag(&crate::mock::MockSiteConfig::new(), &p);
        let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &out[0] else {
            panic!("expected an extension token, got {out:?}");
        };
        let source = t
            .attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("source"))
            .and_then(|kv| kv.value.as_str());
        assert_eq!(source, Some("<pre>a{{#parsoid\u{0}fragment:1}}b</pre>"));
        // The call's own number and the body's piece are spent.
        assert_eq!(crate::pipeline::pfragment::allocate(), 2);
    }

    /// A `#tag:<extension>` reached from another such call's content argument is
    /// expanded lazily as a piece of that argument, so it spends no number of
    /// its own — the outer body's serialization numbers the whole piece.
    #[test]
    fn test_a_lazy_tag_call_spends_no_number_for_itself() {
        crate::pipeline::pfragment::reset();
        let previous = crate::pipeline::pfragment::set_lazy(true);
        let p = params(vec![("pre", ""), ("", "123")]);
        let out = ParserFunctions::pf_tag(&crate::mock::MockSiteConfig::new(), &p);
        crate::pipeline::pfragment::restore_lazy(previous);
        assert!(matches!(
            &out[0],
            Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "extension"
        ));
        assert_eq!(crate::pipeline::pfragment::allocate(), 0);
    }

    /// A `#tag` attribute value that carries HTML markup must keep it.
    /// `Module:OSM Location map` passes `text=<div title="…">&nbsp;…</div>`
    /// through `frame:extensionTag`; dropping the tags emptied the attribute
    /// and the `<maplink>` lost its label.
    #[test]
    fn test_pf_tag_attribute_value_keeps_html_markup() {
        let config = crate::mock::MockSiteConfig::new();
        let value = crate::pipeline::template_handler::tokenize_wikitext_to_items(
            "<div title=\"x\">A&nbsp;B</div>",
            true,
            &[],
        );
        let p = Params::new(vec![
            kv("maplink", ""),
            kv("", ""),
            KV {
                key: KeyValue::Str("text".to_string()),
                value: KeyValue::Tokens(value),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ]);
        let out = ParserFunctions::pf_tag(&config, &p);
        let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &out[0] else {
            panic!("expected an extension token, got {out:?}");
        };
        let text = t
            .data_mw
            .as_ref()
            .and_then(|mw| mw.attribs.iter().find(|a| a.key.as_str() == Some("text")))
            .and_then(|a| a.value.as_str());
        assert_eq!(text, Some("<div title=\"x\">A&nbsp;B</div>"));
    }

    #[test]
    fn test_strip_attr_value_quotes() {
        assert_eq!(strip_attr_value_quotes("\"wikitext\""), "wikitext");
        assert_eq!(strip_attr_value_quotes("'x'"), "x");
        assert_eq!(strip_attr_value_quotes("noquotes"), "noquotes");
        assert_eq!(strip_attr_value_quotes("\"\""), "");
        // Mismatched quotes are left intact.
        assert_eq!(strip_attr_value_quotes("\"oops'"), "\"oops'");
    }

    #[test]
    fn test_pf_dir_bcp47_override() {
        // `{{#dir:en|bcp47}}` → `ltr`; the fixture's `definition_list_template`
        // form passes the format as the value of the first arg.
        let p = params(vec![("en", "bcp47")]);
        assert_eq!(ParserFunctions::pf_dir(&p), vec![Item::Str("ltr".into())]);
        let p = params(vec![("ar-x-rtl", "bcp47")]);
        assert_eq!(ParserFunctions::pf_dir(&p), vec![Item::Str("rtl".into())]);
    }

    #[test]
    fn test_pf_dir_no_args_is_auto() {
        // No code: report the content language's direction, which rustoid cannot
        // resolve without a language table, so `auto`.
        let p = params(vec![]);
        assert_eq!(ParserFunctions::pf_dir(&p), vec![Item::Str("auto".into())]);
    }

    #[test]
    fn test_pf_anchorencode_wraps_delimiters() {
        // `[foo]` has no whitespace/entities to normalize, so the brackets are
        // the only thing that needs `mw:Entity` protection (T179544).
        let p = params(vec![("[foo]", "")]);
        let out = ParserFunctions::pf_anchorencode(&p);
        assert_eq!(tokens_to_string(&out), "[foo]");

        let entities = out
            .iter()
            .filter(|it| {
                matches!(it, Item::Tok(ParsoidToken::Tag(t))
                    if t.attribs.iter().any(|kv| kv.key.as_str() == Some("typeof")
                        && kv.value.as_str() == Some("mw:Entity")))
            })
            .count();
        assert_eq!(entities, 2, "expected one entity span per bracket: {out:?}");
    }

    #[test]
    fn test_pf_anchorencode_normalizes_whitespace() {
        // Runs of spaces/underscores collapse to a single `_` (via the html5
        // id escape) and the result is trimmed-ish.
        let p = params(vec![("foo  _bar", "")]);
        let out = ParserFunctions::pf_anchorencode(&p);
        assert_eq!(tokens_to_string(&out), "foo_bar");
    }

    #[test]
    fn test_entity_encode_all_is_codepoint_wise() {
        // PHP's `mb_encode_numericentity(..., true)` encodes whole codepoints,
        // not UTF-8 bytes, and zero-pads to at least two hex digits.
        assert_eq!(entity_encode_all('['), "&#x5B;");
        assert_eq!(entity_encode_all('\u{9}'), "&#x09;");
        assert_eq!(entity_encode_all('é'), "&#xE9;");
        assert_eq!(entity_encode_all('\u{4E2D}'), "&#x4E2D;");
        // The one `$conventions` entry that matters here.
        assert_eq!(entity_encode_all('\u{A0}'), "&nbsp;");
    }

    #[test]
    fn test_pf_anchorencode_non_ascii() {
        // A non-ASCII anchor must survive as its own literal text (no delimiter
        // to protect), not be mangled into per-byte entities.
        let p = params(vec![("Café", "")]);
        let out = ParserFunctions::pf_anchorencode(&p);
        assert_eq!(tokens_to_string(&out), "Café");
    }
}
