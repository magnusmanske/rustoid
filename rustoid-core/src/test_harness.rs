//! Parsoid parser test harness.
//!
//! Reads `parserTests.txt` files in the format used by the official Parsoid
//! test suite and runs each test case through the rustoid parser, comparing
//! output against expected HTML/wikitext.
//!
//! ## Test file format
//!
//! ```text
//! !! Version 2
//!
//! !! article
//! Template:Name
//! !! text
//! Template wikitext here
//! !! endarticle
//!
//! !! test
//! Description of the test
//! !! options
//! parsoid=wt2html
//! language=fr
//! !! wikitext
//! '''bold''' text
//! !! html/parsoid
//! <p><b>bold</b> text</p>
//! !! end
//! ```

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use crate::error::Result;
use crate::mock::{MockDataSource, MockSiteConfig};
use crate::options::ParserOptions;
use crate::pipeline::stage2::run_stage2;
use crate::pipeline::stage3::run_stage3_html;
use crate::wikitext::tokenizer::{Tokenizer, TokenizerOptions};

// ---------------------------------------------------------------------------
// Test case representation
// ---------------------------------------------------------------------------

/// A single parser test case.
#[derive(Debug, Clone, Default)]
pub struct ParserTestCase {
    /// Description / name of the test.
    pub description: String,
    /// Raw options block text (can be key=value or JSON).
    pub options_raw: String,
    /// Parsed options as key-value pairs.
    pub options: HashMap<String, String>,
    /// Input wikitext.
    pub wikitext: String,
    /// Expected HTML output (Parsoid format).
    pub html_parsoid: Option<String>,
    /// Expected HTML output (PHP parser format, for comparison).
    pub html_php: Option<String>,
    /// Expected HTML output (Parsoid + language variant).
    pub html_parsoid_lang: Option<(String, String)>,
    /// Expected wikitext after round-trip or selser edit.
    pub wikitext_edited: Option<String>,
    /// Source line number in the test file.
    pub line_number: usize,
}

/// Parsed test file content.
#[derive(Debug, Clone)]
pub struct ParserTestFile {
    /// All test cases in the file.
    pub tests: Vec<ParserTestCase>,
    /// Mock articles defined in the file (templates, etc.).
    pub articles: HashMap<String, String>,
    /// Test file path.
    pub path: String,
    /// Version string (usually "2").
    pub version: String,
}

/// Result of running a single test.
#[derive(Debug, Clone)]
pub enum TestResult {
    /// Test passed — output matched expected.
    Pass,
    /// Test failed — output didn't match.
    Fail {
        expected: String,
        actual: String,
        diff_hint: String,
    },
    /// Test skipped (e.g., mode not supported).
    Skip(String),
    /// Test errored during parsing.
    Error(String),
}

impl fmt::Display for TestResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestResult::Pass => write!(f, "PASS"),
            TestResult::Fail { diff_hint, .. } => write!(f, "FAIL ({diff_hint})"),
            TestResult::Skip(reason) => write!(f, "SKIP ({reason})"),
            TestResult::Error(msg) => write!(f, "ERROR ({msg})"),
        }
    }
}

/// Summary of test results.
#[derive(Debug, Clone, Default)]
pub struct TestSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub failures: Vec<(String, TestResult)>,
}

impl fmt::Display for TestSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Results: {}/{} passed", self.passed, self.total)?;
        writeln!(f, "  Failed:  {}", self.failed)?;
        writeln!(f, "  Skipped: {}", self.skipped)?;
        writeln!(f, "  Errors:  {}", self.errors)?;
        if !self.failures.is_empty() {
            writeln!(f, "\nFailures:")?;
            for (name, result) in &self.failures {
                writeln!(f, "  - {name}: {result}")?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Test file parser
// ---------------------------------------------------------------------------

/// Parse a `parserTests.txt` file.
pub fn parse_test_file(path: &Path) -> Result<ParserTestFile> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        crate::error::RustoidError::DataSource(format!("Cannot read {path:?}: {e}"))
    })?;

    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    // Parse version
    let mut version = String::new();
    while i < lines.len() {
        let line = lines[i].trim();
        if let Some(stripped) = line.strip_prefix("!! Version") {
            version = stripped.trim().to_string();
        }
        if line == "!! Version 2" || line.starts_with("!! article") || line.starts_with("!! test") {
            break;
        }
        i += 1;
    }

    let mut articles = HashMap::new();
    let mut tests = Vec::new();

    // Parse articles and tests
    while i < lines.len() {
        let line = lines[i].trim();

        if line == "!! article" {
            i += 1;
            let name = lines.get(i).map(|l| l.trim()).unwrap_or("");
            i += 1;
            if lines.get(i).map(|l| l.trim()) == Some("!! text") {
                i += 1;
                let mut text = String::new();
                while i < lines.len()
                    && lines[i].trim() != "!! endarticle"
                    && lines[i].trim() != "!! end"
                {
                    text.push_str(lines[i]);
                    text.push('\n');
                    i += 1;
                }
                articles.insert(name.to_string(), text.trim_end().to_string());
                if i < lines.len() {
                    i += 1;
                }
            }
        } else if (line == "!! options" || line.starts_with("!! options"))
            && !line.starts_with("!! options ")
        {
            // File-level !! options block (no extra text after "!! options")
            // Skip until !! end at the same level
            while i < lines.len() && lines[i].trim() != "!! end" {
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
        } else if line == "!! hooks" {
            while i < lines.len() && lines[i].trim() != "!! endhooks" {
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
        } else if let Some(stripped) = line.strip_prefix("!! test") {
            let description = stripped.trim().to_string();
            if description.is_empty() {
                // Description might be on the next line
                i += 1;
                if i < lines.len() {
                    let desc = lines[i].trim();
                    // Skip comment lines starting with #
                    if !desc.starts_with("!!") && !desc.starts_with('#') && !desc.is_empty() {
                        let test = parse_test_case(&lines, &mut i, desc.to_string())?;
                        tests.push(test);
                    }
                }
            } else {
                let test = parse_test_case(&lines, &mut i, description)?;
                tests.push(test);
            }
        } else {
            i += 1;
        }
    }

    Ok(ParserTestFile {
        tests,
        articles,
        path: path.to_string_lossy().to_string(),
        version,
    })
}

/// Parse a single test case starting after the !! test line.
fn parse_test_case(lines: &[&str], i: &mut usize, description: String) -> Result<ParserTestCase> {
    let mut test = ParserTestCase {
        description,
        line_number: *i + 1,
        ..Default::default()
    };

    let mut options_lines = Vec::new();
    let mut wikitext_lines = Vec::new();
    let mut html_parsoid_lines = Vec::new();
    let mut html_php_lines = Vec::new();
    let mut html_parsoid_lang_lines = Vec::new();
    let mut wikitext_edited_lines = Vec::new();

    let mut section = Section::None;

    while *i < lines.len() {
        let line = lines[*i];
        let trimmed = line.trim();

        match trimmed {
            "!! options" => {
                section = Section::Options;
                *i += 1;
            }
            "!! wikitext" => {
                section = Section::Wikitext;
                *i += 1;
            }
            "!! html/parsoid" | "!! html/parsoid here" => {
                section = Section::Html;
                *i += 1;
            }
            "!! html" | "!! html/*" => {
                // Generic HTML: use for PHP format. If there's no parsoid section,
                // we can use this, but it won't match Parsoid output.
                section = Section::HtmlPhp;
                *i += 1;
            }
            "!! html/php" => {
                section = Section::HtmlPhp;
                *i += 1;
            }
            "!! html/parsoid+lang" => {
                section = Section::HtmlLang;
                *i += 1;
            }
            "!! html/parsoid+integrated" => {
                section = Section::None; // Skip integrated tests for now
                *i += 1;
            }
            "!! wikitext/edited" => {
                section = Section::WikitextEdited;
                *i += 1;
            }
            "!! end" => {
                *i += 1;
                break;
            }
            _ => {
                if trimmed.starts_with("!! html") {
                    // Unknown html section — skip
                    section = Section::None;
                    *i += 1;
                } else if trimmed.starts_with('#') {
                    // Comment line
                    *i += 1;
                } else {
                    match section {
                        Section::Options => options_lines.push(line.to_string()),
                        Section::Wikitext => wikitext_lines.push(line.to_string()),
                        Section::Html => html_parsoid_lines.push(line.to_string()),
                        Section::HtmlPhp => html_php_lines.push(line.to_string()),
                        Section::HtmlLang => html_parsoid_lang_lines.push(line.to_string()),
                        Section::WikitextEdited => wikitext_edited_lines.push(line.to_string()),
                        Section::None => { /* skip */ }
                    }
                    *i += 1;
                }
            }
        }
    }

    // Assemble collected lines
    test.options_raw = options_lines.join("\n");
    test.wikitext = wikitext_lines.join("\n").trim().to_string();

    if !html_parsoid_lines.is_empty() {
        let html = html_parsoid_lines.join("\n").trim().to_string();
        if html != "NOT NEEDED" {
            test.html_parsoid = Some(html);
        }
    }
    if !html_php_lines.is_empty() {
        test.html_php = Some(html_php_lines.join("\n").trim().to_string());
    }
    if !html_parsoid_lang_lines.is_empty() {
        let lines = html_parsoid_lang_lines.join("\n");
        // May have language + html separated by newline
        if let Some((lang, html)) = lines.split_once('\n') {
            test.html_parsoid_lang = Some((lang.trim().to_string(), html.trim().to_string()));
        } else {
            test.html_parsoid_lang = Some((String::new(), lines.trim().to_string()));
        }
    }
    if !wikitext_edited_lines.is_empty() {
        test.wikitext_edited = Some(wikitext_edited_lines.join("\n").trim().to_string());
    }

    // Parse options
    test.options = parse_options(&test.options_raw);

    Ok(test)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Options,
    Wikitext,
    Html,
    HtmlPhp,
    HtmlLang,
    WikitextEdited,
}

/// Parse options text into key-value pairs.
/// Supports both key=value and JSON formats.
fn parse_options(raw: &str) -> HashMap<String, String> {
    let mut opts = HashMap::new();
    let raw = raw.trim();

    if raw.is_empty() {
        return opts;
    }

    // Try JSON format
    if raw.starts_with('{') {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(raw)
            && let Some(obj) = json.as_object()
        {
            for (k, v) in obj {
                opts.insert(k.clone(), v.to_string());
            }
        }
        return opts;
    }

    // Key=value format (comma or newline separated)
    for part in raw.split(',') {
        let part = part.trim();
        if let Some((key, value)) = part.split_once('=') {
            opts.insert(key.trim().to_string(), value.trim().to_string());
        } else if !part.is_empty() {
            opts.insert(part.to_string(), String::new());
        }
    }

    opts
}

// ---------------------------------------------------------------------------
// Test runner
// ---------------------------------------------------------------------------

/// Run all tests in a file and return a summary.
pub fn run_test_file(path: &Path) -> Result<TestSummary> {
    let test_file = parse_test_file(path)?;
    run_parsed_tests(&test_file)
}

/// Run already-parsed tests.
pub fn run_parsed_tests(test_file: &ParserTestFile) -> Result<TestSummary> {
    let mut summary = TestSummary::default();

    for test in &test_file.tests {
        summary.total += 1;
        let result = run_single_test(test, test_file);
        match &result {
            TestResult::Pass => summary.passed += 1,
            TestResult::Fail { .. } => {
                summary.failed += 1;
                summary.failures.push((test.description.clone(), result));
            }
            TestResult::Skip(_) => summary.skipped += 1,
            TestResult::Error(_) => {
                summary.errors += 1;
                summary.failures.push((test.description.clone(), result));
            }
        }
    }

    Ok(summary)
}

/// Run a single test case (public for diagnostic use).
pub fn run_single_test_public(test: &ParserTestCase, test_file: &ParserTestFile) -> TestResult {
    run_single_test(test, test_file)
}

/// Run a single test case.
fn run_single_test(test: &ParserTestCase, test_file: &ParserTestFile) -> TestResult {
    // Determine mode from options
    let mode = test
        .options
        .get("parsoid")
        .map(|s| s.as_str())
        .unwrap_or("wt2html");

    // Check if this mode contains specific test modes
    let modes: Vec<&str> = if mode.starts_with('{') {
        vec!["wt2html"] // Default for JSON options
    } else {
        mode.split(',').map(|s| s.trim()).collect()
    };

    // Only run wt2html if the mode explicitly supports it
    let supports_wt2html = modes.is_empty() || modes.contains(&"wt2html");

    if supports_wt2html && (test.html_parsoid.is_some() || test.html_php.is_some()) {
        return run_wt2html_test(test, test_file);
    }

    // If wikitext/edited is provided, run selser
    if test.wikitext_edited.is_some() {
        return run_selser_test(test, test_file);
    }

    TestResult::Skip(format!("unsupported mode: {mode}"))
}

/// Run a wikitext → HTML test.
fn run_wt2html_test(test: &ParserTestCase, test_file: &ParserTestFile) -> TestResult {
    if test.wikitext.is_empty() {
        return TestResult::Skip("no wikitext input".to_string());
    }

    let expected_html = match test.html_parsoid.as_ref() {
        Some(h) => h.clone(),
        None => match test.html_php.as_ref() {
            Some(h) => h.clone(),
            None => return TestResult::Skip("no expected HTML".to_string()),
        },
    };

    // Build mock data source with test file articles
    let source = MockDataSource::new();
    for (name, text) in &test_file.articles {
        if name.starts_with("Template:") {
            source.add_template(name, text);
        } else {
            source.add_page(name, text);
            if !name.contains(':') {
                source.add_template(&format!("Template:{name}"), text);
            }
        }
    }

    // Add the page being tested
    let page_title = test
        .options
        .get("title")
        .cloned()
        .unwrap_or_else(|| "TestPage".to_string());
    source.add_page(&page_title, &test.wikitext);

    let _config = MockSiteConfig::new();

    // Expand simple templates in wikitext (sync, for test harness use)
    let expanded = expand_simple_templates(&test.wikitext, &test_file.articles);

    // Tokenize expanded wikitext
    let tokenizer_opts = TokenizerOptions::default();
    let mut tokenizer = Tokenizer::new(&expanded, tokenizer_opts);
    let tokens = match tokenizer.tokenize() {
        Ok(t) => t,
        Err(e) => return TestResult::Error(format!("tokenization error: {e}")),
    };

    let ast = match run_stage2(tokens) {
        Ok(ast) => ast,
        Err(e) => return TestResult::Error(format!("tree building error: {e}")),
    };

    let options = ParserOptions {
        page_title,
        language: test
            .options
            .get("language")
            .cloned()
            .unwrap_or_else(|| "en".to_string()),
        ..ParserOptions::default()
    };

    let actual_html = match run_stage3_html(&ast, &options) {
        Ok(h) => h,
        Err(e) => return TestResult::Error(format!("serialization error: {e}")),
    };

    // Compare: strip the <!DOCTYPE><html><head>...</head><body> wrappers
    let actual_body = extract_body(&actual_html);
    let expected_body = if expected_html.contains("<!DOCTYPE") {
        extract_body(&expected_html)
    } else {
        expected_html.clone()
    };

    // Strip data-parsoid/data-mw from both sides for comparison
    // (PHP-format tests don't have these; Parsoid tests do)
    let actual_body = strip_parsoid_attrs(&actual_body);
    let expected_body = strip_parsoid_attrs(&expected_body);

    if actual_body.trim() == expected_body.trim() {
        TestResult::Pass
    } else {
        // Compute a short diff hint
        let diff_hint = compute_diff_hint(&expected_body, &actual_body);
        TestResult::Fail {
            expected: expected_body,
            actual: actual_body,
            diff_hint,
        }
    }
}

/// Simple synchronous template expansion for test harness.
/// Replaces {{TemplateName|arg1=val1|arg2=val2}} with the template content.
fn expand_simple_templates(wikitext: &str, articles: &HashMap<String, String>) -> String {
    if !wikitext.contains("{{") {
        return wikitext.to_string();
    }

    let mut result = String::with_capacity(wikitext.len());
    let chars: Vec<char> = wikitext.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '{' && chars[i + 1] == '{'
            && !(i + 2 < chars.len() && chars[i + 2] == '{')
        {
            // Count brace depth to find matching }}
            let mut depth: usize = 2;
            let mut j = i + 2;
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
                let inner: String = chars[i + 2..j - 1].iter().collect();
                let parts: Vec<&str> = inner.split('|').collect();
                let name = parts.first().map(|s| s.trim()).unwrap_or("").to_string();

                // Build the template key
                let key = if name.contains(':') {
                    name.clone()
                } else {
                    format!("Template:{name}")
                };

                // Also try without Template: prefix
                if let Some(template_text) = articles.get(&key).or_else(|| articles.get(&name)) {
                    // Substitute arguments
                    let mut substituted = template_text.clone();

                    // Handle positional args with index (1-based)
                    let mut args_map: HashMap<String, String> = HashMap::new();
                    for (idx, part) in parts.iter().skip(1).enumerate() {
                        if let Some(eq_pos) = part.find('=') {
                            let key = part[..eq_pos].trim().to_string();
                            let value = part[eq_pos + 1..].to_string();
                            args_map.insert(key, value);
                        } else {
                            let pos = (idx + 1).to_string();
                            args_map.insert(pos, (*part).to_string());
                        }
                    }

                    // Perform {{{1}}}, {{{name}}} substitution
                    substituted = substitute_template_args(&substituted, &args_map);
                    result.push_str(&substituted);
                } else {
                    // Unknown template — keep as-is
                    result.push_str(&chars[i..=j].iter().collect::<String>());
                }
                i = j + 1;
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Replace {{{arg}}} in template text with values.
fn substitute_template_args(text: &str, args: &HashMap<String, String>) -> String {
    if !text.contains("{{{") {
        return text.to_string();
    }

    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if i + 2 < chars.len() && chars[i] == '{' && chars[i + 1] == '{' && chars[i + 2] == '{' {
            let mut depth: usize = 3;
            let mut j = i + 3;
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
                let inner: String = chars[i + 3..j - 2].iter().collect();
                let arg_ref = crate::expand::tpl_args::parse_arg_reference(&inner);
                let val = args
                    .get(&arg_ref.name)
                    .cloned()
                    .or(arg_ref.default)
                    .unwrap_or_else(|| format!("{{{}}}", arg_ref.name));
                result.push_str(&val);
                i = j + 1;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Run a selser (selective serialization) test. (selective serialization) test.
fn run_selser_test(test: &ParserTestCase, _test_file: &ParserTestFile) -> TestResult {
    if test.wikitext.is_empty() {
        return TestResult::Skip("no wikitext input".to_string());
    }

    let _expected_edited = match test.wikitext_edited.as_ref() {
        Some(w) => w.clone(),
        None => return TestResult::Skip("no expected edited wikitext".to_string()),
    };

    // For now, selser tests are skipped — they need the full VE pipeline
    TestResult::Skip("selser not yet fully supported".to_string())
}

/// Extract the body content from a full HTML document.
fn extract_body(html: &str) -> String {
    let html = html.trim();
    if let Some(body_start) = html.find("<body>") {
        let after_body = &html[body_start + 6..];
        if let Some(body_end) = after_body.rfind("</body>") {
            return after_body[..body_end].trim().to_string();
        }
    }
    // No body tags — return content inside <html> wrapper if present
    if let Some(_html_start) = html.find("<html")
        && let Some(after_head) = html.find("<body")
        && let Some(content_start) = html[after_head..].find('>')
    {
        let start = after_head + content_start + 1;
        if let Some(body_end) = html[start..].rfind("</body>") {
            return html[start..start + body_end].trim().to_string();
        }
    }
    html.to_string()
}

/// Compute a short diff hint for test failures.
/// Strip data-parsoid and data-mw attributes for comparison normalization.
fn strip_parsoid_attrs(html: &str) -> String {
    let mut s = html.to_string();
    // Simple manual stripping: remove data-parsoid='...' and data-mw='...'
    while let Some(start) = s.find(" data-parsoid='") {
        if let Some(end) = s[start + 15..].find("'") {
            s.replace_range(start..start + 15 + end + 1, "");
        } else {
            break;
        }
    }
    while let Some(start) = s.find(" data-mw='") {
        if let Some(end) = s[start + 11..].find("'") {
            s.replace_range(start..start + 11 + end + 1, "");
        } else {
            break;
        }
    }
    s
}

fn compute_diff_hint(expected: &str, actual: &str) -> String {
    // Simple: find the first character position where they differ
    let expected_chars: Vec<char> = expected.chars().collect();
    let actual_chars: Vec<char> = actual.chars().collect();
    let min_len = expected_chars.len().min(actual_chars.len());

    for i in 0..min_len {
        if expected_chars[i] != actual_chars[i] {
            let exp_context: String = expected_chars
                [i.saturating_sub(10)..(i + 10).min(expected_chars.len())]
                .iter()
                .collect();
            let act_context: String = actual_chars
                [i.saturating_sub(10)..(i + 10).min(actual_chars.len())]
                .iter()
                .collect();
            return format!("char {i}: expected \"{exp_context}\", got \"{act_context}\"");
        }
    }

    if expected_chars.len() != actual_chars.len() {
        format!(
            "length mismatch: expected {}, got {}",
            expected_chars.len(),
            actual_chars.len()
        )
    } else {
        "unknown difference".to_string()
    }
}
