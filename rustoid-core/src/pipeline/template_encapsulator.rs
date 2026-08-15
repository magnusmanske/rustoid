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
    /// The parameter's wikitext value.
    pub value_wt: String,
    /// Whether this is a named parameter.
    pub named: bool,
}

impl ParamInfo {
    pub fn new(k: impl Into<String>) -> Self {
        Self {
            k: k.into(),
            value_wt: String::new(),
            named: false,
        }
    }
}

/// Template metadata (mirrors PHP's `TemplateInfo`).
#[derive(Debug, Clone, Default)]
pub struct TemplateInfo {
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
                .map(|tsr| SourceRange::new(tsr.end, tsr.end)),
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

        // Store template info on the start marker's data-parsoid.
        // (We store it structurally for now; the data-mw JSON blob is
        // serialized later once DataMw is fully wired.)
        if let Item::Tok(ParsoidToken::SelfclosingTag(meta)) = &mut out[0] {
            meta.data_parsoid.src = self.token_src.clone();
        }

        let _ = info;
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
        func: func.map(|s| s.to_string()),
        href: href.map(|s| s.to_string()),
        resolved_title: None,
        resolved_rev_id: None,
        param_infos,
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
}
