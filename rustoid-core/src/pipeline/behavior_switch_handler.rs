//! BehaviorSwitchHandler — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/BehaviorSwitchHandler.php`.
//!
//! Converts `behavior-switch` self-closing tokens (e.g. `__TOC__`) into
//! `<meta property="mw:PageProp/...">` tokens.

use crate::wikitext::tokens_v2::{Item, KV, KeyValue, ParsoidToken, SelfclosingTagTk};

/// Map from behavior-switch magic word to an output flag.
/// (Used by the env/metadata subsystem to set parser output flags, not yet
/// ported; kept for parity with PHP's OUTPUT_FLAG_FROM_BEHAVIOR_SWITCH.)
#[allow(dead_code)]
fn output_flag_from_behavior_switch(word: &str) -> Option<&'static str> {
    match word {
        "nogallery" => Some("mw-NoGallery"),
        "newsectionlink" => Some("mw-NewSection"),
        "nonewsectionlink" => Some("mw-HideNewSection"),
        "noeditsection" => Some("no-section-edit-links"),
        _ => None,
    }
}

/// The BehaviorSwitchHandler.
pub struct BehaviorSwitchHandler;

impl BehaviorSwitchHandler {
    /// Run over a token stream, transforming behavior-switch tokens.
    pub fn run(&self, tokens: Vec<Item>) -> Vec<Item> {
        let mut output = Vec::new();
        for token in tokens {
            match &token {
                Item::Tok(ParsoidToken::SelfclosingTag(tk)) if tk.name == "behavior-switch" => {
                    // onTag: behavior-switch → onBehaviorSwitch.
                    let meta = Self::on_behavior_switch(token.clone());
                    output.push(meta);
                }
                _ => output.push(token),
            }
        }
        output
    }

    /// Handle a behavior switch token, returning a meta token.
    fn on_behavior_switch(token: Item) -> Item {
        let magic_word = match &token {
            Item::Tok(ParsoidToken::SelfclosingTag(tk)) => tk
                .attribs
                .first()
                .and_then(|kv| kv.value.as_str())
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        };

        // (In PHP, this records the switch in env/metadata. We drop that state
        // for now since the metadata subsystem isn't ported; the token
        // transformation itself is the essential output.)

        let mut meta = SelfclosingTagTk::new("meta", vec![], Default::default());
        meta.attribs.push(KV {
            key: KeyValue::Str("property".to_string()),
            value: KeyValue::Str(format!("mw:PageProp/{magic_word}")),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });

        // Clone the dataParsoid from the original token.
        if let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = &token {
            meta.data_parsoid = tk.data_parsoid.clone();
        }

        Item::Tok(ParsoidToken::SelfclosingTag(meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::DataParsoid;

    fn behavior_switch(word: &str) -> Item {
        let mut tk = SelfclosingTagTk::new("behavior-switch", vec![], DataParsoid::default());
        tk.add_attribute_str("word", word);
        Item::Tok(ParsoidToken::SelfclosingTag(tk))
    }

    #[test]
    fn test_toc_switch() {
        let handler = BehaviorSwitchHandler;
        let out = handler.run(vec![behavior_switch("toc")]);

        assert_eq!(out.len(), 1);
        match &out[0] {
            Item::Tok(ParsoidToken::SelfclosingTag(tk)) => {
                assert_eq!(tk.name, "meta");
                let property = tk
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("property"))
                    .and_then(|kv| kv.value.as_str());
                assert_eq!(property, Some("mw:PageProp/toc"));
            }
            other => panic!("expected meta token, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_text_passthrough() {
        let handler = BehaviorSwitchHandler;
        let out = handler.run(vec![Item::Str("hello".to_string())]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Item::Str(s) if s == "hello"));
    }
}
