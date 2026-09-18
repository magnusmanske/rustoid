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

    /// Expand a `{{{...}}}` template-argument token. Faithful port of
    /// `Frame::expandTemplateArg`, which works off the token's *attribs* rather
    /// than a reconstructed source string:
    ///
    /// - look the (expanded, trimmed) argument name up in this frame's args;
    ///   a hit is returned trimmed when it came from a named argument;
    /// - on a miss with a **default** (more than one attrib) return the default
    ///   value, expanded — this is the `{{{name|default}}}` case;
    /// - otherwise (no default) return a literal `{{{name}}}`.
    ///
    /// Passing the default through matters: a bare `{{{cmt|}}}` must expand to
    /// the empty default, not to the literal source `{{cmt}}`.
    pub fn expand_template_arg_token(&self, token: &ParsoidToken) -> Vec<Item> {
        let ParsoidToken::SelfclosingTag(stt) = token else {
            return Vec::new();
        };
        let Some(first) = stt.attribs.first() else {
            return Vec::new();
        };

        let key_items = self.expand(&key_value_to_items(&first.key));
        let name = crate::wikitext::token_utils::tokens_to_string(&key_items)
            .trim()
            .to_string();

        if let Some(value) = self.args.named().dict.get(&name) {
            let mut items = key_value_to_items(value);
            if self.args.named().named_args.contains_key(&name) {
                trim_items(&mut items);
            }
            return items;
        }
        if stt.attribs.len() > 1 {
            return self.expand(&key_value_to_items(&stt.attribs[1].value));
        }
        vec![
            Item::Str("{{{".to_string()),
            Item::Str(name),
            Item::Str("}}}".to_string()),
        ]
    }

    /// Expand a `{{{...}}}` template argument token. Mirrors
    /// `Frame::expandTemplateArg` for the string-valued argument case.
    pub fn expand_template_arg(&self, name: &str) -> Vec<Item> {
        let named = self.args.named();

        let arg_name = name.trim();
        // The magic argument `!` (`{{{!}}}`) expands to `{|` (table start),
        // mirroring the MediaWiki preprocessor's special handling so tables can
        // be embedded in arguments without consuming the `|` as a separator.
        if arg_name == "!" {
            return vec![Item::Str("{|".to_string())];
        }
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
    /// Attribute values are walked too, and that is load-bearing rather than a
    /// detail. An argument reference inside an HTML attribute is not a child of
    /// the chunk: it sits in a `KeyValue::Tokens` *inside* the `Tag` token. Left
    /// unvisited it survives this pass and is later expanded by
    /// `expand_attributes`, which runs against the **root** frame — where a
    /// template's arguments do not exist — so `{{{small|DEFAULT}}}` took the
    /// default no matter what the caller passed.
    ///
    /// The alternative, expanding attributes inside each template's own frame,
    /// is not available: `expand_attributes` is a whole-stream pass that runs
    /// after `expand_templates`, by which point the template boundaries and their
    /// frames are gone. Substituting here, where the frame is right by
    /// construction, is both simpler and faithful to PHP — `Frame::expand`
    /// expands the chunk it is given, and a template body's attributes are part
    /// of that chunk.
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
                        // attribs[0].key is the argument name; attribs[1] (when
                        // present) is the default.
                        if stt.attribs.is_empty() {
                            out.push(item.clone());
                        } else {
                            out.extend(self.expand_template_arg_token(t));
                        }
                    } else {
                        out.push(self.expand_in_attributes(t));
                    }
                }
                Item::Str(_) => out.push(item.clone()),
            }
        }
        out
    }

    /// Substitute argument references held in a token's attribute values.
    ///
    /// Only `KeyValue::Tokens` can hold a reference; a plain string attribute
    /// carries its text already. A value that expands to a single string is
    /// folded back to `KeyValue::Str` so downstream code (and the `data-mw`
    /// `attribs` HTML) sees the same shape a literal attribute would have.
    /// Substitute argument references held in a token's attribute keys and values,
    /// and in those of tokens nested *within* them.
    ///
    /// Both halves are needed, and the key half is the easy one to miss. A parser
    /// function packs its whole argument list into a *single* attribute whose key
    /// holds the tokens — `{{#expr:{{{1}}}*2}}` is
    /// `key: Tokens(["#expr:", templatearg, "*2"])` with an empty value — so
    /// rewriting only values leaves the reference untouched, and `#expr` then
    /// evaluates the `0` default.
    ///
    /// The nesting is what makes this recursive rather than a single pass: a
    /// parser function inside an attribute, `style="left:{{#expr:{{{1}}}*2}}px"`
    /// (the shape `Template:Fossil range bar` uses), keeps its argument list
    /// inside the `template` token, which is itself a value of the enclosing tag's
    /// attribute.
    ///
    /// A field that expands to a single string is folded back to `KeyValue::Str`
    /// so downstream code — including the `data-mw` `attribs` HTML — sees the same
    /// shape a literal attribute would have.
    fn expand_in_attributes(&self, token: &ParsoidToken) -> Item {
        let attribs = token.get_attribs();
        if !attribs.iter().any(|kv| {
            matches!(kv.value, KeyValue::Tokens(_)) || matches!(kv.key, KeyValue::Tokens(_))
        }) {
            return Item::Tok(token.clone());
        }

        let expand_field = |field: &KeyValue| match field {
            KeyValue::Tokens(toks) => {
                let items = self.expand(toks);
                match items.as_slice() {
                    [Item::Str(s)] => KeyValue::Str(s.clone()),
                    _ => KeyValue::Tokens(items),
                }
            }
            KeyValue::Str(_) => field.clone(),
        };

        let expanded: Vec<KV> = attribs
            .iter()
            .map(|kv| KV {
                key: expand_field(&kv.key),
                value: expand_field(&kv.value),
                src_offsets: kv.src_offsets.clone(),
                ksrc: kv.ksrc.clone(),
                vsrc: kv.vsrc.clone(),
            })
            .collect();

        let mut new_token = token.clone();
        new_token.set_attribs(expanded);
        Item::Tok(new_token)
    }
}

/// Convert a resolved `KeyValue` into a flat token chunk.
fn key_value_to_items(value: &KeyValue) -> Vec<Item> {
    match value {
        KeyValue::Str(s) => vec![Item::Str(s.clone())],
        KeyValue::Tokens(items) => items.clone(),
    }
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
