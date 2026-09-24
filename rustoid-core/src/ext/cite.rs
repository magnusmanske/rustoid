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
    /// Which use supplied this note's wikitext, as an index into `uses`.
    ///
    /// **Not** necessarily the first. A reuse can precede the definition —
    /// `A<ref name="x"/>B<ref name="x">body</ref>` is a real page shape, and
    /// `Zebra` opens with one inside an infobox — in which case Cite allocates
    /// the number and the first `id` from the *reuse*, and the later definition
    /// merely fills in the text. The `data-mw` `body` pointer follows the text,
    /// so it lands on the defining use rather than on use zero.
    pub body_use: Option<usize>,
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
                    body: String::new(),
                    self_closing,
                    number,
                    uses: Vec::new(),
                    body_use: None,
                });
                let i = self.references.len() - 1;
                self.index.insert(key, i);
                i
            }
        };

        // A use that carries text *defines* the note, wherever it sits. An empty
        // body never overwrites one, and never claims the pointer: a bare
        // `<ref name="x"/>` contributes a back-link and nothing else.
        let defines = !self_closing && !body.is_empty();
        if defines {
            self.references[idx].body = body.to_string();
            self.references[idx].self_closing = false;
        }

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
        // The pointer belongs to the use that supplied the text, which is this one
        // whenever it defines.
        if defines {
            self.references[idx].body_use = Some(self.references[idx].uses.len() - 1);
        }
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
    /// The document's `about` counter, shared with the rest of the expansion.
    ///
    /// A `<sup>` for a `<ref>` carries `about="#mwt42"` drawn from the *same*
    /// sequence every transclusion uses — Cite does not number its own. The
    /// served page's ids are not contiguous (they are allocated as expansions
    /// run), so a separate counter here would drift from the first ref onward.
    about: std::cell::Cell<usize>,
}

impl DocIds {
    pub fn new() -> Self {
        Self {
            about: std::cell::Cell::new(0),
        }
    }

    /// Continue the document's `about` sequence from `next` (the number already
    /// allocated for the last expansion).
    pub fn set_about_counter(&self, next: usize) {
        self.about.set(next);
    }

    /// A fresh transclusion `about` id for a Cite element, e.g. `#mwt13`.
    pub fn take_about(&self) -> String {
        let n = self.about.get() + 1;
        self.about.set(n);
        format!("#mwt{n}")
    }

    /// The `about` counter's current value, so the caller can hand it back to
    /// whoever owns the document sequence.
    pub fn about_counter(&self) -> usize {
        self.about.get()
    }
}

/// Mark `node` as one the document's id pass must number.
///
/// Cite does not number its own elements. Parsoid's Cite post-processor only
/// builds the DOM; the single document-order id pass assigns every `id="mw…"`
/// in the served output. Numbering them here from a second counter put Cite's
/// ids at the *front* of the sequence — `mwAA` first, which the service never
/// emits, then `mwAQ`… — and the real allocator then skipped that whole range,
/// which is why the first body element came out as `mwGg` instead of `mwAQ` on
/// every page.
fn mark_for_id(node: &mut crate::dom::node::Node) {
    node.empty_dp_slot = true;
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
pub fn ref_data_mw(reference: &Reference, use_index: usize) -> String {
    let mut attrs = Vec::new();
    if !reference.name.is_empty() {
        attrs.push(format!("\"name\":{}", json_string(&reference.name)));
    }
    if !reference.group.is_empty() {
        attrs.push(format!("\"group\":{}", json_string(&reference.group)));
    }
    let attrs = format!("{{{}}}", attrs.join(","));
    // Only the use that supplied the note's text points at it. That is *not*
    // necessarily the first use: a reuse may precede the definition
    // (`A<ref name="x"/>B<ref name="x">body</ref>`), and `Zebra` opens with
    // exactly that shape inside an infobox. Every other use carries no `body` at
    // all, because its text already lives in the note the list renders.
    //
    // `attrs` is emitted even when empty, which is Cite's shape for a bare
    // `<ref>`.
    if reference.body_use != Some(use_index) {
        return format!("{{\"name\":\"ref\",\"attrs\":{attrs}}}");
    }
    format!(
        "{{\"name\":\"ref\",\"attrs\":{attrs},\"body\":{{\"id\":{}}}}}",
        json_string(&format!("mw-reference-text-{}", reference.note_id()))
    )
}

/// The `data-mw` JSON for a `<references>` tag.
///
/// `attrs` echoes the attributes **as written** — a bare `<references/>` is
/// `"attrs":{}`, while `<references group="n" responsive="yes"/>` is exactly
/// that pair, values included. Parsoid does not normalise or default them here,
/// and it emits no `body` at all: the notes are children of the wrapper element,
/// not `extsrc`.
///
/// `auto_generated` is set when no `<references>` tag was present and the tag
/// was synthesised to hold the notes.
pub fn references_data_mw(attrs_json: &str, auto_generated: bool) -> String {
    if auto_generated {
        return format!(
            "{{\"name\":\"references\",\"attrs\":{attrs_json},\"autoGenerated\":true}}"
        );
    }
    format!("{{\"name\":\"references\",\"attrs\":{attrs_json}}}")
}

/// Run Cite over a built DOM: collect references, then render both halves.
///
/// A **DOM pass**, not a token pass, and Cite's semantics force that rather than
/// making it a convenience. Two requirements rule out a token-level version:
///
/// 1. A marker's number depends on refs that appear *after* it, so the collection
///    must see the whole document before any marker renders. The token pipeline is
///    a chain of independent passes with nowhere to keep that state.
/// 2. `<references>` renders the notes wherever it appears — usually at the bottom —
///    so the list cannot be built while walking past the refs either.
///
/// One walk collects; a second substitutes. Running after the tree is final also
/// means the node ids Cite allocates line up with the page-bundle pass that runs
/// later, which is why `ids` is threaded from the caller rather than started here.
///
/// `body_of` renders a note's wikitext to a node. It is the caller's job because a
/// note may contain templates, links or nested refs, so rendering needs the
/// pipeline. Passing it in keeps this module independent of the parser.
///
/// Returns the number of `<ref>` markers rendered.
pub fn run(
    root: &mut crate::dom::node::Node,
    page_title: &str,
    ids: &mut DocIds,
    body_of: &dyn Fn(&str) -> crate::dom::node::Node,
) -> usize {
    let mut state = CiteState::new();
    let mut use_ids = Vec::new();
    collect(root, &mut state, &mut use_ids);
    // Deliberately **not** skipped when `state` is empty: a `<references>` tag with
    // no refs still renders its wrapper and an empty `<ol>`, which is what Parsoid
    // emits (verified against the cached output — the list structure is always
    // there, and only the `<li>`s vary). Returning early here left the raw
    // extension element in the output instead.
    render(root, &state, page_title, ids, body_of, &mut use_ids)
}

/// Walk once, recording every `<ref>` in document order.
///
/// `<references>` is deliberately not collected here: it renders in place, from
/// state only this walk can build, so it is handled during substitution.
fn collect(node: &crate::dom::node::Node, state: &mut CiteState, use_ids: &mut Vec<String>) {
    if let Some((name, group, body, self_closing)) = read_ref(node) {
        use_ids.push(state.add(&name, &group, &body, self_closing));
    }
    for child in &node.children {
        collect(child, state, use_ids);
    }
}

/// Read a `<ref>` element into `(name, group, body, self_closing)`.
///
/// The tokenizer leaves an unknown extension tag as an `<extension>` element whose
/// `source` attribute holds the raw text, because it does not parse an extension's
/// interior. So the attributes are recovered from that source rather than read from
/// the element.
fn read_ref(node: &crate::dom::node::Node) -> Option<(String, String, String, bool)> {
    if !typeof_contains(node, "mw:Extension") || node.get_attr("name") != Some("ref") {
        return None;
    }
    let source = node.get_attr("source")?;
    let attrs = start_tag_attrs(source, "ref")?;
    // A self-closing ref carries no body, and its `source` is just the start tag.
    let self_closing = source.trim_end().ends_with("/>");
    let name = attr_value(attrs, "name").unwrap_or_default();
    let group = attr_value(attrs, "group").unwrap_or_default();
    let body = if self_closing {
        String::new()
    } else {
        body_between(source).unwrap_or_default()
    };
    Some((name, group, body, self_closing))
}

/// The attribute text of `<name …>` (or `<name …/>`), without the brackets.
fn start_tag_attrs<'s>(source: &'s str, name: &str) -> Option<&'s str> {
    let rest = source.strip_prefix('<')?.strip_prefix(name)?;
    // The name must end here, so `<reference>` does not match `ref`.
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
        return None;
    }
    let end = rest.find('>')?;
    Some(&rest[..end])
}

/// The text between `>` and `</name>`, which is an extension's raw body.
fn body_between(source: &str) -> Option<String> {
    let open = source.find('>')?;
    let close = source.rfind("</")?;
    (close > open).then(|| source[open + 1..close].to_string())
}

/// A double-quoted, single-quoted or bare attribute value.
///
/// Hand-written because the source is a raw tag, not something a general parser
/// should be pointed at: it is a fragment inside an attribute of another document.
fn attr_value(attrs: &str, key: &str) -> Option<String> {
    let bytes = attrs.as_bytes();
    let mut i = 0;
    while i < attrs.len() {
        while i < attrs.len() && !bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let name_start = i;
        while i < attrs.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
            i += 1;
        }
        if name_start == i {
            break;
        }
        let name = &attrs[name_start..i];
        let after_name = i;
        while i < attrs.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= attrs.len() || bytes[i] != b'=' {
            i = after_name;
            continue;
        }
        i += 1;
        while i < attrs.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= attrs.len() {
            break;
        }
        let value = if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            let start = i;
            while i < attrs.len() && bytes[i] != quote {
                i += 1;
            }
            let v = attrs[start..i].to_string();
            i += 1;
            v
        } else {
            let start = i;
            while i < attrs.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            attrs[start..i].to_string()
        };
        if name == key {
            return Some(value);
        }
    }
    None
}

/// Walk again, replacing refs with markers and `<references>` with the note list.
fn render(
    root: &mut crate::dom::node::Node,
    state: &CiteState,
    page_title: &str,
    ids: &mut DocIds,
    body_of: &dyn Fn(&str) -> crate::dom::node::Node,
    use_ids: &mut [String],
) -> usize {
    let mut rendered = 0usize;
    let mut next_use = 0usize;
    walk_render(
        root,
        state,
        page_title,
        ids,
        body_of,
        use_ids,
        &mut next_use,
        &mut rendered,
    );
    rendered
}

/// Transform one node's children, then the node itself.
#[allow(clippy::too_many_arguments)]
fn walk_render(
    node: &mut crate::dom::node::Node,
    state: &CiteState,
    page_title: &str,
    ids: &mut DocIds,
    body_of: &dyn Fn(&str) -> crate::dom::node::Node,
    use_ids: &mut [String],
    next_use: &mut usize,
    rendered: &mut usize,
) {
    for child in &mut node.children {
        walk_render(
            child, state, page_title, ids, body_of, use_ids, next_use, rendered,
        );
    }

    if let Some((group, attrs_json)) = read_references(node) {
        let refs = state.group(&group);
        // The list renderer works in terms of the `Reference`; the body renderer the
        // caller supplied works in terms of wikitext. Bridging here keeps the
        // renderer's signature about what it needs rather than about what happens to
        // be available.
        let body = |r: &Reference| body_of(&r.body);
        let list = references_list_nodes(&refs, &group, page_title, &body);
        let mut wrap = crate::dom::node::Node::element(crate::dom::node::ElementKind::Other(
            "div".to_string(),
        ));
        wrap.set_attr("class", "mw-references-wrap");
        wrap.set_attr("typeof", "mw:Extension/references");
        wrap.set_attr("about", ids.take_about());
        wrap.set_attr("data-mw", references_data_mw(&attrs_json, false));
        wrap.push_child(list);
        *node = wrap;
        return;
    }

    if read_ref(node).is_some() {
        let Some(ref_id) = use_ids.get(*next_use).cloned() else {
            return;
        };
        *next_use += 1;
        let Some(reference) = state.references.iter().find(|r| r.uses.contains(&ref_id)) else {
            return;
        };
        let marker = ref_marker_nodes(reference, &ref_id, page_title, ids);
        *node = marker;
        *rendered += 1;
    }
}

/// Whether a `typeof` attribute contains `value`.
///
/// `typeof` is space-separated and routinely carries several values — a `#tag:`-built
/// extension is `"mw:Extension mw:Transclusion"` — so an equality check misses the
/// common case of an extension emitted by a template. The DOM spec says consumers
/// must treat these like class names (`[typeof~=…]`).
fn typeof_contains(node: &crate::dom::node::Node, value: &str) -> bool {
    node.get_attr("typeof")
        .is_some_and(|t| t.split_whitespace().any(|v| v == value))
}

/// Read a `<references>` element into `(group, attrs_json)`.
///
/// `attrs_json` is the attribute list **as written**, in source order, because
/// Parsoid echoes it into `data-mw` verbatim rather than normalising it: a bare
/// `<references/>` records `{}`, and `<references group="n" responsive="yes"/>`
/// records both with their values untouched. Reconstructing it from parsed flags
/// would turn `responsive=""` into `false` and drop the distinction between an
/// absent attribute and an empty one.
fn read_references(node: &crate::dom::node::Node) -> Option<(String, String)> {
    if !typeof_contains(node, "mw:Extension") || node.get_attr("name") != Some("references") {
        return None;
    }
    let source = node.get_attr("source")?;
    let attrs = start_tag_attrs(source, "references")?;
    let group = attr_value(attrs, "group").unwrap_or_default();
    let mut pairs = Vec::new();
    let mut rest = attrs;
    while let Some((key, value, consumed)) = next_attr(rest) {
        pairs.push(format!("{}:{}", json_string(key), json_string(value)));
        rest = &rest[consumed..];
    }
    Some((group, format!("{{{}}}", pairs.join(","))))
}

/// Read one `key=value` (or bare `key`) pair from the front of an attribute run.
///
/// Returns the pair and how many bytes it consumed. Mirrors the attribute
/// grammar `start_tag_attrs` already exposes rather than re-parsing the source.
fn next_attr(rest: &str) -> Option<(&str, &str, usize)> {
    let trimmed = rest.trim_start();
    let skipped = rest.len() - trimmed.len();
    if trimmed.is_empty() {
        return None;
    }
    let key_end = trimmed
        .find(|c: char| c == '=' || c.is_whitespace())
        .unwrap_or(trimmed.len());
    let key = &trimmed[..key_end];
    // `/` is the self-closing marker, not an attribute name: a bare
    // `<references/>` must record `"attrs":{}`.
    if key.is_empty() || key == "/" {
        return None;
    }
    let after_key = &trimmed[key_end..];
    if !after_key.starts_with('=') {
        // A valueless attribute: Parsoid records it with an empty string.
        return Some((key, "", skipped + key_end));
    }
    let after_eq = after_key[1..].trim_start();
    let eq_skip = after_key[1..].len() - after_eq.len();
    let (value, value_len) = if let Some(stripped) = after_eq.strip_prefix('"') {
        match stripped.find('"') {
            Some(end) => (&stripped[..end], end + 2),
            None => (stripped, stripped.len() + 1),
        }
    } else {
        let end = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
        (&after_eq[..end], end)
    };
    Some((key, value, skipped + key_end + 1 + eq_skip + value_len))
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

/// Build the inline `<sup>` marker for a `<ref>` as DOM nodes.
///
/// `page_title` is the page the anchor targets: Cite links to a note on the *same*
/// page, so it is the article under parse, not the ref.
///
/// `ids` supplies the `mw`-style ids on the inner elements. They are allocated
/// from the document-wide sequence the page-bundle pass also draws on, so a Cite
/// span is numbered in the same order as every other element on the page.
pub fn ref_marker_nodes(
    reference: &Reference,
    ref_id: &str,
    page_title: &str,
    ids: &mut DocIds,
) -> crate::dom::node::Node {
    use crate::dom::node::{ElementKind, Node};

    // Whether this use is the one that supplied the note's text, which decides
    // the `data-mw` shape: only that use carries the note's `body` pointer.
    let use_index = reference.uses.iter().position(|u| u == ref_id).unwrap_or(0);
    let mut sup = Node::element(ElementKind::Other("sup".to_string()));
    sup.set_attr("about", ids.take_about());
    sup.set_attr("class", "mw-ref reference");
    sup.set_attr("id", ref_id);
    sup.set_attr("rel", "dc:references");
    sup.set_attr("typeof", "mw:Extension/ref");
    sup.set_attr("data-mw", ref_data_mw(reference, use_index));

    let href = format!("./{}#{}", page_title.replace(' ', "_"), reference.anchor());
    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("href", href);
    mark_for_id(&mut a);

    let mut text = Node::element(ElementKind::Other("span".to_string()));
    text.set_attr("class", "mw-reflink-text");
    mark_for_id(&mut text);

    let mut open = Node::element(ElementKind::Other("span".to_string()));
    open.set_attr("class", "cite-bracket");
    mark_for_id(&mut open);
    open.push_child(Node::text("["));

    let mut close = Node::element(ElementKind::Other("span".to_string()));
    close.set_attr("class", "cite-bracket");
    mark_for_id(&mut close);
    close.push_child(Node::text("]"));

    text.push_child(open);
    text.push_child(Node::text(reference.label()));
    text.push_child(close);
    a.push_child(text);
    sup.push_child(a);
    sup
}

/// Build the note list for one group of `<references>` as DOM nodes.
///
/// `body_of` supplies each note's rendered content. It cannot be produced here: a
/// note is wikitext that has to go back through the parser, because it may contain
/// templates, links, or another ref. Passing it in keeps this module independent of
/// the pipeline.
pub fn references_list_nodes(
    refs: &[&Reference],
    group: &str,
    page_title: &str,
    body_of: &dyn Fn(&Reference) -> crate::dom::node::Node,
) -> crate::dom::node::Node {
    use crate::dom::node::{ElementKind, Node};

    let page = page_title.replace(' ', "_");
    let mut ol = Node::element(ElementKind::Other("ol".to_string()));
    ol.set_attr("class", "mw-references references");
    if !group.is_empty() {
        ol.set_attr("data-mw-group", group);
    }
    mark_for_id(&mut ol);

    for r in refs {
        let note_id = r.note_id();
        let mut li = Node::element(ElementKind::Other("li".to_string()));
        li.set_attr("about", format!("#{note_id}"));
        li.set_attr("id", note_id.clone());
        li.set_attr("data-mw-footnote-number", r.label());

        // A single use renders its back-link bare; two or more are wrapped in
        // `<span class="mw-cite-backlink">` and separated by a space. That
        // asymmetry is Cite's actual output, verified against a cached page.
        if r.uses.len() == 1 {
            li.push_child(backlink_node(&r.uses[0], &page, group, None));
        } else {
            let mut wrap = Node::element(ElementKind::Other("span".to_string()));
            wrap.set_attr("class", "mw-cite-backlink");
            mark_for_id(&mut wrap);
            for (n, use_id) in r.uses.iter().enumerate() {
                if n > 0 {
                    wrap.push_child(Node::text(" "));
                }
                wrap.push_child(backlink_node(use_id, &page, group, Some(n + 1)));
            }
            li.push_child(wrap);
        }
        li.push_child(Node::text(" "));

        if !r.self_closing {
            let mut text = Node::element(ElementKind::Other("span".to_string()));
            text.set_attr("id", format!("mw-reference-text-{note_id}"));
            text.set_attr("class", "mw-reference-text reference-text");
            if !group.is_empty() {
                text.set_attr("data-mw-group", group);
            }
            text.push_child(body_of(r));
            li.push_child(text);
        }
        ol.push_child(li);
    }
    ol
}

/// One back-link. `label` is the use number when a note has several, and `None`
/// for the single-use case, which renders an arrow instead.
fn backlink_node(
    use_id: &str,
    page: &str,
    group: &str,
    label: Option<usize>,
) -> crate::dom::node::Node {
    use crate::dom::node::{ElementKind, Node};

    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("href", format!("./{page}#{use_id}"));
    // The group marker and the `referencedBy` relation both appear only on the
    // single-use form in Cite's output.
    if label.is_none() {
        if !group.is_empty() {
            a.set_attr("data-mw-group", group);
        }
        a.set_attr("rel", "mw:referencedBy");
    }
    mark_for_id(&mut a);

    let mut text = Node::element(ElementKind::Other("span".to_string()));
    text.set_attr("class", "mw-linkback-text");
    mark_for_id(&mut text);
    text.push_child(Node::text(match label {
        Some(n) => n.to_string(),
        None => "\u{2191}".to_string(),
    }));
    a.push_child(text);
    a
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
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{"name":"Badenhorst2019"}}"#
        );

        // A ref with text points at its note rather than inlining the source:
        // the text is rendered into the reference list, and `extsrc` is not what
        // the served page carries.
        let mut st = CiteState::new();
        st.add("", "", "some text", false);
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{},"body":{"id":"mw-reference-text-cite_note--1"}}"#
        );
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b"), r#""a\"b""#);
        assert_eq!(json_string("a\\b"), r#""a\\b""#);
        assert_eq!(json_string("a\nb"), r#""a\nb""#);
    }

    /// The marker's structure, with the ids allocated in order.
    ///
    /// The shape is taken from the cached Parsoid output of `Zebra`, and the
    /// assertion is on the *tree* rather than a flattened string: the nesting is
    /// what the page relies on, and it is what a string comparison would obscure.
    #[test]
    fn ref_marker_has_the_cached_structure() {
        let mut st = CiteState::new();
        st.add("Badenhorst2019", "", "", true);
        let mut ids = DocIds::new();
        let sup = ref_marker_nodes(
            &st.references[0],
            "cite_ref-Badenhorst2019_1-0",
            "Zebra",
            &mut ids,
        );

        assert_eq!(
            sup.kind,
            crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::Other(
                "sup".to_string()
            ))
        );
        assert_eq!(sup.get_attr("class"), Some("mw-ref reference"));
        assert_eq!(sup.get_attr("id"), Some("cite_ref-Badenhorst2019_1-0"));
        assert_eq!(sup.get_attr("rel"), Some("dc:references"));
        assert_eq!(sup.get_attr("typeof"), Some("mw:Extension/ref"));
        assert_eq!(
            sup.get_attr("data-mw"),
            Some(r#"{"name":"ref","attrs":{"name":"Badenhorst2019"}}"#)
        );

        // `<sup><a href="./Zebra#cite_note-Badenhorst2019-1"><span class="mw-reflink-text">…`
        let a = &sup.children[0];
        assert_eq!(
            a.get_attr("href"),
            Some("./Zebra#cite_note-Badenhorst2019-1")
        );
        let text = &a.children[0];
        assert_eq!(text.get_attr("class"), Some("mw-reflink-text"));

        // The brackets are two spans around the bare number.
        assert_eq!(text.children.len(), 3, "open, number, close");
        assert_eq!(text.children[0].get_attr("class"), Some("cite-bracket"));
        assert_eq!(
            text.children[0].children[0].kind,
            crate::dom::node::NodeKind::Text("[".to_string())
        );
        assert_eq!(
            text.children[1].kind,
            crate::dom::node::NodeKind::Text("1".to_string())
        );
        assert_eq!(text.children[2].get_attr("class"), Some("cite-bracket"));
        assert_eq!(
            text.children[2].children[0].kind,
            crate::dom::node::NodeKind::Text("]".to_string())
        );
    }

    /// A single-use note renders a bare back-link whose text is an arrow.
    #[test]
    fn a_single_use_note_has_a_bare_backlink() {
        let mut st = CiteState::new();
        st.add("", "", "body", false);
        let refs: Vec<&Reference> = st.references.iter().collect();
        let ol = references_list_nodes(&refs, "", "Zebra", &|_| {
            crate::dom::node::Node::text("BODY")
        });
        assert_eq!(ol.get_attr("class"), Some("mw-references references"));
        let li = &ol.children[0];
        assert_eq!(li.get_attr("id"), Some("cite_note--1"));
        assert_eq!(li.get_attr("data-mw-footnote-number"), Some("1"));

        let a = &li.children[0];
        assert_eq!(a.get_attr("href"), Some("./Zebra#cite_ref-1-0"));
        assert_eq!(a.get_attr("rel"), Some("mw:referencedBy"));
        assert_eq!(
            a.children[0].children[0].kind,
            crate::dom::node::NodeKind::Text("\u{2191}".to_string()),
            "a single use is an arrow"
        );
        assert!(
            li.children
                .iter()
                .all(|c| c.get_attr("class") != Some("mw-cite-backlink")),
            "one use must not be wrapped in the back-link span"
        );

        // The note text span, which carries the body.
        let text = li.children.last().unwrap();
        assert_eq!(text.get_attr("id"), Some("mw-reference-text-cite_note--1"));
        assert_eq!(
            text.get_attr("class"),
            Some("mw-reference-text reference-text")
        );
        assert_eq!(
            text.children[0].kind,
            crate::dom::node::NodeKind::Text("BODY".to_string())
        );
    }

    /// Two uses of one note are wrapped and numbered, the asymmetry the cached page
    /// shows for `Badenhorst2019`.
    #[test]
    fn several_uses_are_wrapped_and_numbered() {
        let mut st = CiteState::new();
        st.add("Badenhorst2019", "", "body", false);
        st.add("Badenhorst2019", "", "", true);
        let refs: Vec<&Reference> = st.references.iter().collect();
        let ol = references_list_nodes(&refs, "", "Zebra", &|_| crate::dom::node::Node::text("B"));
        let li = &ol.children[0];
        let wrap = &li.children[0];
        assert_eq!(wrap.get_attr("class"), Some("mw-cite-backlink"));

        let links: Vec<_> = wrap
            .children
            .iter()
            .filter(|c| c.kind.is_element())
            .collect();
        assert_eq!(links.len(), 2, "one link per use");
        assert_eq!(
            links[0].get_attr("href"),
            Some("./Zebra#cite_ref-Badenhorst2019_1-0")
        );
        assert_eq!(
            links[1].get_attr("href"),
            Some("./Zebra#cite_ref-Badenhorst2019_1-1")
        );
        // The labels count uses, not the note number.
        assert_eq!(
            links[0].children[0].children[0].kind,
            crate::dom::node::NodeKind::Text("1".to_string())
        );
        assert_eq!(
            links[1].children[0].children[0].kind,
            crate::dom::node::NodeKind::Text("2".to_string())
        );
    }

    #[test]
    fn a_group_adds_the_group_attributes() {
        let mut st = CiteState::new();
        st.add("", "lower-alpha", "a", false);
        let refs: Vec<&Reference> = st.references.iter().collect();
        let ol = references_list_nodes(&refs, "lower-alpha", "Zebra", &|_| {
            crate::dom::node::Node::text("A")
        });
        assert_eq!(ol.get_attr("data-mw-group"), Some("lower-alpha"));
        let li = &ol.children[0];
        assert_eq!(
            li.get_attr("data-mw-footnote-number"),
            Some("a"),
            "a lower-alpha note is labelled a"
        );
    }

    #[test]
    fn references_data_mw_matches_the_cached_shape() {
        // `attrs` echoes what was written, and there is no `body`: the notes are
        // children of the wrapper, not `extsrc`.
        assert_eq!(
            references_data_mw("{}", false),
            r#"{"name":"references","attrs":{}}"#
        );
        // A tag the wiki synthesised because the page had none.
        assert_eq!(
            references_data_mw("{}", true),
            r#"{"name":"references","attrs":{},"autoGenerated":true}"#
        );
        // Written attributes are echoed with their values, not normalised.
        assert_eq!(
            references_data_mw(r#"{"group":"n","responsive":"yes"}"#, false),
            r#"{"name":"references","attrs":{"group":"n","responsive":"yes"}}"#
        );
    }

    /// The `body` pointer follows the *defining* use, which need not be the
    /// first — and on a real page often is not, because a reuse can appear
    /// earlier (inside an infobox, say) than the prose that carries the text.
    #[test]
    fn ref_data_mw_points_at_the_defining_use_not_the_first() {
        let mut st = CiteState::new();
        st.add("x", "", "hello", false);
        // Only use, and it defines: the pointer is here.
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{"name":"x"},"body":{"id":"mw-reference-text-cite_note-x-1"}}"#
        );

        // Reuse first, then the definition: the pointer moves to use 1, and use 0
        // carries none. This is the shape `Zebra` opens with.
        let mut st = CiteState::new();
        st.add("x", "", "", true);
        st.add("x", "", "hello", false);
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{"name":"x"}}"#
        );
        assert_eq!(
            ref_data_mw(&st.references[0], 1),
            r#"{"name":"ref","attrs":{"name":"x"},"body":{"id":"mw-reference-text-cite_note-x-1"}}"#
        );
        // The bare reuse must not blank the note's text.
        assert_eq!(st.references[0].body, "hello");
        assert_eq!(st.references[0].number, 1);
    }
}
