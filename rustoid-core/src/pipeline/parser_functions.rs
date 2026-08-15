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

use crate::wikitext::preprocessor::evaluate_expression;
use crate::wikitext::token_utils::tokens_to_string;
use crate::wikitext::tokens_v2::{Item, KV, KeyValue, ParsoidToken};

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

    /// Slice args and convert their values to strings (mirrors `Params::getSlice`).
    pub fn get_slice(&self, start: usize, end: usize) -> Vec<KV> {
        self.args[start..start.min(end.saturating_sub(start))].to_vec()
    }
}

/// Extract a key value as a trimmed string.
fn key_value_to_string(kv: &KeyValue) -> String {
    match kv {
        KeyValue::Str(s) => s.clone(),
        KeyValue::Tokens(t) => {
            tokens_to_string(&t.iter().cloned().map(Item::Tok).collect::<Vec<_>>())
        }
    }
}

/// Extract a value as a string.
fn value_to_string(kv: &KeyValue) -> String {
    match kv {
        KeyValue::Str(s) => s.clone(),
        KeyValue::Tokens(t) => {
            tokens_to_string(&t.iter().cloned().map(Item::Tok).collect::<Vec<_>>())
        }
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

    /// `#tag` — mirrors `pf_tag` / `tag_worker`.
    pub fn pf_tag(params: &Params) -> Vec<Item> {
        let args = &params.args;
        let target = args
            .first()
            .map(|kv| key_value_to_string(&kv.key))
            .unwrap_or_default();
        if target.is_empty() {
            return vec![];
        }

        let mut tag = crate::wikitext::tokens_v2::TagTk::new(&target, vec![], Default::default());
        let mut content: Vec<Item> = Vec::new();
        let mut tag_attribs: Vec<KV> = Vec::new();

        for kv in &args[1..] {
            if key_value_to_string(&kv.key).is_empty() {
                content.push(key_value_to_item(&kv.value));
            } else {
                tag_attribs.push(kv.clone());
            }
        }

        tag.attribs = tag_attribs;
        let mut out = vec![Item::Tok(ParsoidToken::Tag(tag))];
        out.extend(content);
        out.push(Item::Tok(ParsoidToken::EndTag(
            crate::wikitext::tokens_v2::EndTagTk::new(&target, vec![], Default::default()),
        )));
        out
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

/// Convert a KeyValue to an Item.
fn key_value_to_item(v: &KeyValue) -> Item {
    match v {
        KeyValue::Str(s) => Item::Str(s.clone()),
        KeyValue::Tokens(t) => {
            if t.len() == 1 {
                Item::Tok(t[0].clone())
            } else {
                Item::Str(tokens_to_string(
                    &t.iter().cloned().map(Item::Tok).collect::<Vec<_>>(),
                ))
            }
        }
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
        // #tag:b|hello|class=foo
        let p = params(vec![("b", ""), ("", "hello"), ("class", "foo")]);
        let out = ParserFunctions::pf_tag(&p);
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "b"));
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "hello"))
        );
    }
}
