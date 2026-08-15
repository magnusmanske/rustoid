//! Wikitext preprocessor — template expansion and parser function evaluation.
//!
//! This stage takes the raw token stream from the tokenizer and resolves
//! templates, parser functions, magic words, and template arguments into
//! a flat token stream with all transclusions expanded.

use crate::error::{Result, RustoidError};
use crate::expand::transclusion::{self, TemplateInvocation};
use crate::traits::{DataSource, SiteConfig};
use crate::wikitext::tokens::WikitextToken;

/// The preprocessor evaluates templates, parser functions, and magic words.
pub struct Preprocessor<'a, S: DataSource, C: SiteConfig> {
    #[allow(dead_code)]
    source: &'a S,
    #[allow(dead_code)]
    config: &'a C,
    /// Maximum template expansion depth before we error out.
    #[allow(dead_code)]
    max_depth: u32,
}

impl<'a, S: DataSource, C: SiteConfig> Preprocessor<'a, S, C> {
    /// Create a new preprocessor.
    pub fn new(source: &'a S, config: &'a C) -> Self {
        Self {
            source,
            config,
            max_depth: 40,
        }
    }

    /// Expand all templates, parser functions, and arguments in the token stream.
    ///
    /// This is called repeatedly until no more expansions remain (or max depth
    /// is reached).
    pub fn expand(&self, tokens: Vec<WikitextToken>) -> Result<Vec<WikitextToken>> {
        let mut result = Vec::with_capacity(tokens.len());
        let mut i = 0;

        while i < tokens.len() {
            match &tokens[i] {
                WikitextToken::TemplateOpen(name) => {
                    // Find the matching TemplateClose
                    let content = self.collect_template_content(&tokens, i);
                    if let Some((invocation, end_idx)) = content {
                        let expanded = self.expand_template_invocation(name, &invocation)?;
                        // The expanded text needs to be re-tokenized (placeholder: emit as Text)
                        result.push(WikitextToken::Text(expanded));
                        i = end_idx + 1;
                    } else {
                        result.push(tokens[i].clone());
                        i += 1;
                    }
                }
                _ => {
                    result.push(tokens[i].clone());
                    i += 1;
                }
            }
        }

        Ok(result)
    }

    /// Collect the content between TemplateOpen and TemplateClose, parsing
    /// the invocation. Returns (invocation, index_of_close).
    fn collect_template_content(
        &self,
        tokens: &[WikitextToken],
        open_idx: usize,
    ) -> Option<(TemplateInvocation, usize)> {
        let mut content = String::new();
        let mut depth = 1;
        let mut idx = open_idx + 1;

        while idx < tokens.len() && depth > 0 {
            match &tokens[idx] {
                WikitextToken::TemplateOpen(_) | WikitextToken::ParserFnOpen(_) => {
                    content.push('{');
                    for _ in 0..2 {
                        if let WikitextToken::Text(t) = &tokens[idx] {
                            content.push_str(t);
                        }
                    }
                    depth += 1;
                }
                WikitextToken::TemplateClose | WikitextToken::ParserFnClose => {
                    depth -= 1;
                    if depth > 0 {
                        content.push_str("}}");
                    }
                }
                WikitextToken::Text(t) => {
                    content.push_str(t);
                }
                _ => {
                    // Capture other tokens as text representation
                }
            }
            idx += 1;
        }

        if depth == 0 && idx > open_idx {
            let invocation = transclusion::parse_template_invocation(&content);
            Some((invocation, idx - 1))
        } else {
            None
        }
    }

    /// Expand a single template invocation into wikitext.
    fn expand_template_invocation(
        &self,
        template_name: &str,
        invocation: &TemplateInvocation,
    ) -> Result<String> {
        // For now, use a simple synchronous expansion using the arguments only.
        // Full async template fetching will be in the pipeline stage that has tokio.
        // For the sync preprocessor, we substitute known arguments and return the result.
        // Template content fetching is done by the pipeline via async context.

        // Build arguments from the invocation
        let _args = invocation.to_template_args();

        // If this is a parser function, handle it
        if template_name.starts_with('#') {
            return self.evaluate_parser_function(template_name, invocation);
        }

        // For template transclusion, we defer to the pipeline for async fetching.
        // Return a marker that the template needs expansion at the pipeline level.
        Ok(format!("{{{{EXPAND:{template_name}}}}}"))
    }

    /// Evaluate a parser function.
    fn evaluate_parser_function(
        &self,
        name: &str,
        invocation: &TemplateInvocation,
    ) -> Result<String> {
        let fn_name = &name[1..]; // Strip the '#'

        // For parser functions with colon syntax (#fn:arg1|arg2), the first
        // argument is embedded in the invocation name after the colon.
        let first_arg = if let Some(colon_pos) = invocation.name.find(':') {
            invocation.name[colon_pos + 1..].trim().to_string()
        } else {
            invocation
                .positional_args
                .first()
                .cloned()
                .unwrap_or_default()
        };

        match fn_name {
            "if" => self.pf_if(invocation, &first_arg),
            "ifeq" => self.pf_ifeq(invocation, &first_arg),
            "switch" => self.pf_switch(invocation, &first_arg),
            "expr" => self.pf_expr(invocation, &first_arg),
            "ifexpr" => self.pf_ifexpr(invocation, &first_arg),
            "iferror" => self.pf_iferror(invocation, &first_arg),
            "ifexist" => Ok(String::new()),
            "tag" => self.pf_tag(invocation, &first_arg),
            "titleparts" => self.pf_titleparts(invocation, &first_arg),
            _ => Ok(format!("{{{{{{{name}|...}}}}}}")),
        }
    }

    // ---- Parser function implementations ----

    fn pf_if(&self, inv: &TemplateInvocation, first_arg: &str) -> Result<String> {
        let condition = first_arg.trim();
        if !condition.is_empty() {
            Ok(inv.positional_args.first().cloned().unwrap_or_default())
        } else {
            Ok(inv.positional_args.get(1).cloned().unwrap_or_default())
        }
    }

    fn pf_ifeq(&self, inv: &TemplateInvocation, first_arg: &str) -> Result<String> {
        let a = first_arg.trim();
        let b = inv.positional_args.first().map(|s| s.trim()).unwrap_or("");
        let then_val = inv.positional_args.get(1).cloned().unwrap_or_default();
        let else_val = inv.positional_args.get(2).cloned().unwrap_or_default();
        if a == b { Ok(then_val) } else { Ok(else_val) }
    }

    fn pf_switch(&self, inv: &TemplateInvocation, first_arg: &str) -> Result<String> {
        let value = first_arg.trim();

        // Check named args first (case=result pairs)
        for (key, val) in &inv.named_args {
            if key.trim() == value {
                return Ok(val.clone());
            }
        }

        // Check positional args starting from index 1
        for (_i, arg) in inv.positional_args.iter().enumerate().skip(1) {
            if let Some((case, result)) = arg.split_once('=').map(|(c, r)| (c.trim(), r.trim()))
                && case == value
            {
                return Ok(result.to_string());
            }
        }

        // No match — use default (last positional arg without =, or empty)
        if let Some(last) = inv.positional_args.last()
            && !last.contains('=')
        {
            return Ok(last.clone());
        }
        Ok(String::new())
    }

    /// `{{#expr: expression }}`
    fn pf_expr(&self, _inv: &TemplateInvocation, first_arg: &str) -> Result<String> {
        Ok(evaluate_expression(first_arg))
    }

    fn pf_ifexpr(&self, inv: &TemplateInvocation, first_arg: &str) -> Result<String> {
        let result = evaluate_expression(first_arg);
        if result != "0" && !result.is_empty() {
            Ok(inv.positional_args.first().cloned().unwrap_or_default())
        } else {
            Ok(inv.positional_args.get(1).cloned().unwrap_or_default())
        }
    }

    fn pf_iferror(&self, inv: &TemplateInvocation, first_arg: &str) -> Result<String> {
        if first_arg.contains("<strong class=\"error\">") {
            Ok(inv
                .positional_args
                .first()
                .cloned()
                .unwrap_or(first_arg.to_string()))
        } else {
            Ok(inv
                .positional_args
                .get(1)
                .cloned()
                .unwrap_or(first_arg.to_string()))
        }
    }

    fn pf_tag(&self, inv: &TemplateInvocation, first_arg: &str) -> Result<String> {
        let tag = first_arg.trim();
        let tag = if tag.is_empty() { "span" } else { tag };
        let content = inv.positional_args.first().cloned().unwrap_or_default();

        let mut attrs = String::new();
        for (key, val) in &inv.named_args {
            attrs.push_str(&format!(" {key}=\"{val}\""));
        }

        if content.is_empty() {
            Ok(format!("<{tag}{attrs}/>"))
        } else {
            Ok(format!("<{tag}{attrs}>{content}</{tag}>"))
        }
    }

    fn pf_titleparts(&self, inv: &TemplateInvocation, first_arg: &str) -> Result<String> {
        let title = first_arg;
        let segments: usize = inv
            .positional_args
            .first()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1);
        let first: usize = inv
            .positional_args
            .get(1)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1);

        let parts: Vec<&str> = title.split('/').collect();
        let start = (first.saturating_sub(1)).min(parts.len());
        let end = (start + segments).min(parts.len());

        Ok(parts[start..end].join("/"))
    }
}

/// Very basic expression evaluator for `#expr` and `#ifexpr`.
/// Supports +, -, *, / and parentheses. Returns result as string.
pub(crate) fn evaluate_expression(expr: &str) -> String {
    let expr = expr.trim();
    if expr.is_empty() {
        return String::new();
    }

    // Try to parse as a simple integer expression
    // We do a very basic token-based evaluation
    let tokens = tokenize_expr(expr);
    match eval_simple(&tokens) {
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

#[derive(Debug, Clone, PartialEq)]
enum ExprToken {
    Num(f64),
    Op(char),
    LParen,
    RParen,
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
            b'+' | b'-' | b'*' | b'/' | b'%' => {
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
            } // Skip unknown chars
        }
    }
    tokens
}

/// Simple precedence-climbing expression evaluator.
fn eval_simple(tokens: &[ExprToken]) -> std::result::Result<f64, RustoidError> {
    let mut pos = 0;
    expr_parse(tokens, &mut pos, 0)
}

fn expr_parse(
    tokens: &[ExprToken],
    pos: &mut usize,
    min_prec: u8,
) -> std::result::Result<f64, RustoidError> {
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
                    return Err(RustoidError::Parse("division by zero".to_string()));
                }
                lhs / rhs
            }
            '%' => {
                if rhs == 0.0 {
                    return Err(RustoidError::Parse("modulo by zero".to_string()));
                }
                lhs - rhs * (lhs / rhs).trunc()
            }
            _ => lhs,
        };
    }
    Ok(lhs)
}

fn expr_primary(tokens: &[ExprToken], pos: &mut usize) -> std::result::Result<f64, RustoidError> {
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
            let val = expr_primary(tokens, pos)?;
            Ok(-val)
        }
        ExprToken::LParen => {
            *pos += 1;
            let val = expr_parse(tokens, pos, 0)?;
            if *pos < tokens.len() && tokens[*pos] == ExprToken::RParen {
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
        '*' | '/' | '%' => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDataSource, MockSiteConfig};

    fn make_preprocessor<'a>(
        source: &'a MockDataSource,
        config: &'a MockSiteConfig,
    ) -> Preprocessor<'a, MockDataSource, MockSiteConfig> {
        Preprocessor::new(source, config)
    }

    #[test]
    fn test_pf_if_true() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#if:not empty|yes|no");
        let result = pp.evaluate_parser_function("#if", &inv).unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_pf_if_empty() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#if:|then|else");
        let result = pp.evaluate_parser_function("#if", &inv).unwrap();
        assert_eq!(result, "else");
    }

    #[test]
    fn test_pf_ifeq_match() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#ifeq:hello|hello|same|different");
        let result = pp.evaluate_parser_function("#ifeq", &inv).unwrap();
        assert_eq!(result, "same");
    }

    #[test]
    fn test_pf_ifeq_no_match() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#ifeq:a|b|same|different");
        let result = pp.evaluate_parser_function("#ifeq", &inv).unwrap();
        assert_eq!(result, "different");
    }

    #[test]
    fn test_pf_switch() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#switch:b|a=Alpha|b=Beta|Default");
        let result = pp.evaluate_parser_function("#switch", &inv).unwrap();
        assert_eq!(result, "Beta");
    }

    #[test]
    fn test_pf_switch_default() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#switch:missing|a=Alpha|Default");
        let result = pp.evaluate_parser_function("#switch", &inv).unwrap();
        assert_eq!(result, "Default");
    }

    #[test]
    fn test_pf_expr_simple() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#expr:2 + 3 * 4");
        let result = pp.evaluate_parser_function("#expr", &inv).unwrap();
        assert_eq!(result, "14"); // 2 + (3 * 4) = 14
    }

    #[test]
    fn test_pf_expr_parens() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#expr:(2 + 3) * 4");
        let result = pp.evaluate_parser_function("#expr", &inv).unwrap();
        assert_eq!(result, "20");
    }

    #[test]
    fn test_pf_ifexpr() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#ifexpr:1 + 1|true|false");
        let result = pp.evaluate_parser_function("#ifexpr", &inv).unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn test_pf_tag() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#tag:ref|Citation|name=foo");
        let result = pp.evaluate_parser_function("#tag", &inv).unwrap();
        assert_eq!(result, "<ref name=\"foo\">Citation</ref>");
    }

    #[test]
    fn test_pf_titleparts() {
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);

        let inv = transclusion::parse_template_invocation("#titleparts:a/b/c/d|2|2");
        let result = pp.evaluate_parser_function("#titleparts", &inv).unwrap();
        assert_eq!(result, "b/c");
    }

    #[test]
    fn test_preprocessor_no_templates() {
        let tokens = vec![
            WikitextToken::Text("plain text".to_string()),
            WikitextToken::EOF,
        ];
        let source = MockDataSource::new();
        let config = MockSiteConfig::new();
        let pp = make_preprocessor(&source, &config);
        let result = pp.expand(tokens).unwrap();
        assert_eq!(result.len(), 2);
    }
}
