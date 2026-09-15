//! A corpus of pages to compare, and the tag vocabulary used to report on it.
//!
//! The point of a corpus is that a single page is an anecdote: one page's diff
//! says *a* feature is missing, not which feature moves the needle most. A
//! corpus with tags turns that into an attribution — "9 of 30 failures are
//! transclusion, 6 are extension" — which is what decides what to build next.
//!
//! # Format
//!
//! A plain text file, one entry per line:
//!
//! ```text
//! # comments and blank lines are ignored
//! Template-heavy page | transclusion, magic-words
//! Some other page     | table
//! ```
//!
//! The `| tags` part is optional. Tags are free-form labels describing *what the
//! page is meant to exercise*; they are not parsed into an enum, because the
//! useful set of tags changes as the parser grows and a closed set would mean
//! recompiling to add one. They are lowercased and deduplicated.
//!
//! The page title is everything before the first `|`, so titles containing a
//! literal `|` (legal in MediaWiki titles) are not representable. That is a
//! deliberate simplification: a title with a pipe cannot be typed into a URL
//! without escaping, which makes it a poor corpus entry anyway.

use std::collections::BTreeSet;

use crate::error::{CompareError, Result};

/// One page to compare, with the feature areas it is intended to exercise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusEntry {
    pub title: String,
    /// Feature labels, lowercased and deduplicated, in sorted order.
    pub tags: BTreeSet<String>,
}

impl CorpusEntry {
    /// Parse one `title | tag, tag` line. Returns `None` for a blank or comment
    /// line, and an error for a line with an empty title.
    pub fn parse(line: &str) -> Result<Option<Self>> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }
        let (title, tags) = match line.split_once('|') {
            Some((t, rest)) => (t, Some(rest)),
            None => (line, None),
        };
        let title = title.trim();
        if title.is_empty() {
            return Err(CompareError::Corpus(format!(
                "entry has no title before the `|`: {line:?}"
            )));
        }
        let tags = tags
            .unwrap_or_default()
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        Ok(Some(Self {
            title: title.to_string(),
            tags,
        }))
    }
}

/// A list of pages to compare, plus a label for where it came from.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    /// Human-readable origin, for the report ("built-in", or a path).
    pub name: String,
    pub entries: Vec<CorpusEntry>,
}

impl Corpus {
    /// Parse a corpus from text.
    pub fn parse(name: impl Into<String>, text: &str) -> Result<Self> {
        let name = name.into();
        let mut entries = Vec::new();
        for (n, line) in text.lines().enumerate() {
            match CorpusEntry::parse(line) {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => {}
                Err(e) => {
                    return Err(CompareError::Corpus(format!("{name}: line {}: {e}", n + 1)));
                }
            }
        }
        Ok(Self { name, entries })
    }

    /// Read a corpus from a file.
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            CompareError::Corpus(format!("cannot read corpus {}: {e}", path.display()))
        })?;
        Self::parse(path.display().to_string(), &text)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The built-in corpus.
    ///
    /// Chosen to span the failure space rather than to be representative: each
    /// entry is here because it exercises something the parser gets wrong or
    /// might, so a regression in one area shows up as a drop in that area's
    /// number rather than as noise. These are enwiki titles; they move and get
    /// edited, which is why the harness pins revisions in the cache.
    pub fn builtin() -> Self {
        let text = include_str!("../corpus/enwiki.txt");
        Self::parse("built-in (enwiki)", text)
            .expect("the built-in corpus is compiled in, so it must parse")
    }

    /// Every tag appearing anywhere in the corpus, sorted. Used to render a
    /// scoreboard with a stable column order.
    pub fn all_tags(&self) -> BTreeSet<String> {
        self.entries
            .iter()
            .flat_map(|e| e.tags.iter())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_a_title_without_tags() {
        let e = CorpusEntry::parse("Main Page").unwrap().unwrap();
        assert_eq!(e.title, "Main Page");
        assert!(e.tags.is_empty());
    }

    #[test]
    fn parses_tags_and_normalises_them() {
        let e = CorpusEntry::parse("Foo | Tables, TRANSCLUSION , table")
            .unwrap()
            .unwrap();
        assert_eq!(e.title, "Foo");
        // Lowercased and trimmed, but not stemmed: `Tables` and `table` are
        // distinct labels, so the corpus should spell a tag one way.
        assert_eq!(e.tags, tags(&["tables", "transclusion", "table"]));
    }

    /// A tag spelled two ways in one entry collapses to one label.
    #[test]
    fn duplicate_tags_are_deduplicated() {
        let e = CorpusEntry::parse("Foo | table, TABLE , Table")
            .unwrap()
            .unwrap();
        assert_eq!(e.tags, tags(&["table"]));
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let c = Corpus::parse("t", "# a comment\n\n  \nFoo\n  # indented comment\n").unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c.entries[0].title, "Foo");
    }

    #[test]
    fn a_title_before_a_pipe_must_not_be_empty() {
        assert!(CorpusEntry::parse("| table").is_err());
        assert!(CorpusEntry::parse("   | table").is_err());
    }

    #[test]
    fn an_empty_tag_list_is_not_an_error() {
        let e = CorpusEntry::parse("Foo |").unwrap().unwrap();
        assert_eq!(e.title, "Foo");
        assert!(e.tags.is_empty());
    }

    #[test]
    fn a_parse_error_names_the_offending_line() {
        let err = Corpus::parse("mycorpus", "Foo\n| bad\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mycorpus"), "{msg}");
        assert!(msg.contains("line 2"), "{msg}");
    }

    #[test]
    fn all_tags_collects_across_entries() {
        let c = Corpus::parse("t", "A | x\nB | y, x\nC").unwrap();
        assert_eq!(c.all_tags(), tags(&["x", "y"]));
    }

    /// The built-in corpus is compiled in, so a typo in it is a build-time
    /// regression rather than a runtime surprise.
    #[test]
    fn the_builtin_corpus_parses_and_is_not_trivial() {
        let c = Corpus::builtin();
        assert!(
            c.len() >= 20,
            "built-in corpus has only {} entries",
            c.len()
        );
        assert!(!c.all_tags().is_empty());
        // Every entry needs a tag, or a failure could not be attributed.
        for e in &c.entries {
            assert!(!e.tags.is_empty(), "untagged entry: {}", e.title);
        }
        // No duplicate titles: they would double-count in the scoreboard.
        let titles: BTreeSet<&String> = c.entries.iter().map(|e| &e.title).collect();
        assert_eq!(
            titles.len(),
            c.len(),
            "duplicate titles in the built-in corpus"
        );
    }
}
