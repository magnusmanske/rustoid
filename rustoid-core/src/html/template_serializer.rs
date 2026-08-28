//! Template-part serialization — a faithful port of the `TemplateInfo` /
//! `ParamInfo` data model plus `WikitextSerializer::serializeFromParts` (the
//! wikitext reconstruction of a transclusion from its `data-mw.parts`).
//!
//! This is the foundation for `EncapsulatedContentHandler`. It reconstructs the
//! source wikitext of `{{template|...}}`, `{{{templatearg}}}`, and parser
//! functions from the structured `data-mw.parts` JSON, mirroring PHP's
//! `TemplateInfo::newFromJsonArray` / `WikitextSerializer::serializeFromParts`.
//!
//! TemplateData-based parameter reordering (`createParamComparator`) is not yet
//! wired: this codebase's synchronous html2wt path has no data-access layer to
//! fetch template data, so `$tplData` is always `None` (the PHP code path for
//! unedited content, where `data-mw` order is already correct).

use serde_json::Value;

/// A single template/parser-function parameter (port of PHP `ParamInfo`).
#[derive(Debug, Clone, Default)]
pub struct ParamInfo {
    /// The normalized parameter key.
    pub k: String,
    /// Key source wikitext, if different from `k`.
    pub key_wt: Option<String>,
    /// Parameter value source wikitext.
    pub value_wt: Option<String>,
    /// Whether the parameter was written as `name=value`.
    pub named: bool,
    /// Whitespace runs around the key (only from `data-parsoid.pi`).
    pub spc: Option<Vec<String>>,
    /// Rendered (HTML) form of the value, if the wikitext form is absent.
    pub html: Option<String>,
}

impl ParamInfo {
    pub fn new(key: String) -> Self {
        Self {
            k: key,
            ..Self::default()
        }
    }

    /// `ParamInfo::isNumericKey` — a positive integer key (`/^[1-9][0-9]*$/D`).
    pub fn is_numeric_key(&self) -> bool {
        !self.k.is_empty()
            && self.k.bytes().all(|b| b.is_ascii_digit())
            && self.k.as_bytes()[0] != b'0'
    }
}

/// A transclusion/parser-function part (port of PHP `TemplateInfo`).
#[derive(Debug, Clone, Default)]
pub struct TemplateInfo {
    /// Target wikitext (the template name / parser-function sugar).
    pub target_wt: Option<String>,
    /// Parser-function name (for `parserfunction` / `old-parserfunction`).
    pub func: Option<String>,
    /// URL of the resolved target (for templates).
    pub href: Option<String>,
    /// Ordered parameter list.
    pub param_infos: Vec<ParamInfo>,
    /// `template`, `templatearg`, `parserfunction`, or `old-parserfunction`.
    pub type_: Option<String>,
    /// Index into `data-parsoid.pi`.
    pub i: Option<usize>,
}

/// A `data-mw.parts` element: either plain wikitext, or a structured template.
#[derive(Debug, Clone)]
pub enum SrcPart {
    Str(String),
    Template(Box<TemplateInfo>),
}

/// Parse a `data-mw.parts` JSON array into `Vec<SrcPart>`, faithfully to
/// `DOMDataUtils::getDataMw` + `TemplateInfo::newFromJsonArray`. A `None` return
/// means the `parts` array was absent/malformed (so callers fall back to
/// `data-parsoid.src`).
pub fn parse_parts(parts_json: &Value) -> Option<Vec<SrcPart>> {
    let parts = parts_json.as_array()?;
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        if let Some(s) = part.as_str() {
            out.push(SrcPart::Str(s.to_string()));
        } else if let Some(tpl) = part.get("template") {
            out.push(SrcPart::Template(Box::new(template_info_from_json(
                tpl, "template",
            ))));
        } else if let Some(tpl) = part.get("templatearg") {
            out.push(SrcPart::Template(Box::new(template_info_from_json(
                tpl,
                "templatearg",
            ))));
        } else if let Some(tpl) = part.get("parserfunction") {
            out.push(SrcPart::Template(Box::new(template_info_from_json(
                tpl,
                "parserfunction",
            ))));
        }
    }
    Some(out)
}

/// `TemplateInfo::newFromJsonArray` — parse a `data-mw.parts` template object.
fn template_info_from_json(json: &Value, default_type: &str) -> TemplateInfo {
    let target_wt = json
        .get("target")
        .and_then(|t| t.get("wt"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let href = json
        .get("target")
        .and_then(|t| t.get("href"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Parser function: `target.key` or `target.function` (old form).
    let mut old_pf = false;
    let (func, ty): (Option<String>, Option<String>) = if let Some(func) = json
        .get("target")
        .and_then(|t| t.get("key"))
        .and_then(|v| v.as_str())
    {
        (Some(func.to_string()), Some("parserfunction".to_string()))
    } else if let Some(func) = json
        .get("target")
        .and_then(|t| t.get("function"))
        .and_then(|v| v.as_str())
    {
        old_pf = true;
        (
            Some(func.to_string()),
            Some("old-parserfunction".to_string()),
        )
    } else {
        (None, Some(default_type.to_string()))
    };

    // Params: `{ "key": { wt, html, key:{wt}, eq, order } }`.
    let mut param_list: Vec<(usize, ParamInfo)> = Vec::new();
    if let Some(params) = json.get("params").and_then(|p| p.as_object()) {
        for (count, (raw_k, v)) in (1usize..).zip(params) {
            // Strip a leading `=N=` duplicate-key disambiguator.
            let k = strip_dup_disambiguator(raw_k);
            let mut info = ParamInfo::new(k.to_string());
            info.value_wt = v.get("wt").and_then(|x| x.as_str()).map(str::to_string);
            info.html = v.get("html").and_then(|x| x.as_str()).map(str::to_string);
            info.key_wt = v
                .get("key")
                .and_then(|x| x.get("wt"))
                .and_then(|x| x.as_str())
                .map(str::to_string);
            info.named = v
                .get("eq")
                .and_then(|x| x.as_bool())
                .unwrap_or_else(|| !info.is_numeric_key());
            let order = v
                .get("order")
                .and_then(|x| x.as_u64())
                .map(|o| o as usize)
                .unwrap_or_else(|| {
                    if info.is_numeric_key() {
                        k.parse::<usize>().unwrap_or(count)
                    } else {
                        count
                    }
                });
            param_list.push((order, info));
        }
    }
    param_list.sort_by_key(|(order, _)| *order);
    let mut ti = TemplateInfo {
        target_wt,
        func,
        href,
        param_infos: param_list.into_iter().map(|(_, info)| info).collect(),
        type_: ty,
        i: json.get("i").and_then(|v| v.as_u64()).map(|i| i as usize),
    };

    // Backward-compat: split the first arg off an 'old' parser function.
    if old_pf {
        let target_wt = ti.target_wt.clone();
        let split = target_wt
            .as_deref()
            .and_then(|target_wt| target_wt.split_once(':'));
        if let Some((name, arg0)) = split {
            ti.target_wt = Some(name.to_string());
            let mut param0 = ParamInfo::new("1".to_string());
            param0.value_wt = Some(arg0.to_string());
            ti.param_infos.insert(0, param0);
            // Convert any named params to positional (all old-PF args are
            // positional), then renumber.
            for param in &mut ti.param_infos {
                if param.named {
                    param.value_wt = Some(format!(
                        "{}={}",
                        param.key_wt.clone().unwrap_or_default(),
                        param.value_wt.clone().unwrap_or_default()
                    ));
                    param.named = false;
                    param.key_wt = None;
                }
            }
            renumber_param_infos(&mut ti.param_infos);
        }
    }

    ti
}

/// Strip a leading `=N=` duplicate-key disambiguator (PHP's
/// `preg_replace('/^=\d+=/', '', $k)`).
fn strip_dup_disambiguator(k: &str) -> &str {
    let rest = k.strip_prefix('=');
    let Some(rest) = rest else {
        return k;
    };
    let Some((digits, after_eq)) = rest.split_once('=') else {
        return k;
    };
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        after_eq
    } else {
        k
    }
}

/// `TemplateInfo::renumberParamInfos` — renumber all positional params 1..n.
fn renumber_param_infos(param_infos: &mut [ParamInfo]) {
    for (i, param) in param_infos.iter_mut().enumerate() {
        param.k = (i + 1).to_string();
    }
}

/// `WikitextSerializer::serializeFromParts` (with `$tplData` always `None`) —
/// reconstruct wikitext from a `data-mw.parts` structure.
pub fn serialize_from_parts(parts: &[SrcPart]) -> String {
    let mut buf = String::new();
    for part in parts {
        match part {
            SrcPart::Str(s) => buf.push_str(s),
            SrcPart::Template(ti) => buf.push_str(&serialize_part(ti)),
        }
    }
    buf
}

/// `WikitextSerializer::serializePart` (inline format, no `tplData`).
fn serialize_part(ti: &TemplateInfo) -> String {
    let is_parser_function = matches!(
        ti.type_.as_deref(),
        Some("parserfunction") | Some("old-parserfunction")
    );
    let is_template_arg = ti.type_.as_deref() == Some("templatearg");

    let (start, end) = if is_template_arg {
        ("{{{", "}}}")
    } else {
        ("{{", "}}")
    };

    let mut buf = String::new();
    buf.push_str(start);
    buf.push_str(ti.target_wt.as_deref().unwrap_or(""));

    let mut first = true;
    for param in &ti.param_infos {
        // Separator (parser functions use `:` for the first arg).
        let sep = if first && is_parser_function {
            ':'
        } else {
            '|'
        };
        buf.push(sep);

        let serialize_as_named = param.named || param.key_wt.is_some();
        if serialize_as_named {
            let name = param.key_wt.as_deref().unwrap_or(&param.k);
            buf.push_str(name);
            buf.push('=');
            buf.push_str(param.value_wt.as_deref().unwrap_or("").trim());
        } else {
            buf.push_str(param.value_wt.as_deref().unwrap_or(""));
        }
        first = false;
    }

    buf.push_str(end);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_obj(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn test_template_info_basic() {
        // {{Foo|bar|baz=qux}}
        let parts = json_obj(
            r#"{"parts":[{"template":{"target":{"wt":"Foo","href":"./Foo"},"params":{"1":{"wt":"bar"},"baz":{"wt":"qux"}}}}]}"#,
        );
        let parsed = parse_parts(parts.get("parts").unwrap()).unwrap();
        assert_eq!(serialize_from_parts(&parsed), "{{Foo|bar|baz=qux}}");
    }

    #[test]
    fn test_template_arg() {
        // {{{1|default}}}
        let parts = json_obj(
            r#"{"parts":[{"templatearg":{"target":{"wt":"1"},"params":{"1":{"wt":"default"}}}}]}"#,
        );
        let parsed = parse_parts(parts.get("parts").unwrap()).unwrap();
        assert_eq!(serialize_from_parts(&parsed), "{{{1|default}}}");
    }

    #[test]
    fn test_param_info_numeric_key() {
        assert!(ParamInfo::new("1".to_string()).is_numeric_key());
        assert!(ParamInfo::new("123".to_string()).is_numeric_key());
        assert!(!ParamInfo::new("0".to_string()).is_numeric_key());
        assert!(!ParamInfo::new("foo".to_string()).is_numeric_key());
    }

    #[test]
    fn test_strip_dup_disambiguator() {
        assert_eq!(strip_dup_disambiguator("foo"), "foo");
        assert_eq!(strip_dup_disambiguator("=1=foo"), "foo");
        assert_eq!(strip_dup_disambiguator("=x=foo"), "=x=foo");
    }
}
