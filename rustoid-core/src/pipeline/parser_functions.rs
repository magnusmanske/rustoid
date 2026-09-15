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
        out.push_str(&value);
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

/// The ParserFunctions handler.
pub struct ParserFunctions;

impl ParserFunctions {
    /// `#if` — mirrors `pf_if`.
    pub fn pf_if(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let condition = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        if condition.trim() != "" {
            Self::expand_kv(args.get(1), None)
        } else {
            Self::expand_kv(args.get(2), None)
        }
    }

    /// `#ifeq` — mirrors `pf_ifeq` / `ifeq_worker`.
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
        if a.trim() == b.trim() {
            Self::expand_kv(args.get(2), None)
        } else {
            Self::expand_kv(args.get(3), None)
        }
    }

    /// `#switch` — mirrors `pf_switch` / `switchLookupFallback`.
    pub fn pf_switch(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let target = args
            .first()
            .map(|kv| key_value_to_string(&kv.key).trim().to_string())
            .unwrap_or_default();

        // Check dict (named args) first.
        let dict = params.dict();
        if !target.is_empty()
            && let Some(v) = dict.get(&target)
        {
            return Self::trim_res(&Self::stringify_value(v));
        }

        // Fallback lookup over positional entries.
        Self::switch_lookup_fallback(&args[1..], &target, &dict)
    }

    fn switch_lookup_fallback(
        kvs: &[KV],
        key: &str,
        dict: &std::collections::HashMap<String, KeyValue>,
    ) -> Vec<Item> {
        let l = kvs.len();
        if l == 0 {
            return vec![];
        }

        // Fall-through handling is approximated for the common cases:
        // search for the first value-only entry matching the key via
        // "a=b"-style positional pairs, or a bare default.
        for kv in kvs {
            let k = key_value_to_string(&kv.key);
            if !k.is_empty() {
                // Named/equal key: if it matches, return its value.
                if k.trim() == key {
                    return Self::trim_res(&Self::stringify_value(&kv.value));
                }
            } else {
                // Value-only entry: this is a fall-through candidate.
                let v = value_to_string(&kv.value);
                if v.trim() == key {
                    // Find the next non-empty-key entry's value.
                    for next in kvs {
                        let nk = key_value_to_string(&next.key);
                        if !nk.is_empty() {
                            return Self::trim_res(&Self::stringify_value(&next.value));
                        }
                    }
                    return vec![];
                }
            }
        }

        // Default value (last value-only entry).
        if let Some(last) = kvs.last()
            && key_value_to_string(&last.key).is_empty()
        {
            return vec![Item::Str(value_to_string(&last.value))];
        }

        if let Some(default) = dict.get("#default") {
            return Self::trim_res(&Self::stringify_value(default));
        }

        vec![]
    }

    fn stringify_value(v: &KeyValue) -> String {
        value_to_string(v)
    }

    fn trim_res(s: &str) -> Vec<Item> {
        vec![Item::Str(s.trim().to_string())]
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
    pub fn pf_ifexpr(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let target = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        let res = evaluate_expression(&target);
        if res != "0" && !res.is_empty() && !res.contains("error") {
            Self::expand_kv(args.get(1), None)
        } else {
            Self::expand_kv(args.get(2), None)
        }
    }

    /// `#iferror` — mirrors `pf_iferror`.
    pub fn pf_iferror(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let target = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        let has_error =
            target.contains("class=\"error\"") || target.contains("<strong class=\"error\">");
        if has_error {
            Self::expand_kv(args.get(1), None)
        } else {
            Self::expand_kv(args.get(2), None)
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

        // Collect the tag attributes (named args) and content (positional args),
        // losing the attribute order like the legacy `tagObj`/`tag_worker` do.
        // Attribute values have any surrounding single/double quotes stripped,
        // mirroring `tagObj`'s `preg_match('/^(?:["'](.+)["']|""|\'\')$/s', …)`.
        let mut content: Vec<Item> = Vec::new();
        let mut tag_attribs: Vec<KV> = Vec::new();
        for kv in &args[1..] {
            if key_value_to_string(&kv.key).is_empty() {
                content.extend(key_value_to_items(&kv.value));
            } else {
                let mut kv = kv.clone();
                kv.value = KeyValue::Str(strip_attr_value_quotes(&key_value_to_string(&kv.value)));
                tag_attribs.push(kv);
            }
        }

        let lc_target = target.to_lowercase();
        let is_ext = config
            .extension_tags()
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&lc_target));

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
        let content_src: String = crate::expand::tpl_args::replace_magic_pipe(
            &content
                .iter()
                .map(|it| match it {
                    Item::Str(s) => s.clone(),
                    Item::Tok(t) => token_to_source(t),
                })
                .collect::<String>(),
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

    /// `#urlencode` — mirrors `pf_urlencode`.
    pub fn pf_urlencode(params: &Params) -> Vec<Item> {
        let target = params
            .args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        vec![Item::Str(crate::sanitizer::encode_url_for_ext_link(
            target.trim(),
        ))]
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
            .unwrap_or_default()
            .trim()
            .to_lowercase();

        let dir = if code.is_empty() {
            "auto"
        } else if code.ends_with("-x-rtl") {
            "rtl"
        } else {
            "ltr"
        };
        vec![Item::Str(dir.to_string())]
    }

    /// Expand a KV into items (mirrors `expandKV` for string keys/values).
    fn expand_kv(kv: Option<&KV>, default: Option<&str>) -> Vec<Item> {
        match kv {
            None => vec![Item::Str(default.unwrap_or("").to_string())],
            Some(kv) => {
                let k = key_value_to_string(&kv.key);
                let v = value_to_string(&kv.value);
                if !k.is_empty() {
                    vec![Item::Str(format!("{k}={v}"))]
                } else {
                    vec![Item::Str(v)]
                }
            }
        }
    }
}

/// Append a single character to `out` wrapped in an `mw:Entity` span, so it
/// survives as literal text instead of being re-interpreted as wikitext.
/// Mirrors `ParserFunctions::encodeCharEntity`.
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

/// Very basic expression evaluator for `#expr` and `#ifexpr`.
/// Supports +, -, *, /, % and parentheses. Returns result as string.
pub(crate) fn evaluate_expression(expr: &str) -> String {
    let expr = expr.trim();
    if expr.is_empty() {
        return String::new();
    }

    // Try to parse as a simple integer expression.
    let tokens = tokenize_expr(expr);
    match eval_simple(&tokens) {
        Ok(val) => {
            if val == val.trunc() {
                val.to_string()
            } else {
                format!("{val:.6}")
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_string()
            }
        }
        Err(_) => format!("<strong class=\"error\">Expression error: {expr}</strong>"),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ExprToken {
    Num(f64),
    Op(char),
    LParen,
    RParen,
}

fn tokenize_expr(expr: &str) -> Vec<ExprToken> {
    let mut tokens = Vec::new();
    let mut i = 0;
    let bytes = expr.as_bytes();

    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' => {
                i += 1;
            }
            b'+' | b'-' | b'*' | b'/' | b'%' => {
                tokens.push(ExprToken::Op(bytes[i] as char));
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
            b'0'..=b'9' | b'.' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
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
    tokens
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
        let op = match tokens.get(*pos) {
            Some(ExprToken::Op(c)) => *c,
            _ => break,
        };
        let prec = precedence(op);
        if prec < min_prec {
            break;
        }
        *pos += 1;
        let rhs = expr_parse(tokens, pos, prec + 1)?;
        lhs = match op {
            '+' => lhs + rhs,
            '-' => lhs - rhs,
            '*' => lhs * rhs,
            '/' => {
                if rhs == 0.0 {
                    return Err(RustoidError::Parse("division by zero".to_string()));
                }
                lhs / rhs
            }
            '%' => {
                if rhs == 0.0 {
                    return Err(RustoidError::Parse("modulo by zero".to_string()));
                }
                lhs - rhs * (lhs / rhs).trunc()
            }
            _ => lhs,
        };
    }
    Ok(lhs)
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
        ExprToken::Op('-') => {
            *pos += 1;
            let val = expr_primary(tokens, pos)?;
            Ok(-val)
        }
        ExprToken::LParen => {
            *pos += 1;
            let val = expr_parse(tokens, pos, 0)?;
            if *pos < tokens.len() && tokens[*pos] == ExprToken::RParen {
                *pos += 1;
            }
            Ok(val)
        }
        _ => Ok(0.0),
    }
}

fn precedence(op: char) -> u8 {
    match op {
        '+' | '-' => 1,
        '*' | '/' | '%' => 2,
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

    #[test]
    fn test_pf_expr() {
        let p = params(vec![("2+3*4", "")]);
        let out = ParserFunctions::pf_expr(&p);
        assert_eq!(out, vec![Item::Str("14".to_string())]);
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
    fn test_pf_urlencode() {
        let p = params(vec![("a b|c", "")]);
        let out = ParserFunctions::pf_urlencode(&p);
        assert_eq!(out, vec![Item::Str("a%20b%7Cc".to_string())]);
    }

    #[test]
    fn test_pf_padleft() {
        // #padleft:7|3|0 → target=7, n=3, pad=0
        let p = params(vec![("7", ""), ("", "3"), ("", "0")]);
        let out = ParserFunctions::pf_padleft(&p);
        assert_eq!(out, vec![Item::Str("007".to_string())]);
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
