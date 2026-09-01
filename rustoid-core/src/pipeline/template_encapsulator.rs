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
pub fn serialize_template_info(info: &TemplateInfo) -> String {
    let mut target = serde_json::Map::new();
    target.insert(
        "wt".to_string(),
        info.target_wt
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
            .unwrap_or(serde_json::Value::Null),
    );
    if let Some(func) = &info.func {
        if info.ty.as_deref() == Some("parserfunction") {
            target.insert("key".to_string(), serde_json::Value::String(func.clone()));
        } else {
            target.insert(
                "function".to_string(),
                serde_json::Value::String(func.clone()),
            );
        }
    }
    if let Some(href) = &info.href {
        target.insert("href".to_string(), serde_json::Value::String(href.clone()));
    }

    // Params object (preserve PHP's disambiguating "=N=key" for duplicate keys).
    let mut params = serde_json::Map::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut count = 0usize;
    for param in &info.param_infos {
        count += 1;
        let mut key = param.k.clone();
        if seen.contains_key(&key) {
            key = format!("={count}={key}");
        }
        seen.insert(param.k.clone(), count);

        let mut value = serde_json::Map::new();
        // Empty wikitext serializes as null (mirrors PHP's `?string` default).
        value.insert(
            "wt".to_string(),
            if param.value_wt.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(param.value_wt.clone())
            },
        );
        if let Some(html) = &param.html {
            value.insert("html".to_string(), serde_json::Value::String(html.clone()));
        }
        if let Some(key_wt) = &param.key_wt {
            let mut key_obj = serde_json::Map::new();
            key_obj.insert("wt".to_string(), serde_json::Value::String(key_wt.clone()));
            value.insert("key".to_string(), serde_json::Value::Object(key_obj));
        }
        // For parser-function params, emit `eq` (named-ness) and `order`
        // deviations from defaults (mirrors TemplateInfo::toJsonArray).
        if info.ty.as_deref() == Some("parserfunction") {
            let is_numeric = param.is_numeric_key();
            if is_numeric == param.named {
                value.insert("eq".to_string(), serde_json::Value::Bool(param.named));
            }
            let order = count;
            let default_order = if is_numeric {
                param.k.parse::<usize>().ok()
            } else {
                None
            };
            if default_order != Some(order) {
                value.insert("order".to_string(), serde_json::Value::from(order));
            }
        }
        params.insert(key, serde_json::Value::Object(value));
    }

    let mut out = serde_json::Map::new();
    out.insert("target".to_string(), serde_json::Value::Object(target));
    out.insert("params".to_string(), serde_json::Value::Object(params));
    serde_json::Value::Object(out).to_string()
}

/// Serialize a `TemplateInfo` into the full `data-mw` envelope that
/// Parsoid stores on a transclusion/param marker, mirroring PHP's
/// `DataMw::toJsonArray` legacy `parts` encoding, i.e.
/// `{"parts": [{"<type>": <TemplateInfo>}]}` where `<type>` is one of
/// `template`, `parserfunction`, or `templatearg`.
///
/// Old parser functions (`ty = "old-parserfunction"`) map back to
/// `"template"` (with `func` set on the inner TemplateInfo), while v3 parser
/// functions (`ty = "parserfunction"`) use the `"parserfunction"` key.
pub fn serialize_data_mw(info: &TemplateInfo) -> String {
    // The parts key is `type`, with `old-parserfunction` normalized to
    // `template` (mirrors `DataMw::toJsonArray`).
    let type_key = match info.ty.as_deref() {
        Some("parserfunction") => "parserfunction",
        Some("templatearg" | "template") | None => "template",
        Some(_) => "template",
    };
    let mut part = serde_json::Map::new();
    let inner = serde_json::from_str::<serde_json::Value>(&serialize_template_info(info))
        .unwrap_or(serde_json::Value::Null);
    part.insert(type_key.to_string(), inner);

    let mut out = serde_json::Map::new();
    out.insert(
        "parts".to_string(),
        serde_json::Value::Array(vec![serde_json::Value::Object(part)]),
    );
    serde_json::Value::Object(out).to_string()
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
pub fn prepare_pf_param_infos(
    target_wt: &str,
    params: &crate::pipeline::parser_functions::Params,
) -> Vec<ParamInfo> {
    use crate::wikitext::token_utils::key_value_to_string;

    let mut out = Vec::new();
    let mut arg_index = 1usize;

    // Split the colon-separated first argument from target_wt.
    if let Some(pos) = target_wt.find([':', '：']) {
        let arg0 = &target_wt[pos + 1..];
        let mut info = ParamInfo::new(arg_index.to_string());
        info.value_wt = arg0.to_string();
        out.push(info);
        arg_index += 1;
    }

    // params[0] was the target; iterate params[1..].
    for param in params.args.iter().skip(1) {
        let k = key_value_to_string(&param.key);
        let v = key_value_to_string(&param.value);
        let mut info = ParamInfo::new(arg_index.to_string());
        info.value_wt = if k.is_empty() { v } else { format!("{k}={v}") };
        out.push(info);
        arg_index += 1;
    }

    out
}

/// Prepare `ParamInfo` for a template transclusion. Mirrors
/// `TemplateEncapsulator::prepareTplParamInfos` for string-valued args.
pub fn prepare_tpl_param_infos(
    params: &crate::pipeline::parser_functions::Params,
) -> Vec<ParamInfo> {
    use crate::wikitext::token_utils::key_value_to_string;

    let mut out = Vec::new();
    let mut arg_index = 1usize;

    // Ignore params[0] (the template name).
    for param in params.args.iter().skip(1) {
        let k = key_value_to_string(&param.key).trim().to_string();
        let v = key_value_to_string(&param.value);

        let mut info = if k.is_empty() {
            let mut info = ParamInfo::new(arg_index.to_string());
            arg_index += 1;
            info.value_wt = v;
            info
        } else {
            let mut info = ParamInfo::new(k.clone());
            info.named = true;
            info.value_wt = v.trim().to_string();
            info
        };

        // Preserve original key wikitext when it differs.
        if info.named && k != info.k {
            info.key_wt = Some(k);
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

        let infos = prepare_pf_param_infos("#if:x", &params);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].k, "1");
        assert_eq!(infos[0].value_wt, "x");
        assert_eq!(infos[1].k, "2");
        assert_eq!(infos[1].value_wt, "yes");
    }

    #[test]
    fn test_prepare_tpl_param_infos() {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

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
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
            KV {
                key: KeyValue::Str(" name ".to_string()),
                value: KeyValue::Str(" value ".to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            },
        ]);

        let infos = prepare_tpl_param_infos(&params);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].k, "1");
        assert_eq!(infos[0].value_wt, "pos");
        assert!(!infos[0].named);
        assert_eq!(infos[1].k, "name");
        assert_eq!(infos[1].value_wt, "value");
        assert!(infos[1].named);
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
        // parts key (mirrors `DataMw::toJsonArray`), with the func name in
        // `target.key`.
        let mut info = template_info_from(Some("if"), None, vec![]);
        info.ty = Some("parserfunction".to_string());
        info.target_wt = Some("#if:foo".to_string());

        let json = serialize_data_mw(&info);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert!(parsed["parts"][0].get("parserfunction").is_some());
        assert!(parsed["parts"][0].get("template").is_none());
        assert_eq!(parsed["parts"][0]["parserfunction"]["target"]["key"], "if");
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
