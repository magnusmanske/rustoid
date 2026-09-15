//! Tally a corpus run into a scoreboard.
//!
//! Two attributions matter, and they answer different questions:
//!
//! - **By diff category** — from the parser's own diagnosis of what diverged.
//!   This is what says "6 of 31 failures look like table problems".
//! - **By corpus tag** — from what the *page* was chosen to exercise. This is
//!   what says "every one of the 5 Lua-tagged pages fails", which a category
//!   tally cannot, because a single page yields only one category but carries
//!   several tags.
//!
//! Both are needed. Category tells you what the code did wrong; tag tells you
//! which body of work that maps onto, and whether anything passes at all.

use std::collections::BTreeMap;

use crate::harness::{Outcome, Unexpanded};

/// The result of one entry in the run.
#[derive(Debug, Clone)]
pub struct Row {
    pub title: String,
    pub tags: Vec<String>,
    /// Revision compared, or `None` if the comparison never got that far.
    pub revid: Option<u64>,
    pub outcome: Outcome,
    /// Bytes of parsoid output, for the size-gap summary.
    pub parsoid_bytes: usize,
    pub rustoid_bytes: usize,
    /// Literal unexpanded wikitext each side emitted.
    pub unexpanded_rustoid: Unexpanded,
    pub unexpanded_parsoid: Unexpanded,
}

impl Row {
    pub fn category(&self) -> &'static str {
        self.outcome.category()
    }

    pub fn is_match(&self) -> bool {
        self.outcome.is_match()
    }

    pub fn is_skipped(&self) -> bool {
        matches!(self.outcome, Outcome::Skipped { .. })
    }
}

/// One line of the by-category table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub name: String,
    pub count: usize,
}

/// Aggregate diagnosis of unexpanded wikitext across a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnexpandedSummary {
    /// Comparisons that produced two renderings.
    pub compared: usize,
    /// Pages whose rustoid output still contains literal `{{…}}` in its text.
    pub pages_with_literal_wikitext: usize,
    /// Pages whose rustoid output still contains a literal `{{#invoke:…}}`.
    pub pages_with_unexpanded_invoke: usize,
    /// Pages emitting more literal template syntax than Parsoid does.
    pub pages_worse_than_parsoid: usize,
}

/// A finished run.
#[derive(Debug, Clone, Default)]
pub struct Scoreboard {
    pub corpus_name: String,
    pub rows: Vec<Row>,
}

impl Scoreboard {
    pub fn new(corpus_name: impl Into<String>, rows: Vec<Row>) -> Self {
        Self {
            corpus_name: corpus_name.into(),
            rows,
        }
    }

    pub fn total(&self) -> usize {
        self.rows.len()
    }

    pub fn matches(&self) -> usize {
        self.rows.iter().filter(|r| r.is_match()).count()
    }

    pub fn skipped(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Skipped { .. }))
            .count()
    }

    /// Comparisons that actually produced two renderings to judge.
    pub fn compared(&self) -> usize {
        self.total() - self.skipped()
    }

    /// Percentage matched, over *compared* pages. Skips are excluded because a
    /// page that could not be fetched says nothing about parser parity, and
    /// including them would let an offline run flatter or deflate the score.
    pub fn percent(&self) -> f64 {
        let compared = self.compared();
        if compared == 0 {
            return 0.0;
        }
        self.matches() as f64 * 100.0 / compared as f64
    }

    /// Counts by [`Outcome::category`], most common first.
    pub fn by_category(&self) -> Vec<Bucket> {
        tally(self.rows.iter().map(|r| r.category().to_string()))
    }

    /// Counts by corpus tag, over pages that produced a comparison and were
    /// **not** a match.
    ///
    /// Skips are excluded: a page that could not be fetched says nothing about
    /// parser parity, and crediting its tags would fill the work list with
    /// entries that have nothing wrong with them. Counting failures rather than
    /// pages, because a page with three tags that fails should add one to each
    /// of the three — and a page yields only a single category.
    pub fn failures_by_tag(&self) -> Vec<Bucket> {
        tally(
            self.rows
                .iter()
                .filter(|r| !r.is_match() && !r.is_skipped())
                .flat_map(|r| r.tags.iter().cloned()),
        )
    }

    /// Pass/total per tag, so a tag that is nearly working is distinguishable
    /// from one that has never worked. Skips are excluded from `total` for the
    /// same reason as above.
    pub fn by_tag(&self) -> Vec<(String, usize, usize)> {
        let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for row in self.rows.iter().filter(|r| !r.is_skipped()) {
            for tag in &row.tags {
                let e = counts.entry(tag.clone()).or_insert((0, 0));
                e.1 += 1;
                if row.is_match() {
                    e.0 += 1;
                }
            }
        }
        let mut out: Vec<(String, usize, usize)> = counts
            .into_iter()
            .map(|(tag, (pass, total))| (tag, pass, total))
            .collect();
        // Worst first: the point of the table is to show what to work on.
        out.sort_by(|a, b| {
            let ra = a.1 as f64 / a.2.max(1) as f64;
            let rb = b.1 as f64 / b.2.max(1) as f64;
            ra.partial_cmp(&rb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        out
    }

    /// Sum of both sides' output, for the size-gap summary.
    pub fn byte_totals(&self) -> (usize, usize) {
        self.rows.iter().fold((0, 0), |(a, b), r| {
            (a + r.parsoid_bytes, b + r.rustoid_bytes)
        })
    }

    /// How many compared pages show literal unexpanded wikitext, and how many of
    /// those involve `#invoke`.
    ///
    /// Reported separately from the outcome histogram because it answers a
    /// different question. "Nothing matches" says the parser is far from parity;
    /// "N pages still contain `{{…}}` in their output" says *how* far, and it
    /// separates a rendering that is wrong from one that never happened.
    pub fn unexpanded_summary(&self) -> UnexpandedSummary {
        let mut s = UnexpandedSummary::default();
        for row in &self.rows {
            if row.is_skipped() {
                continue;
            }
            s.compared += 1;
            if row.unexpanded_rustoid.any() {
                s.pages_with_literal_wikitext += 1;
            }
            if row.unexpanded_rustoid.invoke > 0 {
                s.pages_with_unexpanded_invoke += 1;
            }
            if row.unexpanded_rustoid.braces > row.unexpanded_parsoid.braces {
                s.pages_worse_than_parsoid += 1;
            }
        }
        s
    }

    /// Render the report.
    ///
    /// `detailed` adds the first difference for every failure, which is the
    /// actionable part but runs to hundreds of lines on a full corpus.
    pub fn render(&self, detailed: bool) -> String {
        let mut out = String::new();
        out.push_str(&format!("corpus: {}\n", self.corpus_name));

        out.push_str(&format!(
            "\nscore: {}/{} compared ({:.1}%)",
            self.matches(),
            self.compared(),
            self.percent()
        ));
        if self.skipped() > 0 {
            out.push_str(&format!(", {} skipped", self.skipped()));
        }
        out.push('\n');

        let (p, r) = self.byte_totals();
        if p > 0 {
            out.push_str(&format!(
                "output: parsoid {} bytes, rustoid {} bytes ({:.2}x)\n",
                p,
                r,
                r as f64 / p as f64
            ));
        }

        out.push_str("\nby outcome:\n");
        for b in self.by_category() {
            out.push_str(&format!("  {:20} {}\n", b.name, b.count));
        }

        // The diagnosis that matters while expansion is incomplete.
        let un = self.unexpanded_summary();
        if un.compared > 0 {
            out.push_str("\nunexpanded wikitext (rustoid side):\n");
            out.push_str(&format!(
                "  {:34} {}/{}\n",
                "pages with literal {{...}}", un.pages_with_literal_wikitext, un.compared
            ));
            out.push_str(&format!(
                "  {:34} {}/{}\n",
                "pages with literal {{#invoke:", un.pages_with_unexpanded_invoke, un.compared
            ));
            out.push_str(&format!(
                "  {:34} {}/{}\n",
                "more literal syntax than parsoid", un.pages_worse_than_parsoid, un.compared
            ));
        }

        let by_tag = self.by_tag();
        if !by_tag.is_empty() {
            out.push_str("\nby tag (worst first):\n");
            for (tag, pass, total) in by_tag {
                let flag = if pass == 0 {
                    "  <-- nothing passes"
                } else {
                    ""
                };
                out.push_str(&format!("  {:20} {pass}/{total}{flag}\n", tag));
            }
        }

        let failing: Vec<&Row> = self.rows.iter().filter(|r| !r.is_match()).collect();
        if !failing.is_empty() {
            out.push_str("\nfailures:\n");
            for row in &failing {
                let verdict = match &row.outcome {
                    Outcome::Skipped { reason } => format!("skip: {reason}"),
                    _ => row.category().to_string(),
                };
                out.push_str(&format!(
                    "  {:40} r{}  {verdict}\n",
                    truncate(&row.title, 40),
                    row.revid.unwrap_or(0)
                ));
            }
        }

        if detailed {
            out.push_str("\nunexpanded by page (rustoid / parsoid):\n");
            for row in &self.rows {
                if row.is_skipped() {
                    continue;
                }
                out.push_str(&format!(
                    "  {:40} invoke={:<4} braces={:<6} (parsoid braces={})\n",
                    truncate(&row.title, 40),
                    row.unexpanded_rustoid.invoke,
                    row.unexpanded_rustoid.braces,
                    row.unexpanded_parsoid.braces,
                ));
            }

            out.push_str("\nfirst differences:\n");
            for row in &failing {
                if let Outcome::Differ { detail } = &row.outcome {
                    out.push_str(&format!("\n=== {} ===\n{}\n", row.title, detail));
                }
            }
        }

        out
    }
}

/// Count occurrences and sort by descending count, then name.
fn tally(items: impl Iterator<Item = String>) -> Vec<Bucket> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        *counts.entry(item).or_insert(0) += 1;
    }
    let mut out: Vec<Bucket> = counts
        .into_iter()
        .map(|(name, count)| Bucket { name, count })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Truncate to `n` chars on a char boundary, marking the cut.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, tags: &[&str], outcome: Outcome) -> Row {
        Row {
            title: title.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            revid: Some(1),
            outcome,
            parsoid_bytes: 100,
            rustoid_bytes: 400,
            unexpanded_rustoid: Unexpanded::default(),
            unexpanded_parsoid: Unexpanded::default(),
        }
    }

    fn differ(detail: &str) -> Outcome {
        Outcome::Differ {
            detail: detail.to_string(),
        }
    }

    fn board() -> Scoreboard {
        Scoreboard::new(
            "test",
            vec![
                row("A", &["lua"], Outcome::Match),
                row("B", &["lua", "table"], differ("first difference: <td>")),
                row("C", &["table"], differ("first difference: <table>")),
                row(
                    "D",
                    &["cite"],
                    Outcome::Skipped {
                        reason: "offline".to_string(),
                    },
                ),
            ],
        )
    }

    #[test]
    fn counts_matches_over_compared_pages_not_all_pages() {
        let b = board();
        assert_eq!(b.total(), 4);
        assert_eq!(
            b.compared(),
            3,
            "the skipped page must not count as compared"
        );
        assert_eq!(b.matches(), 1);
        assert_eq!(b.skipped(), 1);
        // 1 of 3 compared, not 1 of 4.
        assert!((b.percent() - 33.333).abs() < 0.01, "{}", b.percent());
    }

    #[test]
    fn percent_of_an_empty_run_is_zero_not_a_panic() {
        let b = Scoreboard::new("empty", vec![]);
        assert_eq!(b.percent(), 0.0);
        assert_eq!(b.compared(), 0);
    }

    #[test]
    fn categories_are_counted_most_common_first() {
        let b = board();
        let cats = b.by_category();
        let get = |n: &str| cats.iter().find(|c| c.name == n).map(|c| c.count);
        assert_eq!(get("match"), Some(1));
        assert_eq!(get("table"), Some(2));
        assert_eq!(get("skipped"), Some(1));
    }

    /// A tag must count *failures*, so the numbers stay comparable across tags
    /// even though a page carries several. A skip is not a failure.
    #[test]
    fn tags_count_failures_and_both_of_a_pages_tags_are_credited() {
        let b = board();
        let fails: Vec<(String, usize)> = b
            .failures_by_tag()
            .into_iter()
            .map(|x| (x.name, x.count))
            .collect();
        // B fails and is tagged lua+table; C fails and is tagged table.
        assert_eq!(
            fails,
            vec![("table".to_string(), 2), ("lua".to_string(), 1)]
        );
        // D's `cite` tag must not appear: D was skipped, not failed.
        assert!(!fails.iter().any(|(name, _)| name == "cite"));
    }

    /// A tag whose only page was skipped must be absent entirely, not reported
    /// as 0/1 — there is nothing to work on there.
    #[test]
    fn a_tag_with_only_skipped_pages_is_omitted() {
        let b = Scoreboard::new(
            "t",
            vec![
                row("A", &["lua"], Outcome::Match),
                row(
                    "D",
                    &["cite"],
                    Outcome::Skipped {
                        reason: "offline".to_string(),
                    },
                ),
            ],
        );
        let tags: Vec<String> = b.by_tag().into_iter().map(|(t, _, _)| t).collect();
        assert_eq!(tags, vec!["lua"]);
    }

    /// The by-tag table must rank worst-first, so it reads as a work list.
    #[test]
    fn tag_table_is_worst_first() {
        let b = board();
        let table = b.by_tag();
        assert_eq!(table[0].0, "table");
        assert_eq!(table[0].1, 0, "no table-tagged page passes");
        assert_eq!(table[0].2, 2);
        assert_eq!(table[1].0, "lua");
        assert_eq!(table[1].1, 1, "one of two lua pages passes");
        assert_eq!(table[1].2, 2);
    }

    #[test]
    fn renders_a_score_and_a_histogram() {
        let text = board().render(false);
        assert!(text.contains("score: 1/3 compared (33.3%)"), "{text}");
        assert!(text.contains("1 skipped"), "{text}");
        assert!(text.contains("by outcome:"), "{text}");
        assert!(text.contains("by tag"), "{text}");
        assert!(text.contains("nothing passes"), "{text}");
        assert!(text.contains("failures:"), "{text}");
        // The detailed section is opt-in.
        assert!(!text.contains("first differences:"), "{text}");
    }

    #[test]
    fn detailed_render_includes_the_first_difference() {
        let text = board().render(true);
        assert!(text.contains("first differences:"), "{text}");
        assert!(text.contains("=== B ==="), "{text}");
        assert!(text.contains("<td>"), "{text}");
    }

    #[test]
    fn a_clean_run_says_so_without_a_failure_list() {
        let b = Scoreboard::new("test", vec![row("A", &["x"], Outcome::Match)]);
        let text = b.render(false);
        assert!(text.contains("1/1 compared (100.0%)"), "{text}");
        assert!(!text.contains("failures:"), "{text}");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // Must not panic on multi-byte input at the cut point.
        let s = "日本語のタイトルです";
        let t = truncate(s, 4);
        assert_eq!(t.chars().count(), 4);
        assert!(truncate("short", 10) == "short");
    }

    #[test]
    fn byte_totals_sum_both_sides() {
        let (p, r) = board().byte_totals();
        assert_eq!(p, 400);
        assert_eq!(r, 1600);
    }

    /// The unexpanded summary must distinguish "rendered differently" from
    /// "never rendered", and must not count skipped pages as compared.
    #[test]
    fn unexpanded_summary_counts_only_compared_pages() {
        let mut with_literal = row("E", &["lua"], differ("x"));
        with_literal.unexpanded_rustoid = Unexpanded {
            invoke: 2,
            braces: 5,
        };
        let b = Scoreboard::new(
            "t",
            vec![
                row("A", &["x"], Outcome::Match),
                with_literal,
                // A skip must not be counted, and must not be counted as clean.
                row(
                    "S",
                    &["x"],
                    Outcome::Skipped {
                        reason: "offline".to_string(),
                    },
                ),
            ],
        );
        let un = b.unexpanded_summary();
        assert_eq!(un.compared, 2);
        assert_eq!(un.pages_with_literal_wikitext, 1);
        assert_eq!(un.pages_with_unexpanded_invoke, 1);
    }

    /// Literal `{{…}}` inside an attribute is wikitext data, not unexpanded
    /// output, so it must not be counted — otherwise every page looks broken.
    #[test]
    fn unexpanded_counting_ignores_attribute_values() {
        let html = "<span data-mw='{\"wt\":\"{{cite web}}\"}'>text</span>";
        assert_eq!(Unexpanded::count(html), Unexpanded::default());

        // But the same syntax in the text is counted, and `{{{` counts once.
        let bad = "<span>{{{x|y}}}</span>{{#invoke:mod|fn}}";
        let c = Unexpanded::count(bad);
        assert_eq!(c.braces, 2);
        assert_eq!(c.invoke, 1);
    }
}
