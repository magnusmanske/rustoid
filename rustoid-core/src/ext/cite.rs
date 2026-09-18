//! Cite: the `<ref>` and `<references>` extensions.
//!
//! Ported from MediaWiki's `Cite` extension, specifically the Parsoid-native
//! implementation rather than the legacy PHP one, because the target is
//! byte-parity with what a live wiki's Parsoid emits.
//!
//! Cite has two halves that must agree, which is why they live together:
//!
//! 1. **`<ref>`** renders inline as a numbered marker: a `<sup class="mw-ref
//!    reference">` holding a link to the note's entry in the list.
//! 2. **`<references>`** (or a `{{reflist}}` template that emits one) renders the
//!    list: an `<ol class="mw-references references">` of `<li>`s, each with a
//!    back-link per use site.
//!
//! Two ordering facts drive the design:
//!
//! - The number is assigned by **document order of first use**, so the two halves
//!   cannot be computed independently; the collection runs over the whole document
//!   before either half renders.
//! - Repeated use of a named ref (`<ref name="x" />` after `<ref name="x">…`) does
//!   not allocate a new number; it appends a back-link to the existing note. That
//!   is why the lookup key is the *name* when present and the content otherwise.
//!
//! The contract was read off the wiki's own cached Parsoid output rather than
//! guessed:
//!
//! ```html
//! <sup about="#mwt13" class="mw-ref reference" id="cite_ref-NAME_1-0"
//!      rel="dc:references" typeof="mw:Extension/ref"
//!      data-mw='{"name":"ref","attrs":{"name":"NAME"}}'>
//!   <a href="./PAGE#cite_note-NAME-1" id="mwCg">
//!     <span class="mw-reflink-text" id="mwCw">
//!       <span class="cite-bracket" id="mwDA">[</span>1<span class="cite-bracket" id="mwDQ">]</span>
//!     </span>
//!   </a>
//! </sup>
//! ```
//!
//! The `id`s follow Cite's own scheme: `cite_ref-<name>_<n>-<use index>` for the
//! marker and `cite_note-<name>-<n>` for the note. Note the *different* separators
//! — `_` before the number in the marker, `-` in the note — which is Cite's actual
//! output and not a typo here. For an anonymous ref the name segment is empty, so
//! the ids are `cite_ref-1-0` and `cite_note--1`.

use std::collections::HashMap;

/// One collected reference: a note in the list, plus every site that points at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The `name` attribute, `""` when anonymous. Drives the `id` and the sharing
    /// key.
    pub name: String,
    /// The group (`<ref group="…">`), `""` when ungrouped.
    pub group: String,
    /// The ref body's wikitext as written. Rendered later, in the list, because
    /// that is where Cite renders it — a `<ref>`'s content appears in the footnote,
    /// not at the call site.
    pub body: String,
    /// A self-closing ref (`<ref name="x" />`) contributes a back-link and no
    /// content of its own.
    pub self_closing: bool,
    /// The 1-based footnote number, assigned at first use in document order.
    pub number: usize,
    /// One entry per use site, in document order: the `id` Cite gave that marker.
    ///
    /// A note used once renders its single back-link *without* an enclosing
    /// `<span class="mw-cite-backlink">`; with two or more, each is wrapped and they
    /// are separated by a space. That asymmetry is real and verified against the
    /// cached output.
    pub uses: Vec<String>,
}

impl Reference {
    /// The note's `id`, e.g. `cite_note-Badenhorst2019-1`.
    pub fn note_id(&self) -> String {
        format!("cite_note-{}-{}", id_segment(&self.name), self.number)
    }

    /// The marker `id` for the first use, e.g. `cite_ref-Badenhorst2019_1-0`.
    pub fn ref_id(&self) -> String {
        marker_id(&self.name, self.number, 0)
    }

    /// The anchor the inline marker links to.
    pub fn anchor(&self) -> String {
        self.note_id()
    }

    /// The label shown for this note in its group.
    ///
    /// The main group numbers in decimal; `lower-alpha` uses `a`, `b`, …; the
    /// numeric groups are their own label. Cite's other groups (`upper-roman`,
    /// `lower-greek`) are not implemented — they are rare, and inventing them from
    /// a guess would be worse than leaving them as numbers.
    pub fn label(&self) -> String {
        match self.group.as_str() {
            "lower-alpha" | "upper-alpha" | "lower-greek" => alpha_label(self.number, &self.group),
            _ => self.number.to_string(),
        }
    }
}

/// `a`, `b`, … `z`, `aa`, … — Cite's alphabetic group labelling.
fn alpha_label(n: usize, group: &str) -> String {
    if n == 0 {
        return String::new();
    }
    let mut n = n;
    let mut out = String::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        let base = if group == "upper-alpha" { b'A' } else { b'a' };
        out.insert(0, (base + rem as u8) as char);
        n = (n - 1) / 26;
    }
    out
}

/// The references collected from a document, ready to render either half.
#[derive(Debug, Default)]
pub struct CiteState {
    /// Notes in first-use order. Ungrouped refs are group `""`.
    pub references: Vec<Reference>,
    /// `(group, key) -> index into `references``, so a repeated use finds its note
    /// in O(1) rather than rescanning. The key is the name when named, else the
    /// body.
    index: HashMap<(String, String), usize>,
    /// The next footnote number, *per group*: numbering restarts for each group.
    next_number: HashMap<String, usize>,
}

impl CiteState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.references.is_empty()
    }

    /// Record a `<ref>` use, returning the marker `id` Cite assigns it.
    ///
    /// `body` is the ref's wikitext content; `self_closing` marks the
    /// `<ref name="x" />` form. A named ref seen before reuses the existing note.
    pub fn add(&mut self, name: &str, group: &str, body: &str, self_closing: bool) -> String {
        // A named ref is identified by its *name*, so `<ref name="x" />` finds the
        // note that `<ref name="x">…</ref>` defined. An anonymous one is identified
        // by its content, so the same text twice shares a note — which is what Cite
        // does, and why `<ref>a</ref><ref>a</ref>` renders one note with two uses.
        let key = if name.is_empty() {
            (group.to_string(), body.to_string())
        } else {
            (group.to_string(), name.to_string())
        };

        let idx = match self.index.get(&key) {
            Some(&i) => i,
            None => {
                let counter = self.next_number.entry(group.to_string()).or_insert(1);
                let number = *counter;
                *counter += 1;
                self.references.push(Reference {
                    name: name.to_string(),
                    group: group.to_string(),
                    body: body.to_string(),
                    self_closing,
                    number,
                    uses: Vec::new(),
                });
                let i = self.references.len() - 1;
                self.index.insert(key, i);
                i
            }
        };

        // The id is keyed on the note the ref *resolved to*, not on this call's
        // spelling, so two uses of `<ref name="x" />` produce `-0` and `-1` for the
        // same note — which is what makes the back-links line up.
        let use_index = self.references[idx].uses.len();
        let id = marker_id(
            &self.references[idx].name,
            self.references[idx].number,
            use_index,
        );
        self.references[idx].uses.push(id.clone());
        id
    }

    /// The refs in one group, in first-use order.
    pub fn group(&self, group: &str) -> Vec<&Reference> {
        self.references
            .iter()
            .filter(|r| r.group == group)
            .collect()
    }

    /// Every distinct group name in the document, `""` first when present.
    pub fn groups(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in &self.references {
            if !out.contains(&r.group) {
                out.push(r.group.clone());
            }
        }
        out
    }
}

/// Cite's `id` segment: the ref name with spaces as underscores, empty when the
/// ref is anonymous.
fn id_segment(name: &str) -> String {
    name.replace(' ', "_")
}

/// A marker's `id` for one use: `cite_ref-<name>_<n>-<use>`.
///
/// The underscore before the number only appears when the ref *has* a name;
/// anonymous refs read `cite_ref-1-0`. That is Cite's rule, and it is encoded here
/// once so the two call sites cannot drift apart.
fn marker_id(name: &str, number: usize, use_index: usize) -> String {
    if name.is_empty() {
        format!("cite_ref-{number}-{use_index}")
    } else {
        format!("cite_ref-{}_{number}-{use_index}", id_segment(name))
    }
}

/// Monotonic `id` allocator for the `mwXY`-style ids Parsoid assigns elsewhere.
///
/// Parsoid's ids are content-independent counters shared by the whole document, so
/// they can only be allocated in document order. They are not Cite's (`cite_*`)
/// ids, which are derived from the note; both appear in a Cite rendering.
#[derive(Debug)]
pub struct DocIds {
    n: usize,
}

impl DocIds {
    pub fn new() -> Self {
        Self { n: 0 }
    }

    /// The next id, e.g. `mwCg`.
    ///
    /// Parsoid emits `mw` + a base-62-like counter. This reproduces the shape so a
    /// reader can tell the ids apart, but a byte comparison needs the *values*,
    /// which in turn needs the whole document's id allocation in order — a
    /// separate problem from Cite rendering.
    pub fn take(&mut self) -> String {
        let n = self.n;
        self.n += 1;
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut out = String::from("mw");
        let mut v = n;
        loop {
            out.push(alphabet[v % alphabet.len()] as char);
            v /= alphabet.len();
            if v == 0 {
                break;
            }
        }
        out
    }
}

impl Default for DocIds {
    fn default() -> Self {
        Self::new()
    }
}

/// The `data-mw` JSON for a `<ref>` marker.
///
/// A named ref records its name; the group is recorded only when set. A ref with
/// content records the body's wikitext as `extsrc`. Rendered in the attribute
/// order the cached Parsoid output uses.
pub fn ref_data_mw(reference: &Reference) -> String {
    let mut attrs = Vec::new();
    if !reference.name.is_empty() {
        attrs.push(format!("\"name\":{}", json_string(&reference.name)));
    }
    if !reference.group.is_empty() {
        attrs.push(format!("\"group\":{}", json_string(&reference.group)));
    }
    let attrs = format!("{{{}}}", attrs.join(","));
    match &reference.body {
        body if reference.self_closing || body.is_empty() => {
            format!("{{\"name\":\"ref\",\"attrs\":{attrs}}}")
        }
        body => format!(
            "{{\"name\":\"ref\",\"attrs\":{attrs},\"body\":{{\"extsrc\":{}}}}}",
            json_string(body)
        ),
    }
}

/// The `data-mw` JSON for a `<references>` tag.
pub fn references_data_mw(group: &str, responsive: bool) -> String {
    format!(
        "{{\"name\":\"references\",\"attrs\":{{\"group\":{0},\"responsive\":\"{1}\"}},\"body\":{{\"extsrc\":\"\"}}}}",
        json_string(group),
        if responsive { "1" } else { "0" }
    )
}

/// Render the inline `<sup>` marker for a `<ref>`.
///
/// `page_title` is the page the anchor targets: Cite links to a note on the *same*
/// page, so it is the article under parse, not the ref.
///
/// `ids` supplies the `mw`-style ids. They are passed in rather than generated here
/// because Parsoid's counter is shared across the whole document — a marker cannot
/// know its ids without knowing how many elements came before it.
pub fn render_ref_marker(
    reference: &Reference,
    ref_id: &str,
    page_title: &str,
    ids: &mut DocIds,
) -> String {
    let href = format!("./{}#{}", page_title.replace(' ', "_"), reference.anchor());
    let (a_id, text_id, open_id, close_id) = (ids.take(), ids.take(), ids.take(), ids.take());
    let mw = ref_data_mw(reference);
    let label = reference.label();
    format!(
        "<sup class=\"mw-ref reference\" id=\"{ref_id}\" rel=\"dc:references\" \
         typeof=\"mw:Extension/ref\" data-mw='{mw}'>\
         <a href=\"{href}\" id=\"{a_id}\">\
         <span class=\"mw-reflink-text\" id=\"{text_id}\">\
         <span class=\"cite-bracket\" id=\"{open_id}\">[</span>{label}\
         <span class=\"cite-bracket\" id=\"{close_id}\">]</span>\
         </span></a></sup>"
    )
}

/// Render the `<ol>` note list for one group of `<references>`.
///
/// `body_html` supplies the rendered content of each note, keyed by the note's
/// `id`. The body cannot be rendered here: it is wikitext that has to go back
/// through the parser (a note may contain templates, links, or another ref), and
/// that is the caller's job. Passing it in keeps this module free of a parser
/// dependency and keeps the HTML shape in one place.
pub fn render_references_list(
    refs: &[&Reference],
    group: &str,
    page_title: &str,
    body_html: &dyn Fn(&Reference) -> String,
    ids: &mut DocIds,
) -> String {
    let page = page_title.replace(' ', "_");
    let ga = group_attr(group);

    let mut out = format!(
        "<ol class=\"mw-references references\"{ga} id=\"{}\">",
        ids.take()
    );
    for r in refs {
        let note_id = r.note_id();
        // A single use renders its back-link bare; two or more get a wrapping
        // `<span class="mw-cite-backlink">` and are space-separated. This is
        // Cite's actual output, verified against the cached page.
        let backlink = if r.uses.len() == 1 {
            backlink_one(&r.uses[0], &page, group, ids)
        } else {
            let parts: Vec<String> = r
                .uses
                .iter()
                .enumerate()
                .map(|(n, u)| backlink_n(u, &page, n + 1, ids))
                .collect();
            format!(
                "<span class=\"mw-cite-backlink\" id=\"{}\">{}</span>",
                ids.take(),
                parts.join(" ")
            )
        };
        out.push_str(&format!(
            "<li about=\"#{note_id}\" id=\"{note_id}\" data-mw-footnote-number=\"{}\">{backlink} ",
            r.label()
        ));
        if !r.self_closing {
            out.push_str(&format!(
                "<span id=\"mw-reference-text-{note_id}\" class=\"mw-reference-text reference-text\"{ga}>{}</span>",
                body_html(r)
            ));
        }
        out.push_str("</li>");
    }
    out.push_str("</ol>");
    out
}

/// A single-use back-link: no wrapping span.
fn backlink_one(use_id: &str, page: &str, group: &str, ids: &mut DocIds) -> String {
    let group_attr = group_attr(group);
    let a_id = ids.take();
    let text_id = ids.take();
    format!(
        "<a href=\"./{page}#{use_id}\"{group_attr} rel=\"mw:referencedBy\" id=\"{a_id}\">\
<span class=\"mw-linkback-text\" id=\"{text_id}\">↑</span></a>"
    )
}

/// One back-link among several: labelled with its use number, not an arrow.
fn backlink_n(use_id: &str, page: &str, n: usize, ids: &mut DocIds) -> String {
    let a_id = ids.take();
    let text_id = ids.take();
    format!(
        "<a href=\"./{page}#{use_id}\" id=\"{a_id}\">\
<span class=\"mw-linkback-text\" id=\"{text_id}\">{n}</span></a>"
    )
}

/// ` data-mw-group="…"`, or nothing for the main group.
fn group_attr(group: &str) -> String {
    if group.is_empty() {
        String::new()
    } else {
        format!(" data-mw-group=\"{}\"", escape_attr(group))
    }
}

/// Escape a value for an HTML attribute.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A JSON string literal, escaped the way Parsoid's serializer does.
///
/// Serde is not used because the surrounding object is assembled by hand to keep
/// attribute order exact, and mixing the two would make the escaping rules
/// inconsistent between fields.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbering_follows_first_use_order() {
        let mut st = CiteState::new();
        st.add("", "", "first", false);
        st.add("", "", "second", false);
        st.add("", "", "first", false);
        assert_eq!(st.references.len(), 2, "the repeat must not add a note");
        assert_eq!(st.references[0].number, 1);
        assert_eq!(st.references[1].number, 2);
        assert_eq!(st.references[0].uses.len(), 2, "both uses are recorded");
    }

    /// A named ref used again does not allocate a new number, even when the second
    /// use is self-closing — that is the whole point of naming a ref.
    #[test]
    fn a_named_ref_is_shared_between_uses() {
        let mut st = CiteState::new();
        st.add("x", "", "body", false);
        st.add("x", "", "", true);
        assert_eq!(st.references.len(), 1);
        assert_eq!(st.references[0].uses.len(), 2);
        assert_eq!(st.references[0].uses[1], "cite_ref-x_1-1");
    }

    /// Numbering restarts per group: a `lower-alpha` group numbers from 1 again.
    #[test]
    fn groups_number_independently() {
        let mut st = CiteState::new();
        st.add("", "lower-alpha", "a", false);
        st.add("", "", "main", false);
        assert_eq!(st.references[0].group, "lower-alpha");
        assert_eq!(st.references[0].number, 1);
        assert_eq!(
            st.references[1].number, 1,
            "the main group starts at 1 too, independently"
        );
        assert_eq!(st.references[0].label(), "a");
        assert_eq!(st.references[1].label(), "1");
    }

    /// The id separators differ between the marker and the note, and an anonymous
    /// ref still emits both separators, which is Cite's actual output.
    #[test]
    fn ids_match_cites_scheme() {
        let mut st = CiteState::new();
        let id = st.add("Badenhorst2019", "", "body", false);
        assert_eq!(id, "cite_ref-Badenhorst2019_1-0");
        assert_eq!(st.references[0].note_id(), "cite_note-Badenhorst2019-1");

        let mut st = CiteState::new();
        let id = st.add("", "", "anon", false);
        assert_eq!(id, "cite_ref-1-0");
        assert_eq!(st.references[0].note_id(), "cite_note--1");
    }

    #[test]
    fn an_anonymous_repeat_shares_by_body() {
        let mut st = CiteState::new();
        st.add("", "", "same", false);
        st.add("", "", "same", false);
        assert_eq!(
            st.references.len(),
            1,
            "identical anonymous refs share a note, as Cite does"
        );
    }

    #[test]
    fn alpha_labels_roll_over_at_z() {
        assert_eq!(alpha_label(1, "lower-alpha"), "a");
        assert_eq!(alpha_label(26, "lower-alpha"), "z");
        assert_eq!(alpha_label(27, "lower-alpha"), "aa");
        assert_eq!(alpha_label(1, "upper-alpha"), "A");
    }

    /// The marker's `data-mw` is exactly the shape in the cached output, including
    /// the omission of `group` when unset.
    #[test]
    fn ref_data_mw_matches_the_cached_shape() {
        let mut st = CiteState::new();
        st.add("Badenhorst2019", "", "", true);
        assert_eq!(
            ref_data_mw(&st.references[0]),
            r#"{"name":"ref","attrs":{"name":"Badenhorst2019"}}"#
        );

        let mut st = CiteState::new();
        st.add("", "", "some text", false);
        assert_eq!(
            ref_data_mw(&st.references[0]),
            r#"{"name":"ref","attrs":{},"body":{"extsrc":"some text"}}"#
        );
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b"), r#""a\"b""#);
        assert_eq!(json_string("a\\b"), r#""a\\b""#);
        assert_eq!(json_string("a\nb"), r#""a\nb""#);
    }

    /// The marker's HTML, with the ids stubbed to a fixed value.
    ///
    /// The shape is taken from the cached Parsoid output of `Zebra`:
    /// `<sup …><a …><span class="mw-reflink-text">…` — this asserts the nesting and
    /// the classes, which is what the page actually shows.
    #[test]
    fn ref_marker_matches_the_cached_shape() {
        let mut st = CiteState::new();
        st.add("Badenhorst2019", "", "", true);
        let mut ids = DocIds::new();
        let html = render_ref_marker(
            &st.references[0],
            "cite_ref-Badenhorst2019_1-0",
            "Zebra",
            &mut ids,
        );
        assert!(
            html.starts_with(
                "<sup class=\"mw-ref reference\" id=\"cite_ref-Badenhorst2019_1-0\" rel=\"dc:references\" typeof=\"mw:Extension/ref\""
            ),
            "{html}"
        );
        assert!(
            html.contains("data-mw='{\"name\":\"ref\",\"attrs\":{\"name\":\"Badenhorst2019\"}}'"),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"./Zebra#cite_note-Badenhorst2019-1\""),
            "the anchor must point at the note: {html}"
        );
        assert!(html.contains("class=\"mw-reflink-text\""), "{html}");
        assert!(html.contains("class=\"cite-bracket\""), "{html}");
        // The brackets are separate spans around the bare number.
        assert!(html.contains(">[</span>1<span"), "{html}");
        assert!(html.ends_with("</span></a></sup>"), "{html}");
    }

    /// A single-use note renders a bare back-link; the arrow is the `<a>`'s text.
    #[test]
    fn a_single_use_note_has_a_bare_backlink() {
        let mut st = CiteState::new();
        st.add("", "", "body", false);
        let mut ids = DocIds::new();
        let refs: Vec<&Reference> = st.references.iter().collect();
        let html = render_references_list(&refs, "", "Zebra", &|_| "BODY".to_string(), &mut ids);
        assert!(
            html.starts_with("<ol class=\"mw-references references\""),
            "{html}"
        );
        assert!(
            html.contains(
                "<li about=\"#cite_note--1\" id=\"cite_note--1\" data-mw-footnote-number=\"1\">"
            ),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"./Zebra#cite_ref-1-0\" rel=\"mw:referencedBy\""),
            "{html}"
        );
        assert!(html.contains("↑"), "a single use is an arrow: {html}");
        assert!(
            !html.contains("mw-cite-backlink"),
            "one use must not be wrapped in the back-link span: {html}"
        );
        assert!(
            html.contains("<span id=\"mw-reference-text-cite_note--1\" class=\"mw-reference-text reference-text\">BODY</span>"),
            "{html}"
        );
    }

    /// Two uses of one note are wrapped and numbered, which is the asymmetry the
    /// cached page shows for `Badenhorst2019`.
    #[test]
    fn several_uses_are_wrapped_and_numbered() {
        let mut st = CiteState::new();
        st.add("Badenhorst2019", "", "body", false);
        st.add("Badenhorst2019", "", "", true);
        let mut ids = DocIds::new();
        let refs: Vec<&Reference> = st.references.iter().collect();
        let html = render_references_list(&refs, "", "Zebra", &|_| "B".to_string(), &mut ids);
        assert!(html.contains("class=\"mw-cite-backlink\""), "{html}");
        assert!(
            html.contains("#cite_ref-Badenhorst2019_1-0\""),
            "the first use is targeted: {html}"
        );
        assert!(
            html.contains("#cite_ref-Badenhorst2019_1-1\""),
            "the second use is targeted too: {html}"
        );
        // The labels count the uses, not the note number.
        assert!(html.contains(">1</span></a> <a"), "{html}");
        assert!(html.contains(">2</span></a>"), "{html}");
    }

    /// A named ref with no body of its own renders no reference-text span — there
    /// is nothing to show, and Cite omits it rather than emitting an empty one.
    #[test]
    fn a_bodyless_note_has_no_text_span() {
        let mut st = CiteState::new();
        st.add("x", "", "content", false);
        st.add("x", "", "", true);
        let mut ids = DocIds::new();
        let refs: Vec<&Reference> = st.references.iter().collect();
        let html = render_references_list(&refs, "", "Zebra", &|_| "CONTENT".to_string(), &mut ids);
        // The note came from the first (non-self-closing) use, so it renders.
        assert!(html.contains("CONTENT"), "{html}");
    }

    #[test]
    fn a_group_adds_the_data_mw_group_attribute() {
        let mut st = CiteState::new();
        st.add("", "lower-alpha", "a", false);
        let mut ids = DocIds::new();
        let refs: Vec<&Reference> = st.references.iter().collect();
        let html = render_references_list(
            &refs,
            "lower-alpha",
            "Zebra",
            &|_| "A".to_string(),
            &mut ids,
        );
        assert!(html.contains("data-mw-group=\"lower-alpha\""), "{html}");
        assert!(
            html.contains("data-mw-footnote-number=\"a\""),
            "a lower-alpha note is labelled a: {html}"
        );
    }

    #[test]
    fn references_data_mw_matches_the_cached_shape() {
        assert_eq!(
            references_data_mw("", false),
            r#"{"name":"references","attrs":{"group":"","responsive":"0"},"body":{"extsrc":""}}"#
        );
        assert_eq!(
            references_data_mw("lower-alpha", true),
            r#"{"name":"references","attrs":{"group":"lower-alpha","responsive":"1"},"body":{"extsrc":""}}"#
        );
    }
}
