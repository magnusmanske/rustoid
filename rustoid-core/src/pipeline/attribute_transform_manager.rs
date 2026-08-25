//! AttributeTransformManager — faithful port of PHP Parsoid's
//! `src/Wt2Html/TT/AttributeTransformManager.php`.
//!
//! Expands the keys and values of a token's attribute list when they are not
//! plain strings (e.g. templated attribute names/values). Each expanded
//! key/value is run through the frame's `expand` (which substitutes `{{{...}}}`
//! references and, when wired, expands templates).

use crate::wikitext::tokens_v2::{Item, KV, KeyValue};

use super::frame::Frame;

/// Expand both the key and value of all key/value pairs. Mirrors
/// `AttributeTransformManager::process`.
///
/// Returns `Some` with the expanded attributes if anything changed, else
/// `None` (signifying the token is unmodified).
pub fn process(
    frame: &Frame,
    _expand_templates: bool,
    _in_template: bool,
    attributes: &[KV],
) -> Option<Vec<KV>> {
    let mut expanded = false;
    let mut result: Vec<KV> = Vec::with_capacity(attributes.len());

    for cur in attributes {
        let k = &cur.key;
        let v = &cur.value;

        // Fast path: string-only key and value leave the attribute unchanged.
        if matches!(k, KeyValue::Str(_)) && matches!(v, KeyValue::Str(_)) {
            result.push(cur.clone());
            continue;
        }

        let expand_k = contains_non_string(k);
        let expand_v = contains_non_string(v);

        let new_k = if expand_k {
            let items = key_value_to_items(k);
            let expanded = frame.expand(&items);
            items_to_key_value(expanded)
        } else {
            k.clone()
        };

        let new_v = if expand_v {
            let items = key_value_to_items(v);
            let expanded = frame.expand(&items);
            items_to_key_value(expanded)
        } else {
            v.clone()
        };

        if expand_k || expand_v {
            expanded = true;
        }

        result.push(KV {
            key: new_k,
            value: new_v,
            src_offsets: cur.src_offsets.clone(),
            ksrc: cur.ksrc.clone(),
            vsrc: cur.vsrc.clone(),
        });
    }

    if expanded { Some(result) } else { None }
}

fn contains_non_string(kv: &KeyValue) -> bool {
    matches!(kv, KeyValue::Tokens(_))
}

/// Convert a `KeyValue` into a flat `Vec<Item>` token chunk.
pub fn key_value_to_items(kv: &KeyValue) -> Vec<Item> {
    match kv {
        KeyValue::Str(s) => vec![Item::Str(s.clone())],
        KeyValue::Tokens(items) => items.clone(),
    }
}

/// Convert a flat token chunk back into a `KeyValue`. A single string becomes
/// a `Str`; otherwise a `Tokens` list.
pub fn items_to_key_value(items: Vec<Item>) -> KeyValue {
    if items.len() == 1
        && let Item::Str(s) = &items[0]
    {
        return KeyValue::Str(s.clone());
    }
    KeyValue::Tokens(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;
    use crate::title::TitleParser;
    use crate::wikitext::tokens_v2::{ParsoidToken, SelfclosingTagTk};

    fn str_kv(key: &str, value: &str) -> KV {
        KV {
            key: KeyValue::Str(key.to_string()),
            value: KeyValue::Str(value.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        }
    }

    #[test]
    fn test_process_plain_strings_noop() {
        let config = MockSiteConfig::new();
        let title = TitleParser::parse("Template:Foo", &config);
        let frame = Frame::new(title, vec![]);

        let attrs = vec![str_kv("style", "color:red")];

        // All strings => no changes => None.
        assert!(process(&frame, true, false, &attrs).is_none());
    }

    #[test]
    fn test_process_expands_templated_value() {
        let config = MockSiteConfig::new();
        let title = TitleParser::parse("Template:Foo", &config);
        // Frame with a single positional arg "red".
        let frame = Frame::new(title, vec![str_kv("", "red")]);

        // A templated value `{{{1}}}` represented as a token KV.
        let mut tplarg = SelfclosingTagTk::new(
            "templatearg",
            vec![],
            crate::wikitext::tokens_v2::DataParsoid::default(),
        );
        tplarg.attribs.push(str_kv("1", ""));

        let attrs = vec![KV {
            key: KeyValue::Str("style".to_string()),
            value: KeyValue::Tokens(vec![Item::Tok(ParsoidToken::SelfclosingTag(tplarg))]),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        }];

        let result = process(&frame, true, false, &attrs);
        let attrs = result.expect("expected expansion");
        assert_eq!(attrs.len(), 1);

        // The expanded value resolves to the string "red".
        assert_eq!(attrs[0].value.as_str(), Some("red"));
    }
}
