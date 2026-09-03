//! Parsoid parser test harness.
//!
//! Reads `parserTests.txt` files in the format used by the official Parsoid
//! test suite and runs each test case through the rustoid V2 parser, comparing
//! output against expected HTML/wikitext.
//!
//! This module lives under `tests/` (test-only) because it depends on a Tokio
//! runtime to drive the async template-expansion path. It is not part of the
//! public library.
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

// This module is shared across multiple test binaries (integration tests and
// debug tooling). Not every binary uses every entry point, so dead-code
// analysis is intentionally suppressed here.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use rustoid_core::error::Result;
use rustoid_core::error::RustoidError;
use rustoid_core::mock::{MockDataSource, MockSiteConfig};
use rustoid_core::options::ParserOptions;
use rustoid_core::pipeline::parser::Parser;
use rustoid_core::traits::FileInfo;

// ---------------------------------------------------------------------------
// Standard mock media files (faithful to `MockApiHelper::FILE_PROPS`)
// ---------------------------------------------------------------------------

/// Seed the mock data source with the standard image files Parsoid's own test
/// runner hardcodes in `MockApiHelper` (`FILE_PROPS` / `imageInfo`). Each file
/// gets its natural dimensions and the `http://example.com/images/<md5-prefix>`
/// raw URL, plus thumbnail URLs for the widths the media fixture exercises.
fn seed_media_files(source: &MockDataSource) {
    const BASE: &str = "http://example.com/images";

    // (canonical title, file name as it appears in the URL, width, height, mime,
    //  md5 prefix dirs, natural/thumb URL template).
    let files: &[(&str, &str, u32, u32, &str, &str)] = &[
        (
            "File:Foobar.jpg",
            "Foobar.jpg",
            1941,
            220,
            "image/jpeg",
            "3/3a",
        ),
        (
            "File:File_&_file.jpg",
            "File_%26_file.jpg",
            1941,
            220,
            "image/jpeg",
            "7/74",
        ),
        ("File:Thumb.png", "Thumb.png", 135, 135, "image/png", "e/ea"),
        (
            "File:Foobar.svg",
            "Foobar.svg",
            240,
            180,
            "image/svg+xml",
            "f/ff",
        ),
        ("File:Bad.jpg", "Bad.jpg", 320, 240, "image/jpeg", "0/09"),
        (
            "File:LoremIpsum.djvu",
            "LoremIpsum.djvu",
            2480,
            3508,
            "image/vnd.djvu",
            "5/5f",
        ),
        (
            "File:Hi-ho.jpg",
            "Hi-ho.jpg",
            1941,
            220,
            "image/jpeg",
            "9/9d",
        ),
        ("File:Tall.jpg", "Tall.jpg", 400, 600, "image/jpeg", "8/88"),
    ];

    for (title, fname, width, height, mime, prefix) in files {
        let file_url = format!("{BASE}/{prefix}/{fname}");
        let mut thumb_urls = HashMap::new();
        // Populate thumbnails for a representative set of widths (the fixture
        // exercises 50/120/137/180/220/274/320/360/440/…-px variants). The
        // exact value for a given width is recomputed by the parser's
        // `handle_size`; here we only need the *URL* string.
        for w in [50u32, 100, 120, 137, 180, 220, 240, 274, 320, 360, 440] {
            thumb_urls.insert(
                w.to_string(),
                format!("{BASE}/thumb/{prefix}/{fname}/{w}px-{fname}"),
            );
        }
        source.add_file(
            title,
            FileInfo {
                title: (*fname).to_string(),
                mime_type: (*mime).to_string(),
                size: 0,
                width: *width,
                height: *height,
                description_url: format!("{BASE}/{fname}"),
                file_url,
                thumb_urls,
                bad_file: *fname == "Bad.jpg",
            },
        );
    }
}

/// Extract the target of a `#REDIRECT [[Target]]` article, if `text` is a
/// redirect. Mirrors the redirect-detection used by the MediaWiki API when it
/// reports redirects.
fn redirect_target(text: &str) -> Option<String> {
    let mut rest = text.trim_start();
    rest = rest
        .strip_prefix("#REDIRECT")
        .or_else(|| rest.strip_prefix("#redirect"))?;
    rest = rest.trim_start();
    let start = rest.find("[[")?;
    let inner = &rest[start + 2..];
    let end = inner.find("]]")?;
    let target = inner[..end].trim();
    if target.is_empty() {
        None
    } else {
        Some(target.to_string())
    }
}

// ---------------------------------------------------------------------------
// Test case representation
// ---------------------------------------------------------------------------

/// A single parser test case.
#[derive(Debug, Clone, Default)]
pub struct ParserTestCase {
    pub description: String,
    pub options_raw: String,
    pub options: HashMap<String, String>,
    /// Raw `!! config` lines (MediaWiki config values like
    /// `wgParsoidExperimentalParserFunctionOutput=true`).
    pub config_raw: String,
    pub wikitext: String,
    pub html_parsoid: Option<String>,
    pub html_php: Option<String>,
    pub html_parsoid_lang: Option<(String, String)>,
    pub wikitext_edited: Option<String>,
    pub line_number: usize,
    /// Whether this test is Parsoid-only (has an `html/parsoid`,
    /// `html/parsoid+standalone`, `html/parsoid+integrated`, or
    /// `html/parsoid+langconv` section). Mirrors PHP `Test::normalizeHTML`'s
    /// `parsoidOnly` flag, which selects between `normalizeOut` (Parsoid-only)
    /// and `normalizeHTML` (legacy) expected-output normalization.
    pub parsoid_only: bool,
}

/// A parsed test file.
#[derive(Debug, Clone, Default)]
pub struct ParserTestFile {
    pub tests: Vec<ParserTestCase>,
    pub articles: HashMap<String, String>,
    pub path: String,
    pub version: String,
    /// Known divergences between Parsoid and the legacy parser, recorded in a
    /// sibling `*-standalone-knownFailures.json` file (mirroring Parsoid's own
    /// `tests/parser/*-standalone-knownFailures.json`). Each entry maps a test
    /// description to the Parsoid output it is *expected* to produce in a given
    /// mode; when a test's actual (normalized) output matches the recorded
    /// value, it is accepted as an expected divergence rather than a failure.
    pub known_failures: KnownFailures,
}

/// A known-failures file: test description → mode → recorded Parsoid output.
///
/// Mirrors the shape of Parsoid's `*-standalone-knownFailures.json`, where each
/// value is a map of mode (`wt2html`, `html2wt`, `wt2wt`, …) to the output
/// string Parsoid actually produces (which differs from the fixture's `!! html`
/// legacy expectation).
#[derive(Debug, Clone, Default)]
pub struct KnownFailures {
    pub entries: HashMap<String, HashMap<String, String>>,
}

impl KnownFailures {
    /// Look up the recorded Parsoid output for a test in a given mode.
    pub fn get(&self, test_name: &str, mode: &str) -> Option<&str> {
        self.entries
            .get(test_name)
            .and_then(|modes| modes.get(mode))
            .map(String::as_str)
    }
}

/// The outcome of running a single test.
#[derive(Debug, Clone)]
pub enum TestResult {
    Pass,
    Fail {
        expected: String,
        actual: String,
        diff_hint: String,
    },
    Skip(String),
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
    let content = std::fs::read_to_string(path)
        .map_err(|e| RustoidError::DataSource(format!("Cannot read {path:?}: {e}")))?;

    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    // Parse version
    let mut version = String::new();
    while i < lines.len() {
        let line = lines[i].trim();
        if let Some(stripped) = line.strip_prefix("!! Version") {
            version = stripped.trim().to_string();
        }
        if line == "!! Version 2"
            || line.starts_with("!! article")
            || line == "!!article"
            || line.starts_with("!! test")
            || line == "!!test"
        {
            break;
        }
        i += 1;
    }

    let mut articles = HashMap::new();
    let mut tests = Vec::new();

    // Parse articles and tests
    while i < lines.len() {
        let line = lines[i].trim();

        // Accept both `!!article` and `!! article` (and the same for text/end).
        let article_start = line == "!! article" || line == "!!article";
        if article_start {
            i += 1;
            let name = lines.get(i).map(|l| l.trim()).unwrap_or("");
            i += 1;
            let text_kw = lines.get(i).map(|l| l.trim()).unwrap_or("");
            if text_kw == "!! text" || text_kw == "!!text" {
                i += 1;
                let mut text = String::new();
                while i < lines.len()
                    && lines[i].trim() != "!! endarticle"
                    && lines[i].trim() != "!!endarticle"
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
        known_failures: load_known_failures(path),
        path: path.to_string_lossy().to_string(),
        version,
    })
}

/// Load a sibling `*-standalone-knownFailures.json` file, if present.
///
/// Parsoid ships each fixture with a corresponding known-failures file that
/// records cases where its native (standalone) output diverges from the legacy
/// `!! html` expectation. We honor those so the harness reflects Parsoid's own
/// pass/fail accounting rather than treating a faithful divergence as a bug.
fn load_known_failures(path: &Path) -> KnownFailures {
    // `foo.txt` → `foo-standalone-knownFailures.json` (and `.txt` is stripped
    // so `php/foo.txt` maps to `php/foo-standalone-knownFailures.json`).
    let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
        return KnownFailures::default();
    };
    let sidecar = path.with_file_name(format!("{stem}-standalone-knownFailures.json"));
    let Ok(text) = std::fs::read_to_string(&sidecar) else {
        return KnownFailures::default();
    };

    // Tolerate a missing/empty file; only parse well-formed JSON.
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return KnownFailures::default();
    };
    let Some(obj) = json.as_object() else {
        return KnownFailures::default();
    };

    let mut entries = HashMap::new();
    for (name, value) in obj {
        let Some(modes) = value.as_object() else {
            continue;
        };
        let mut map = HashMap::new();
        for (mode, output) in modes {
            if let Some(s) = output.as_str() {
                map.insert(mode.clone(), s.to_string());
            }
        }
        entries.insert(name.clone(), map);
    }
    KnownFailures { entries }
}

/// Parse a single test case starting after the !! test line.
fn parse_test_case(lines: &[&str], i: &mut usize, description: String) -> Result<ParserTestCase> {
    let mut test = ParserTestCase {
        description,
        line_number: *i + 1,
        ..Default::default()
    };

    let mut options_lines = Vec::new();
    let mut config_lines = Vec::new();
    let mut wikitext_lines = Vec::new();
    let mut html_parsoid_lines = Vec::new();
    let mut html_php_lines = Vec::new();
    let mut html_parsoid_lang_lines = Vec::new();
    let mut wikitext_edited_lines = Vec::new();

    let mut section = Section::None;
    let mut parsoid_only = false;

    while *i < lines.len() {
        let line = lines[*i];
        let trimmed = line.trim();

        match trimmed {
            "!! options" => {
                section = Section::Options;
                *i += 1;
            }
            "!! config" => {
                section = Section::Config;
                *i += 1;
            }
            "!! wikitext" => {
                section = Section::Wikitext;
                *i += 1;
            }
            "!! metadata" | "!! metadata/php" | "!! metadata/parsoid+standalone" => {
                // Tracking categories/links emitted by the parser (e.g.
                // `cat=Pages_with_broken_file_links sort=`). Not part of the
                // rendered HTML; skipped by the harness (mirrors the PHP
                // TestRunner, which compares metadata separately).
                section = Section::Metadata;
                *i += 1;
            }
            "!! html/parsoid" | "!! html/parsoid here" => {
                section = Section::Html;
                parsoid_only = true;
                *i += 1;
            }
            "!! html" | "!! html/*" => {
                // Generic HTML: canonical output. Served as BOTH the Parsoid
                // and legacy expected output (the two parsers agree here),
                // mirroring Parsoid's PARSOID_HTML_KEYS / LEGACY_HTML_KEYS.
                section = Section::HtmlBoth;
                *i += 1;
            }
            "!! html/php" => {
                section = Section::HtmlPhp;
                *i += 1;
            }
            "!! html/parsoid+lang" => {
                section = Section::HtmlLang;
                parsoid_only = true;
                *i += 1;
            }
            "!! html/parsoid+integrated" => {
                // The integrated (production) Parsoid mode, which registers
                // extension tags (so #tag:pre → mw:Extension/pre).
                section = Section::HtmlIntegrated;
                parsoid_only = true;
                *i += 1;
            }
            "!! html/parsoid+standalone" => {
                // The standalone (native) Parsoid mode, where extension tags
                // are not registered (so #tag:pre is a plain <pre>).
                section = Section::HtmlStandalone;
                parsoid_only = true;
                *i += 1;
            }
            "!! wikitext/edited" => {
                section = Section::WikitextEdited;
                *i += 1;
            }
            "!! end" | "!!end" => {
                *i += 1;
                break;
            }
            _ => {
                if trimmed.starts_with("!! html") {
                    // Unknown html section — skip
                    section = Section::None;
                    *i += 1;
                } else if section == Section::None && trimmed.starts_with('#') {
                    // Comment line (only at the top level, between tests).
                    // Inside a wikitext/html section, a `#`-prefixed line is
                    // content (e.g. `#REDIRECT`, ordered-list items).
                    *i += 1;
                } else {
                    match section {
                        Section::Options => options_lines.push(line.to_string()),
                        Section::Config => config_lines.push(line.to_string()),
                        Section::Wikitext => wikitext_lines.push(line.to_string()),
                        Section::Html => html_parsoid_lines.push(line.to_string()),
                        Section::HtmlIntegrated => html_parsoid_lines.push(line.to_string()),
                        Section::HtmlStandalone => { /* standalone: ignore (integrated takes precedence) */
                        }
                        Section::HtmlPhp => html_php_lines.push(line.to_string()),
                        Section::HtmlBoth => {
                            html_parsoid_lines.push(line.to_string());
                            html_php_lines.push(line.to_string());
                        }
                        Section::HtmlLang => html_parsoid_lang_lines.push(line.to_string()),
                        Section::WikitextEdited => wikitext_edited_lines.push(line.to_string()),
                        Section::Metadata => { /* tracking metadata: not compared */ }
                        Section::None => { /* skip */ }
                    }
                    *i += 1;
                }
            }
        }
    }

    // Assemble collected lines
    test.options_raw = options_lines.join("\n");
    test.config_raw = config_lines.join("\n");
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

    // `parsoidOnly` (mirrors PHP `Test::normalizeHTML`): an explicit
    // `html/parsoid` (+standalone/+integrated/+langconv) section, or a `parsoid`
    // option without `normalizePhp`.
    test.parsoid_only = parsoid_only
        || (test.options.contains_key("parsoid") && !test.options_raw.contains("normalizePhp"));

    Ok(test)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Options,
    Config,
    Wikitext,
    Html,
    HtmlIntegrated,
    HtmlStandalone,
    HtmlPhp,
    HtmlBoth,
    HtmlLang,
    WikitextEdited,
    Metadata,
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
    for part in raw.split([',', '\n']) {
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

    if modes.contains(&"wt2wt") {
        return run_wt2wt_test(test, test_file);
    }
    if modes.contains(&"html2wt") {
        return run_html2wt_test(test, test_file);
    }

    if supports_wt2html && (test.html_parsoid.is_some() || test.html_php.is_some()) {
        return run_wt2html_test(test, test_file);
    }

    // If wikitext/edited is provided, run selser
    if test.wikitext_edited.is_some() {
        return run_selser_test(test, test_file);
    }

    TestResult::Skip(format!("unsupported mode: {mode}"))
}

/// Run a wikitext → HTML test using the V2 parser.
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

    // Build mock data source with test file articles.
    let source = MockDataSource::new();
    seed_media_files(&source);
    for (name, text) in &test_file.articles {
        if name.starts_with("Template:") {
            source.add_template(name, text);
        } else if let Some(target) = redirect_target(text) {
            // A `#REDIRECT [[Target]]` article: register it as both a page and a
            // redirect mapping so `resolve_redirect` follows it.
            source.add_page(name, text);
            source.add_redirect(name, &target);
        } else {
            source.add_page(name, text);
            if !name.contains(':') {
                source.add_template(&format!("Template:{name}"), text);
            }
        }
    }

    // Add the page being tested.
    let page_title = test
        .options
        .get("title")
        .cloned()
        .unwrap_or_else(|| "TestPage".to_string());
    source.add_page(&page_title, &test.wikitext);

    let mut config = MockSiteConfig::new();
    // Apply `!! config` MediaWiki config values (e.g.
    // `wgParsoidExperimentalParserFunctionOutput=true`).
    for line in test.config_raw.lines() {
        let line = line.trim();
        if line == "wgParsoidExperimentalParserFunctionOutput=true" {
            config.set_parsoid_experimental_parser_function_output(true);
        } else if let Some(v) = line.strip_prefix("wgExternalLinkTarget=") {
            config.set_external_link_target(v.trim_matches('"'));
        } else if let Some(v) = line.strip_prefix("wgNoFollowLinks=") {
            config.set_no_follow_links(v.trim_matches('"') == "true");
        } else if let Some(v) = line.strip_prefix("wgNoFollowDomainExceptions=") {
            // The config value is a JSON array (`["example.com", ...]`).
            let v = v.trim();
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(v) {
                for domain in arr {
                    config.add_no_follow_domain_exception(&domain);
                }
            } else {
                for domain in v.split(',') {
                    config.add_no_follow_domain_exception(domain.trim().trim_matches('"'));
                }
            }
        }
    }
    // The `i18next` option registers the `i18ntag`/`i18nattr` extension tags
    // (mirrors `SiteConfig::registerParserTestExtension(I18nTag::class)`).
    if test
        .options
        .get("i18next")
        .is_some_and(|v| v == "1" || v == "true")
    {
        config.add_extension_tag("i18ntag");
        config.add_extension_tag("i18nattr");
    }
    let parser = Parser::new(&config);

    // The `parsoid` option can enable section wrapping via a JSON object like
    // `parsoid={ "wrapSections": true }`. Mirror that here.
    let wrap_sections = test.options_raw.contains("wrapSections\": true")
        || test.options_raw.contains("wrapSections\":true");

    let options = ParserOptions {
        page_title: page_title.clone(),
        language: test
            .options
            .get("language")
            .cloned()
            .unwrap_or_else(|| "en".to_string()),
        // The `html/parsoid` sections are bare fragments (no document wrapper).
        body_only: true,
        wrap_sections,
        ..ParserOptions::default()
    };

    // Run the async, template-expanding parse on the caller's Tokio runtime.
    let parse_result = if let Ok(rt) = tokio::runtime::Runtime::new() {
        rt.block_on(parser.wikitext_to_html_expanded(&test.wikitext, &source, &options))
    } else {
        return TestResult::Error("failed to build tokio runtime".to_string());
    };

    let actual_html = match parse_result {
        Ok(h) => h,
        Err(e) => return TestResult::Error(format!("parse error: {e}")),
    };

    let result = compare_html(&actual_html, &expected_html, test.parsoid_only);
    if let TestResult::Fail { actual, .. } = &result {
        // A known divergence: Parsoid's own `*-standalone-knownFailures.json`
        // records the output it actually produces here (which differs from the
        // fixture's legacy `!! html`). If our normalized output matches that
        // recorded value, the parser is faithful — accept it as an expected
        // divergence instead of failing.
        if let Some(recorded) = test_file.known_failures.get(&test.description, "wt2html") {
            let recorded_norm = normalize_html(&extract_body(recorded), test.parsoid_only);
            if actual.trim() == recorded_norm.trim() {
                return TestResult::Skip("known Parsoid divergence (standalone)".to_string());
            }
        }
    }
    result
}

/// Normalize and compare the actual V2 output against expected Parsoid HTML.
fn compare_html(actual_html: &str, expected_html: &str, parsoid_only: bool) -> TestResult {
    let actual_body = extract_body(actual_html);
    let expected_body = if expected_html.contains("<!DOCTYPE") {
        extract_body(expected_html)
    } else {
        expected_html.to_string()
    };

    let actual_norm = normalize_html(&actual_body, parsoid_only);
    let expected_norm = normalize_html(&expected_body, parsoid_only);

    if actual_norm.trim() == expected_norm.trim() {
        TestResult::Pass
    } else {
        let diff_hint = compute_diff_hint(&expected_norm, &actual_norm);
        TestResult::Fail {
            expected: expected_norm,
            actual: actual_norm,
            diff_hint,
        }
    }
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
    if html.find("<html").is_some()
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

/// Run a wikitext → wikitext (wt2wt) round-trip test: parse to AST, then
/// serialize back, comparing against the input wikitext.
fn run_wt2wt_test(test: &ParserTestCase, _test_file: &ParserTestFile) -> TestResult {
    if test.wikitext.is_empty() {
        return TestResult::Skip("no wikitext input".to_string());
    }
    let config = MockSiteConfig::new();
    let parser = Parser::new(&config);

    let wrap_sections = test.options_raw.contains("wrapSections\": true")
        || test.options_raw.contains("wrapSections\":true");

    let ast = match parser.wikitext_to_ast(&test.wikitext, wrap_sections) {
        Ok(ast) => ast,
        Err(e) => return TestResult::Error(format!("parse error: {e}")),
    };

    let page_title = test
        .options
        .get("title")
        .cloned()
        .unwrap_or_else(|| "TestPage".to_string());
    let title = rustoid_core::title::Title::new_main(&page_title);
    let env = rustoid_core::html::env::SerializerEnv::new(&config, &title);
    let actual =
        rustoid_core::html::serializer::WikitextSerializer::serialize_dom_with_env(ast, env);

    let expected = test.wikitext.clone();
    if actual == expected {
        TestResult::Pass
    } else {
        TestResult::Fail {
            expected,
            actual,
            diff_hint: String::new(),
        }
    }
}

/// Run an HTML → wikitext (html2wt) test.
fn run_html2wt_test(test: &ParserTestCase, _test_file: &ParserTestFile) -> TestResult {
    let html = match test.html_parsoid.as_ref() {
        Some(h) => h.clone(),
        None => match test.html_php.as_ref() {
            Some(h) => h.clone(),
            None => return TestResult::Skip("no HTML input".to_string()),
        },
    };
    let expected = test.wikitext.clone();
    // Parse the HTML fragment to an AST (semantic `data-parsoid` metadata is
    // NOT recoverable from stripped fixture HTML; html2wt parity is limited).
    let ast = match rustoid_core::html::parse::parse_html(&html) {
        Ok(ast) => ast,
        Err(e) => return TestResult::Error(format!("html parse error: {e}")),
    };
    let config = MockSiteConfig::new();
    let page_title = test
        .options
        .get("title")
        .cloned()
        .unwrap_or_else(|| "TestPage".to_string());
    let title = rustoid_core::title::Title::new_main(&page_title);
    let env = rustoid_core::html::env::SerializerEnv::new(&config, &title);
    let actual =
        rustoid_core::html::serializer::WikitextSerializer::serialize_dom_with_env(ast, env);
    if actual == expected {
        TestResult::Pass
    } else {
        TestResult::Fail {
            expected,
            actual,
            diff_hint: String::new(),
        }
    }
}

fn run_selser_test(test: &ParserTestCase, _test_file: &ParserTestFile) -> TestResult {
    if test.wikitext.is_empty() {
        return TestResult::Skip("no wikitext input".to_string());
    }

    let expected_edited = match test.wikitext_edited.as_ref() {
        Some(w) => w.clone(),
        None => return TestResult::Skip("no expected edited wikitext".to_string()),
    };
    let _ = expected_edited;

    // For now, selser tests are skipped — they need the full VE pipeline.
    TestResult::Skip("selser not yet fully supported".to_string())
}

/// Strip newlines in paragraph context (PHP format difference).
fn normalize_paragraphs(html: &str) -> String {
    let mut s = html.to_string();
    while let Some(pos) = s.find("\n</p>") {
        s.replace_range(pos..pos + 1, "");
    }
    while let Some(pos) = s.find("</p>\n<p>") {
        s.replace_range(pos + 4..pos + 5, "");
    }
    while let Some(pos) = s.find("</p>\n\n<p>") {
        s.replace_range(pos + 4..pos + 6, "");
    }
    while let Some(pos) = s.find("<b></b>") {
        s.replace_range(pos..pos + 7, "");
    }
    while let Some(pos) = s.find("<i></i>") {
        s.replace_range(pos..pos + 7, "");
    }
    s
}

/// A minimal HTML tree node for the IEW normalizer. The raw opening tag string
/// (including all attributes) is retained so attribute values survive.
#[derive(Debug, Clone)]
enum MNode {
    Elem {
        name: String,
        open_tag: String,
        self_closing: bool,
        children: Vec<MNode>,
    },
    Text(String),
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Lowercase the tag name from an opening tag's inner text (e.g. `a href="x"`).
fn open_name(inner: &str) -> String {
    let s = inner.trim().trim_end_matches('/').trim();
    s.split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Re-serialize an opening tag (e.g. `<pre typeof="x" about="y">`) with its
/// attributes sorted alphabetically by name, mirroring PHP's
/// `XHtmlSerializer` `sortAttrs` option (enabled by the parser-test
/// normalization). Attribute quoting is normalized per PHP's `smartQuote` rule
/// (single quotes only when the value contains `"` and no `'`, or more `"`
/// than `'`), matching `XHtmlSerializer::serializeToString`.
fn sort_open_tag_attrs(open_tag: &str) -> String {
    // Strip the leading `<` and the trailing `>` (and any `/` self-closing slash).
    let body = open_tag.strip_prefix('<').unwrap_or(open_tag);
    let (body, self_close) = match body.strip_suffix("/>") {
        Some(b) => (b, "/>"),
        None => match body.strip_suffix('>') {
            Some(b) => (b, ">"),
            None => (body, ""),
        },
    };
    let body = body.strip_suffix('/').unwrap_or(body);

    // Tag name is the first whitespace-delimited token.
    let trimmed = body.trim_start();
    let name_len = trimmed
        .find([' ', '\t', '\n', '\r'])
        .unwrap_or(trimmed.len());
    let name = &trimmed[..name_len];
    let rest = &trimmed[name_len..];

    // Split the remaining attribute text on whitespace, respecting quoted
    // values (which may contain spaces and `>`).
    let mut attrs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in rest.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    cur.push(c);
                } else if c.is_whitespace() {
                    if !cur.trim().is_empty() {
                        attrs.push(cur.trim().to_string());
                        cur.clear();
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if !cur.trim().is_empty() {
        attrs.push(cur.trim().to_string());
    }

    attrs.sort_by(|a, b| attr_name(a).cmp(attr_name(b)));

    let mut out = format!("<{name}");
    for a in &attrs {
        out.push(' ');
        out.push_str(&normalize_attr_quoting(a));
    }
    out.push_str(self_close);
    if !out.ends_with('>') {
        out.push('>');
    }
    out
}

/// Normalize a single attribute (`name="value"` or `name='value'` or a bare
/// boolean `name`) to its `XHtmlSerializer` `smartQuote` form: single quotes
/// only when the value contains `"` and (no `'` or more `"` than `'`), else
/// double quotes, with the value's entities re-escaped for the chosen quote.
fn normalize_attr_quoting(attr: &str) -> String {
    // Bare boolean attribute (no `=`): emit verbatim.
    let Some(eq) = attr.find('=') else {
        return attr.to_string();
    };
    let name = attr[..eq].trim();
    let raw_value = attr[eq + 1..].trim();

    // Strip the surrounding quotes (either single or double).
    let value = if raw_value.len() >= 2 {
        let first = raw_value.as_bytes()[0];
        let last = raw_value.as_bytes()[raw_value.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            &raw_value[1..raw_value.len() - 1]
        } else {
            raw_value
        }
    } else {
        raw_value
    };

    // smartQuote: single quotes when there is a `"` and (no `'` or more `"`
    // than `'`).
    let dq = value.matches('"').count();
    let sq = value.matches('\'').count();
    let use_single = value.contains('"') && (sq == 0 || dq > sq);

    if use_single {
        let escaped = value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('\'', "&apos;");
        format!("{name}='{escaped}'")
    } else {
        let escaped = value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('"', "&quot;");
        format!("{name}=\"{escaped}\"")
    }
}

/// Extract an attribute's name (up to the first `=`, trimmed).
fn attr_name(attr: &str) -> &str {
    attr.split('=').next().unwrap_or(attr).trim()
}

/// Decode XML/HTML character references in a *text* (non-attribute) string,
/// mirroring PHP's `DOMUtils::parseHTML` pass in `TestUtils::normalizeHTML`.
/// The decoded characters are re-escaped later by `html_escape` in
/// `serialize_iew`, so the net effect matches Parsoid's `XHtmlSerializer`
/// (which escapes `&` and `<` but leaves `>` literal).
fn decode_xml_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'&' {
            // Fast path: copy ASCII byte-by-byte (fixtures are ASCII); fall
            // back to a char boundary for the rare non-ASCII text.
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        // Scan for the terminating `;`.
        let Some(semi_rel) = s[i..].find(';') else {
            out.push_str(&s[i..]);
            break;
        };
        let semi = i + semi_rel;
        let body = &s[i + 1..semi]; // text between `&` and `;`
        let decoded: Option<String> =
            if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            } else if let Some(dec) = body.strip_prefix('#') {
                dec.parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            } else {
                match body {
                    "lt" => Some("<".to_string()),
                    "gt" => Some(">".to_string()),
                    "amp" => Some("&".to_string()),
                    "quot" => Some("\"".to_string()),
                    "apos" => Some("'".to_string()),
                    "rarr" => Some("\u{2192}".to_string()),
                    _ => None,
                }
            };
        match decoded {
            Some(d) => out.push_str(&d),
            None => out.push_str(&s[i..=semi]), // unknown entity: leave verbatim
        }
        i = semi + 1;
    }
    out
}

/// Parse a well-formed HTML fragment into a minimal tree.
fn parse_fragment(html: &str) -> Vec<MNode> {
    fn walk(html: &str, pos: &mut usize, out: &mut Vec<MNode>) {
        let bytes = html.as_bytes();
        let mut text = String::new();
        let flush = |text: &mut String, out: &mut Vec<MNode>| {
            if !text.is_empty() {
                out.push(MNode::Text(decode_xml_entities(&std::mem::take(text))));
            }
        };
        while *pos < bytes.len() {
            if bytes[*pos] != b'<' {
                // Accumulate text (assume ASCII in the test fixtures).
                text.push(bytes[*pos] as char);
                *pos += 1;
                continue;
            }
            if html[*pos..].starts_with("</") {
                flush(&mut text, out);
                return; // end tag: caller consumes it.
            }
            if html[*pos..].starts_with("<!--") {
                flush(&mut text, out);
                match html[*pos..].find("-->") {
                    Some(e) => *pos += e + 3,
                    None => *pos = bytes.len(),
                }
                continue;
            }
            let Some(gt_rel) = find_tag_end(&html[*pos..]) else {
                text.push_str(&html[*pos..]);
                *pos = bytes.len();
                break;
            };
            let open_tag_raw = html[*pos..*pos + gt_rel + 1].to_string();
            let open_tag = sort_open_tag_attrs(&open_tag_raw);
            let inner = &html[*pos + 1..*pos + gt_rel];
            let name = open_name(inner);
            let self_closing = inner.trim_end().ends_with('/') || is_void(&name);
            *pos += gt_rel + 1;
            flush(&mut text, out);
            if self_closing {
                out.push(MNode::Elem {
                    name,
                    open_tag,
                    self_closing: true,
                    children: Vec::new(),
                });
            } else {
                let mut children = Vec::new();
                walk(html, pos, &mut children);
                // Consume the matching end tag `</name>`.
                if *pos < bytes.len()
                    && bytes[*pos] == b'<'
                    && html[*pos..].starts_with("</")
                    && let Some(egt) = html[*pos..].find('>')
                {
                    *pos += egt + 1;
                }
                out.push(MNode::Elem {
                    name,
                    open_tag,
                    self_closing: false,
                    children,
                });
            }
        }
        flush(&mut text, out);
    }

    let mut pos = 0;
    let mut out = Vec::new();
    walk(html, &mut pos, &mut out);
    out
}

fn newline_around(name: &str) -> bool {
    matches!(
        name,
        "body"
            | "caption"
            | "div"
            | "dd"
            | "dt"
            | "li"
            | "p"
            | "table"
            | "tr"
            | "td"
            | "th"
            | "tbody"
            | "thead"
            | "tfoot"
            | "dl"
            | "ol"
            | "ul"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
    )
}

/// Collapse a text node's whitespace runs and strip a leading/trailing space at
/// block boundaries (mirrors `normalizeIEWVisitor`'s text handling).
fn collapse_text(s: &str, strip_leading: bool, strip_trailing: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c') {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    let mut result = out;
    strip_ws(&mut result, strip_leading, strip_trailing);
    result
}

/// Strip leading/trailing `\s+` runs, mirroring PHP's `stripLeadingWS` /
/// `stripTrailingWS` (which applies both inside and outside `<pre>`).
fn strip_ws(s: &mut String, strip_leading: bool, strip_trailing: bool) {
    if strip_leading {
        let trimmed = s.trim_start().to_string();
        *s = trimmed;
    }
    if strip_trailing {
        let trimmed = s.trim_end().to_string();
        *s = trimmed;
    }
}

/// Normalization options threaded through the tree walk (mirrors
/// `normalizeIEWVisitor`'s opts).
#[derive(Clone, Copy)]
struct NOpts {
    in_pre: bool,
    strip_le: bool,
    strip_te: bool,
}

/// Recursively normalize text nodes: collapse whitespace runs (outside
/// `<pre>`) and strip at block boundaries (inside and outside `<pre>`).
/// Mirrors `normalizeIEWVisitor` faithfully.
fn normalize_nodes(nodes: &mut [MNode], mut opts: NOpts) {
    let n = nodes.len();
    for (i, node) in nodes.iter_mut().enumerate() {
        // Only the last sibling strips trailing whitespace.
        let mut node_opts = opts;
        node_opts.strip_te = opts.strip_te && i == n - 1;

        match node {
            MNode::Text(t) => {
                if !node_opts.in_pre {
                    *t = collapse_text(t, node_opts.strip_le, node_opts.strip_te);
                } else {
                    // Inside `<pre>`: preserve newlines, but still strip the
                    // leading/trailing whitespace runs per PHP (the
                    // `stripLeadingWS`/`stripTrailingWS` regexes are applied
                    // outside the `!inPRE` guard).
                    strip_ws(t, node_opts.strip_le, node_opts.strip_te);
                }
            }
            MNode::Elem { name, children, .. } => {
                let nm = name.clone();
                let is_pre = nm == "pre";
                let next_in_pre = node_opts.in_pre || is_pre;
                let (next_le, next_te) = if is_pre {
                    // `<pre>`: preserve content newlines, but strip a trailing
                    // newline before `</pre>` (legacy parser parity).
                    (false, true)
                } else {
                    let around = newline_around(&nm);
                    (around, around)
                };
                normalize_nodes(
                    children,
                    NOpts {
                        in_pre: next_in_pre,
                        strip_le: next_le,
                        strip_te: next_te,
                    },
                );
            }
        }
        // After the first child, no more leading-whitespace stripping.
        opts.strip_le = false;
    }
}

/// Insert a single newline text node around block elements and after `<br>`,
/// mirroring `normalizeIEWVisitor`'s newline pass.
fn add_newlines(nodes: &mut Vec<MNode>) {
    for node in nodes.iter_mut() {
        if let MNode::Elem { children, .. } = node {
            add_newlines(children);
        }
    }

    let n = nodes.len();
    let mut out: Vec<MNode> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let node = std::mem::replace(&mut nodes[i], MNode::Text(String::new()));
        let block_around = matches!(&node, MNode::Elem { name, .. } if newline_around(name));
        let is_br = matches!(&node, MNode::Elem { name, .. } if name == "br");
        // A newline before this node if it's a block or br.
        if (block_around || is_br) && i > 0 {
            ensure_nl_before(&mut out);
        }
        out.push(node);
        // A newline after a block (or br): if the next sibling is a text node,
        // force its leading whitespace to a newline (mirrors PHP `addAfter`'s
        // `preg_replace('/^\\s*/', "\\n", $next->data)`); otherwise insert a
        // newline node.
        if block_around || is_br {
            if i + 1 < n {
                if let MNode::Text(t) = &mut nodes[i + 1] {
                    *t = format!("\n{}", t.trim_start());
                } else {
                    ensure_nl_after(&mut out);
                }
            } else {
                ensure_nl_after(&mut out);
            }
        }
        i += 1;
    }
    *nodes = out;
}

/// Ensure `out` does not end with whitespace, then push a newline text node.
fn ensure_nl_before(out: &mut Vec<MNode>) {
    if let Some(MNode::Text(t)) = out.last_mut() {
        *t = t.trim_end().to_string();
        if t.is_empty() {
            out.pop();
        }
    }
    if !out.is_empty() && !matches!(out.last(), Some(MNode::Text(t)) if t.ends_with('\n')) {
        out.push(MNode::Text("\n".to_string()));
    }
}

fn ensure_nl_after(out: &mut Vec<MNode>) {
    if !out.is_empty() && !matches!(out.last(), Some(MNode::Text(t)) if t.ends_with('\n')) {
        out.push(MNode::Text("\n".to_string()));
    }
}

/// Find the first `>` that terminates an HTML tag in `s`, skipping `>`
/// characters that appear inside quoted attribute values (so a literal `>` in
/// e.g. a caption-mirrored `title` attribute does not truncate the tag).
/// Mirrors the quote-aware tag scanning of a real HTML parser.
fn find_tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '>' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// HTML entity escaping for text content, mirroring Parsoid's
/// `XHtmlSerializer` (`&` → `&amp;`, `<` → `&lt;`; `>` is left literal, unlike
/// the legacy PHP parser).
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

/// Serialize the normalized tree with no further whitespace adjustments.
fn serialize_iew(nodes: &[MNode], out: &mut String) {
    for node in nodes {
        match node {
            MNode::Text(s) => out.push_str(&html_escape(s)),
            MNode::Elem {
                name,
                open_tag,
                self_closing,
                children,
            } => {
                if *self_closing {
                    out.push_str(&format!("<{name}/>"));
                } else {
                    out.push_str(open_tag);
                    serialize_iew(children, out);
                    out.push_str(&format!("</{name}>"));
                }
            }
        }
    }
}

/// Full IEW normalization: strip metadata/comments (and, for legacy
/// comparisons, the Parsoid-inserted attributes), collapse inter-element
/// whitespace, and place newlines around blocks.
fn normalize_html(html: &str, parsoid_only: bool) -> String {
    let mut stripped = strip_data_attrs(html);
    if !parsoid_only {
        stripped = strip_legacy_attrs(&stripped);
    }
    let mut nodes = parse_fragment(&stripped);
    normalize_nodes(
        &mut nodes,
        NOpts {
            in_pre: false,
            strip_le: false,
            strip_te: false,
        },
    );
    add_newlines(&mut nodes);
    let mut out = String::new();
    serialize_iew(&nodes, &mut out);
    out
}

/// Strip only the round-trip metadata (`data-parsoid`, `data-mw`) and HTML
/// comments, plus the parsoid-only "unnecessary" attributes that PHP's
/// `normalizeOut` removes even in `parsoidOnly` mode (`about`, `prefix`, `rev`,
/// `datatype`, `inlist`, `usemap`, `vocab`, `data-mw-original-href`). Structural
/// attributes (`rel`, `href`, `title`, `class`, `typeof`) are kept, so the
/// comparison is faithful to Parsoid's rendered HTML.
fn strip_data_attrs(html: &str) -> String {
    let mut s = html.to_string();
    // Strip HTML comments.
    while let Some(start) = s.find("<!--") {
        if let Some(end) = s[start..].find("-->") {
            s.replace_range(start..start + end + 3, "");
        } else {
            break;
        }
    }
    // Strip data-parsoid, data-mw, and the parsoid-only "unnecessary"
    // attributes (mirroring PHP `normalizeOut`'s `$unnecessaryAttribs` list),
    // in both single- and double-quoted forms.
    for attr in [
        "data-mw-original-href",
        "data-parsoid",
        "data-mw",
        "prefix",
        "about",
        "rev",
        "datatype",
        "inlist",
        "usemap",
        "vocab",
    ] {
        for quote in ['\'', '"'] {
            let needle = format!(" {attr}={quote}");
            while let Some(start) = s.find(&needle) {
                let after = &s[start + needle.len()..];
                if let Some(end) = after.find(quote) {
                    s.replace_range(start..start + needle.len() + end + 1, "");
                } else {
                    break;
                }
            }
        }
    }
    s
}

/// Strip Parsoid-inserted attributes and `<meta>`/`<link>` elements for a
/// *legacy* comparison (mirrors PHP `normalizeOut`'s `parsoidOnly = false`
/// branch). The legacy parser doesn't emit these, so they must be removed from
/// the Parsoid output before comparing against a legacy `html`/`html/php`
/// expected value.
fn strip_legacy_attrs(html: &str) -> String {
    let mut s = html.to_string();

    // Flatten `mw:Nowiki`/`mw:Entity` spans first (while their `typeof` is still
    // present): the legacy parser unwraps `<nowiki>` to plain text, so the
    // Parsoid span wrapper must be removed for a structural legacy comparison.
    s = flatten_nowiki_spans(&s);

    // Strip `<meta ...>` and `<link ...>` elements (void/self-closing).
    while let Some(start) = s.find("<meta ") {
        if let Some(end) = s[start..].find('>') {
            s.replace_range(start..start + end + 1, "");
        } else {
            break;
        }
    }
    while let Some(start) = s.find("<link ") {
        if let Some(end) = s[start..].find('>') {
            s.replace_range(start..start + end + 1, "");
        } else {
            break;
        }
    }

    // Strip Parsoid-inserted attributes (both single- and double-quoted).
    // Order matters: `typeof` is stripped last in PHP.
    for attr in [
        "data-parsoid",
        "data-mw",
        "data-mw-original-href",
        "prefix",
        "about",
        "rev",
        "datatype",
        "inlist",
        "usemap",
        "vocab",
        "resource",
        "rel",
        "property",
        "class",
        "typeof",
    ] {
        for quote in ['\'', '"'] {
            let needle = format!(" {attr}={quote}");
            while let Some(start) = s.find(&needle) {
                let after = &s[start + needle.len()..];
                if let Some(end) = after.find(quote) {
                    s.replace_range(start..start + needle.len() + end + 1, "");
                } else {
                    break;
                }
            }
        }
    }

    s
}

/// Unwrap `<span>` elements whose `typeof` is `mw:Nowiki` or `mw:Entity`, for
/// the legacy comparison. Recurses so nested `mw:Entity` spans inside a
/// `mw:Nowiki` span are unwrapped too. The content of these spans is plain text
/// (possibly nested spans), so a balanced scan for the matching `</span>` is
/// sufficient.
fn flatten_nowiki_spans(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' || !s[i..].starts_with("<span") {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let Some(gt) = s[i..].find('>') else {
            out.push_str(&s[i..]);
            break;
        };
        let open_tag = &s[i..i + gt + 1];
        let is_nowiki = open_tag.contains("mw:Nowiki") || open_tag.contains("mw:Entity");
        if !is_nowiki {
            // Not a nowiki/entity span: keep the open tag and recurse into its
            // content by advancing past the tag and continuing.
            out.push_str(open_tag);
            i += gt + 1;
            continue;
        }
        // Unwrap: find the matching `</span>` (balanced over nested spans).
        let content_start = i + gt + 1;
        let mut depth = 1usize;
        let mut j = content_start;
        let mut content_end = None;
        while j < bytes.len() {
            if s[j..].starts_with("<span") {
                depth += 1;
                if let Some(nxt) = s[j..].find('>') {
                    j += nxt + 1;
                    continue;
                }
                break;
            }
            if s[j..].starts_with("</span") {
                depth -= 1;
                if depth == 0 {
                    content_end = Some(j);
                    break;
                }
                if let Some(nxt) = s[j..].find('>') {
                    j += nxt + 1;
                    continue;
                }
                break;
            }
            j += 1;
        }
        match content_end {
            Some(end) => {
                // Recurse into the inner content (to unwrap nested entity spans).
                let inner = &s[content_start..end];
                out.push_str(&flatten_nowiki_spans(inner));
                // Skip past the closing `</span>`.
                let close_len = s[end..].find('>').map(|e| e + 1).unwrap_or(0);
                i = end + close_len;
            }
            None => {
                out.push_str(&s[i..]);
                break;
            }
        }
    }
    out
}

fn compute_diff_hint(expected: &str, actual: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_xml_entities_predefines() {
        assert_eq!(decode_xml_entities("&lt;&gt;&amp;&quot;&apos;"), "<>&\"'");
        assert_eq!(decode_xml_entities("&rarr;"), "\u{2192}");
    }

    #[test]
    fn test_decode_xml_entities_numeric() {
        assert_eq!(decode_xml_entities("&#8594;"), "\u{2192}");
        assert_eq!(decode_xml_entities("&#x2192;"), "\u{2192}");
        assert_eq!(decode_xml_entities("&#x2D;"), "-");
        assert_eq!(decode_xml_entities("&#160;"), "\u{a0}");
    }

    #[test]
    fn test_decode_xml_entities_unknown_leaves_verbatim() {
        assert_eq!(decode_xml_entities("&unknown;foo"), "&unknown;foo");
        assert_eq!(decode_xml_entities("no entities"), "no entities");
    }

    #[test]
    fn test_decode_then_re_escape_matches_xhtml_serializer() {
        // Legacy expected `&lt;President&gt;` decodes to `<President>` then
        // re-escapes to `&lt;President>` (the `>` is NOT re-escaped), exactly
        // matching Parsoid's `XHtmlSerializer` output for the same text.
        let decoded = decode_xml_entities("&lt;President&gt;");
        assert_eq!(html_escape(&decoded), "&lt;President>");
    }

    #[test]
    fn test_normalize_attr_quoting_smart_quote() {
        // No embedded quotes → double quotes.
        assert_eq!(
            normalize_attr_quoting("class='mw-empty-elt'"),
            "class=\"mw-empty-elt\""
        );
        assert_eq!(
            normalize_attr_quoting("class=\"mw-empty-elt\""),
            "class=\"mw-empty-elt\""
        );
        // Value with a `"` and no `'` → single quotes.
        assert_eq!(
            normalize_attr_quoting("title='has \"quote\"'"),
            "title='has \"quote\"'"
        );
        // Value with a `'` and a `"` → double quotes (single-count > double-count).
        assert_eq!(
            normalize_attr_quoting("title='it runs'"),
            "title=\"it runs\""
        );
        // Bare boolean attribute is left verbatim.
        assert_eq!(normalize_attr_quoting("hidden"), "hidden");
    }
}
