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
//! The `id`s follow Cite's `AnchorFormatter`: `cite_note-<name>-<n>` for a note,
//! `cite_ref-<name>_<n>-<use>` for a marker. Note the *different* separators —
//! `_` before the number in the marker, `-` in the note. An anonymous ref has no
//! name segment, so its note is `cite_note-<n>`, and its marker drops the `-<use>`
//! suffix entirely on the first use (`cite_ref-<n>`): Cite cannot know then whether
//! the ref will be reused, and a later use does get `-1`, `-2`, … A named ref
//! always carries the suffix, since its marker is keyed by name and number. Runs
//! of `_`/whitespace in a name collapse to one `_`.

use std::collections::HashMap;

/// The `data-mw.body` shape a single `<ref>` use carries.
///
/// Mirrors `References::renderReference`: a self-closing `<ref name="x"/>` has
/// no `body` key; an empty *tag pair* `<ref name="x"></ref>` gets
/// `"body":{"html":""}`; a use that carried content points at the note with
/// `"body":{"id":"mw-reference-text-…"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseBody {
    /// Self-closing: no `body` key.
    None,
    /// An empty tag pair: `"body":{"html":""}`.
    Empty,
    /// A use that carried content: `"body":{"id":"mw-reference-text-…"}`.
    Content,
}

/// One collected reference: a note in the list, plus every site that points at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The `name` attribute, `""` when anonymous. Drives the `id` and the sharing
    /// key; an anonymous ref shares with nothing.
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
    /// The **per-group** sequence (`numberInGroup`), starting from 1: the label
    /// shown in the marker and the note list (`[1]`, `[a]`, …).
    pub number: usize,
    /// The **global** sequence (`globalId`) across every group, starting from 1.
    /// The `cite_*` ids are keyed on this, not on `number`: an `upper-alpha` note's
    /// id is `cite_note-187` even though its label is `[c]`.
    pub global_id: usize,
    /// One entry per use site, in document order: the `id` Cite gave that marker.
    ///
    /// A note used once renders its single back-link *without* an enclosing
    /// `<span class="mw-cite-backlink">`; with two or more, each is wrapped and they
    /// are separated by a space. That asymmetry is real and verified against the
    /// cached output.
    pub uses: Vec<String>,
    /// One entry per use site, parallel to `uses`: the `<ref>` tag's own
    /// attributes, in source order, with empty values preserved. Parsoid echoes
    /// them verbatim into each use's `data-mw.attrs` (keys lowercased, values
    /// trimmed — `TokenUtils::kvToHash`), which is why a `<ref group="">` keeps
    /// its empty `group` where a `<ref name="x">` has none.
    pub use_attrs: Vec<Vec<(String, String)>>,
    /// One entry per use site, parallel to `uses`: the `data-mw.body` shape
    /// that use carries (see [`UseBody`]).
    ///
    /// Parsoid points every content-carrying use at the note's text, not only
    /// the one that supplied it: `References::renderReference` sets
    /// `$refDataMw->body->id` whenever the use's own body is non-empty, while a
    /// bare self-closing reuse (empty body) gets none and an empty *tag pair*
    /// gets an empty `html` body. An `{{sfn}}` footnote, which repeats the same
    /// content at every occurrence, therefore carries a `data-mw.body` on each —
    /// a shape that a `body_use`-only rule got wrong.
    pub use_body: Vec<UseBody>,
    /// The `about` id of the use that supplied this note's body.
    ///
    /// A `<ref>` body is expanded *at the ref site* during template expansion
    /// (`Parser::expand_templates`), and the resulting DOM is stashed under that
    /// about. The list renderer looks the body up by this key instead of
    /// re-tokenizing the wikitext, which is what gets the body's own extension
    /// ids (its `<templatestyles>`, say) spent in the position the service spends
    /// them — before the marker's id, and after the templates of the enclosing
    /// chunk.
    pub body_about: Option<String>,
}

impl Reference {
    /// The note's `id`, e.g. `cite_note-Badenhorst2019-1`, or `cite_note-1`
    /// when the ref is anonymous.
    pub fn note_id(&self) -> String {
        if self.name.is_empty() {
            format!("cite_note-{}", self.global_id)
        } else {
            format!(
                "cite_note-{}-{}",
                normalize_fragment_identifier(&self.name),
                self.global_id
            )
        }
    }

    /// The marker `id` for the first use, e.g. `cite_ref-Badenhorst2019_1-0`,
    /// or `cite_ref-1` when the ref is anonymous.
    pub fn ref_id(&self) -> String {
        marker_id(&self.name, self.global_id, 0)
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

    /// The label in the marker's `mw-reflink-text`.
    ///
    /// Cite's `MarkSymbolRenderer::renderFootnoteMarkLabel` prepends the group
    /// name for a non-default group, *unless* the group has a custom label list
    /// (the `cite_link_label_group-<group>` message on the wiki — the standard
    /// alphabetic groups ship one). So a `group="note"` ref reads `[note 1]`,
    /// while a `group="upper-alpha"` one reads `[A]`; the default group reads
    /// `[1]`. The note list's `data-mw-footnote-number` uses
    /// [`renderFootnoteNumber`](Self::label) instead, which never adds the
    /// prefix.
    pub fn mark_label(&self) -> String {
        match self.group.as_str() {
            "" | "lower-alpha" | "upper-alpha" | "lower-greek" => self.label(),
            group => format!("{group} {}", self.label()),
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
    /// `(group, name) -> index into `references``, so a repeated use of a **named**
    /// ref finds its note in O(1) rather than rescanning.
    ///
    /// Only named refs are indexed: Cite's `RefGroup::$indexByName` is filled
    /// under `if ( $ref->name )`, so every anonymous `<ref>` is a note of its own
    /// even when its content repeats (`<ref>a</ref><ref>a</ref>` renders two
    /// notes, `cite_note-1` and `cite_note-2`, both reading `a`).
    index: HashMap<(String, String), usize>,
    /// The next footnote number, *per group*: numbering restarts for each group.
    next_number: HashMap<String, usize>,
    /// The next global id (`globalId`), one sequence shared by every group.
    next_global: usize,
    /// Indices into `references` for each group's **current instance**, in
    /// first-use order. A `<references>` element consumes the instance it finds
    /// ([`CiteState::cut_group`]), so refs seen after it form a fresh one.
    group_members: HashMap<String, Vec<usize>>,
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
    /// `about` is the id the expansion spent on this use, which the body renderer
    /// uses to find the pre-expanded note content.
    pub fn add(
        &mut self,
        name: &str,
        group: &str,
        body: &str,
        self_closing: bool,
        about: Option<&str>,
    ) -> String {
        // Build the `data-mw.attrs` list Parsoid would echo for this tag: the
        // name when set, then the group when set. The real pipeline parses the
        // tag's attributes directly (see `add_with_attrs`) so that an explicit
        // empty value survives; this convenience form is for callers that only
        // have the parsed name and group.
        let mut attrs = Vec::new();
        if !name.is_empty() {
            attrs.push(("name".to_string(), name.to_string()));
        }
        if !group.is_empty() {
            attrs.push(("group".to_string(), group.to_string()));
        }
        self.add_with_attrs(name, group, attrs, body, self_closing, about)
    }

    /// Record a `<ref>` use, keeping its parsed attributes for `data-mw`.
    ///
    /// `attrs` are the tag's attributes in source order, empty values included
    /// (`TokenUtils::kvToHash` semantics); they are echoed verbatim into this
    /// use's `data-mw.attrs`. `name`/`group` are passed separately because they
    /// drive the note's identity and numbering.
    pub fn add_with_attrs(
        &mut self,
        name: &str,
        group: &str,
        attrs: Vec<(String, String)>,
        body: &str,
        self_closing: bool,
        about: Option<&str>,
    ) -> String {
        self.add_full(name, group, attrs, body, self_closing, about, true)
    }

    /// Record a **list-defined** ref — one defined inside `<references>`.
    ///
    /// Parsoid adds it to its group but not to the page's markers: outside a
    /// reference list a ref is "embedded content"
    /// (`ReferencesData::inReferenceList`), so it lands in `embeddedNodes`, gets
    /// no `cite_ref-…` use and no back-link. It still creates a note — and spends
    /// a `globalId`, if its name is new — and supplies that note's body.
    pub fn add_embedded(
        &mut self,
        name: &str,
        group: &str,
        attrs: Vec<(String, String)>,
        body: &str,
        self_closing: bool,
        about: Option<&str>,
    ) {
        self.add_full(name, group, attrs, body, self_closing, about, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn add_full(
        &mut self,
        name: &str,
        group: &str,
        attrs: Vec<(String, String)>,
        body: &str,
        self_closing: bool,
        about: Option<&str>,
        record_use: bool,
    ) -> String {
        // Cite trims a ref's name and group (`Validator::validateRef`):
        // `name="x "` and `name="x"` are the same note. Without the trim a
        // trailing space made a second note with the wrong number and id
        // (`Grand Theft Auto V`'s `PlayStationAwards 2013 `).
        let name = name.trim();
        let group = group.trim();
        // A **named** ref is identified by its name, so `<ref name="x" />` finds
        // the note that `<ref name="x">…</ref>` defined. An anonymous one is not
        // identified at all: Cite indexes only named refs (`RefGroup::push` under
        // `if ( $ref->name )`), so `<ref>a</ref><ref>a</ref>` is two notes and two
        // global ids, not one note used twice.
        let idx = if name.is_empty() {
            None
        } else {
            self.index
                .get(&(group.to_string(), name.to_string()))
                .copied()
        };
        let idx = match idx {
            Some(i) => i,
            None => {
                let counter = self.next_number.entry(group.to_string()).or_insert(1);
                let number = *counter;
                *counter += 1;
                let global_id = self.next_global + 1;
                self.next_global = global_id;
                self.references.push(Reference {
                    name: name.to_string(),
                    group: group.to_string(),
                    body: String::new(),
                    self_closing,
                    number,
                    global_id,
                    uses: Vec::new(),
                    use_attrs: Vec::new(),
                    use_body: Vec::new(),
                    body_about: None,
                });
                let i = self.references.len() - 1;
                self.group_members
                    .entry(group.to_string())
                    .or_default()
                    .push(i);
                if !name.is_empty() {
                    self.index.insert((group.to_string(), name.to_string()), i);
                }
                i
            }
        };

        // `References::renderReference` treats a body that is blank after
        // trimming as empty (`$isEmptyBody`), so `""` and `"   "` behave alike.
        let empty_body = body.trim().is_empty();

        // A use that carries text *defines* the note, wherever it sits. An empty
        // body never overwrites one, and never claims the pointer: a bare
        // `<ref name="x"/>` contributes a back-link and nothing else.
        let defines = !self_closing && !empty_body;
        if defines {
            self.references[idx].body = body.to_string();
            self.references[idx].self_closing = false;
            self.references[idx].body_about = about.map(str::to_string);
        }

        // The id is keyed on the note the ref *resolved to*, not on this call's
        // spelling, so two uses of `<ref name="x" />` produce `-0` and `-1` for the
        // same note — which is what makes the back-links line up. An *embedded*
        // (list-defined) ref produces no use at all.
        if !record_use {
            return String::new();
        }
        let use_index = self.references[idx].uses.len();
        let id = marker_id(
            &self.references[idx].name,
            self.references[idx].global_id,
            use_index,
        );
        self.references[idx].uses.push(id.clone());
        self.references[idx].use_attrs.push(attrs);
        // The use's *own* body drives its `data-mw.body` shape, not which use
        // defined the note (see `use_body`).
        let shape = if self_closing {
            UseBody::None
        } else if empty_body {
            UseBody::Empty
        } else {
            UseBody::Content
        };
        self.references[idx].use_body.push(shape);
        id
    }

    /// Take the notes of `group` collected so far, and start a fresh instance of
    /// the group.
    ///
    /// A `<references>` element renders the notes that exist when it is reached
    /// and then drops the group (`References::insertReferencesIntoDOM` ends in
    /// `ReferencesData::removeRefGroup`), so a later `<ref>` of the same group
    /// becomes a *new* note with a new number and `globalId`, and a second
    /// `<references>` for that group does not repeat the first one's notes. The
    /// returned indices are into [`CiteState::references`], in first-use order.
    pub fn cut_group(&mut self, group: &str) -> Vec<usize> {
        let members = self.group_members.remove(group).unwrap_or_default();
        // Drop the named-ref index and the number counter with it, so a reused
        // name defines a new note and the next ref of this group is numbered from
        // 1 again — both as a freshly created `RefGroup` would.
        self.index.retain(|(g, _), _| g != group);
        self.next_number.remove(group);
        members
    }

    /// The refs in one group, in first-use order.
    ///
    /// This is the *whole* group across every instance of it; a `<references>`
    /// element normally renders only its own [`CiteState::cut_group`] slice, and
    /// this is the fallback when the two walks disagree.
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

/// `AnchorFormatter::normalizeFragmentIdentifier`: collapse a run of `_` and
/// whitespace to a single `_`. MediaWiki normalizes underscores in `[[#…]]`
/// links but not in `id="…"` attributes, so Cite normalizes up front to make the
/// two agree.
fn normalize_fragment_identifier(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    let mut in_run = false;
    for c in id.chars() {
        if c == '_' || c.is_whitespace() {
            if !in_run {
                out.push('_');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// A marker's `id` for one use (`AnchorFormatter::getBacklinkIdentifier`):
/// `cite_ref-<name>_<n>-<use>` when named, `cite_ref-<n>` when anonymous.
///
/// The `-<use>` suffix is skipped for the first use of an *anonymous* ref: Cite
/// cannot know yet whether the ref will be reused, so `cite_ref-1` stays as it is
/// and a later use appends `-1`, `-2`, … A named ref always carries the suffix
/// because its marker is keyed by name and number from the outset.
fn marker_id(name: &str, number: usize, use_index: usize) -> String {
    if name.is_empty() {
        if use_index == 0 {
            format!("cite_ref-{number}")
        } else {
            format!("cite_ref-{number}-{use_index}")
        }
    } else {
        format!(
            "cite_ref-{}-{use_index}",
            normalize_fragment_identifier(&format!("{name}_{number}"))
        )
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
fn mark_for_id(node: &mut rustoid_ast::dom::node::Node) {
    node.empty_dp_slot = true;
}

impl Default for DocIds {
    fn default() -> Self {
        Self::new()
    }
}

/// The `data-mw` JSON for a `<ref>` marker.
///
/// The `attrs` come from the use's own tag attributes, in source order, empty
/// values included (`TokenUtils::kvToHash`), because Parsoid echoes them
/// verbatim — a `<ref group="" name="x">` records `{"group":"","name":"x"}`.
/// A ref with content records the body's wikitext as `extsrc`.
///
/// Rendered in the attribute order the cached Parsoid output uses.
pub fn ref_data_mw(reference: &Reference, use_index: usize) -> String {
    let attrs = reference
        .use_attrs
        .get(use_index)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let attrs = format!(
        "{{{}}}",
        attrs
            .iter()
            .map(|(k, v)| format!("{}:{}", json_string(k), json_string(v)))
            .collect::<Vec<_>>()
            .join(",")
    );
    // Every use that carried its own body points at the note's text; only a bare
    // reuse (empty body) carries none. That is *not* the same as "the defining
    // use": an `{{sfn}}` footnote repeats its content at every occurrence, so
    // each of its uses has a `body`, and a reuse may precede the definition —
    // `A<ref name="x"/>B<ref name="x">body</ref>` is a real page shape, and
    // `Zebra` opens with one inside an infobox — where the bare reuse carries
    // none and the defining use,
    // [`use_has_body`](Reference::use_has_body), does.
    //
    // `attrs` is emitted even when empty, which is Cite's shape for a bare
    // `<ref>`.
    match reference.use_body.get(use_index) {
        Some(UseBody::Content) => format!(
            "{{\"name\":\"ref\",\"attrs\":{attrs},\"body\":{{\"id\":{}}}}}",
            json_string(&format!("mw-reference-text-{}", reference.note_id()))
        ),
        Some(UseBody::Empty) => {
            format!("{{\"name\":\"ref\",\"attrs\":{attrs},\"body\":{{\"html\":\"\"}}}}")
        }
        // A bare `<ref>` (or a use whose shape was never recorded) has no body.
        _ => format!("{{\"name\":\"ref\",\"attrs\":{attrs}}}"),
    }
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
pub fn references_data_mw(attrs_json: &str, auto_generated: bool, has_body: bool) -> String {
    if auto_generated {
        return format!(
            "{{\"name\":\"references\",\"attrs\":{attrs_json},\"autoGenerated\":true}}"
        );
    }
    // A tag pair records an (empty) `body`; a self-closing `<references/>` does
    // not. `Template:Reflist` builds its list with `{{#tag:references|…}}`, which
    // is a tag pair, so the served output carries `"body":{"extsrc":""}`.
    if has_body {
        return format!(
            "{{\"name\":\"references\",\"attrs\":{attrs_json},\"body\":{{\"extsrc\":\"\"}}}}"
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
    root: &mut rustoid_ast::dom::node::Node,
    page_title: &str,
    ids: &mut DocIds,
    body_of: &dyn Fn(&Reference) -> rustoid_ast::dom::node::Node,
    nested_body: &dyn Fn(&str) -> Option<rustoid_ast::dom::node::Node>,
    references_body: &dyn Fn(&str) -> Option<rustoid_ast::dom::node::Node>,
) -> usize {
    let mut state = CiteState::new();
    let mut use_ids = Vec::new();
    // One entry per `<references>` element, in document order: the notes that
    // element renders. `collect` fills it, `render` consumes it (see
    // [`CiteState::cut_group`]).
    let mut cuts = Vec::new();
    collect(
        root,
        &mut state,
        &mut use_ids,
        &mut cuts,
        nested_body,
        references_body,
    );
    // Deliberately **not** skipped when `state` is empty: a `<references>` tag with
    // no refs still renders its wrapper and an empty `<ol>`, which is what Parsoid
    // emits (verified against the cached output — the list structure is always
    // there, and only the `<li>`s vary). Returning early here left the raw
    // extension element in the output instead.
    //
    // A note reached through a transclusion (`{{#tag:ref|…<ref>…</ref>…}}`) may
    // itself hold a `<ref>`; Cite renders it into the note body before the body is
    // spliced, so those converted bodies are stashed here and preferred over the
    // raw ones.
    let converted: std::cell::RefCell<
        std::collections::HashMap<String, rustoid_ast::dom::node::Node>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    let body_for_list = |r: &Reference| -> rustoid_ast::dom::node::Node {
        if let Some(n) = converted.borrow().get(&r.body) {
            return n.clone();
        }
        body_of(r)
    };
    render(
        root,
        &state,
        page_title,
        ids,
        &body_for_list,
        nested_body,
        &converted,
        &mut use_ids,
        &cuts,
    )
}

/// Walk once, recording every `<ref>` in document order, and the notes each
/// `<references>` element claims.
fn collect(
    node: &rustoid_ast::dom::node::Node,
    state: &mut CiteState,
    use_ids: &mut Vec<String>,
    cuts: &mut Vec<Vec<usize>>,
    nested_body: &dyn Fn(&str) -> Option<rustoid_ast::dom::node::Node>,
    references_body: &dyn Fn(&str) -> Option<rustoid_ast::dom::node::Node>,
) {
    collect_in(
        node,
        state,
        use_ids,
        cuts,
        nested_body,
        references_body,
        Ctx::default(),
    );
}

/// The Cite context a node is collected in.
#[derive(Clone, Copy, Default)]
struct Ctx<'a> {
    /// The node sits inside a `<references>` body, so its refs are list-defined:
    /// they fill notes and spend `globalId`s but record no marker
    /// (`ReferencesData::inReferenceList`).
    embedded: bool,
    /// The enclosing `<references>` element's group. A ref inside one takes that
    /// group whatever its own `group` attribute says
    /// (`Validator::validateRefInReferenceList`), and the list it feeds is the one
    /// that element renders. `None` outside such an element, and inside a nested
    /// `<ref>`, where the list context is dropped
    /// (`ReferencesData::referenceListGroup`).
    refs_group: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
fn collect_in(
    node: &rustoid_ast::dom::node::Node,
    state: &mut CiteState,
    use_ids: &mut Vec<String>,
    cuts: &mut Vec<Vec<usize>>,
    nested_body: &dyn Fn(&str) -> Option<rustoid_ast::dom::node::Node>,
    references_body: &dyn Fn(&str) -> Option<rustoid_ast::dom::node::Node>,
    ctx: Ctx,
) {
    // A `<references>` element renders the notes of its group that exist when it
    // is reached, then starts a fresh instance of that group
    // (`References::insertReferencesIntoDOM` renders the current `RefGroup` and
    // ends in `ReferencesData::removeRefGroup`), which is what keeps a repeated
    // list from repeating the notes. A *nested* list does neither. Its body holds
    // its **list-defined** refs (`{{reflist|refs=<ref name="A">…</ref>}}`), which
    // Parsoid parses into the element's children
    // (`ReferenceListTagHandler::sourceToDom`) and `processRefs` then walks at the
    // element's position; rustoid's element is childless, so the body is read from
    // the stash. Those refs join the list they sit in, before it is cut.
    if let Some(group) = references_group(node) {
        if let Some(body) = read_references_body(node)
            && let Some(body_node) = references_body(&body)
        {
            let inner = Ctx {
                embedded: true,
                refs_group: Some(&group),
            };
            for child in &body_node.children {
                collect_in(
                    child,
                    state,
                    use_ids,
                    cuts,
                    nested_body,
                    references_body,
                    inner,
                );
            }
        }
        if ctx.refs_group.is_none() {
            cuts.push(state.cut_group(&group));
        }
    }
    if let Some((name, group, body, self_closing, attrs)) = read_ref(node) {
        // A ref nested in a transcluded note is processed *before* the note itself
        // (Cite's `References::extractRefFromNode` calls `processNestedRefInRef`
        // before `addRef`), so the nested refs take the lower numbers. `nested_body`
        // returns the body only when Parsoid would have kept a nested ref in it —
        // the note's pipeline ran `inTemplate` (`{{#tag:ref|…}}`); see
        // [`Parser::nested_ref_bodies`]. `processNestedRefInRef` also raises the
        // nested depth, which drops `referenceListGroup()` — so a nested ref is
        // never embedded, and never takes the enclosing list's group, whatever
        // encloses it.
        if !self_closing
            && let Some(body_node) = nested_body(&body)
            && has_ref(&body_node)
        {
            let inner = Ctx::default();
            for child in &body_node.children {
                collect_in(
                    child,
                    state,
                    use_ids,
                    cuts,
                    nested_body,
                    references_body,
                    inner,
                );
            }
        }
        let about = node.get_attr("about");
        let group = ctx.refs_group.unwrap_or(&group);
        if ctx.embedded {
            state.add_embedded(&name, group, attrs, &body, self_closing, about);
        } else {
            use_ids.push(state.add_with_attrs(&name, group, attrs, &body, self_closing, about));
        }
    }
    for child in &node.children {
        collect_in(
            child,
            state,
            use_ids,
            cuts,
            nested_body,
            references_body,
            ctx,
        );
    }
}

/// Read a `<ref>` element into `(name, group, body, self_closing)`.
///
/// The tokenizer leaves an unknown extension tag as an `<extension>` element whose
/// `source` attribute holds the raw text, because it does not parse an extension's
/// interior. So the attributes are recovered from that source rather than read from
/// the element.
/// What a `<ref>` tag's source yields: its name, group, body wikitext,
/// self-closing flag, and the raw attributes echoed into `data-mw.attrs`.
type RefTag = (String, String, String, bool, Vec<(String, String)>);

fn read_ref(node: &rustoid_ast::dom::node::Node) -> Option<RefTag> {
    if !typeof_contains(node, "mw:Extension") || node.get_attr("name") != Some("ref") {
        return None;
    }
    let source = node.get_attr("source")?;
    let attrs = start_tag_attrs(source, "ref")?;
    // A self-closing ref carries no body, and its `source` is just the start tag.
    let self_closing = source.trim_end().ends_with("/>");
    let pairs = tag_attrs(attrs);
    let name = attr_value(attrs, "name").unwrap_or_default();
    let group = attr_value(attrs, "group").unwrap_or_default();
    let body = if self_closing {
        String::new()
    } else {
        body_between(source).unwrap_or_default()
    };
    Some((name, group, body, self_closing, pairs))
}

/// The tag's attributes as ordered `(key, value)` pairs, empty values kept.
///
/// The keys are lowercased and the values trimmed, matching
/// `TokenUtils::kvToHash` — the shape Parsoid stores in an extension's
/// `data-mw.attrs`.
fn tag_attrs(attrs: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = attrs;
    while let Some((key, value, consumed)) = next_attr(rest) {
        out.push((key.to_lowercase(), value.trim().to_string()));
        rest = &rest[consumed..];
    }
    out
}

/// The attribute text of `<name …>` (or `<name …/>`), without the brackets.
fn start_tag_attrs<'s>(source: &'s str, name: &str) -> Option<&'s str> {
    let rest = source.strip_prefix('<')?.strip_prefix(name)?;
    // The name must end here, so `<reference>` does not match `ref`.
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
        return None;
    }
    let end = rest.find('>')?;
    let attrs = &rest[..end];
    // A trailing `/` is the self-close marker, not part of the last value. An
    // *unquoted* value keeps slashes generally (`name=a/b`), but Parsoid's
    // `attribute_preprocessor_text` refuses a `/` immediately followed by `>`,
    // so `<ref name=X/>` names the ref `X`, not `X/`.
    Some(attrs.strip_suffix('/').unwrap_or(attrs))
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
#[allow(clippy::too_many_arguments)]
fn render(
    root: &mut rustoid_ast::dom::node::Node,
    state: &CiteState,
    page_title: &str,
    ids: &mut DocIds,
    body_of: &dyn Fn(&Reference) -> rustoid_ast::dom::node::Node,
    nested_body: &dyn Fn(&str) -> Option<rustoid_ast::dom::node::Node>,
    converted: &std::cell::RefCell<std::collections::HashMap<String, rustoid_ast::dom::node::Node>>,
    use_ids: &mut [String],
    cuts: &[Vec<usize>],
) -> usize {
    let mut rendered = 0usize;
    let mut next_use = 0usize;
    let mut cut_cursor = 0usize;
    walk_render(
        root,
        state,
        page_title,
        ids,
        body_of,
        nested_body,
        converted,
        use_ids,
        &mut next_use,
        &mut rendered,
        cuts,
        &mut cut_cursor,
        /* templated */ false,
    );
    rendered
}

/// Transform one node's children, then the node itself.
///
/// Returns whether the node must be dropped from its parent: a reference list
/// that comes out empty *and* from a template is removed rather than left behind
/// as an empty `<ol>` (Cite's T364830).
#[allow(clippy::too_many_arguments)]
fn walk_render(
    node: &mut rustoid_ast::dom::node::Node,
    state: &CiteState,
    page_title: &str,
    ids: &mut DocIds,
    body_of: &dyn Fn(&Reference) -> rustoid_ast::dom::node::Node,
    nested_body: &dyn Fn(&str) -> Option<rustoid_ast::dom::node::Node>,
    converted: &std::cell::RefCell<std::collections::HashMap<String, rustoid_ast::dom::node::Node>>,
    use_ids: &mut [String],
    next_use: &mut usize,
    rendered: &mut usize,
    cuts: &[Vec<usize>],
    cut_cursor: &mut usize,
    templated: bool,
) -> bool {
    // `templated` is "this node is templated content", which its own wrapper group
    // can make true as well as an ancestor; track the group as the children come,
    // since only the first element of a same-`about` run carries the `typeof`.
    let mut group_about: Option<String> = None;
    let mut group_is_tpl = false;
    node.children.retain_mut(|child| {
        match child.get_attr("about") {
            Some(about) => {
                if group_about.as_deref() != Some(about) {
                    group_about = Some(about.to_string());
                    group_is_tpl = typeof_contains(child, "mw:Transclusion");
                }
            }
            None => {
                group_about = None;
                group_is_tpl = false;
            }
        }
        let in_tpl = templated || (group_about.is_some() && group_is_tpl);
        !walk_render(
            child,
            state,
            page_title,
            ids,
            body_of,
            nested_body,
            converted,
            use_ids,
            next_use,
            rendered,
            cuts,
            cut_cursor,
            in_tpl,
        )
    });

    if let Some((group, attrs_json, has_body, responsive)) = read_references(node) {
        // The notes this element renders: those of `group` that were collected up
        // to here, not the whole group — a second `<references>` for the same group
        // renders only what came after the first (see [`collect`]). Falls back to
        // the whole group if the two walks ever disagree, so a desync empties no
        // list outright.
        let members = cuts.get(*cut_cursor);
        *cut_cursor += 1;
        let refs: Vec<&Reference> = match members {
            Some(indices) => indices
                .iter()
                .filter_map(|&i| state.references.get(i))
                .collect(),
            None => state.group(&group),
        };
        if refs.is_empty() && templated {
            return true;
        }
        // The list renderer works in terms of the `Reference`; the body renderer the
        // caller supplied works in terms of wikitext. Bridging here keeps the
        // renderer's signature about what it needs rather than about what happens to
        // be available.
        let body = |r: &Reference| body_of(r);
        let list = references_list_nodes(&refs, &group, page_title, &body);
        // Cite wraps the list in a responsive container unless `responsive` is
        // falsy (`Template:Reflist` passes `responsive=0` for a fixed-width
        // reflist). Either way the *container* is the extension element — the
        // wrapper when there is one, the `<ol>` otherwise — so it is the node
        // that carries `typeof`/`about`/`data-mw` and draws an id.
        let about = node.get_attr("about").map(str::to_string);
        let data_mw = references_data_mw(&attrs_json, false, has_body);
        if responsive {
            let mut wrap = rustoid_ast::dom::node::Node::element(
                rustoid_ast::dom::node::ElementKind::Other("div".to_string()),
            );
            // Cite adds `mw-references-columns` when the responsive wrapper holds
            // more than `CiteResponsiveReferencesThreshold` (10) notes.
            const RESPONSIVE_THRESHOLD: usize = 10;
            let class = if refs.len() > RESPONSIVE_THRESHOLD {
                "mw-references-wrap mw-references-columns"
            } else {
                "mw-references-wrap"
            };
            wrap.set_attr("class", class);
            wrap.set_attr("typeof", "mw:Extension/references");
            // The wrapper is the element Cite leaves behind, so it draws a node id
            // here (Cite's `$refsNode` is the extension element itself).
            mark_for_id(&mut wrap);
            // The id was spent during expansion (see `expand_templates`), where the
            // service spends it; only a tag that somehow reached here unnumbered
            // takes a fresh one.
            wrap.set_attr("about", about.unwrap_or_else(|| ids.take_about()));
            wrap.set_attr("data-mw", data_mw);
            wrap.push_child(list);
            *node = wrap;
        } else {
            // No wrapper: the `<ol>` is the extension element, so the extension's
            // `typeof`/`about`/`data-mw` land on it (the `<ol>` already drew its id
            // in `references_list_nodes`).
            let mut list = list;
            list.set_attr("typeof", "mw:Extension/references");
            list.set_attr("about", about.unwrap_or_else(|| ids.take_about()));
            list.set_attr("data-mw", data_mw);
            *node = list;
        }
        return false;
    }

    if let Some((_, _, body, self_closing, _)) = read_ref(node) {
        // A note reached through a transclusion may hold a nested `<ref>`. Cite
        // renders it into the body here, before the note's own marker is built,
        // and the body renderer later prefers the converted body. The nested refs
        // were collected first (`collect`), so they take the next ids. `nested_body`
        // is `None` for a body Parsoid would never have kept a nested ref in.
        if !self_closing
            && let Some(mut body_node) = nested_body(&body)
            && has_ref(&body_node)
        {
            walk_render(
                &mut body_node,
                state,
                page_title,
                ids,
                body_of,
                nested_body,
                converted,
                use_ids,
                next_use,
                rendered,
                cuts,
                cut_cursor,
                templated,
            );
            converted.borrow_mut().insert(body, body_node);
        }
        let Some(ref_id) = use_ids.get(*next_use).cloned() else {
            return false;
        };
        *next_use += 1;
        let Some(reference) = state.references.iter().find(|r| r.uses.contains(&ref_id)) else {
            return false;
        };
        let about = node.get_attr("about").map(str::to_string);
        let mut marker = ref_marker_nodes(reference, &ref_id, page_title, ids, about);
        // A `<ref>` that is the sole content of a template call is that
        // transclusion's encapsulation target. In Parsoid the ref travels as a
        // *sealed* fragment (`mw:DOMFragment/sealed/ref`), so `tplwrap` stamps
        // it before Cite swaps the sealed type for `mw:Extension/ref`: the
        // `typeof` keeps `mw:Transclusion` first and the template's `parts`
        // stay in `data-mw`. An *unpacked* fragment instead gets the type
        // appended by `UnpackDOMFragments`, which is why `<pre>` and the ref
        // differ.
        apply_transclusion_encapsulation(&mut marker, node);
        *node = marker;
        *rendered += 1;
    }
    false
}

/// Whether a node's subtree contains a `<ref>` element (Cite's
/// `References::hasRef`, one fragment deep — a ref inside a nested ref's own body
/// is reached by the recursion, not here).
fn has_ref(node: &rustoid_ast::dom::node::Node) -> bool {
    node.children
        .iter()
        .any(|c| read_ref(c).is_some() || has_ref(c))
}

/// Carry a transclusion's encapsulation onto a Cite marker.
///
/// `source` is the encapsulation target the tree builder produced for the
/// enclosing `{{…}}` (the ref placeholder). Its `about` already sits on the
/// marker; the `typeof` and `data-mw` are merged here so the marker reports the
/// template it came from, as the service serves it.
fn apply_transclusion_encapsulation(
    marker: &mut rustoid_ast::dom::node::Node,
    source: &rustoid_ast::dom::node::Node,
) {
    let in_transclusion = source
        .get_attr("typeof")
        .is_some_and(|t| t.split_whitespace().any(|x| x == "mw:Transclusion"));
    if !in_transclusion {
        return;
    }
    // `mw:Transclusion` leads the list (Parsoid prepends it while the ref is
    // still a sealed fragment).
    let typeof_attr = marker.get_attr("typeof").unwrap_or_default().to_string();
    if !typeof_attr
        .split_whitespace()
        .any(|x| x == "mw:Transclusion")
    {
        marker.set_attr("typeof", format!("mw:Transclusion {typeof_attr}"));
    }
    // Append the via-template wikitext to the ref's own `data-mw`.
    let parts = source
        .data_mw
        .clone()
        .or_else(|| source.get_attr("data-mw").map(str::to_string));
    let target = marker.get_attr("data-mw").map(str::to_string);
    if let Some(merged) = rustoid_ast::data_mw::append_parts_to_data_mw(parts, target) {
        marker.set_attr("data-mw", merged);
    }
}

/// Whether a `typeof` attribute contains `value`.
///
/// `typeof` is space-separated and routinely carries several values — a `#tag:`-built
/// extension is `"mw:Extension mw:Transclusion"` — so an equality check misses the
/// common case of an extension emitted by a template. The DOM spec says consumers
/// must treat these like class names (`[typeof~=…]`).
fn typeof_contains(node: &rustoid_ast::dom::node::Node, value: &str) -> bool {
    node.get_attr("typeof")
        .is_some_and(|t| t.split_whitespace().any(|v| v == value))
}

/// The raw body of a `<references>` tag pair, which holds its list-defined refs.
///
/// `None` for a self-closing `<references/>`, and for a tag pair whose body is
/// empty (the element is `childless` either way, and the stash is keyed by the
/// body text). The extraction mirrors [`read_ref`]'s: the body is what sits
/// between the start tag's `>` and the closing `</references>`.
fn read_references_body(node: &rustoid_ast::dom::node::Node) -> Option<String> {
    let source = node.get_attr("source")?;
    if source.trim_end().ends_with("/>") {
        return None;
    }
    let body = body_between(source)?;
    (!body.is_empty()).then_some(body)
}

/// The group a `<references>` element renders, or `None` if the node is not one.
fn references_group(node: &rustoid_ast::dom::node::Node) -> Option<String> {
    read_references(node).map(|(group, _, _, _)| group)
}

/// Read a `<references>` element into `(group, attrs_json, has_body)`.
///
/// `attrs_json` is the attribute list **as written**, in source order, because
/// Parsoid echoes it into `data-mw` verbatim rather than normalising it: a bare
/// `<references/>` records `{}`, and `<references group="n" responsive="yes"/>`
/// records both with their values untouched. Reconstructing it from parsed flags
/// would turn `responsive=""` into `false` and drop the distinction between an
/// absent attribute and an empty one.
///
/// `has_body` is false for a self-closing `<references/>` and true for a tag
/// pair (which records an empty `data-mw.body`), matching the `selfClose` flag
/// Cite reads.
///
/// `responsive` is whether Cite wraps the list in its responsive container.
/// Cite treats the attribute as a PHP boolean-style switch, so an absent
/// attribute is *on* (the default) and only a falsy value (`""`/`"0"`)
/// turns it off — `Template:Reflist` passes `responsive=0` for a fixed-width
/// reflist, and the `<ol>` is then the extension element with no wrapper.
fn read_references(node: &rustoid_ast::dom::node::Node) -> Option<(String, String, bool, bool)> {
    if !typeof_contains(node, "mw:Extension") || node.get_attr("name") != Some("references") {
        return None;
    }
    let source = node.get_attr("source")?;
    let has_body = !source.trim_end().ends_with("/>");
    let attrs = start_tag_attrs(source, "references")?;
    let group = attr_value(attrs, "group").unwrap_or_default();
    let responsive = attr_value(attrs, "responsive").is_none_or(|v| !(v.is_empty() || v == "0"));
    let mut pairs = Vec::new();
    let mut rest = attrs;
    while let Some((key, value, consumed)) = next_attr(rest) {
        pairs.push(format!("{}:{}", json_string(key), json_string(value)));
        rest = &rest[consumed..];
    }
    Some((
        group,
        format!("{{{}}}", pairs.join(",")),
        has_body,
        responsive,
    ))
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
    // HTML allows whitespace around the `=`, and a ref may be written
    // `<ref name = mcgavin>`; without skipping it here the `name` looked like a
    // valueless attribute (`name=""`), so `data-mw.attrs` lost the name even
    // though the note and its markers were keyed on it.
    let after_ws = after_key.trim_start();
    let ws_len = after_key.len() - after_ws.len();
    if !after_ws.starts_with('=') {
        // A valueless attribute: Parsoid records it with an empty string.
        return Some((key, "", skipped + key_end + ws_len));
    }
    let after_eq = after_ws[1..].trim_start();
    let eq_skip = after_ws[1..].len() - after_eq.len();
    let (value, value_len) = match after_eq.chars().next() {
        // A quoted value (double or single) ends at the matching quote; the
        // quotes are not part of it. Mirrors `attr_value`.
        Some(q @ ('"' | '\'')) => {
            let stripped = &after_eq[1..];
            match stripped.find(q) {
                Some(end) => (&stripped[..end], end + 2),
                None => (stripped, stripped.len() + 1),
            }
        }
        // An unquoted value runs to the next whitespace.
        _ => {
            let end = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
            (&after_eq[..end], end)
        }
    };
    Some((
        key,
        value,
        skipped + key_end + ws_len + 1 + eq_skip + value_len,
    ))
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
    about: Option<String>,
) -> rustoid_ast::dom::node::Node {
    use rustoid_ast::dom::node::{ElementKind, Node};

    // Whether this use is the one that supplied the note's text, which decides
    // the `data-mw` shape: only that use carries the note's `body` pointer.
    let use_index = reference.uses.iter().position(|u| u == ref_id).unwrap_or(0);
    let mut sup = Node::element(ElementKind::Other("sup".to_string()));
    // The expansion already spent the id (see `expand_templates`); a marker that
    // reached here without one — a ref built outside the expansion, e.g. in a
    // test — falls back to a fresh one.
    sup.set_attr("about", about.unwrap_or_else(|| ids.take_about()));
    sup.set_attr("class", "mw-ref reference");
    sup.set_attr("id", ref_id);
    sup.set_attr("rel", "dc:references");
    // A named ref with no text carries the error on every use
    // (`ErrorUtils::addErrorsToNode`), both as an extra `typeof` and in
    // `data-mw.errors`.
    let no_text = !reference.name.is_empty() && reference.body.trim().is_empty();
    sup.set_attr(
        "typeof",
        if no_text {
            "mw:Extension/ref mw:Error"
        } else {
            "mw:Extension/ref"
        },
    );
    let mut dmw = ref_data_mw(reference, use_index);
    if no_text {
        dmw.pop();
        dmw.push_str(&format!(
            ",\"errors\":[{{\"key\":\"cite_error_references_no_text\",\"params\":[{}]}}]}}",
            json_string(&reference.name)
        ));
    }
    sup.set_attr("data-mw", dmw);

    let href = format!("./{}#{}", page_title.replace(' ', "_"), reference.anchor());
    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("href", href);
    // The citation link carries the group marker when the ref is grouped
    // (mirrors `References::renderRef`'s `if ($ref->group)`).
    if !reference.group.is_empty() {
        a.set_attr("data-mw-group", &reference.group);
    }
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
    text.push_child(Node::text(reference.mark_label()));
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
    body_of: &dyn Fn(&Reference) -> rustoid_ast::dom::node::Node,
) -> rustoid_ast::dom::node::Node {
    use rustoid_ast::dom::node::{ElementKind, Node};

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

        // Cite always wraps the back-links in `<span class="mw-cite-backlink">`,
        // even for a single use. The `referencedBy` relation sits on the wrapping
        // span when a note has several uses, and on the `<a>` when it has one.
        let multi = r.uses.len() > 1;
        let mut wrap = Node::element(ElementKind::Other("span".to_string()));
        if multi {
            wrap.set_attr("rel", "mw:referencedBy");
        }
        wrap.set_attr("class", "mw-cite-backlink");
        mark_for_id(&mut wrap);
        for (n, use_id) in r.uses.iter().enumerate() {
            if n > 0 {
                wrap.push_child(Node::text(" "));
            }
            let label = if multi { Some(n + 1) } else { None };
            wrap.push_child(backlink_node(use_id, &page, group, label));
        }
        li.push_child(wrap);
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
        // A named ref that never carried content has no text to show. Parsoid
        // attaches `cite_error_references_no_text` to every use and the list
        // renders it after the (empty) note text (`RefGroup::
        // renderReferenceListElement`).
        if !r.name.is_empty() && r.body.trim().is_empty() {
            li.push_child(Node::text(" "));
            li.push_child(cite_error_span("cite_error_references_no_text", &[&r.name]));
        }
        ol.push_child(li);
        // Backward-compatibility: a newline follows every note (T372889, PHP
        // `RefGroup::renderReferenceListElement`). It is a real text node child
        // of the `<ol>`, so it is serialized as `</li>\n<li …` and `</li>\n</ol>`.
        ol.push_child(Node::text("\n"));
    }
    ol
}

/// `ErrorUtils::renderParsoidError`: an interface-language `mw:I18n` span
/// wrapping `cite_error` around the specific error.
fn cite_error_span(key: &str, params: &[&str]) -> rustoid_ast::dom::node::Node {
    use rustoid_ast::dom::node::{ElementKind, Node};

    let params = params
        .iter()
        .map(|p| json_string(p))
        .collect::<Vec<_>>()
        .join(",");
    let mut span = Node::element(ElementKind::Other("span".to_string()));
    span.set_attr("typeof", "mw:I18n");
    span.set_attr("class", "error mw-ext-cite-error");
    span.set_attr(
        "data-mw-i18n",
        format!(
            "{{\"/\":{{\"lang\":\"x-user\",\"key\":\"cite_error\",\"params\":[{{\"text\":{{\"key\":\"{key}\",\"params\":[{params}]}}}}]}}}}"
        ),
    );
    mark_for_id(&mut span);
    span
}

/// One back-link. `label` is the use number when a note has several, and `None`
/// for the single-use case, which renders an arrow instead.
fn backlink_node(
    use_id: &str,
    page: &str,
    group: &str,
    label: Option<usize>,
) -> rustoid_ast::dom::node::Node {
    use rustoid_ast::dom::node::{ElementKind, Node};

    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("href", format!("./{page}#{use_id}"));
    // Cite's `createLinkback` stamps `data-mw-group` on every back-link of a
    // grouped note, single-use or not. The `referencedBy` relation, by contrast,
    // is on the `<a>` only for the single-use form (a multi-use note puts it on
    // the wrapping `<span>`).
    if !group.is_empty() {
        a.set_attr("data-mw-group", group);
    }
    if label.is_none() {
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
    fn an_anonymous_ref_is_never_shared() {
        // Cite indexes only named refs, so repeating the same anonymous content
        // makes a second note rather than a reuse: `<ref>a</ref><ref>a</ref>`
        // renders `cite_note-1` and `cite_note-2`, both reading `a`.
        let mut st = CiteState::new();
        st.add("", "", "a", false, None);
        st.add("", "", "b", false, None);
        st.add("", "", "a", false, None);
        assert_eq!(st.references.len(), 3, "each anonymous ref is its own note");
        assert_eq!(st.references[0].number, 1);
        assert_eq!(st.references[1].number, 2);
        assert_eq!(st.references[2].number, 3);
        assert_eq!(st.references[2].global_id, 3);
        assert_eq!(st.references[2].note_id(), "cite_note-3");
        assert_eq!(st.references[2].uses.len(), 1);
    }

    /// A named ref used again does not allocate a new number, even when the second
    /// use is self-closing — that is the whole point of naming a ref.
    #[test]
    fn a_named_ref_is_shared_between_uses() {
        let mut st = CiteState::new();
        st.add("x", "", "body", false, None);
        st.add("x", "", "", true, None);
        assert_eq!(st.references.len(), 1);
        assert_eq!(st.references[0].uses.len(), 2);
        assert_eq!(st.references[0].uses[1], "cite_ref-x_1-1");
    }

    /// The reflink text of a grouped ref carries the group name, unless the group
    /// has its own label list (the alphabetic groups do). The note list's
    /// `data-mw-footnote-number` never does.
    #[test]
    fn a_grouped_ref_marks_its_note_with_the_group_name() {
        let mut st = CiteState::new();
        st.add("", "note", "x", false, None);
        st.add("", "upper-alpha", "y", false, None);
        st.add("", "", "z", false, None);
        assert_eq!(st.references[0].label(), "1");
        assert_eq!(st.references[0].mark_label(), "note 1");
        assert_eq!(st.references[1].mark_label(), "A");
        assert_eq!(st.references[2].mark_label(), "1");
    }

    /// Numbering restarts per group: a `lower-alpha` group numbers from 1 again.
    #[test]
    fn groups_number_independently() {
        let mut st = CiteState::new();
        st.add("", "lower-alpha", "a", false, None);
        st.add("", "", "main", false, None);
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
        let id = st.add("Badenhorst2019", "", "body", false, None);
        assert_eq!(id, "cite_ref-Badenhorst2019_1-0");
        assert_eq!(st.references[0].note_id(), "cite_note-Badenhorst2019-1");

        let mut st = CiteState::new();
        let id = st.add("", "", "anon", false, None);
        assert_eq!(id, "cite_ref-1");
        assert_eq!(st.references[0].note_id(), "cite_note-1");
    }

    #[test]
    fn anonymous_ids_carry_neither_name_nor_use_suffix() {
        // `AnchorFormatter::getBacklinkIdentifier` drops the `-0` suffix on a
        // first, unnamed use; `getNoteIdentifier` uses the bare global id.
        let mut st = CiteState::new();
        let first = st.add("", "", "a", false, None);
        let second = st.add("", "", "a", false, None);
        assert_eq!(first, "cite_ref-1");
        assert_eq!(second, "cite_ref-2");
        assert_eq!(st.references[0].note_id(), "cite_note-1");
        assert_eq!(st.references[1].note_id(), "cite_note-2");
    }

    #[test]
    fn name_runs_of_underscore_and_space_collapse() {
        // `normalizeFragmentIdentifier`: `[\s_]+` becomes one `_`.
        assert_eq!(super::normalize_fragment_identifier("a b"), "a_b");
        assert_eq!(super::normalize_fragment_identifier("a__b"), "a_b");
        assert_eq!(super::normalize_fragment_identifier("a _b"), "a_b");
        assert_eq!(super::normalize_fragment_identifier("a_"), "a_");
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
        st.add("Badenhorst2019", "", "", true, None);
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{"name":"Badenhorst2019"}}"#
        );

        // A ref with text points at its note rather than inlining the source:
        // the text is rendered into the reference list, and `extsrc` is not what
        // the served page carries.
        let mut st = CiteState::new();
        st.add("", "", "some text", false, None);
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{},"body":{"id":"mw-reference-text-cite_note-1"}}"#
        );
    }

    /// An empty *tag pair* is not the same as a self-closing ref: Parsoid records
    /// an empty `html` body for it (`References::renderReference`, the
    /// `$hasMissingContent` arm that is not `selfClose`). A self-closing ref gets
    /// no `body` at all, and blank content counts as empty.
    #[test]
    fn an_empty_tag_pair_carries_an_empty_html_body() {
        let mut st = CiteState::new();
        st.add("U23", "upper-alpha", "", false, None);
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{"name":"U23","group":"upper-alpha"},"body":{"html":""}}"#
        );

        // Whitespace-only content is empty after trimming, as in PHP.
        let mut st = CiteState::new();
        st.add("", "", "  \n", false, None);
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{},"body":{"html":""}}"#
        );

        // The self-closing form still carries none.
        let mut st = CiteState::new();
        st.add("", "", "", true, None);
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{}}"#
        );
    }

    /// A `<ref group="" name="…">` keeps the empty `group`, in the order it was
    /// written. `Module:Footnotes` emits exactly this through
    /// `frame:extensionTag`, and the served `attrs` record both keys — unlike a
    /// `<ref name="x">`, where `group` is absent.
    #[test]
    fn ref_data_mw_keeps_an_explicit_empty_group_in_source_order() {
        let mut st = CiteState::new();
        st.add_with_attrs(
            "FOOTNOTEPlumbShaw201854",
            "",
            vec![
                ("group".to_string(), String::new()),
                ("name".to_string(), "FOOTNOTEPlumbShaw201854".to_string()),
            ],
            "body",
            false,
            None,
        );
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{"group":"","name":"FOOTNOTEPlumbShaw201854"},"body":{"id":"mw-reference-text-cite_note-FOOTNOTEPlumbShaw201854-1"}}"#
        );
    }

    /// A ref that is the sole content of a template call is that transclusion's
    /// encapsulation target: its marker gains a leading `mw:Transclusion` and the
    /// template's `parts`. A ref with no enclosing transclusion keeps a bare
    /// `mw:Extension/ref` and no `parts`.
    #[test]
    fn a_ref_inherits_its_enclosing_transclusion() {
        use rustoid_ast::dom::node::{ElementKind, Node};

        let mut st = CiteState::new();
        st.add("x", "", "body", false, None);
        let mut ids = DocIds::new();
        let build = |ids: &mut DocIds| {
            ref_marker_nodes(
                &st.references[0],
                "cite_ref-x_1-0",
                "Zebra",
                ids,
                Some("#mwt1".to_string()),
            )
        };

        // Inside a transclusion: `mw:Transclusion` leads, `parts` are appended.
        let mut marker = build(&mut ids);
        let mut source = Node::element(ElementKind::Other("extension".to_string()));
        source.set_attr("typeof", "mw:Extension mw:Transclusion");
        source.data_mw =
            Some(r#"{"parts":[{"template":{"target":{"wt":"sfn"}},"i":0}]}"#.to_string());
        apply_transclusion_encapsulation(&mut marker, &source);
        assert_eq!(
            marker.get_attr("typeof"),
            Some("mw:Transclusion mw:Extension/ref")
        );
        assert_eq!(
            marker.get_attr("data-mw"),
            Some(
                r#"{"name":"ref","attrs":{"name":"x"},"body":{"id":"mw-reference-text-cite_note-x-1"},"parts":[{"template":{"target":{"wt":"sfn"}},"i":0}]}"#
            )
        );

        // At the page level (no `mw:Transclusion`): unchanged.
        let mut plain = build(&mut ids);
        let mut plain_source = Node::element(ElementKind::Other("extension".to_string()));
        plain_source.set_attr("typeof", "mw:Extension");
        apply_transclusion_encapsulation(&mut plain, &plain_source);
        assert_eq!(plain.get_attr("typeof"), Some("mw:Extension/ref"));
        assert!(!plain.get_attr("data-mw").unwrap().contains("parts"));
    }

    /// A `<ref>` inside a transcluded note's body is collected before the note
    /// itself, so it takes the lower number: Cite's `extractRefFromNode` calls
    /// `processNestedRefInRef` before `addRef`. Whether the nested ref survived at
    /// all is what `nested_body` decides — it returns the body only for a note
    /// whose pipeline ran `inTemplate`.
    #[test]
    fn a_nested_ref_is_collected_before_its_note() {
        use rustoid_ast::dom::node::{ElementKind, Node};

        let ref_node = |typeof_: &str, source: &str| {
            let mut n = Node::element(ElementKind::Other("extension".to_string()));
            n.set_attr("typeof", typeof_);
            n.set_attr("name", "ref");
            n.set_attr("source", source);
            n
        };

        let mut body = Node::document();
        body.push_child(Node::text("A"));
        body.push_child(ref_node("mw:Extension", "<ref>B</ref>"));
        let nested_body = move |text: &str| text.contains("A<ref>B</ref>C").then(|| body.clone());
        let references_body = |_text: &str| None;

        let mut root = Node::document();
        root.push_child(ref_node(
            "mw:Transclusion mw:Extension",
            "<ref name=\"o\">A<ref>B</ref>C</ref>",
        ));

        let mut state = CiteState::new();
        let mut ids = Vec::new();
        let mut cuts = Vec::new();
        collect(
            &root,
            &mut state,
            &mut ids,
            &mut cuts,
            &nested_body,
            &references_body,
        );

        assert_eq!(state.references.len(), 2);
        // The anonymous nested ref is numbered first; the named note follows.
        assert_eq!(state.references[0].global_id, 1);
        assert_eq!(state.references[0].body, "B");
        assert_eq!(state.references[1].name, "o");
        assert_eq!(state.references[1].global_id, 2);
    }

    /// A `<references>` element's body holds **list-defined** refs, which Parsoid
    /// parses into the element's children (`ReferenceListTagHandler::sourceToDom`)
    /// and collects at the element's position. They are *embedded* refs — inside a
    /// reference list — so they fill their notes' bodies without adding a use: the
    /// note a use already made gets its text, and a definition nobody used still
    /// becomes a note (`ReferencesData::inReferenceList` puts it in
    /// `embeddedNodes`).
    #[test]
    fn a_list_defined_ref_fills_a_note_without_a_use() {
        use rustoid_ast::dom::node::{ElementKind, Node};

        let ref_node = |source: &str| {
            let mut n = Node::element(ElementKind::Other("extension".to_string()));
            n.set_attr("typeof", "mw:Extension");
            n.set_attr("name", "ref");
            n.set_attr("source", source);
            n
        };

        // The definition as the `<references>` body renders it.
        let mut body = Node::document();
        body.push_child(ref_node("<ref name=\"A\">TEXT</ref>"));
        body.push_child(ref_node("<ref name=\"Z\">ZZZ</ref>"));
        let references_body = move |text: &str| text.contains("name=\"A\"").then(|| body.clone());
        let nested_body = |_text: &str| None;

        let mut refs = Node::element(ElementKind::Other("div".to_string()));
        refs.set_attr("typeof", "mw:Extension");
        refs.set_attr("name", "references");
        refs.set_attr(
            "source",
            "<references><ref name=\"A\">TEXT</ref></references>",
        );

        let mut root = Node::document();
        // The use comes first, so the note is created with no body.
        root.push_child(ref_node("<ref name=\"A\"/>"));
        root.push_child(refs);

        let mut state = CiteState::new();
        let mut ids = Vec::new();
        let mut cuts = Vec::new();
        collect(
            &root,
            &mut state,
            &mut ids,
            &mut cuts,
            &nested_body,
            &references_body,
        );

        // The use is the only marker; neither definition records a use.
        assert_eq!(ids, vec!["cite_ref-A_1-0".to_string()]);
        assert_eq!(
            state.references.len(),
            2,
            "A gains its body, Z is a new note"
        );
        assert_eq!(state.references[0].name, "A");
        assert_eq!(state.references[0].body, "TEXT");
        assert_eq!(state.references[0].uses.len(), 1);
        assert_eq!(state.references[1].name, "Z");
        assert_eq!(state.references[1].global_id, 2);
    }

    /// A trailing `/` before `>` is the self-close marker, not part of an
    /// unquoted value: `<ref name=a/b/>` names the ref `a/b`, while an internal
    /// slash or one before whitespace is kept.
    #[test]
    fn a_trailing_solidus_is_the_self_close_marker() {
        let pairs = |s: &str| tag_attrs(start_tag_attrs(s, "ref").unwrap());
        let name = |s: &str| pairs(s)[0].1.clone();
        assert_eq!(name("<ref name=a/b>x</ref>"), "a/b");
        assert_eq!(name("<ref name=a/b/>"), "a/b");
        assert_eq!(name("<ref name=a/b//>"), "a/b/");
        assert_eq!(name("<ref name=a/b/ >x</ref>"), "a/b/");
        assert_eq!(name("<ref name=ab/>"), "ab");
    }

    /// HTML allows whitespace around the `=`; `<ref name = mcgavin>` names the
    /// ref — and `data-mw.attrs` — `mcgavin`, not the empty string. A genuinely
    /// valueless attribute still records `""`.
    #[test]
    fn whitespace_around_the_equals_sign_is_allowed() {
        let pairs = |s: &str| tag_attrs(start_tag_attrs(s, "ref").unwrap());
        assert_eq!(
            pairs("<ref name = mcgavin>x</ref>"),
            vec![("name".to_string(), "mcgavin".to_string())]
        );
        assert_eq!(
            pairs("<ref name =mcgavin group = note>x</ref>"),
            vec![
                ("name".to_string(), "mcgavin".to_string()),
                ("group".to_string(), "note".to_string())
            ]
        );
        assert_eq!(
            pairs("<ref name = 'mcgavin'/>"),
            vec![("name".to_string(), "mcgavin".to_string())]
        );
        assert_eq!(
            pairs("<ref name>x</ref>"),
            vec![("name".to_string(), String::new())]
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
        st.add("Badenhorst2019", "", "", true, None);
        let mut ids = DocIds::new();
        let sup = ref_marker_nodes(
            &st.references[0],
            "cite_ref-Badenhorst2019_1-0",
            "Zebra",
            &mut ids,
            None,
        );

        assert_eq!(
            sup.kind,
            rustoid_ast::dom::node::NodeKind::Element(rustoid_ast::dom::node::ElementKind::Other(
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
            rustoid_ast::dom::node::NodeKind::Text("[".to_string())
        );
        assert_eq!(
            text.children[1].kind,
            rustoid_ast::dom::node::NodeKind::Text("1".to_string())
        );
        assert_eq!(text.children[2].get_attr("class"), Some("cite-bracket"));
        assert_eq!(
            text.children[2].children[0].kind,
            rustoid_ast::dom::node::NodeKind::Text("]".to_string())
        );
    }

    /// A single-use note wraps its back-link in `mw-cite-backlink` too, with the
    /// `referencedBy` relation on the `<a>` and an arrow as the text.
    #[test]
    fn a_single_use_note_wraps_its_backlink() {
        let mut st = CiteState::new();
        st.add("", "", "body", false, None);
        let refs: Vec<&Reference> = st.references.iter().collect();
        let ol = references_list_nodes(&refs, "", "Zebra", &|_| {
            rustoid_ast::dom::node::Node::text("BODY")
        });
        assert_eq!(ol.get_attr("class"), Some("mw-references references"));
        let li = &ol.children[0];
        assert_eq!(li.get_attr("id"), Some("cite_note-1"));
        assert_eq!(li.get_attr("data-mw-footnote-number"), Some("1"));

        // The wrapping span, with no `rel` (that is on the `<a>` here).
        let wrap = &li.children[0];
        assert_eq!(wrap.get_attr("class"), Some("mw-cite-backlink"));
        assert_eq!(wrap.get_attr("rel"), None);
        let a = &wrap.children[0];
        assert_eq!(a.get_attr("href"), Some("./Zebra#cite_ref-1"));
        assert_eq!(a.get_attr("rel"), Some("mw:referencedBy"));
        assert_eq!(
            a.children[0].children[0].kind,
            rustoid_ast::dom::node::NodeKind::Text("\u{2191}".to_string()),
            "a single use is an arrow"
        );

        // The note text span, which carries the body.
        let text = li.children.last().unwrap();
        assert_eq!(text.get_attr("id"), Some("mw-reference-text-cite_note-1"));
        assert_eq!(
            text.get_attr("class"),
            Some("mw-reference-text reference-text")
        );
        assert_eq!(
            text.children[0].kind,
            rustoid_ast::dom::node::NodeKind::Text("BODY".to_string())
        );
    }

    /// Two uses of one note are wrapped and numbered, the asymmetry the cached page
    /// shows for `Badenhorst2019`.
    #[test]
    fn several_uses_are_wrapped_and_numbered() {
        let mut st = CiteState::new();
        st.add("Badenhorst2019", "", "body", false, None);
        st.add("Badenhorst2019", "", "", true, None);
        let refs: Vec<&Reference> = st.references.iter().collect();
        let ol = references_list_nodes(&refs, "", "Zebra", &|_| {
            rustoid_ast::dom::node::Node::text("B")
        });
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
            rustoid_ast::dom::node::NodeKind::Text("1".to_string())
        );
        assert_eq!(
            links[1].children[0].children[0].kind,
            rustoid_ast::dom::node::NodeKind::Text("2".to_string())
        );
    }

    #[test]
    fn a_group_adds_the_group_attributes() {
        let mut st = CiteState::new();
        st.add("", "lower-alpha", "a", false, None);
        let refs: Vec<&Reference> = st.references.iter().collect();
        let ol = references_list_nodes(&refs, "lower-alpha", "Zebra", &|_| {
            rustoid_ast::dom::node::Node::text("A")
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
        // A self-closing `<references/>` has no `body`; a tag pair records an
        // (empty) `extsrc`.
        assert_eq!(
            references_data_mw("{}", false, false),
            r#"{"name":"references","attrs":{}}"#
        );
        assert_eq!(
            references_data_mw("{}", false, true),
            r#"{"name":"references","attrs":{},"body":{"extsrc":""}}"#
        );
        // A tag the wiki synthesised because the page had none.
        assert_eq!(
            references_data_mw("{}", true, false),
            r#"{"name":"references","attrs":{},"autoGenerated":true}"#
        );
        // Written attributes are echoed with their values, not normalised.
        assert_eq!(
            references_data_mw(r#"{"group":"n","responsive":"yes"}"#, false, true),
            r#"{"name":"references","attrs":{"group":"n","responsive":"yes"},"body":{"extsrc":""}}"#
        );
    }

    /// The `body` pointer follows the use's *own* content, so an `{{sfn}}`
    /// footnote — whose every occurrence repeats the same body — carries a
    /// `data-mw.body` on each use, while a bare reuse carries none. A reuse can
    /// also appear earlier than the definition (inside an infobox, say), which is
    /// why the pointer is not simply "use zero".
    #[test]
    fn ref_data_mw_points_every_body_carrying_use_at_the_note() {
        let mut st = CiteState::new();
        st.add("x", "", "hello", false, None);
        // The only use, and it carries a body: the pointer is here.
        assert_eq!(
            ref_data_mw(&st.references[0], 0),
            r#"{"name":"ref","attrs":{"name":"x"},"body":{"id":"mw-reference-text-cite_note-x-1"}}"#
        );

        // Every occurrence of a repeated body points at the same note — the
        // `{{sfn}}` shape, where each use repeats the identical content.
        let mut st = CiteState::new();
        st.add("x", "", "hello", false, None);
        st.add("x", "", "hello", false, None);
        for use_index in 0..2 {
            assert_eq!(
                ref_data_mw(&st.references[0], use_index),
                r#"{"name":"ref","attrs":{"name":"x"},"body":{"id":"mw-reference-text-cite_note-x-1"}}"#
            );
        }

        // Reuse first, then the definition: the bare reuse carries no body, and
        // the defining use does. This is the shape `Zebra` opens with.
        let mut st = CiteState::new();
        st.add("x", "", "", true, None);
        st.add("x", "", "hello", false, None);
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

    /// Cite trims a ref's name, so a trailing space is the same note
    /// (`Grand Theft Auto V`'s `PlayStationAwards 2013 `) — and the marker id
    /// uses the trimmed spelling.
    #[test]
    fn named_refs_are_trimmed() {
        let mut st = CiteState::new();
        let first = st.add("PlayStationAwards 2013 ", "", "body", false, Some("#mwt1"));
        let second = st.add("PlayStationAwards 2013", "", "", true, None);
        assert_eq!(st.references.len(), 1);
        assert_eq!(first, "cite_ref-PlayStationAwards_2013_1-0");
        assert_eq!(second, "cite_ref-PlayStationAwards_2013_1-1");
    }
}
