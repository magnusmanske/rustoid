//! Differential test: does `render` byte-match what Parsoid emitted?
//!
//! The corpus cache holds Parsoid's rendered HTML alongside the wikitext it came
//! from, so a stylesheet can be checked by extracting the `<style>` body Parsoid
//! produced and comparing it against `render` applied to the cached source.
//! Comparing against real output this way is the only trustworthy oracle: a
//! hand-written expectation silently encodes whatever the author believed.
//!
//! The test needs a populated cache, so it skips (rather than fails) when the
//! cache directory or the relevant pages are absent.

use rustoid_core::pipeline::templatestyles::render;
use std::collections::BTreeMap;
use std::path::Path;

/// Cache root, matching the harness's own default.
fn cache_pages() -> Option<std::path::PathBuf> {
    let root = std::env::var("RUSTOID_CACHE_DIR").unwrap_or_else(|_| "/tmp/rustoid-cache".into());
    let pages = Path::new(&root).join("en.wikipedia.org").join("pages");
    pages.is_dir().then_some(pages)
}

/// The offset just past the `>` that closes the tag starting at `start`.
///
/// Attribute values are quoted and `data-mw` is single-quoted JSON that can
/// itself contain `>` or `'`, so neither `find(">")` nor `find("'>")` locates
/// the tag end reliably. Scan the quotes instead.
fn tag_end(text: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, &b) in text.as_bytes()[start..].iter().enumerate() {
        match (quote, b) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, c @ (b'"' | b'\'')) => quote = Some(c),
            (None, b'>') => return Some(start + offset + 1),
            (None, _) => {}
        }
    }
    None
}

/// Every `(src, css)` Parsoid emitted, keyed by the `src` attribute.
///
/// Parsoid renders a given stylesheet identically wherever it appears, except
/// that an empty stylesheet yields an empty `<style>`. Asserting the first part
/// guards the extraction: if a `src` were read from the wrong node, every
/// comparison below would be meaningless.
fn parsoid_stylesheets(pages: &Path) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut files = std::fs::read_dir(pages)
        .expect("cache dir")
        .flatten()
        .map(|e| e.path())
        .collect::<Vec<_>>();
    files.sort();
    for path in files {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if !name.starts_with("html__") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut rest = text.as_str();
        while let Some(start) = rest.find("data-mw-deduplicate=\"TemplateStyles:r") {
            let after = &rest[start..];
            let Some(src_at) = after.find("\"attrs\":{\"src\":\"") else {
                rest = &after[1..];
                continue;
            };
            let src_start = src_at + "\"attrs\":{\"src\":\"".len();
            let Some(src_end) = after[src_start..].find('"') else {
                break;
            };
            let src = after[src_start..src_start + src_end].to_string();
            // The CSS runs from the end of the open tag to `</style>`.
            let Some(gt) = tag_end(after, 0) else {
                break;
            };
            let Some(close) = after[gt..].find("</style>") else {
                break;
            };
            let css = after[gt..gt + close].to_string();
            // Prefer a non-empty rendering: an empty stylesheet legitimately
            // yields an empty `<style>`, which would otherwise shadow the real
            // body and make the comparison below vacuous.
            if !css.is_empty() {
                if let Some(seen) = out.get(&src) {
                    assert_eq!(seen, &css, "{src} rendered differently on {name}");
                } else {
                    out.insert(src.clone(), css);
                }
            }
            rest = &after[gt + close..];
        }
    }
    out
}

/// The cached source of a stylesheet page, addressed by its `src` value.
///
/// A bare `src` lives in the Template namespace (`$wgTemplateStylesDefaultNamespace`).
fn cached_source(pages: &Path, src: &str) -> Option<String> {
    let (ns, rest) = match src.split_once(':') {
        Some((ns, rest)) => (ns, rest),
        None => ("Template", src),
    };
    let file = format!(
        "page__{}__{}.txt",
        ns.replace(' ', "_"),
        rest.replace([' ', '/'], "_")
    );
    std::fs::read_to_string(pages.join(file)).ok()
}

/// The largest char boundary at or below `i`, for slicing non-ASCII CSS.
fn floor_char(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[test]
fn render_matches_parsoid_for_every_cached_stylesheet() {
    let Some(pages) = cache_pages() else {
        eprintln!("no corpus cache; skipping");
        return;
    };
    let expected = parsoid_stylesheets(&pages);
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for (src, want) in &expected {
        let Some(source) = cached_source(&pages, src) else {
            continue;
        };
        checked += 1;
        let got = render(&source, None);
        if got == *want {
            continue;
        }
        let at = got
            .bytes()
            .zip(want.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.len()));
        let window = |s: &str| {
            let start = floor_char(s, at.saturating_sub(40));
            let end = floor_char(s, (at + 80).min(s.len()));
            format!("{:?}", &s[start..end])
        };
        failures.push(format!(
            "{src} differs at byte {at}\n  want: {}\n  got:  {}",
            window(want),
            window(&got),
        ));
    }
    assert!(checked > 0, "cache holds no stylesheet sources");
    assert!(
        failures.is_empty(),
        "{} of {checked} stylesheet(s) mismatch:\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!("{checked} stylesheet(s) byte-identical to Parsoid");
}
