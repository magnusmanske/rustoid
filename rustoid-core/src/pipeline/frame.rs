//! Frame — faithful port of PHP Parsoid's `src/Wt2Html/Frame.php`
//! (the subset needed by TemplateHandler).
//!
//! A frame represents a template expansion scope including the parameters
//! passed to the template (`args`). It provides:
//! - `loop_and_depth_check` (loop / depth limit enforcement), and
//! - `expand_template_arg` (parameter lookup for `{{{...}}}`).
//!
//! The generic `Frame::expand` (which re-runs a chunk through the
//! TokenTransform pipeline) is not yet wired because the corresponding
//! `PipelineUtils::processContentInPipeline` path still needs porting.

use crate::title::Title;

use super::parser_functions::Params;
use crate::wikitext::token_utils::key_value_to_string;
use crate::wikitext::tokens_v2::{Item, KV, KeyValue, ParsoidToken};

/// A template expansion scope.
#[derive(Debug, Clone)]
pub struct Frame {
    parent_frame: Option<Box<Frame>>,
    title: Title,
    args: Params,
    depth: usize,
}

impl Frame {
    /// Create a root frame.
    pub fn new(title: Title, args: Vec<KV>) -> Self {
        Self {
            parent_frame: None,
            title,
            args: Params::new(args),
            depth: 0,
        }
    }

    pub fn title(&self) -> &Title {
        &self.title
    }

    pub fn args(&self) -> &Params {
        &self.args
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Create a new child frame. Mirrors `Frame::newChild`.
    pub fn new_child(&self, title: Title, args: Vec<KV>) -> Frame {
        Frame {
            parent_frame: Some(Box::new(self.clone())),
            title,
            args: Params::new(args),
            depth: self.depth + 1,
        }
    }

    /// Check if expanding a template would lead to a loop, or would exceed
    /// the maximum expansion depth. Mirrors `Frame::loopAndDepthCheck`.
    ///
    /// Returns `Some(message)` on error (loop/depth exceeded), else `None`.
    pub fn loop_and_depth_check(
        &self,
        title: &Title,
        max_depth: usize,
        ignore_loop: bool,
    ) -> Option<String> {
        if self.depth > max_depth {
            return Some(format!(
                "Template recursion depth limit exceeded ({max_depth}): "
            ));
        }
        if ignore_loop {
            return None;
        }

        let mut frame = Some(self);
        while let Some(f) = frame {
            if title == &f.title {
                return Some("Template loop detected: ".to_string());
            }
            frame = f.parent_frame.as_deref();
        }
        None
    }

    /// Expand a `{{{...}}}` template argument token. Mirrors
    /// `Frame::expandTemplateArg` for the string-valued argument case.
    pub fn expand_template_arg(&self, name: &str) -> Vec<Item> {
        let named = self.args.named();

        let arg_name = name.trim();
        if let Some(value) = named.dict.get(arg_name) {
            let mut items = key_value_to_items(value);
            // Named arguments are trimmed (mirrors `TokenUtils::tokenTrim`).
            if named.named_args.contains_key(arg_name) {
                trim_items(&mut items);
            }
            return items;
        }

        // Undefined arguments expand to a literal `{{{name}}}` marker.
        vec![
            Item::Str("{{{".to_string()),
            Item::Str(name.to_string()),
            Item::Str("}}}".to_string()),
        ]
    }

    /// Expand / convert a thunk (a chunk of tokens not yet fully expanded).
    /// Mirrors `Frame::expand` for the subset of the pipeline we've ported:
    /// re-tokenize the chunk and expand any `templatearg` (`{{{...}}}`)
    /// references against this frame's arguments.
    ///
    /// Full `template` (`{{...}}`) expansion via the TemplateHandler is wired
    /// separately (see `TemplateHandler::handle_template`); here we only
    /// substitute the parameter references that don't need data access.
    pub fn expand(&self, chunk: &[Item]) -> Vec<Item> {
        let mut out = Vec::new();
        for item in chunk {
            match item {
                Item::Tok(t) => {
                    if let ParsoidToken::SelfclosingTag(stt) = t
                        && stt.name == "templatearg"
                    {
                        // attribs[0].key is the argument name.
                        if let Some(kv) = stt.attribs.first() {
                            let name = match &kv.key {
                                KeyValue::Str(s) => s.clone(),
                                KeyValue::Tokens(toks) => to_strings(toks),
                            };
                            out.extend(self.expand_template_arg(&name));
                        } else {
                            out.push(item.clone());
                        }
                    } else {
                        out.push(item.clone());
                    }
                }
                Item::Str(_) => out.push(item.clone()),
            }
        }
        out
    }
}

/// Convert a resolved `KeyValue` into a flat token chunk.
fn key_value_to_items(value: &KeyValue) -> Vec<Item> {
    match value {
        KeyValue::Str(s) => vec![Item::Str(s.clone())],
        KeyValue::Tokens(items) => items.clone(),
    }
}

/// Convert a token chunk (`Vec<Item>`) to a single concatenated string.
fn to_strings(items: &[Item]) -> String {
    items
        .iter()
        .map(|it| match it {
            Item::Str(s) => s.clone(),
            Item::Tok(t) => match t {
                ParsoidToken::Comment(_) | ParsoidToken::Nl(_) => String::new(),
                other => other
                    .data_parsoid()
                    .and_then(|d| d.src.clone())
                    .unwrap_or_default(),
            },
        })
        .collect()
}

/// Trim leading/trailing whitespace from a token chunk (mirrors
/// `TokenUtils::tokenTrim` for the string-token subset).
fn trim_items(items: &mut [Item]) {
    if items.is_empty() {
        return;
    }
    if let Some(Item::Str(first)) = items.first_mut() {
        *first = first.trim_start().to_string();
    }
    if let Some(Item::Str(last)) = items.last_mut() {
        *last = last.trim_end().to_string();
    }
}

/// Convenience: resolve a template argument reference (e.g. `"1"`, `"name"`)
/// against a frame's args, returning the value string. Mirrors the
/// lookup in `expandTemplateArg` (without the `{{{|...|}}}` fallback).
pub fn resolve_arg_string(frame: &Frame, name: &str) -> Option<String> {
    let dict = frame.args().dict();
    dict.get(name.trim()).map(key_value_to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;
    use crate::title::TitleParser;
    use crate::wikitext::token_utils::tokens_to_string;
    use crate::wikitext::tokens_v2::{KV, KeyValue};

    fn kv(key: &str, value: &str) -> KV {
        KV {
            key: KeyValue::Str(key.to_string()),
            value: KeyValue::Str(value.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        }
    }

    #[test]
    fn test_loop_and_depth_check() {
        let config = MockSiteConfig::new();
        let root_title = TitleParser::parse("Template:Foo", &config);
        let frame = Frame::new(root_title.clone(), vec![]);

        // Same title => loop.
        assert!(frame.loop_and_depth_check(&root_title, 40, false).is_some());

        // Different title => no loop.
        let other = TitleParser::parse("Template:Bar", &config);
        assert!(frame.loop_and_depth_check(&other, 40, false).is_none());

        // ignore_loop bypasses loop detection.
        assert!(frame.loop_and_depth_check(&root_title, 40, true).is_none());
    }

    #[test]
    fn test_loop_and_depth_check_depth() {
        let config = MockSiteConfig::new();
        let root_title = TitleParser::parse("Template:Foo", &config);
        let frame = Frame::new(root_title.clone(), vec![]);
        let child = frame.new_child(TitleParser::parse("Template:Bar", &config), vec![]);

        // max_depth 0 => depth exceeded (child has depth 1).
        assert!(child.loop_and_depth_check(&root_title, 0, false).is_some());
    }

    #[test]
    fn test_expand_template_arg() {
        let config = MockSiteConfig::new();
        let title = TitleParser::parse("Template:Foo", &config);
        let frame = Frame::new(title, vec![kv("", "world"), kv("name", " Alice ")]);

        // Positional arg "1" resolves to "world".
        let items = frame.expand_template_arg("1");
        assert_eq!(tokens_to_string(&items), "world");

        // Named args are trimmed.
        let items = frame.expand_template_arg("name");
        assert_eq!(tokens_to_string(&items), "Alice");

        // Undefined arg becomes a literal `{{{missing}}}` marker.
        let items = frame.expand_template_arg("missing");
        assert_eq!(tokens_to_string(&items), "{{{missing}}}");
    }
}
