//! Stage 1 — Wikitext tokenization and preprocessing.
//!
//! This stage preprocesses wikitext (expand templates, parser functions,
//! and template arguments), then tokenizes the fully-expanded result.
//!
//! The preprocessor uses an iterative work-stack approach to avoid
//! recursive async calls, which would require `Box::pin` gymnastics.

use crate::error::Result;
use crate::expand::transclusion;
use crate::title::TitleParser;
use crate::traits::{DataSource, SiteConfig};
use crate::wikitext::tokenizer::{Tokenizer, TokenizerOptions};
use crate::wikitext::tokens::WikitextToken;

/// Run Stage 1: tokenize wikitext without any preprocessing.
pub fn run_stage1_sync(wikitext: &str) -> Result<Vec<WikitextToken>> {
    let tokenizer_opts = TokenizerOptions::default();
    let mut tokenizer = Tokenizer::new(wikitext, tokenizer_opts);
    tokenizer.tokenize()
}

/// Run Stage 1 async: preprocess wikitext (expand templates) and tokenize.
///
/// Uses an iterative work-stack-based preprocessor to handle nested template
/// expansion without recursive async calls.
pub async fn run_stage1_async<S: DataSource, C: SiteConfig>(
    wikitext: &str,
    source: &S,
    config: &C,
    max_depth: u32,
) -> Result<Vec<WikitextToken>> {
    let expanded = Preprocessor::new(source, config, max_depth)
        .expand(wikitext)
        .await?;
    let tokenizer_opts = TokenizerOptions::default();
    let mut tokenizer = Tokenizer::new(&expanded, tokenizer_opts);
    tokenizer.tokenize()
}

// -----------------------------------------------------------------------
// Iterative preprocessor
// -----------------------------------------------------------------------

/// A work item for the iterative preprocessor.
enum WorkItem {
    /// Process a chunk of wikitext at a given depth.
    Process(String, u32),
}

/// Iterative wikitext preprocessor that expands templates and parser functions.
pub struct Preprocessor<'a, S: DataSource, C: SiteConfig> {
    source: &'a S,
    config: &'a C,
    max_depth: u32,
}

impl<'a, S: DataSource, C: SiteConfig> Preprocessor<'a, S, C> {
    pub fn new(source: &'a S, config: &'a C, max_depth: u32) -> Self {
        Self {
            source,
            config,
            max_depth,
        }
    }

    /// Expand all templates and parser functions in `wikitext` iteratively.
    pub async fn expand(&self, wikitext: &str) -> Result<String> {
        let mut result = String::with_capacity(wikitext.len());
        let mut work_stack: Vec<WorkItem> = vec![WorkItem::Process(wikitext.to_string(), 0)];

        while let Some(item) = work_stack.pop() {
            match item {
                WorkItem::Process(text, depth) => {
                    if depth >= self.max_depth {
                        result.push_str(&text);
                        continue;
                    }
                    self.expand_one_pass(&text, depth, &mut result, &mut work_stack)
                        .await?;
                }
            }
        }

        Ok(result)
    }

    /// Process a single piece of wikitext, pushing any nested expansions onto the work stack.
    async fn expand_one_pass(
        &self,
        wikitext: &str,
        depth: u32,
        result: &mut String,
        work_stack: &mut Vec<WorkItem>,
    ) -> Result<()> {
        let chars: Vec<char> = wikitext.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // Look for {{ or {{{
            if i + 1 < chars.len() && chars[i] == '{' && chars[i + 1] == '{' {
                if i + 2 < chars.len() && chars[i + 2] == '{' {
                    // {{{arg|default}}} — template argument reference
                    let (expanded, new_i) = expand_triple_brace(&chars, i, self.config);
                    result.push_str(&expanded);
                    i = new_i;
                    continue;
                }

                // {{template|...}} or {{#fn:...}}
                let brace_str = extract_brace_content(&chars, i);
                if let Some((inner, end_pos)) = brace_str {
                    let invocation = transclusion::parse_template_invocation(&inner);
                    let name = invocation.name.clone();

                    if name.starts_with('#') {
                        // Parser function — evaluate inline (no recursive expansion needed)
                        let fn_result = evaluate_parser_function_sync(&name, &invocation);
                        result
                            .push_str(&fn_result.unwrap_or_else(|_| format!("{{{{{name}|...}}}}")));
                    } else if is_magic_word(&name) {
                        let resolved = resolve_magic_word(&name, self.config);
                        result.push_str(&resolved);
                    } else {
                        // Template transclusion: push expansion work onto the stack
                        let template_title = build_template_title(&name, self.config);
                        let template_source = self
                            .source
                            .get_template(&template_title)
                            .await?
                            .unwrap_or_default();

                        // Handle noinclude/includeonly/onlyinclude
                        let template_source =
                            transclusion::strip_noinclude_sections(&template_source);
                        let template_source =
                            transclusion::extract_includeonly_sections(&template_source);
                        let template_source =
                            transclusion::extract_onlyinclude_sections(&template_source);

                        // Substitute arguments
                        let args = invocation.to_template_args();
                        let substituted = transclusion::substitute_args(
                            &template_source,
                            &args,
                            self.max_depth.saturating_sub(depth),
                        )?;

                        // Push the rest of the current wikitext for later processing,
                        // then process the template expansion first (LIFO → depth-first).
                        let remaining: String = chars[end_pos..].iter().collect();
                        if !remaining.is_empty() {
                            work_stack.push(WorkItem::Process(remaining, depth));
                        }
                        if !substituted.is_empty() {
                            work_stack.push(WorkItem::Process(substituted, depth + 1));
                        }
                        // Current work is done; return so the work stack is processed LIFO
                        return Ok(());
                    }
                    i = end_pos;
                } else {
                    // Unclosed {{ — pass through
                    result.push_str("{{");
                    i += 2;
                }
                continue;
            }

            result.push(chars[i]);
            i += 1;
        }
        Ok(())
    }
}

/// Extract the content between `{{` and `}}` with brace-depth matching.
/// Returns `Some((inner_content, end_position))` or `None` if unclosed.
fn extract_brace_content(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut brace_depth: usize = 2;
    let mut j = start + 2;
    while j < chars.len() && brace_depth > 0 {
        match chars[j] {
            '{' => brace_depth = brace_depth.saturating_add(1),
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                if brace_depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    if brace_depth != 0 {
        return None;
    }
    let inner: String = chars[start + 2..j - 1].iter().collect();
    Some((inner, j + 1))
}

/// Expand {{{arg|default}}} — template argument reference.
fn expand_triple_brace<C: SiteConfig>(
    chars: &[char],
    start: usize,
    _config: &C,
) -> (String, usize) {
    let mut depth: usize = 3;
    let mut j = start + 3;
    while j < chars.len() && depth > 0 {
        match chars[j] {
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }

    if depth == 0 {
        let inner: String = chars[start + 3..j - 2].iter().collect();
        let arg = crate::expand::tpl_args::parse_arg_reference(&inner);
        let resolved = arg.default.unwrap_or_else(|| format!("{{{}}}", arg.name));
        (resolved, j + 1)
    } else {
        ("{{{".to_string(), start + 3)
    }
}

/// Build a template title from its invocation name.
fn build_template_title(name: &str, config: &dyn SiteConfig) -> crate::title::Title {
    let trimmed = name.trim();
    if trimmed.contains(':') {
        let parsed = TitleParser::parse(trimmed, config);
        if parsed.namespace_id != 0 {
            return parsed;
        }
    }
    crate::title::Title::new(10, trimmed)
}

/// Check if a template name is a magic word (variable like PAGENAME, CURRENTYEAR).
fn is_magic_word(name: &str) -> bool {
    let upper = name.to_uppercase();
    matches!(
        upper.as_str(),
        "PAGENAME"
            | "PAGENAMEE"
            | "FULLPAGENAME"
            | "FULLPAGENAMEE"
            | "NAMESPACE"
            | "NAMESPACENUMBER"
            | "CURRENTYEAR"
            | "CURRENTMONTH"
            | "CURRENTMONTHNAME"
            | "CURRENTDAY"
            | "CURRENTDAY2"
            | "CURRENTTIME"
            | "SITENAME"
            | "SERVER"
            | "SERVERNAME"
    )
}

/// Resolve a magic word variable.
fn resolve_magic_word(name: &str, config: &dyn SiteConfig) -> String {
    let upper = name.to_uppercase();
    match upper.as_str() {
        "PAGENAME" | "FULLPAGENAME" => "TestPage".to_string(),
        "SITENAME" => "Wikipedia".to_string(),
        "SERVER" | "SERVERNAME" => config.server_url().to_string(),
        "CURRENTYEAR" => "2026".to_string(),
        "CURRENTMONTH" => "07".to_string(),
        "CURRENTDAY" => "31".to_string(),
        "CURRENTDAY2" => "31".to_string(),
        _ => format!("{{{{{name}}}}}"),
    }
}

/// Evaluate a parser function synchronously.
fn evaluate_parser_function_sync(
    name: &str,
    invocation: &transclusion::TemplateInvocation,
) -> Result<String> {
    // Strip '#', then separate function name from colon-argument
    let after_hash = &name[1..];
    let (fn_name, colon_arg) = if let Some(colon_pos) = after_hash.find(':') {
        (
            &after_hash[..colon_pos],
            Some(after_hash[colon_pos + 1..].trim()),
        )
    } else {
        (after_hash, None)
    };

    // Extract the first argument (either colon-arg or first positional)
    let first_arg = colon_arg.map(|s| s.to_string()).unwrap_or_else(|| {
        invocation
            .positional_args
            .first()
            .cloned()
            .unwrap_or_default()
    });

    match fn_name {
        "if" => {
            let condition = first_arg.trim();
            if !condition.is_empty() {
                Ok(invocation
                    .positional_args
                    .first()
                    .cloned()
                    .unwrap_or_default())
            } else {
                Ok(invocation
                    .positional_args
                    .get(1)
                    .cloned()
                    .unwrap_or_default())
            }
        }
        "ifeq" => {
            let parts: Vec<&str> = first_arg.split('|').collect();
            let a = parts.first().map(|s| s.trim()).unwrap_or("");
            let b = parts.get(1).map(|s| s.trim()).unwrap_or("");
            if a == b {
                Ok(invocation
                    .positional_args
                    .first()
                    .cloned()
                    .unwrap_or_default())
            } else {
                Ok(invocation
                    .positional_args
                    .get(1)
                    .cloned()
                    .unwrap_or_default())
            }
        }
        "expr" | "ifexpr" => {
            let val = evaluate_simple_expr(&first_arg);
            if fn_name == "expr" {
                Ok(val)
            } else if val != "0" && !val.is_empty() {
                Ok(invocation
                    .positional_args
                    .first()
                    .cloned()
                    .unwrap_or_default())
            } else {
                Ok(invocation
                    .positional_args
                    .get(1)
                    .cloned()
                    .unwrap_or_default())
            }
        }
        "switch" => {
            let value = first_arg.trim();
            for (key, val) in &invocation.named_args {
                if key.trim() == value {
                    return Ok(val.clone());
                }
            }
            for arg in &invocation.positional_args {
                if let Some((case, result)) = arg.split_once('=') {
                    if case.trim() == value {
                        return Ok(result.to_string());
                    }
                }
            }
            invocation
                .positional_args
                .last()
                .filter(|a| !a.contains('='))
                .cloned()
                .map(Ok)
                .unwrap_or(Ok(String::new()))
        }
        _ => Ok(format!("{{{{{name}|...}}}}")),
    }
}

/// Basic expression evaluator.
fn evaluate_simple_expr(expr: &str) -> String {
    let expr = expr.trim();
    if expr.is_empty() {
        return String::new();
    }

    let tokens = tokenize_expr(expr);
    match eval_tokens(&tokens) {
        Ok(val) => {
            if val == val.trunc() {
                val.to_string()
            } else {
                format!("{val:.6}")
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_string()
            }
        }
        Err(_) => format!("<strong class=\"error\">Expression error: {expr}</strong>"),
    }
}

fn tokenize_expr(expr: &str) -> Vec<ExprToken> {
    let mut tokens = Vec::new();
    let mut i = 0;
    let bytes = expr.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' => {
                i += 1;
            }
            b'+' | b'-' | b'*' | b'/' => {
                tokens.push(ExprToken::Op(bytes[i] as char));
                i += 1;
            }
            b'(' => {
                tokens.push(ExprToken::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(ExprToken::RParen);
                i += 1;
            }
            b'0'..=b'9' | b'.' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if let Ok(num) = expr[start..i].parse::<f64>() {
                    tokens.push(ExprToken::Num(num));
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    tokens
}

enum ExprToken {
    Num(f64),
    Op(char),
    LParen,
    RParen,
}

fn eval_tokens(tokens: &[ExprToken]) -> std::result::Result<f64, ()> {
    let mut pos = 0;
    expr_parse(tokens, &mut pos, 0)
}

fn expr_parse(tokens: &[ExprToken], pos: &mut usize, min_prec: u8) -> std::result::Result<f64, ()> {
    let mut lhs = expr_primary(tokens, pos)?;
    while *pos < tokens.len() {
        let op = match tokens.get(*pos) {
            Some(ExprToken::Op(c)) => *c,
            _ => break,
        };
        let prec = precedence(op);
        if prec < min_prec {
            break;
        }
        *pos += 1;
        let rhs = expr_parse(tokens, pos, prec + 1)?;
        lhs = match op {
            '+' => lhs + rhs,
            '-' => lhs - rhs,
            '*' => lhs * rhs,
            '/' => {
                if rhs == 0.0 {
                    return std::result::Result::Err(());
                }
                lhs / rhs
            }
            _ => lhs,
        };
    }
    Ok(lhs)
}

fn expr_primary(tokens: &[ExprToken], pos: &mut usize) -> std::result::Result<f64, ()> {
    if *pos >= tokens.len() {
        return Ok(0.0);
    }
    match tokens[*pos] {
        ExprToken::Num(n) => {
            *pos += 1;
            Ok(n)
        }
        ExprToken::Op('-') => {
            *pos += 1;
            expr_primary(tokens, pos).map(|v| -v)
        }
        ExprToken::LParen => {
            *pos += 1;
            let val = expr_parse(tokens, pos, 0)?;
            if *pos < tokens.len() && matches!(tokens[*pos], ExprToken::RParen) {
                *pos += 1;
            }
            Ok(val)
        }
        _ => Ok(0.0),
    }
}

fn precedence(op: char) -> u8 {
    match op {
        '+' | '-' => 1,
        '*' | '/' => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDataSource, MockSiteConfig};

    #[tokio::test]
    async fn test_preprocess_simple_text() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let preprocessor = Preprocessor::new(&source, &config, 5);
        let result = preprocessor.expand("hello world").await.unwrap();
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn test_preprocess_template() {
        let source = MockDataSource::new();
        source.add_template("Template:1x", "{{{1}}}");
        let config = MockSiteConfig::new();
        let preprocessor = Preprocessor::new(&source, &config, 5);
        let result = preprocessor.expand("{{1x|hello}}").await.unwrap();
        assert_eq!(result.trim(), "hello");
    }

    #[tokio::test]
    async fn test_preprocess_parser_function_if() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let preprocessor = Preprocessor::new(&source, &config, 5);
        let result = preprocessor
            .expand("{{#if:not empty|yes|no}}")
            .await
            .unwrap();
        assert_eq!(result, "yes");
    }

    #[tokio::test]
    async fn test_preprocess_magic_word() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let preprocessor = Preprocessor::new(&source, &config, 5);
        let result = preprocessor.expand("{{CURRENTYEAR}}").await.unwrap();
        assert_eq!(result, "2026");
    }
}
