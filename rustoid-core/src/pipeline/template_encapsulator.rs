//! TemplateEncapsulator — port of the encapsulation-marker subset of PHP
//! Parsoid's `src/Wt2Html/TT/TemplateEncapsulator.php`.
//!
//! Wraps expanded template/parser-function/variable output with
//! `<meta typeof="mw:Transclusion">` ... `<meta typeof="mw:Transclusion/End">`
//! markers (and `mw:Param` for bare template arguments), plus the `TemplateInfo`
//! / `ParamInfo` data-mw metadata that enables round-tripping.

use crate::wikitext::tokens_v2::{
    DataParsoid, Item, KV, KeyValue, ParsoidToken, SelfclosingTagTk, SourceRange,
};

/// A single template parameter's metadata (mirrors PHP's `ParamInfo`).
#[derive(Debug, Clone, Default)]
pub struct ParamInfo {
    /// Parameter key (string form, positional index for unnamed args).
    pub k: String,
    /// The key source wikitext, if different from `k`.
    pub key_wt: Option<String>,
    /// The parameter's wikitext value.
    pub value_wt: String,
    /// Whether this is a named parameter.
    pub named: bool,
    /// Four-element whitespace array for non-standard spacing.
    pub spc: Option<[String; 4]>,
    /// Precomputed HTML representation (optional).
    pub html: Option<String>,
}

impl ParamInfo {
    pub fn new(k: impl Into<String>) -> Self {
        Self {
            k: k.into(),
            key_wt: None,
            value_wt: String::new(),
            named: false,
            spc: None,
            html: None,
        }
    }

    /// Returns true if this parameter uses a positive-integer key, like a
    /// positional argument. Mirrors `ParamInfo::isNumericKey`.
    pub fn is_numeric_key(&self) -> bool {
        self.k
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit() && b != b'0')
            && self.k.bytes().all(|b| b.is_ascii_digit())
    }
}

/// Template metadata (mirrors PHP's `TemplateInfo`).
#[derive(Debug, Clone, Default)]
pub struct TemplateInfo {
    /// The target wikitext.
    pub target_wt: Option<String>,
    /// Parser function / variable name (for `mw:Transclusion` of functions).
    pub func: Option<String>,
    /// Template target href (for template transclusions).
    pub href: Option<String>,
    /// Resolved template title (absolute href).
    pub resolved_title: Option<String>,
    /// Resolved template revision id.
    pub resolved_rev_id: Option<i64>,
    /// The parameter list.
    pub param_infos: Vec<ParamInfo>,
    /// Template type descriptor.
    pub ty: Option<String>,
}

/// Builds the transclusion encapsulation markers around a token chunk.
pub struct TemplateEncapsulator {
    wrapper_type: String,
    about_id: String,
    token_tsr: Option<SourceRange>,
    token_src: Option<String>,
    token_colon: Option<String>,
}

impl TemplateEncapsulator {
    /// Create an encapsulator for a token, mirroring the PHP constructor.
    pub fn new(wrapper_type: &str, about_id: String, token: &ParsoidToken) -> Self {
        let dp = token.data_parsoid();
        Self {
            wrapper_type: wrapper_type.to_string(),
            about_id,
            token_tsr: dp.and_then(|d| d.tsr.clone()),
            token_src: dp.and_then(|d| d.src.clone()),
            token_colon: None,
        }
    }

    /// Set the colon separator (for parser functions with colon syntax),
    /// mirroring `$token->dataParsoid->colon`.
    pub fn set_colon(&mut self, colon: Option<String>) {
        self.token_colon = colon;
    }

    /// Produce the opening `<meta typeof="mw:Transclusion">` marker, mirroring
    /// `getEncapsulationInfo`.
    pub fn encapsulation_info_start(&self) -> Item {
        let dp = DataParsoid {
            tsr: self.token_tsr.clone(),
            src: self.token_src.clone(),
            ..Default::default()
        };

        let mut meta = SelfclosingTagTk::new("meta", vec![], dp);
        meta.attribs.push(string_kv("typeof", &self.wrapper_type));
        meta.attribs.push(string_kv("about", &self.about_id));

        Item::Tok(ParsoidToken::SelfclosingTag(meta))
    }

    /// Produce the closing `<meta typeof="mw:Transclusion/End">` marker,
    /// mirroring `getEncapsulationInfoEndTag`.
    pub fn encapsulation_info_end(&self) -> Item {
        let dp = DataParsoid {
            tsr: self
                .token_tsr
                .as_ref()
                .map(|tsr| SourceRange::with_null_start(tsr.end)),
            ..Default::default()
        };

        let mut meta = SelfclosingTagTk::new("meta", vec![], dp);
        meta.attribs
            .push(string_kv("typeof", &format!("{}/End", self.wrapper_type)));
        meta.attribs.push(string_kv("about", &self.about_id));

        Item::Tok(ParsoidToken::SelfclosingTag(meta))
    }

    /// Wrap a token chunk in the encapsulation markers, and store the
    /// template info on the start marker (mirrors `encapTokens`).
    pub fn encap_tokens(&self, tokens: Vec<Item>, info: &TemplateInfo) -> Vec<Item> {
        let mut out = vec![self.encapsulation_info_start()];
        out.extend(tokens);
        out.push(self.encapsulation_info_end());

        if let Item::Tok(ParsoidToken::SelfclosingTag(meta)) = &mut out[0] {
            meta.data_parsoid.src = self.token_src.clone();
            meta.data_parsoid.colon = self.token_colon.clone();

            // Serialize the template info as the data-mw `parts` envelope
            // (mirrors `DataMw::toJsonArray` + `TemplateInfo::toJsonArray`).
            let data_mw = serialize_data_mw(info);
            if !data_mw.is_empty() {
                meta.attribs.push(string_kv("data-mw", &data_mw));
            }

            // Preserve the *rich* parameter list (`named`/`spc`) for the
            // `DOMRangeBuilder` `pi` build step (mirrors PHP's
            // `TempData->tplarginfo`, a serialized `TemplateInfo`). The
            // `data-mw.parts` editor form drops `named`/`spc`, so this is the
            // only place they survive to data-parsoid.pi.
            let tplarginfo = serialize_param_infos(&info.param_infos);
            if !tplarginfo.is_empty() {
                meta.data_parsoid.tmp.tplarginfo = Some(tplarginfo);
            }
        }

        out
    }
}

/// Build a TemplateInfo from a resolved target name/kind, mirroring
/// `getTemplateInfo`'s func/href population.
pub fn template_info_from(
    func: Option<&str>,
    href: Option<&str>,
    param_infos: Vec<ParamInfo>,
) -> TemplateInfo {
    TemplateInfo {
        target_wt: None,
        func: func.map(|s| s.to_string()),
        href: href.map(|s| s.to_string()),
        resolved_title: None,
        resolved_rev_id: None,
        param_infos,
        ty: if func.is_some() {
            Some("old-parserfunction".to_string())
        } else {
            None
        },
    }
}

fn string_kv(key: &str, value: &str) -> KV {
    KV {
        key: KeyValue::Str(key.to_string()),
        value: KeyValue::Str(value.to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    }
}

/// Serialize a template's ordered parameter list to the `data-parsoid.pi` inner
/// array form (a `list<ParamInfo>` → `[{k, named?, spc?}]`). Faithful to PHP
/// `ParamInfo::toJsonArray` (T404772): only `k`, `named`, and `spc` are kept;
/// the temporary value-wikitext/HTML are dropped (they live in `data-mw.parts`).
pub fn serialize_param_infos(param_infos: &[ParamInfo]) -> String {
    let arr: Vec<serde_json::Value> = param_infos
        .iter()
        .map(|p| {
            let mut obj = serde_json::Map::new();
            obj.insert("k".to_string(), serde_json::Value::String(p.k.clone()));
            if p.named {
                obj.insert("named".to_string(), serde_json::Value::Bool(true));
            }
            if let Some(spc) = &p.spc {
                obj.insert(
                    "spc".to_string(),
                    serde_json::Value::Array(
                        spc.iter()
                            .map(|s| serde_json::Value::String(s.clone()))
                            .collect(),
                    ),
                );
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    serde_json::Value::Array(arr).to_string()
}

/// Serialize a `TemplateInfo` to the JSON object that PHP's
/// `TemplateInfo::toJsonArray` emits (the `target`/`params`/`i` shape).
/// A JSON object whose key order is the insertion order.
///
/// `serde_json`'s own `Map` sorts its keys (`BTreeMap`), but `data-mw` is
/// compared byte-for-byte against Parsoid's output and Parsoid writes
/// `target` before `params` before `i`. A sorting map therefore loses the
/// comparison on every transclusion no matter how correct the values are, so
/// the order has to survive serialization. Values are pre-serialized JSON.
#[derive(Default)]
struct OrderedJson(Vec<(String, String)>);

impl OrderedJson {
    fn put(&mut self, key: &str, value: impl Into<String>) {
        self.0.push((key.to_string(), value.into()));
    }

    fn put_str(&mut self, key: &str, value: &str) {
        self.put(key, json_string(value));
    }

    fn put_opt_str(&mut self, key: &str, value: Option<&str>) {
        match value {
            Some(v) => self.put_str(key, v),
            // Absent wikitext serialises as null, as PHP's `?string` default does.
            None => self.put(key, "null"),
        }
    }

    /// Write `key` only when there is a value, leaving it out otherwise — for
    /// fields whose absence the live service expresses by omission rather than by
    /// an explicit `null` (`href` on a parser function, which has no page).
    fn put_some_str(&mut self, key: &str, value: Option<&str>) {
        if let Some(v) = value {
            self.put_str(key, v);
        }
    }

    fn finish(self) -> String {
        let body: Vec<String> = self
            .0
            .into_iter()
            .map(|(k, v)| format!("{}:{v}", json_string(&k)))
            .collect();
        format!("{{{}}}", body.join(","))
    }
}

/// A JSON string literal for `s`.
///
/// `serde_json` alone is enough *here*: the `&`/`<`/`'` that a `data-mw`
/// attribute needs are applied once, when the attribute is written
/// ([`crate::html::serialize`]), because escaping at both points would double
/// them. In particular the nested HTML inside a `data-mw` `attribs[].html`
/// field already carries its own escaping from the time it was built, and that
/// must not be escaped again either.
fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Serialize a `TemplateInfo` as the inner object of a `data-mw` part.
pub fn serialize_template_info(info: &TemplateInfo) -> String {
    // An `old-parserfunction` — the v2 shape, which is what a wiki with
    // `ParsoidExperimentalParserFunctionOutput` off serves, i.e. enwiki — folds
    // the *first* argument onto `target.wt`, after a colon, and renumbers the
    // survivors positionally. Mirrors the `array_shift` + `renumberParamInfos`
    // pair in `TemplateInfo::toJsonArray`:
    //
    //     a {{#if:foo|bar|baz}} b
    //     → target.wt = "#if:foo", params = {1:bar, 2:baz}
    //
    // The v3 shape (`parserfunction`) keeps every parameter and writes the
    // function as `target.key` instead. Both shapes are exercised by
    // `fixtures/v3ParserFunctions.txt`.
    //
    // `info.target_wt` is the part *before* the colon (`"#if"`) for a parser
    // function, so the fold appends the colon and the argument. Appending to a
    // `target_wt` that already carries the colon gives `"#if:foo:foo"`, and
    // reading that double-append as "the fold must not apply here" led to a
    // version that dropped the first argument of every call instead.
    // The fold applies to an `old-parserfunction` whose `target.wt` does *not*
    // already carry the colon argument — i.e. one the tokenizer split into
    // ``target` + first parameter. A call whose `target.wt` is already
    // `"#if:1"` keeps every parameter, which is what the live service serves.
    // The two spellings really do occur: `{{#if:1|yes|no}}` reaches here with the
    // colon argument in the target, while the fixture's `{{#if:foo|bar|baz}}`
    // reaches it as parameter 1.
    let is_v2 = info.ty.as_deref() != Some("parserfunction");
    let folds_first_arg =
        is_v2 && info.func.is_some() && !info.target_wt.as_deref().unwrap_or("").contains(':');
    let wt = if folds_first_arg {
        match info.param_infos.first() {
            Some(first) => {
                let mut wt = info.target_wt.clone().unwrap_or_default();
                wt.push(':');
                wt.push_str(&first.value_wt);
                wt
            }
            None => info.target_wt.clone().unwrap_or_default(),
        }
    } else {
        info.target_wt.clone().unwrap_or_default()
    };

    // `target` first: Parsoid's order, verified against the live service.
    let mut target = OrderedJson::default();
    target.put_opt_str("wt", Some(&wt));
    if let Some(func) = &info.func {
        if info.ty.as_deref() == Some("parserfunction") {
            target.put_str("key", func);
        } else {
            target.put_str("function", func);
        }
    }
    // `href` is omitted, not nulled, when there is none: a parser function has no
    // page to link to, and the live service writes no `href` at all.
    target.put_some_str("href", info.href.as_deref());

    // Params object (preserve PHP's disambiguating "=N=key" for duplicate keys).
    let mut params = OrderedJson::default();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut count = 0usize;
    for (idx, param) in info.param_infos.iter().enumerate() {
        // The folded first argument is already on `wt`; it is dropped from
        // `params` and the rest are renumbered from 1, positionally.
        if folds_first_arg && idx == 0 {
            continue;
        }
        count += 1;
        let (k, named) = if folds_first_arg {
            (count.to_string(), false)
        } else {
            (param.k.clone(), param.named)
        };
        let mut key = k;
        if seen.contains_key(&key) {
            key = format!("={count}={key}");
        }
        seen.insert(key.clone(), count);

        let mut value = OrderedJson::default();
        value.put_opt_str(
            "wt",
            if param.value_wt.is_empty() {
                None
            } else {
                Some(&param.value_wt)
            },
        );
        if let Some(html) = &param.html {
            value.put_str("html", html);
        }
        if let Some(key_wt) = &param.key_wt {
            let mut key_obj = OrderedJson::default();
            key_obj.put_str("wt", key_wt);
            value.put("key", key_obj.finish());
        }
        // For parser-function params, emit `eq` (named-ness) and `order`
        // deviations from defaults (mirrors TemplateInfo::toJsonArray). The
        // defaults are `eq = !isNumeric` and `order = isNumeric ? k : null`, so
        // a key is written only when the param *departs* from its default —
        // which is what keeps `{"1":{"wt":"a"}}` free of both.
        if info.ty.as_deref() == Some("parserfunction") {
            let is_numeric = param.is_numeric_key();
            if named == is_numeric {
                value.put("eq", if named { "true" } else { "false" });
            }
            let order = count;
            let default_order = if is_numeric {
                param.k.parse::<usize>().ok()
            } else {
                None
            };
            if default_order != Some(order) {
                value.put("order", order.to_string());
            }
        }
        params.put(&key, value.finish());
    }

    let mut out = OrderedJson::default();
    out.put("target", target.finish());
    out.put("params", params.finish());
    // `i` is the part index, zero for a single part. Parsoid emits it last.
    out.put("i", "0");
    out.finish()
}

/// Serialize a `TemplateInfo` into the full `data-mw` envelope that
/// Parsoid stores on a transclusion/param marker, mirroring PHP's
/// `DataMw::toJsonArray` legacy `parts` encoding, i.e.
/// `{"parts": [{"<type>": <TemplateInfo>}]}` where `<type>` is one of
/// `template`, `parserfunction`, or `templatearg`.
///
/// `old-parserfunction` serializes under the `template` key: PHP's
/// `DataMw::toJsonArray` normalizes exactly that one name. A genuine
/// `parserfunction` keeps the `parserfunction` key — that is the v3 shape, and
/// it is what `ParsoidExperimentalParserFunctionOutput=true` turns on.
///
/// Both are reachable on enwiki, which is *why* the `ty` distinction has to
/// survive all the way to serialization: the fixture `v3ParserFunctions.txt`
/// pins the v3 output, and the served HTML pins the v2 one. The `parts` key and
/// the function's field name (`key` vs `function`) move together.
pub fn serialize_data_mw(info: &TemplateInfo) -> String {
    let type_key = match info.ty.as_deref() {
        Some("parserfunction") => "parserfunction",
        Some("templatearg") => "templatearg",
        Some(_) | None => "template",
    };
    // Assembled textually rather than through `serde_json::Value`: a
    // `serde_json` object sorts its keys, which would undo the ordering
    // [`serialize_template_info`] is careful to preserve.
    let part = format!("\"{}\":{}", type_key, serialize_template_info(info));
    format!("{{\"parts\":[{{{part}}}]}}")
}

/// Split the first colon-delimited argument from `params[0]` (for old
/// parser functions like `{{#if:x|...}}`). Mirrors
/// `TemplateEncapsulator::adjustParserFunctionArg0` for the string-KV subset.
///
/// Returns the adjusted params and an optional colon string.
pub fn adjust_parser_function_arg0(
    params: &crate::pipeline::parser_functions::Params,
) -> (crate::pipeline::parser_functions::Params, Option<String>) {
    use crate::wikitext::token_utils::key_value_to_string;

    // The first arg's key is the full target, e.g. "#if:x". Split on the
    // first ':' or fullwidth '：'.
    let Some(first) = params.args.first() else {
        return (params.clone(), None);
    };
    let key = key_value_to_string(&first.key);
    let colon_pos = key.find([':', '：']);
    let Some(pos) = colon_pos else {
        return (params.clone(), None);
    };
    let colon = key[pos..pos + 1].to_string();
    let name = key[..pos].to_string();
    let arg0 = key[pos + 1..].to_string();
    let src_offsets = first.src_offsets.clone();

    let mut new_args = Vec::with_capacity(params.args.len() + 1);
    // Replace args[0] with [name, arg0], then append args[1..].
    new_args.push(crate::wikitext::tokens_v2::KV {
        key: KeyValue::Str(name),
        value: KeyValue::Str(String::new()),
        src_offsets,
        ksrc: None,
        vsrc: None,
    });
    new_args.push(crate::wikitext::tokens_v2::KV {
        key: KeyValue::Str(String::new()),
        value: KeyValue::Str(arg0),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });
    new_args.extend(params.args.iter().skip(1).cloned());

    (
        crate::pipeline::parser_functions::Params::new(new_args),
        Some(colon),
    )
}

/// Prepare `ParamInfo` for a parser function transclusion. Mirrors
/// `TemplateEncapsulator::preparePfParamInfos` for string-valued args (no
/// source offsets are available yet).
/// Prepare `ParamInfo` for a parser-function call.
///
/// Like [`prepare_tpl_param_infos`], the recorded wikitext comes from the
/// argument's `srcOffsets` — the source range — rather than from stringifying
/// the argument tokens. `key_value_to_string` has no arm that re-emits an HTML
/// tag, so an argument such as `{{#ifeq:1|0|y|<div>hi</div>}}` recorded its
/// fourth argument as `hi`, and the rendered branch lost the `<div>` entirely.
/// Parsoid keeps the tags in `data-mw` and renders the element.
pub fn prepare_pf_param_infos(
    target_wt: &str,
    params: &crate::pipeline::parser_functions::Params,
    source: &str,
) -> Vec<ParamInfo> {
    use crate::wikitext::token_utils::key_value_to_string;

    let mut out = Vec::new();
    let mut arg_index = 1usize;

    // The colon argument is part of `target_wt`, but whether it is *also* a
    // parameter depends on how the tokenizer split the call — and getting that
    // wrong shifts every later parameter by one, which is a `data-mw`
    // difference on every parser function.
    //
    // It is already a parameter exactly when `params[0].key` holds the whole
    // `"#name:arg"`, which is what `{{#ifeq:x|y|…}}` produces: `args[1..]` are
    // then the branches and the loop below numbers them from 1, while the colon
    // argument is already represented by `target.wt` and must **not** be emitted
    // again. When the two were split apart the colon argument is absent from
    // `params` entirely, and it becomes parameter 1.
    let colon_arg_is_a_param = params
        .args
        .first()
        .map(|kv| key_value_to_string(&kv.key))
        .is_some_and(|k| k.contains(':'));
    if !colon_arg_is_a_param && let Some(pos) = target_wt.find([':', '：']) {
        let arg0 = &target_wt[pos + 1..];
        let mut info = ParamInfo::new(arg_index.to_string());
        info.value_wt = arg0.to_string();
        out.push(info);
        arg_index += 1;
    }

    // params[0] was the target; iterate params[1..].
    for param in params.args.iter().skip(1) {
        let k = key_value_to_string(&param.key);
        // Prefer the source range, as the template path does; fall back to the
        // stringified tokens only when the range is unavailable.
        let v = match &param.src_offsets {
            Some(so) => strip_include_directives(so.value_substr(source)),
            None => key_value_to_string(&param.value),
        };
        let mut info = ParamInfo::new(arg_index.to_string());
        info.value_wt = if k.is_empty() { v } else { format!("{k}={v}") };
        out.push(info);
        arg_index += 1;
    }

    out
}

/// Prepare `ParamInfo` for a template transclusion. Faithful port of
/// `TemplateEncapsulator::prepareTplParamInfos`.
///
/// The name and value wikitext come from the argument's `srcOffsets` — i.e.
/// straight out of the source — *not* from stringifying the argument tokens.
/// That matters: `TokenUtils::tokensToString` has no arm for a
/// `wikilink`/`extlink` token, so a link argument would otherwise serialize to
/// an empty `wt` (and an empty `data-mw` parameter).
///
/// `source` is the ambient wikitext, used only for arguments whose range carries
/// no source of its own.
/// Strip `<includeonly>`/`<noinclude>`/`<onlyinclude>` directives from an
/// argument's wikitext, as MediaWiki's preprocessor does before recording it.
///
/// A directive can appear in an argument value — `{{#if:1|<includeonly>x</includeonly>y}}`
/// — and the live wiki records the parameter as `y` with the directive gone
/// entirely, not with it kept as text. Leaving it in puts raw `<includeonly>`
/// markup inside the `data-mw` attribute, where it both differs from the wiki
/// and *corrupts the attribute*, because an unescaped `<`/`>` inside a
/// single-quoted JSON string ends the attribute early and the rest of the page
/// is then read as markup. That is how `Template:Short description` came to
/// shatter: its body's `<includeonly>` block leaked into a parameter and the
/// document after it was re-read as literal `{{#ifeq:…}}` text.
///
/// `<includeonly>` drops its contents *and* its tags: the value is stringified
/// outside a transclusion context, and the wiki records `{{#if:1|<includeonly>x</includeonly>y}}`
/// as `y`. `<noinclude>` keeps its contents and drops the tags (`xy`), and
/// `<onlyinclude>` behaves like `<noinclude>` in an argument value.
fn strip_include_directives(text: &str) -> String {
    let mut out = text.to_string();
    // `<includeonly>…</includeonly>` goes whole, contents included. A stray
    // opening or closing tag with no partner is dropped on its own, so a
    // malformed directive cannot leave half of it in a `data-mw` attribute.
    while let Some(start) = out.find("<includeonly>") {
        let after = start + "<includeonly>".len();
        match out[after..].find("</includeonly>") {
            Some(end) => out.replace_range(start..after + end + "</includeonly>".len(), ""),
            None => {
                out.replace_range(start..after, "");
                break;
            }
        }
    }
    out.replace("</includeonly>", "")
        .replace("<noinclude>", "")
        .replace("</noinclude>", "")
        .replace("<onlyinclude>", "")
        .replace("</onlyinclude>", "")
}

/// Build `ParamInfo`s for a template call from the source ranges of its
/// arguments, so `data-mw` records the wikitext as written.
pub fn prepare_tpl_param_infos(
    params: &crate::pipeline::parser_functions::Params,
    source: &str,
) -> Vec<ParamInfo> {
    use crate::wikitext::token_utils::key_value_to_string;

    let mut out = Vec::new();
    let mut arg_index = 1usize;

    // Ignore params[0] (the template name).
    for param in params.args.iter().skip(1) {
        let (k_src, v_src) = match &param.src_offsets {
            Some(so) => (
                so.key_substr(source).to_string(),
                so.value_substr(source).to_string(),
            ),
            None => (
                key_value_to_string(&param.key),
                key_value_to_string(&param.value),
            ),
        };
        let v_src = strip_include_directives(&v_src);
        let k_wt = k_src.trim().to_string();

        // `TokenUtils::tokensToString` returns a string; only when it cannot (the
        // argument name is a non-string token array) does PHP fall back to the
        // original source text `$kWt` (which it has already trimmed).
        let k = match &param.key {
            KeyValue::Str(s) => s.trim().to_string(),
            KeyValue::Tokens(_) => k_wt.clone(),
        };
        let mut v = v_src.clone();
        // Even an empty `k` stays positional only when the value directly follows
        // the key; otherwise it is a blank *named* parameter (which is valid).
        let is_positional = k.is_empty()
            && param
                .src_offsets
                .as_ref()
                .is_some_and(|so| so.key_end == so.value_start);
        let mut info = if is_positional {
            let info = ParamInfo::new(arg_index.to_string());
            arg_index += 1;
            info
        } else {
            // Named parameters get their value whitespace stripped.
            v = v.trim().to_string();
            let mut info = ParamInfo::new(k.clone());
            info.named = true;
            info
        };
        info.value_wt = v;

        // Only add the original parameter wikitext when named and different from
        // the actual parameter.
        if info.named && k_wt != info.k {
            info.key_wt = Some(k_wt);
        }
        out.push(info);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::TagTk;

    fn template_token() -> ParsoidToken {
        let mut tk = TagTk::new("template", vec![], DataParsoid::default());
        tk.data_parsoid.tsr = Some(SourceRange::new(0, 7));
        tk.data_parsoid.src = Some("{{Foo}}".to_string());
        ParsoidToken::Tag(tk)
    }

    #[test]
    fn test_encapsulation_markers() {
        let token = template_token();
        let encap = TemplateEncapsulator::new("mw:Transclusion", "#mwt1".to_string(), &token);

        let info = TemplateInfo::default();
        let out = encap.encap_tokens(vec![Item::Str("content".to_string())], &info);

        // Start marker: <meta typeof=mw:Transclusion about=#mwt1>.
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "meta"));
        if let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &out[0] {
            let type_of = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(type_of, Some("mw:Transclusion"));
        }

        // End marker: <meta typeof=mw:Transclusion/End>.
        assert!(
            matches!(out.last(), Some(Item::Tok(ParsoidToken::SelfclosingTag(t))) if t.name == "meta")
        );
        if let Some(Item::Tok(ParsoidToken::SelfclosingTag(t))) = out.last() {
            let type_of = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(type_of, Some("mw:Transclusion/End"));
        }

        // Content is wrapped.
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Str(s) if s == "content"))
        );
    }

    #[test]
    fn test_adjust_parser_function_arg0() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        let params = crate::pipeline::parser_functions::Params::new(vec![KV {
            key: KeyValue::Str("#if:x".to_string()),
            value: KeyValue::Str(String::new()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        }]);

        let (adjusted, colon) = adjust_parser_function_arg0(&params);
        assert_eq!(colon.as_deref(), Some(":"));
        assert_eq!(adjusted.args[0].key.as_str(), Some("#if"));
        assert_eq!(adjusted.args[1].value.as_str(), Some("x"));
    }

    #[test]
    fn test_prepare_pf_param_infos() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        let params = crate::pipeline::parser_functions::Params::new(vec![
            KV {
                key: KeyValue::Str("#if:x".to_string()),
                value: KeyValue::Str(String::new()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str(String::new()),
                value: KeyValue::Str("yes".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ]);

        let infos = prepare_pf_param_infos("#if:x", &params, "");
        // Only `yes`: the colon argument `x` is already represented by
        // `target.wt`, so it is not a parameter. Verified against the live
        // service, which records `{{#if:x|yes}}` as `params:{"1":{"wt":"yes"}}`.
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].k, "1");
        assert_eq!(infos[0].value_wt, "yes");
    }

    /// A parser-function argument carrying an HTML tag must record the tag, not
    /// just the text: `data-mw` is what a round-trip reconstructs the source
    /// from, and `{{#ifeq:1|0|y|<div>hi</div>}}` has to keep its `wt` intact.
    #[test]
    fn test_prepare_pf_param_infos_keeps_markup() {
        use crate::wikitext::tokens_v2::{KV, KVSourceRange, KeyValue};

        // `#ifeq:1|0|y|<div>hi</div>` — the fourth argument is the div.
        let src = "#ifeq:1|0|y|<div>hi</div>";
        let value_start = src.find("<div>").unwrap();
        let params = crate::pipeline::parser_functions::Params::new(vec![
            KV {
                key: KeyValue::Str("#ifeq:1".to_string()),
                value: KeyValue::Str(String::new()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str(String::new()),
                value: KeyValue::Tokens(vec![]),
                src_offsets: Some(KVSourceRange {
                    key_start: value_start,
                    key_end: value_start,
                    value_start,
                    value_end: src.len(),
                    source: None,
                }),
                ksrc: None,
                vsrc: None,
            },
        ]);

        let infos = prepare_pf_param_infos("#ifeq:1", &params, src);
        // One parameter, the div: the colon argument `1` is already in
        // `target.wt`, so it is not repeated here (see
        // `test_prepare_pf_param_infos`).
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].value_wt, "<div>hi</div>");
    }

    #[test]
    fn test_prepare_tpl_param_infos() {
        use crate::wikitext::tokens_v2::{KV, KVSourceRange, KeyValue};

        // `Foo |pos| name = value ` — the second argument is positional (its key
        // range is empty and abuts the value), the third is named.
        let src = "Foo|pos| name = value ";
        let params = crate::pipeline::parser_functions::Params::new(vec![
            KV {
                key: KeyValue::Str("Foo".to_string()),
                value: KeyValue::Str(String::new()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str(String::new()),
                value: KeyValue::Str("pos".to_string()),
                src_offsets: Some(KVSourceRange {
                    key_start: 4,
                    key_end: 4,
                    value_start: 4,
                    value_end: 7,
                    source: None,
                }),
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str(" name ".to_string()),
                value: KeyValue::Str(" value ".to_string()),
                src_offsets: Some(KVSourceRange {
                    key_start: 8,
                    key_end: 14,
                    value_start: 15,
                    value_end: 21,
                    source: None,
                }),
                ksrc: None,
                vsrc: None,
            },
        ]);

        let infos = prepare_tpl_param_infos(&params, src);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].k, "1");
        assert_eq!(infos[0].value_wt, "pos");
        assert!(!infos[0].named);
        assert_eq!(infos[1].k, "name");
        assert_eq!(infos[1].value_wt, "value");
        assert!(infos[1].named);
    }

    #[test]
    fn test_prepare_tpl_param_infos_without_source_offsets() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        // Without `srcOffsets` PHP cannot prove the value abuts the key, so the
        // argument is *named* (with an empty key) and its value is trimmed.
        let params = crate::pipeline::parser_functions::Params::new(vec![
            KV {
                key: KeyValue::Str("Foo".to_string()),
                value: KeyValue::Str(String::new()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str(String::new()),
                value: KeyValue::Str(" pos ".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ]);

        let infos = prepare_tpl_param_infos(&params, "");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].k, "");
        assert_eq!(infos[0].value_wt, "pos");
        assert!(infos[0].named);
    }

    #[test]
    fn test_prepare_tpl_param_infos_uses_source_ranges() {
        use crate::wikitext::tokens_v2::{KV, KVSourceRange, KeyValue};

        // `{{1x|[[http://e.com |123]]}}`: a single positional argument whose
        // token array holds a `wikilink`. `tokensToString` has no arm for that,
        // so the `valueWt` must come from the source span — exactly as PHP's
        // `prepareTplParamInfos` reads `$srcOffsets->value->substr($src)`.
        let src = "1x|[[http://e.com |123]]";
        let params = crate::pipeline::parser_functions::Params::new(vec![
            KV {
                key: KeyValue::Str("1x".to_string()),
                value: KeyValue::Str(String::new()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str(String::new()),
                value: KeyValue::Tokens(vec![]),
                src_offsets: Some(KVSourceRange {
                    key_start: 3,
                    key_end: 3,
                    value_start: 3,
                    value_end: 26,
                    source: None,
                }),
                ksrc: None,
                vsrc: None,
            },
        ]);

        let infos = prepare_tpl_param_infos(&params, src);
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].k, "1");
        assert!(!infos[0].named);
        assert_eq!(infos[0].value_wt, "[[http://e.com |123]]");
    }

    #[test]
    fn test_serialize_template_info() {
        let mut info = template_info_from(Some("if"), None, vec![]);
        info.target_wt = Some("#if:x".to_string());

        let json = serialize_template_info(&info);
        assert!(json.contains("\"function\":\"if\""));
        assert!(json.contains("\"wt\":\"#if:x\""));
    }

    #[test]
    fn test_serialize_data_mw() {
        let mut info = template_info_from(None, Some("Template:Foo"), vec![]);
        info.target_wt = Some("Foo".to_string());

        let json = serialize_data_mw(&info);
        // The data-mw envelope wraps the TemplateInfo in `parts` → `template`.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert!(parsed.get("parts").and_then(|p| p.get(0)).is_some());
        assert!(parsed["parts"][0].get("template").is_some());
        assert_eq!(parsed["parts"][0]["template"]["target"]["wt"], "Foo");
    }

    #[test]
    fn test_serialize_data_mw_parserfunction_v3() {
        // A v3 parser function (ty = "parserfunction") uses the "parserfunction"
        // parts key and writes the func as `target.key`, keeping every parameter
        // (mirrors `TemplateInfo::toJsonArray`). PHP's `DataMw::toJsonArray`
        // normalizes only `old-parserfunction` to `template`.
        //
        // This is the shape `ParsoidExperimentalParserFunctionOutput=true` turns
        // on, and the tree builder reads the `parserfunction` key to add the
        // `mw:ParserFunction/<name>` typeof — so both the key and the field name
        // are load-bearing, on a config a real wiki can set.
        let mut info = template_info_from(Some("if"), None, vec![]);
        info.ty = Some("parserfunction".to_string());
        info.target_wt = Some("#if".to_string());
        let mut first = ParamInfo::new("1");
        first.value_wt = "foo".to_string();
        info.param_infos = vec![first];

        let json = serialize_data_mw(&info);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert!(parsed["parts"][0].get("parserfunction").is_some());
        assert!(parsed["parts"][0].get("template").is_none());
        assert_eq!(parsed["parts"][0]["parserfunction"]["target"]["key"], "if");
        assert_eq!(parsed["parts"][0]["parserfunction"]["target"]["wt"], "#if");
        assert_eq!(
            parsed["parts"][0]["parserfunction"]["params"]["1"]["wt"],
            "foo"
        );
    }

    /// An `old-parserfunction` whose target carries no colon folds its first
    /// argument onto `target.wt` and renumbers the rest. The fixture
    /// `v3ParserFunctions.txt` pins the exact `data-mw` string.
    #[test]
    fn test_serialize_data_mw_v2_folds_the_colon_argument() {
        let mut info = template_info_from(Some("if"), None, vec![]);
        info.ty = Some("old-parserfunction".to_string());
        info.target_wt = Some("#if".to_string());
        info.param_infos = ["foo", "bar", "baz"]
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut p = ParamInfo::new((i + 1).to_string());
                p.value_wt = v.to_string();
                p
            })
            .collect();

        let json = serialize_data_mw(&info);
        assert_eq!(
            json,
            r##"{"parts":[{"template":{"target":{"wt":"#if:foo","function":"if"},"params":{"1":{"wt":"bar"},"2":{"wt":"baz"}},"i":0}}]}"##
        );
    }

    /// A call whose target already carries the colon argument is *not* folded:
    /// the live service serves `{{#if:1|yes|no}}` with `wt = "#if:1"` and both
    /// parameters intact.
    #[test]
    fn test_serialize_data_mw_a_colon_in_the_target_is_not_folded_again() {
        let mut info = template_info_from(Some("if"), None, vec![]);
        info.ty = Some("old-parserfunction".to_string());
        info.target_wt = Some("#if:1".to_string());
        info.param_infos = ["yes", "no"]
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut p = ParamInfo::new((i + 1).to_string());
                p.value_wt = v.to_string();
                p
            })
            .collect();

        let json = serialize_data_mw(&info);
        assert_eq!(
            json,
            r##"{"parts":[{"template":{"target":{"wt":"#if:1","function":"if"},"params":{"1":{"wt":"yes"},"2":{"wt":"no"}},"i":0}}]}"##
        );
    }

    #[test]
    fn test_serialize_data_mw_parserfunction_v2() {
        // An old parser function (ty = "old-parserfunction") maps back to the
        // "template" parts key with `func` set (mirrors `DataMw::toJsonArray`).
        let mut info = template_info_from(Some("if"), None, vec![]);
        info.ty = Some("old-parserfunction".to_string());
        info.target_wt = Some("#if:foo".to_string());

        let json = serialize_data_mw(&info);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert!(parsed["parts"][0].get("template").is_some());
        assert!(parsed["parts"][0].get("parserfunction").is_none());
        assert_eq!(parsed["parts"][0]["template"]["target"]["function"], "if");
    }

    #[test]
    fn test_serialize_param_infos() {
        // A named parameter serializes to `{k, named}` (the `pi` inner form).
        let mut named = ParamInfo::new("1");
        named.named = true;
        let mut positional = ParamInfo::new("1");
        positional.named = false;

        let json = serialize_param_infos(&[named, positional]);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr[0]["k"], "1");
        assert_eq!(arr[0]["named"], true);
        // Unnamed (positional) params omit `named`, per ParamInfo::toJsonArray.
        assert_eq!(arr[1]["k"], "1");
        assert!(arr[1].get("named").is_none());
    }

    #[test]
    fn test_encap_tokens_stores_tplarginfo() {
        // A templated token records its rich parameter list as `tmp.tplarginfo`
        // (for the DOMRangeBuilder `pi` build), in addition to `data-mw`.
        let token = template_token();
        let encap = TemplateEncapsulator::new("mw:Transclusion", "#mwt1".to_string(), &token);

        let mut info = TemplateInfo::default();
        let mut p = ParamInfo::new("1");
        p.named = true;
        p.value_wt = "v".to_string();
        info.param_infos = vec![p];

        let out = encap.encap_tokens(vec![Item::Str("v".to_string())], &info);
        if let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &out[0] {
            assert_eq!(
                t.data_parsoid.tmp.tplarginfo.as_deref(),
                Some("[{\"k\":\"1\",\"named\":true}]")
            );
        } else {
            panic!("expected start meta");
        }
    }
}
