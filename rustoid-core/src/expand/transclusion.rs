//! Template transclusion engine.
//!
//! Expands `{{TemplateName|arg1|arg2=val}}` by fetching the template source,
//! substituting arguments, and recursively expanding any nested templates.

use std::collections::HashMap;

use crate::error::{Result, RustoidError};
use crate::expand::tpl_args::{self, TemplateArgs};
use crate::title::{Title, TitleParser};
use crate::traits::{DataSource, SiteConfig};

/// Result of parsing a template invocation.
#[derive(Debug, Clone)]
pub struct TemplateInvocation {
    /// The template name (without namespace prefix).
    pub name: String,
    /// Positional arguments (in order).
    pub positional_args: Vec<String>,
    /// Named arguments.
    pub named_args: HashMap<String, String>,
}

/// Parse a template invocation from its content string (between `{{` and `}}`).
///
/// The content is split at `|` characters, with the first part being the
/// template name and subsequent parts being arguments.
/// Arguments with `=` are named, others are positional.
pub fn parse_template_invocation(content: &str) -> TemplateInvocation {
    let parts: Vec<&str> = content.split('|').collect();

    let name = parts.first().map(|s| s.trim()).unwrap_or("").to_string();

    let mut positional_args = Vec::new();
    let mut named_args = HashMap::new();

    for part in &parts[1..] {
        if let Some(eq_pos) = part.find('=') {
            let key = part[..eq_pos].trim().to_string();
            let value = part[eq_pos + 1..].to_string(); // Keep original whitespace
            named_args.insert(key, value);
        } else {
            positional_args.push((*part).to_string());
        }
    }

    TemplateInvocation {
        name,
        positional_args,
        named_args,
    }
}

impl TemplateInvocation {
    /// Convert this invocation into TemplateArgs for argument substitution.
    pub fn to_template_args(&self) -> TemplateArgs {
        let mut args = TemplateArgs::new();
        for (i, val) in self.positional_args.iter().enumerate() {
            args.add_positional(val.clone());
            // Also add as named "1", "2", etc.
            args.add_named((i + 1).to_string(), val.clone());
        }
        for (key, val) in &self.named_args {
            args.add_named(key.clone(), val.clone());
        }
        args
    }
}

/// Expand template arguments in wikitext by substituting `{{{...}}}` references.
///
/// This operates on raw wikitext (string), not tokens. It replaces each
/// `{{{arg}}}` or `{{{arg|default}}}` with the argument value.
/// Nested `{{{...}}}` in defaults are handled recursively.
pub fn substitute_args(wikitext: &str, args: &TemplateArgs, max_depth: u32) -> Result<String> {
    if max_depth == 0 {
        return Err(RustoidError::RecursionDepthExceeded(
            "template argument expansion".to_string(),
        ));
    }

    let mut result = String::with_capacity(wikitext.len());
    let chars: Vec<char> = wikitext.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Magic pipe words — `{{!}}`, `{{{!}}}`, `{{{!}}` — are handled at the
        // token level (`TemplateHandler`/`Frame::expand_template_arg`) and must
        // be left *intact* here so they are not mis-parsed as a `{{{…}}}`
        // argument reference (and so their `|` isn't emitted early and then
        // consumed as a template-argument separator before the magic word is
        // expanded). Skip `{{{!` atomically.
        if i + 3 < chars.len()
            && chars[i] == '{'
            && chars[i + 1] == '{'
            && chars[i + 2] == '{'
            && chars[i + 3] == '!'
        {
            result.push_str("{{{");
            i += 3;
            continue;
        }
        if i + 2 < chars.len() && chars[i] == '{' && chars[i + 1] == '{' && chars[i + 2] == '{' {
            // Found {{{ — count depth to find matching }}}
            let open_len = 3;
            let mut depth: usize = open_len;
            let mut j = i + open_len;
            let found = loop {
                if j >= chars.len() {
                    break None;
                }
                match chars[j] {
                    '{' => {
                        depth = depth.saturating_add(1);
                        j += 1;
                    }
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break Some(j); // j is last closing brace
                        }
                        j += 1;
                    }
                    _ => {
                        j += 1;
                    }
                }
            };

            if let Some(last_brace) = found {
                let close_start = last_brace.saturating_sub(open_len - 1);
                let inner: String = chars[i + open_len..close_start].iter().collect();
                let arg_ref = tpl_args::parse_arg_reference(&inner);
                let resolved = tpl_args::resolve_arg(&arg_ref, args);

                let expanded = if resolved.contains("{{{") {
                    substitute_args(&resolved, args, max_depth - 1)?
                } else {
                    resolved
                };

                result.push_str(&expanded);
                i = last_brace + 1; // skip past }}}
            } else {
                result.push_str("{{{");
                i += 3;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    Ok(result)
}

/// Fetch and expand a template, returning the expanded wikitext.
///
/// This looks up the template page, expands its content with the given
/// arguments, and handles `<noinclude>`, `<includeonly>`, and `<onlyinclude>`
/// sections.
pub async fn expand_template(
    name: &str,
    invocation: &TemplateInvocation,
    source: &dyn DataSource,
    config: &dyn SiteConfig,
    max_depth: u32,
) -> Result<String> {
    if max_depth == 0 {
        return Err(RustoidError::RecursionDepthExceeded(format!(
            "template `{name}`"
        )));
    }

    // Build the template title
    let template_title = build_template_title(name, config);

    // Fetch template source
    let source_text = source
        .get_template(&template_title)
        .await?
        .unwrap_or_default();

    // Handle <noinclude>, <includeonly>, <onlyinclude>
    let source_text = strip_noinclude_sections(&source_text);
    let source_text = extract_includeonly_sections(&source_text);
    let source_text = extract_onlyinclude_sections(&source_text);

    // Substitute arguments
    let args = invocation.to_template_args();
    substitute_args(&source_text, &args, max_depth - 1)
}

/// Build a Title for a template from its invocation name.
///
/// If the name already has a namespace prefix, use it.
/// Otherwise, prepend `Template:`.
fn build_template_title(name: &str, config: &dyn SiteConfig) -> Title {
    let trimmed = name.trim();
    // If it already has a namespace prefix (e.g., "Template:Foo"), parse it
    if trimmed.contains(':') {
        let parsed = TitleParser::parse(trimmed, config);
        if parsed.namespace_id != 0 {
            return parsed;
        }
    }
    // Default: assume Template namespace
    Title::new(10, trimmed)
}

/// Strip `<noinclude>...</noinclude>` sections and their content.
/// Content outside `<noinclude>` is kept; content inside is removed.
pub(crate) fn strip_noinclude_sections(text: &str) -> String {
    let mut result = String::new();
    let mut pos = 0;
    while pos < text.len() {
        if let Some(start) = text[pos..].find("<noinclude>") {
            result.push_str(&text[pos..pos + start]);
            let abs_start = pos + start;
            if let Some(end) = text[abs_start..].find("</noinclude>") {
                pos = abs_start + end + 12; // Skip past </noinclude>
            } else {
                pos = abs_start + 11; // Skip just <noinclude>
            }
        } else {
            result.push_str(&text[pos..]);
            break;
        }
    }
    result
}

/// Handle `<includeonly>...</includeonly>` sections.
/// When transcluding, we keep this content; when viewing the template page,
/// it would be hidden. For our purposes, we simply remove the tags.
pub(crate) fn extract_includeonly_sections(text: &str) -> String {
    text.replace("<includeonly>", "")
        .replace("</includeonly>", "")
}

/// Handle `<onlyinclude>...</onlyinclude>` sections.
/// When transcluding, ONLY content inside these tags is kept.
pub(crate) fn extract_onlyinclude_sections(text: &str) -> String {
    if !text.contains("<onlyinclude>") {
        return text.to_string();
    }
    let mut result = String::new();
    let mut pos = 0;
    let open_tag = "<onlyinclude>";
    let close_tag = "</onlyinclude>";
    while pos < text.len() {
        if let Some(start) = text[pos..].find(open_tag) {
            let abs_start = pos + start + open_tag.len();
            if let Some(end) = text[abs_start..].find(close_tag) {
                result.push_str(&text[abs_start..abs_start + end]);
                pos = abs_start + end + close_tag.len();
            } else {
                break;
            }
        } else {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    #[test]
    fn test_parse_simple() {
        let inv = parse_template_invocation("Foo");
        assert_eq!(inv.name, "Foo");
        assert!(inv.positional_args.is_empty());
        assert!(inv.named_args.is_empty());
    }

    #[test]
    fn test_parse_with_positional() {
        let inv = parse_template_invocation("Foo|bar|baz");
        assert_eq!(inv.name, "Foo");
        assert_eq!(inv.positional_args, vec!["bar", "baz"]);
    }

    #[test]
    fn test_parse_with_named() {
        let inv = parse_template_invocation("Foo|key=value|other=something");
        assert_eq!(inv.name, "Foo");
        assert!(inv.positional_args.is_empty());
        assert_eq!(inv.named_args.get("key").unwrap(), "value");
        assert_eq!(inv.named_args.get("other").unwrap(), "something");
    }

    #[test]
    fn test_parse_mixed() {
        let inv = parse_template_invocation("Card|Ace|suit=Spades");
        assert_eq!(inv.positional_args, vec!["Ace"]);
        assert_eq!(inv.named_args.get("suit").unwrap(), "Spades");
    }

    #[test]
    fn test_substitute_simple() {
        let mut args = TemplateArgs::new();
        args.add_positional("world");
        let result = substitute_args("Hello {{{1}}}!", &args, 5).unwrap();
        assert_eq!(result, "Hello world!");
    }

    #[test]
    fn test_substitute_named() {
        let mut args = TemplateArgs::new();
        args.add_named("greeting", "Hello");
        let result = substitute_args("{{{greeting}}} world!", &args, 5).unwrap();
        assert_eq!(result, "Hello world!");
    }

    #[test]
    fn test_substitute_with_default() {
        let args = TemplateArgs::new();
        let result = substitute_args("{{{missing|default}}}", &args, 5).unwrap();
        assert_eq!(result, "default");
    }

    #[test]
    fn test_substitute_nested_default() {
        let mut args = TemplateArgs::new();
        args.add_positional("inner");
        // Outer arg has no value, so default is used.
        // The default itself contains {{{1}}} which resolves to "inner".
        let result = substitute_args("{{{outer|{{{1}}}}}}", &args, 5).unwrap();
        assert_eq!(result, "inner");
    }

    #[test]
    fn test_magic_pipe_in_args() {
        let mut args = TemplateArgs::new();
        args.add_positional("a|b");
        let result = substitute_args("Value: {{{1}}}", &args, 5).unwrap();
        assert_eq!(result, "Value: a|b");
    }

    #[test]
    fn test_magic_pipe_words_left_intact() {
        // `{{{!}}` and `{{!}}` are token-level magic words (table escapes);
        // `substitute_args` must leave them intact rather than mis-parsing
        // `{{{!}}` as a `{{{…}}}` argument or eagerly emitting a `|`.
        let args = TemplateArgs::new();
        let result = substitute_args("{{{!}}", &args, 5).unwrap();
        assert_eq!(result, "{{{!}}");
        let result = substitute_args("{{!}}", &args, 5).unwrap();
        assert_eq!(result, "{{!}}");
        let result = substitute_args("a {{{!}} b {{!}} c", &args, 5).unwrap();
        assert_eq!(result, "a {{{!}} b {{!}} c");
    }

    #[test]
    fn test_noinclude_stripping() {
        let text = "before<noinclude>hidden</noinclude>after";
        assert_eq!(strip_noinclude_sections(text), "beforeafter");
    }

    #[test]
    fn test_includeonly_extraction() {
        let text = "<includeonly>keep</includeonly>drop";
        assert_eq!(extract_includeonly_sections(text), "keepdrop");
    }

    #[test]
    fn test_onlyinclude_extraction() {
        let text = "drop<onlyinclude>keep</onlyinclude>drop";
        assert_eq!(extract_onlyinclude_sections(text), "keep");
    }

    #[test]
    fn test_onlyinclude_no_tag() {
        let text = "all content";
        assert_eq!(extract_onlyinclude_sections(text), "all content");
    }

    #[test]
    fn test_to_template_args() {
        let inv = parse_template_invocation("Test|pos1|pos2|named=val");
        let args = inv.to_template_args();
        assert_eq!(args.get("1"), Some("pos1"));
        assert_eq!(args.get("2"), Some("pos2"));
        assert_eq!(args.get("named"), Some("val"));
    }

    #[test]
    fn test_build_template_title() {
        let config = MockSiteConfig::new();
        let title = build_template_title("Foo", &config);
        assert_eq!(title.namespace_id, 10);
        assert_eq!(title.text, "Foo");
    }
}
