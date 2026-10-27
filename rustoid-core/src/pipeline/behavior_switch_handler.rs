//! BehaviorSwitchHandler — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/BehaviorSwitchHandler.php`.
//!
//! Converts `behavior-switch` self-closing tokens (e.g. `__TOC__`) into
//! `<meta property="mw:PageProp/...">` tokens.

use std::collections::HashMap;

use crate::traits::MagicWordMap;
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

/// The site's behavior-switch aliases, resolved to their canonical names.
///
/// The tokenizer cannot know the site's magic words, so it turns every `__…__`
/// into a `behavior-switch` token; this table is what decides whether the wiki
/// would have done the same. An unknown `__…__` is ordinary text — the service
/// keeps `__1B0X_5H1P__` literal in `Module:Infobox ship`'s sentinel label.
#[derive(Debug, Default)]
pub struct BehaviorSwitchAliases {
    /// Aliases that must match exactly (case-sensitive magic words).
    exact: HashMap<String, String>,
    /// Aliases matched case-insensitively.
    folded: HashMap<String, String>,
}

impl BehaviorSwitchAliases {
    pub fn new(ids: &std::collections::HashSet<String>, magic_words: &MagicWordMap) -> Self {
        let mut aliases = Self::default();
        for canonical in ids {
            let Some(entry) = magic_words.get(canonical) else {
                continue;
            };
            for alias in &entry.aliases {
                aliases.exact.insert(alias.clone(), entry.canonical.clone());
                if !entry.case_sensitive {
                    aliases
                        .folded
                        .insert(alias.to_lowercase(), entry.canonical.clone());
                }
            }
            if entry.aliases.is_empty() {
                aliases
                    .exact
                    .insert(canonical.clone(), entry.canonical.clone());
            }
        }
        aliases
    }

    /// The canonical name for a whole `__…__` token, if it is a behavior switch.
    ///
    /// The site's magic-word aliases keep their underscores (`__NOTOC__`), which
    /// is what PHP's grammar matches; the inner text alone never matches.
    fn canonical(&self, text: &str) -> Option<&str> {
        self.exact
            .get(text)
            .or_else(|| self.folded.get(&text.to_lowercase()))
            .map(String::as_str)
    }
}

/// The site's switch table, or `None` when the site does not report one.
pub fn behavior_switch_aliases(
    config: &dyn crate::traits::SiteConfig,
) -> Option<BehaviorSwitchAliases> {
    config
        .behavior_switch_ids()
        .map(|ids| BehaviorSwitchAliases::new(ids, config.magic_words()))
}

/// The BehaviorSwitchHandler.
pub struct BehaviorSwitchHandler;

impl BehaviorSwitchHandler {
    /// Run over a token stream, transforming behavior-switch tokens.
    ///
    /// `aliases` is the site's switch table; `None` accepts every word (a site
    /// that did not report `doubleunderscores`).
    pub fn run(&self, tokens: Vec<Item>, aliases: Option<&BehaviorSwitchAliases>) -> Vec<Item> {
        let mut output = Vec::with_capacity(tokens.len());
        for token in tokens {
            match &token {
                Item::Tok(ParsoidToken::SelfclosingTag(tk)) if tk.name == "behavior-switch" => {
                    // onTag: behavior-switch → onBehaviorSwitch.
                    match aliases {
                        None => output.push(Self::on_behavior_switch(token, None)),
                        Some(aliases) => {
                            let text = Self::token_text(&token);
                            match aliases.canonical(&text) {
                                Some(canonical) => {
                                    output.push(Self::on_behavior_switch(token, Some(canonical)))
                                }
                                // Not a switch on this wiki: PHP returns the
                                // `__…__` text unchanged.
                                None => output.push(Item::Str(text)),
                            }
                        }
                    }
                }
                _ => output.push(token),
            }
        }
        output
    }

    /// The whole `__…__` token, in its original case.
    fn token_text(token: &Item) -> String {
        let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = token else {
            return String::new();
        };
        if let Some(src) = tk.data_parsoid.src.as_deref() {
            return src.to_string();
        }
        format!(
            "__{}__",
            tk.attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("word"))
                .and_then(|kv| kv.value.as_str())
                .unwrap_or("")
        )
    }

    /// Handle a behavior switch token, returning a meta token.
    fn on_behavior_switch(token: Item, canonical: Option<&str>) -> Item {
        let magic_word = canonical
            .map(str::to_string)
            .unwrap_or_else(|| match &token {
                Item::Tok(ParsoidToken::SelfclosingTag(tk)) => tk
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("word"))
                    .and_then(|kv| kv.value.as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            });

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
    use crate::traits::MagicWordEntry;
    use crate::wikitext::tokens_v2::DataParsoid;

    fn behavior_switch(word: &str) -> Item {
        let mut tk = SelfclosingTagTk::new("behavior-switch", vec![], DataParsoid::default());
        tk.add_attribute_str("word", word);
        Item::Tok(ParsoidToken::SelfclosingTag(tk))
    }

    /// A switch token with the source range the tokenizer records.
    fn behavior_switch_src(src: &str) -> Item {
        let dp = DataParsoid {
            src: Some(src.to_string()),
            ..Default::default()
        };
        let mut tk = SelfclosingTagTk::new("behavior-switch", vec![], dp);
        tk.add_attribute_str("word", src.trim_matches('_').to_lowercase());
        Item::Tok(ParsoidToken::SelfclosingTag(tk))
    }

    fn property(item: &Item) -> Option<&str> {
        let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = item else {
            return None;
        };
        tk.attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("property"))
            .and_then(|kv| kv.value.as_str())
    }

    #[test]
    fn test_toc_switch() {
        let handler = BehaviorSwitchHandler;
        let out = handler.run(vec![behavior_switch("toc")], None);

        assert_eq!(out.len(), 1);
        assert_eq!(property(&out[0]), Some("mw:PageProp/toc"));
    }

    #[test]
    fn test_plain_text_passthrough() {
        let handler = BehaviorSwitchHandler;
        let out = handler.run(vec![Item::Str("hello".to_string())], None);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Item::Str(s) if s == "hello"));
    }

    /// Only the site's switches become metas. `Module:Infobox ship` writes the
    /// sentinel label `__1B0X_5H1P__`, which is not a magic word: the service
    /// keeps it as text, and the module's `gsub` deletes the whole row.
    #[test]
    fn an_unknown_word_stays_text() {
        let mut ids = std::collections::HashSet::new();
        ids.insert("toc".to_string());
        let mut words = MagicWordMap::default();
        words.insert(
            "toc".to_string(),
            MagicWordEntry {
                canonical: "toc".to_string(),
                case_sensitive: false,
                aliases: vec!["__TOC__".to_string()],
            },
        );
        let aliases = BehaviorSwitchAliases::new(&ids, &words);
        let handler = BehaviorSwitchHandler;
        let out = handler.run(
            vec![
                behavior_switch_src("__TOC__"),
                behavior_switch_src("__1B0X_5H1P__"),
            ],
            Some(&aliases),
        );
        assert_eq!(property(&out[0]), Some("mw:PageProp/toc"));
        assert!(matches!(&out[1], Item::Str(s) if s == "__1B0X_5H1P__"));
    }
}
