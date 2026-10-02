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

/// One stylesheet as Parsoid rendered it: the revision it was rendered from
/// and the minified CSS body.
struct Sheet {
    revid: u64,
    css: String,
}

/// Every stylesheet Parsoid emitted, keyed by `(src, wrapper)`.
///
/// Parsoid renders a given stylesheet identically wherever it appears, except
/// that an empty stylesheet yields an empty `<style>`. Asserting the first part
/// guards the extraction: if a `src` were read from the wrong node, every
/// comparison below would be meaningless.
fn parsoid_stylesheets(pages: &Path) -> BTreeMap<(String, Option<String>), Sheet> {
    let mut out: BTreeMap<(String, Option<String>), Sheet> = BTreeMap::new();
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
        // The oracle bodies have been written under several filename schemes
        // (`html__…`, `html:…`); every `html*` file is a Parsoid rendering.
        if !name.starts_with("html") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut rest = text.as_str();
        while let Some(start) = rest.find("data-mw-deduplicate=\"TemplateStyles:r") {
            let after = &rest[start..];
            // The dedup key is `TemplateStyles:r<revid>` optionally followed by
            // `/mw-parser-output/<wrapper>`.
            let key_start = "data-mw-deduplicate=\"".len();
            let Some(key_end) = after[key_start..].find('"') else {
                break;
            };
            let key = &after[key_start..key_start + key_end];
            let Some(rest_key) = key.strip_prefix("TemplateStyles:r") else {
                rest = &after[1..];
                continue;
            };
            let (revid, wrapper) = match rest_key.split_once("/mw-parser-output/") {
                Some((r, w)) => (r.parse::<u64>().ok(), Some(w.to_string())),
                None => (rest_key.parse::<u64>().ok(), None),
            };
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
            if !css.is_empty()
                && let Some(revid) = revid
            {
                let sheet = Sheet { revid, css };
                if let Some(seen) = out.get(&(src.clone(), wrapper.clone())) {
                    assert_eq!(seen.css, sheet.css, "{src} rendered differently on {name}");
                } else {
                    out.insert((src, wrapper), sheet);
                }
            }
            rest = &after[gt + close..];
        }
    }
    out
}

/// The revision the cache holds a stylesheet page's source at, read from the
/// cache manifest. `None` when the page is not cached or carries no revision.
fn cached_revid(pages: &Path, src: &str) -> Option<u64> {
    let (ns, rest) = match src.split_once(':') {
        Some((ns, rest)) => (ns, rest),
        None => ("Template", src),
    };
    let index = pages.parent()?.join("index.json");
    let text = std::fs::read_to_string(index).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("entries")?
        .get(format!("page:{ns}:{rest}"))?
        .get("revid")?
        .as_u64()
}

/// The cached source of a stylesheet page, addressed by its `src` value.
///
/// A bare `src` lives in the Template namespace (`$wgTemplateStylesDefaultNamespace`).
/// The compare crate encodes cache keys into filenames with one of three
/// schemes (a corpus cache is hours of rate-limited fetching, so older names are
/// still read); this crate does not depend on it, so the schemes are mirrored
/// here.
fn cached_source(pages: &Path, src: &str) -> Option<String> {
    let (ns, rest) = match src.split_once(':') {
        Some((ns, rest)) => (ns, rest),
        None => ("Template", src),
    };
    let key = format!("page:{ns}:{rest}");
    for stem in candidate_stems(&key) {
        if let Ok(s) = std::fs::read_to_string(pages.join(format!("{stem}.txt"))) {
            return Some(s);
        }
    }
    None
}

/// The three filename stems the cache may have written for `key`, in priority
/// order: the injective `escape_body_stem`, then `sanitize_path_separators`, then
/// the `:`-doubling legacy layout.
fn candidate_stems(key: &str) -> Vec<String> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut escaped = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        if b.is_ascii()
            && !b.is_ascii_uppercase()
            && !matches!(b, b'%' | b'/' | b'\\' | b'\0' | b'~')
        {
            escaped.push(b as char);
        } else {
            escaped.push('%');
            escaped.push(HEX[(b >> 4) as usize] as char);
            escaped.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    vec![
        escaped,
        key.replace(['/', '\\', '\0'], "_").replace("..", "__"),
        key.replace(':', "__"),
    ]
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
    let mut drift = 0usize;
    let mut failures = Vec::new();
    for ((src, wrapper), want) in &expected {
        let Some(source) = cached_source(&pages, src) else {
            continue;
        };
        // The wiki edits a stylesheet after an article's oracle was captured.
        // The cache holds only the *latest* source, so comparing it against an
        // older rendering would report the wiki's own edit as a render
        // difference. Skip those rather than pretend to measure them.
        if let Some(src_revid) = cached_revid(&pages, src)
            && src_revid != want.revid
        {
            drift += 1;
            continue;
        }
        checked += 1;
        let got = render(&source, wrapper.as_deref());
        if got == want.css {
            continue;
        }
        let at = got
            .bytes()
            .zip(want.css.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.css.len()));
        let window = |s: &str| {
            let start = floor_char(s, at.saturating_sub(40));
            let end = floor_char(s, (at + 80).min(s.len()));
            format!("{:?}", &s[start..end])
        };
        failures.push(format!(
            "{src} differs at byte {at}\n  want: {}\n  got:  {}",
            window(&want.css),
            window(&got),
        ));
    }
    assert!(checked > 0, "cache holds no stylesheet sources");
    if drift > 0 {
        eprintln!("skipped {drift} stylesheet(s): cached source is newer than the oracle");
    }
    assert!(
        failures.is_empty(),
        "{} of {checked} stylesheet(s) mismatch:\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!("{checked} stylesheet(s) byte-identical to Parsoid");
}
