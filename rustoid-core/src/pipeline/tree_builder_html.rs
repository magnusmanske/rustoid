//! HTML5 tree construction for the Parsoid token stream, driven by the faithful
//! RemexHtml port (see `crate::html5`) rather than html5ever.
//!
//! Ports PHP Parsoid's `TreeBuilderStage` + `RemexPipeline` adapters:
//!
//!   * `data-parsoid` / `data-mw` are stashed in a side table keyed by a
//!     `data-object-id` attribute (mirrors `DOMDataUtils::stashObjectInDoc`).
//!   * transclusion/param metas are inserted *unfostered* (mirrors
//!     `insertUnfosteredMeta` / `InHead::startTag`).
//!   * text/enewline tokens are buffered and flushed as one character run,
//!     tracking `tableDepth` and `inTransclusion`.
//!   * deleted start tags and stripped tags emit `mw:Placeholder` metas and
//!     re-emit wikitext source (mirrors `handleDeletedStartTag` and
//!     `insertPlaceholderMeta`).

use std::collections::HashMap;

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::html5::active_formatting_elements::AfeEntry;
use crate::html5::dispatcher::{Dispatcher, ModeId};
use crate::html5::element::Attributes;
use crate::html5::modes;
use crate::html5::node_handler::NodeTreeHandler;
use crate::html5::tree_builder::TreeBuilder;
use crate::wikitext::tokens_v2::{DataParsoid as TDataParsoid, Item, KV, KeyValue, ParsoidToken};

/// The attribute name used to smuggle the stashed node-data id through the tree
/// builder (mirrors `DOMDataUtils::DATA_OBJECT_ATTR_NAME`).
const DATA_OBJECT_ATTR_NAME: &str = "data-object-id";

/// Placeholder `about` value on a fragment built before the tree exists.
///
/// A `<templatestyles>` fragment is built during expansion, but its `about` id
/// belongs to document order, which is only known once the tree is built. The
/// fragment therefore carries this marker and
/// [`Html5TreeBuilder::resolve_deferred_about_ids`] replaces it at splice time.
/// The value is deliberately not a valid `#mwtN`, so a marker that ever escaped
/// resolution would be visible in a diff rather than silently plausible.
pub const DEFERRED_ABOUT: &str = "#mwt-deferred";

/// A stashed `NodeData` (mirrors PHP's `NodeData`).
#[derive(Debug, Default)]
struct StashedNodeData {
    data_parsoid: Option<String>,
    /// Structured token-level `data-parsoid`, preserved for `ComputeDSR`.
    dp: Option<TDataParsoid>,
    data_mw: Option<String>,
    /// A stashed sub-fragment for `mw:DOMFragment` placeholders.
    fragment: Option<Node>,
}

/// The faithful tree-builder adapter.
pub struct Html5TreeBuilder {
    builder: TreeBuilder<NodeTreeHandler>,
    dispatcher: Dispatcher,
    /// Assigned to start/self-closing tags (mirrors `$tagId`).
    tag_id: usize,
    /// Whether we are inside a transclusion (mirrors `$inTransclusion`).
    in_transclusion: bool,
    /// Crude table-depth tracking (mirrors `$tableDepth`).
    table_depth: usize,
    /// Buffered string/Nl tokens (mirrors `$textContentBuffer`).
    text_buffer: String,
    /// The page source, for `tsr`-based source recovery (mirrors
    /// `$this->frame->getSource()`).
    source: String,
    /// Stashed node data, keyed by `data-object-id`.
    stash: HashMap<usize, StashedNodeData>,
    next_data_id: usize,
    /// Tree-builder element `uid`s closed by an *explicit* end tag (mirror
    /// `TreeMutationRelay`'s matched-vs-auto-inserted end-tag distinction).
    /// Keyed by `uid` (not the shared `data-object-id`) so that reconstructed
    /// formatting-element clones — which copy the original's `data-object-id` —
    /// are tracked independently.
    explicitly_ended: std::collections::HashSet<usize>,
    /// Maps a tree-builder element `uid` to its stashed `data-object-id`, so an
    /// explicit end tag can mark the *correct* stash entry as explicitly ended
    /// even though `modes::end_tag` has already popped the element.
    uid_to_data_id: HashMap<usize, usize>,
    /// Pre-built sub-fragments keyed by id (carried by `mw:dom-fragment-token`).
    fragments: HashMap<usize, Node>,
    /// Allocates the next transclusion `about` id (`#mwtN`), shared with the
    /// expansion that produced the tokens.
    ///
    /// An extension fragment's `about` id cannot be allocated when the fragment
    /// is *built* — that pass runs over the whole token stream before the tree
    /// exists, so its ids come out in stream order rather than document order.
    /// The live service allocates at the point the expansion reaches the
    /// element, which is why a bare `{{#invoke:Infobox|infobox}}` serves
    /// `#mwt1` for the wrapper and `#mwt2` for the `<style>` inside it. The
    /// splice below is that point, so the id is taken there.
    about_counter: Option<std::rc::Rc<std::cell::Cell<usize>>>,
}

impl Html5TreeBuilder {
    pub fn new() -> Self {
        Self::with_source("")
    }

    /// Construct with the page source available for `tsr` resolution.
    pub fn with_source(source: &str) -> Self {
        Self::with_source_and_fragments(source, HashMap::new())
    }

    /// Construct with the page source and a map of pre-built sub-fragments.
    pub fn with_source_and_fragments(source: &str, fragments: HashMap<usize, Node>) -> Self {
        let handler = NodeTreeHandler::new();
        let mut builder = TreeBuilder::new(handler);
        // Parsoid builds a fragment with a `<body>` context element (mirrors
        // RemexPipeline's `startDocument(..., 'body')`).
        builder.start_document(Some(crate::html5::html_data::NS_HTML), Some("body"));
        let mut dispatcher = Dispatcher::new();
        dispatcher.switch_mode(ModeId::Initial);
        dispatcher.reset(&builder);
        Html5TreeBuilder {
            builder,
            dispatcher,
            tag_id: 1,
            in_transclusion: false,
            table_depth: 0,
            text_buffer: String::new(),
            source: source.to_string(),
            stash: HashMap::new(),
            next_data_id: 0,
            explicitly_ended: std::collections::HashSet::new(),
            uid_to_data_id: HashMap::new(),
            fragments,
            about_counter: None,
        }
    }

    /// Provide the shared transclusion `about` counter, so an extension fragment
    /// spliced into the tree can take its id in document order.
    pub fn with_about_counter(mut self, counter: std::rc::Rc<std::cell::Cell<usize>>) -> Self {
        self.about_counter = Some(counter);
        self
    }

    /// Take the next `#mwtN`, or `None` when no counter was supplied (the
    /// standalone/fixture path, which has no transclusion wrappers to number).
    fn next_about_id(&self) -> Option<String> {
        let counter = self.about_counter.as_ref()?;
        let n = counter.get() + 1;
        counter.set(n);
        Some(format!("#mwt{n}"))
    }

    /// Replace the [`DEFERRED_ABOUT`] marker on any element in a freshly spliced
    /// fragment with a real `about` id, in document order.
    ///
    /// Walking the fragment (rather than only its root) matters because a
    /// stylesheet can be nested: `Module:Infobox` emits its `<style>` inside the
    /// transclusion it is itself part of, and the outer element may already have
    /// an id of its own.
    fn resolve_deferred_about_ids(&self, node: &mut Node) {
        if node.get_attr("about") == Some(DEFERRED_ABOUT) {
            match self.next_about_id() {
                Some(id) => node.set_attr("about", id),
                // No counter: leave no attribute rather than a marker string,
                // which would end up in the output.
                None => node.attrs.retain(|a| a.key != "about"),
            }
        }
        for child in &mut node.children {
            self.resolve_deferred_about_ids(child);
        }
    }

    fn stash(&mut self, dp: &TDataParsoid, data_mw: Option<String>) -> usize {
        let id = self.next_data_id;
        self.next_data_id += 1;
        self.stash.insert(
            id,
            StashedNodeData {
                data_parsoid: dp.to_data_parsoid_json(),
                dp: Some(dp.clone()),
                data_mw,
                fragment: None,
            },
        );
        id
    }

    /// Stash a pre-built sub-fragment node (and the placeholder token's `dp`, so
    /// its `tsr`/`extTagOffsets` survive into `ComputeDSR`) and return its id.
    fn stash_fragment(&mut self, fragment: Node, dp: &TDataParsoid) -> usize {
        let id = self.next_data_id;
        self.next_data_id += 1;
        self.stash.insert(
            id,
            StashedNodeData {
                data_parsoid: dp.to_data_parsoid_json(),
                dp: Some(dp.clone()),
                data_mw: None,
                fragment: Some(fragment),
            },
        );
        id
    }

    /// Stash data-parsoid/data-mw and return an attributes list carrying the
    /// `data-object-id` indirection (mirrors `stashDataAttribs`).
    ///
    /// Templated attributes recorded in `data-mw.attribs` (for `mw:ExpandedAttrs`
    /// elements) are dropped from the plain attribute list: they are reconstructed
    /// from `data-mw` at serialization time, so leaking them as plain DOM
    /// attributes (e.g. a templated `k=""` table attribute) is incorrect.
    fn stash_data_attribs(
        &mut self,
        attribs: &[KV],
        dp: &TDataParsoid,
        data_mw: Option<String>,
    ) -> (Attributes, usize) {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for kv in attribs {
            let Some(k) = kv.key.as_str() else { continue };
            if k == "data-parsoid"
                || k == "data-mw"
                || k == DATA_OBJECT_ATTR_NAME
                || k == "data-fragment-id"
            {
                continue;
            }
            // A token-valued attribute (a templated/extension value that never
            // collapsed to a plain string) is flattened here. With
            // `unpack_dom_fragments` this resolves an `mw:DOMFragment`
            // placeholder to its text content, mirroring PHP's
            // `tokensToString( …, [ 'unpackDOMFragments' => true ] )` — which is
            // how a `<nowiki>` cell attribute flattens to its literal text
            // (T280115).
            let v = match &kv.value {
                KeyValue::Str(s) => s.clone(),
                KeyValue::Tokens(items) => {
                    crate::wikitext::token_utils::tokens_to_string_with_opts(
                        items,
                        crate::wikitext::token_utils::TokensToStringOpts {
                            unpack_dom_fragments: true,
                            fragments: Some(&self.fragments),
                        },
                    )
                }
            };
            pairs.push((k.to_string(), v));
        }
        let id = self.stash(dp, data_mw);
        pairs.push((DATA_OBJECT_ATTR_NAME.to_string(), id.to_string()));
        (Attributes::from_pairs(pairs), id)
    }

    /// Extract `data-mw` string attribute from a token's attribs.
    fn extract_data_mw(attribs: &[KV]) -> Option<String> {
        attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("data-mw"))
            .and_then(|kv| kv.value.as_str())
            .map(str::to_string)
    }

    /// Insert a `<meta>` tag *unfostered* (mirrors `insertUnfosteredMeta`).
    ///
    /// Note it calls the **InHead** handler directly rather than dispatching on
    /// the current insertion mode. That is deliberate and load-bearing: a
    /// transclusion marker meta emitted while a table row/cell is open must stay
    /// where it is, whereas a normal `meta` start tag in a table context is
    /// foster-parented out of the table.
    fn insert_unfostered_meta(&mut self, attrs: Attributes) {
        self.dispatcher.flush_table_text(&mut self.builder);
        modes::in_head::start_tag(
            &mut self.builder,
            &mut self.dispatcher,
            "meta",
            attrs,
            true,
            0,
            0,
        );
    }

    /// Whether the current open element is a fosterable position (a text or
    /// placeholder node inserted now would be fostered out). Mirrors
    /// `RemexPipeline::isFosterablePosition`.
    fn is_fosterable_position(&self) -> bool {
        self.builder
            .stack
            .current()
            .map(|elt| crate::wikitext::consts::fosterable_position().contains(&elt.html_name))
            .unwrap_or(false)
    }

    /// Insert an `mw:Placeholder/StrippedTag` meta for a deleted start/end tag.
    /// Mirrors `TreeBuilderStage::insertPlaceholderMeta`.
    fn insert_placeholder_meta(&mut self, name: &str, dp: &TDataParsoid, is_start: bool) {
        // If the placeholder would be fostered out, skip it (browsers move it
        // out of the table anyway, so round-tripping wouldn't see it).
        if self.is_fosterable_position() {
            return;
        }

        // Parsoid drops unmatched closing `<pre>` tags (they're always a parse
        // error outside a `<pre>` block, so there's nothing meaningful to
        // round-trip). See the `<nowiki> and <pre> preference` fixture.
        if name == "pre" && !is_start {
            return;
        }

        let mut src = dp.src.clone();

        // PHP treats both an unset `src` and an empty/`'0'`-falsy `src` as
        // absent, so fall back to the TSR (or the literal tag name) accordingly.
        if src.as_deref().is_none_or(str::is_empty) {
            if let Some(tsr) = &dp.tsr {
                src = Some(tsr.substr(&self.source).to_string());
            } else if dp.stx.as_deref() == Some("html") {
                src = Some(if is_start {
                    format!("<{name}>")
                } else {
                    format!("</{name}>")
                });
            }
        }

        if let Some(src) = src
            && !src.is_empty()
        {
            let meta_dp = TDataParsoid {
                src: Some(src),
                name: Some(name.to_string()),
                ..TDataParsoid::default()
            };
            let (attrs, _) = self.stash_data_attribs(
                &[KV {
                    key: crate::wikitext::tokens_v2::KeyValue::Str("typeof".to_string()),
                    value: crate::wikitext::tokens_v2::KeyValue::Str(
                        "mw:Placeholder/StrippedTag".to_string(),
                    ),
                    src_offsets: None,
                    ksrc: None,
                    vsrc: None,
                }],
                &meta_dp,
                None,
            );
            self.insert_unfostered_meta(attrs);
        }
    }

    /// Process a chunk of tokens.
    pub fn process_chunk(&mut self, tokens: &[Item]) {
        let n = tokens.len();
        let mut i = 0;
        while i < n {
            let token = &tokens[i];
            // If there are exactly two newlines directly after a `</p>`, and we
            // have active formatting elements, process one of the newlines
            // *inside* the paragraph (before the `</p>`) rather than after
            // (T368720). Mirrors `TreeBuilderStage::processChunk`.
            let mut nl_index = i + 1;
            if matches!(token, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "p")
                && self.has_afe()
            {
                while nl_index < n && matches!(tokens[nl_index], Item::Tok(ParsoidToken::Nl(_))) {
                    nl_index += 1;
                }
            }
            if nl_index == i + 3 {
                self.process_token(&tokens[i + 1]);
                self.process_token(&tokens[i + 2]);
                self.process_token(token);
                i += 3;
            } else {
                self.process_token(token);
                i += 1;
            }
        }
    }

    /// Whether there is an active formatting element (an `Element` entry in the
    /// AFE list; scope markers and bookmarks do not count). Mirrors
    /// `TreeBuilderStage::hasAfe` (which walks from the tail, skipping `Marker`).
    fn has_afe(&self) -> bool {
        let mut node = self.builder.afe.tail_node();
        while let Some(ci) = node {
            if let AfeEntry::Element(_) = self.builder.afe.entry(ci) {
                return true;
            }
            node = self.builder.afe.prev_node(ci);
        }
        false
    }

    /// Feed a single token.
    pub fn process_token(&mut self, token: &Item) {
        let is_string =
            matches!(token, Item::Str(_)) || matches!(token, Item::Tok(ParsoidToken::Nl(_)));

        if !is_string && !self.text_buffer.is_empty() {
            self.flush_text();
        }

        match token {
            Item::Str(s) => self.text_buffer.push_str(s),
            Item::Tok(tok) => match tok {
                ParsoidToken::Nl(_) => self.text_buffer.push('\n'),
                ParsoidToken::Tag(t) => {
                    self.tag_id += 1;
                    let name = t.name.clone();
                    if name == "table" {
                        self.table_depth += 1;
                    }
                    self.process_start_tag(&name, &t.attribs, &t.data_parsoid);
                }
                ParsoidToken::SelfclosingTag(t) => {
                    self.tag_id += 1;
                    self.process_selfclosing(&t.name, &t.attribs, &t.data_parsoid);
                }
                ParsoidToken::EndTag(t) => {
                    let name = t.name.clone();
                    if name == "table" && self.table_depth > 0 {
                        self.table_depth -= 1;
                    }
                    let ended =
                        modes::end_tag(&mut self.builder, &mut self.dispatcher, &name, 0, 0);
                    if let Some(uid) = ended {
                        // The end tag matched an element: copy its source data
                        // onto that element's node data, exactly as PHP's
                        // `TreeBuilderStage::processToken` EndTag branch does
                        // (`endTSR`, `stx`, `endTagSrc`, autoInserted promotion).
                        self.apply_end_tag_data(uid, &t.data_parsoid);
                    } else {
                        // The tag was stripped; insert an mw:Placeholder for
                        // round-tripping (mirrors `insertPlaceholderMeta`).
                        self.insert_placeholder_meta(&name, &t.data_parsoid, false);
                    }
                }
                ParsoidToken::Comment(c) => {
                    self.builder.comment(None, &c.value, 0, 0);
                }
                ParsoidToken::Eof(_) => {
                    modes::end_document(&mut self.builder, &mut self.dispatcher, 0);
                }
                ParsoidToken::EmptyLine(t) => {
                    for tok in &t.tokens {
                        self.process_token(&Item::Tok(tok.clone()));
                    }
                }
                ParsoidToken::IndentPre(t) => {
                    self.process_chunk(&t.nested_tokens);
                }
                ParsoidToken::List(t) => {
                    self.process_chunk(&t.nested_tokens);
                }
            },
        }
    }

    fn flush_text(&mut self) {
        let text = std::mem::take(&mut self.text_buffer);
        if text.is_empty() {
            return;
        }
        modes::characters(
            &mut self.builder,
            &mut self.dispatcher,
            &text,
            0,
            text.len(),
            0,
            0,
        );
        if self.in_transclusion && self.table_depth > 0 {
            let nonspace = !text
                .chars()
                .all(|c| matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' '));
            if nonspace {
                self.insert_unfostered_meta(Attributes::from_pairs(vec![(
                    "typeof".to_string(),
                    "mw:TransclusionShadow".to_string(),
                )]));
            }
        }
    }

    fn process_start_tag(&mut self, name: &str, attribs: &[KV], dp: &TDataParsoid) {
        // `data-mw` is node data, not a real attribute (PHP stores it on the
        // token's `DataMw` object). rustoid models it as a string attribute, so
        // take it out before sanitizing: `Sanitizer::sanitizeTagAttrs` would
        // drop it as a reserved `data-` attribute.
        let data_mw = Self::extract_data_mw(attribs);
        let attribs: Vec<KV> = attribs
            .iter()
            .filter(|kv| kv.key.as_str() != Some("data-mw"))
            .cloned()
            .collect();

        // Wikitext-syntax table cells run their attributes through the sanitizer
        // allowlist here, discarding disallowed attributes (e.g. a valueless
        // `|foo|` marker) so they don't leak into the DOM as `foo=""` — PHP
        // applies `SanitizerHandler`/`sanitizeTagAttrs` to every `td`/`th` before
        // tree building. HTML-syntax tags are sanitized by the SanitizerHandler
        // stage; media/link attributes are resolved before this point and must be
        // left intact, so limit this pass to table cells only.
        let attribs = if matches!(name, "table" | "tr" | "td" | "th" | "caption")
            && dp.stx.as_deref() != Some("html")
        {
            crate::sanitizer::sanitize_tag_attrs_with_fragments(
                name,
                attribs,
                |_p| true,
                &self.fragments,
            )
        } else {
            attribs
        };
        let (attrs, data_id) = self.stash_data_attribs(&attribs, dp, data_mw);

        // A start tag carrying a `data-fragment-id` tunnels a pre-built DOM
        // fragment (mirrors `tunnelDOMThroughTokens`, which stores the fragment
        // on the wrapper token). Stash it into the same `data-object-id` entry so
        // `resolve_data_ids` attaches it to the element for `UnpackDOMFragments`.
        let fragment_id = attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("data-fragment-id"))
            .and_then(|kv| kv.value.as_str())
            .and_then(|s| s.parse::<usize>().ok());
        if let Some(fragment_id) = fragment_id
            && let Some(mut fragment) = self.fragments.remove(&fragment_id)
        {
            // This is where the wrapper tag sits in document order, so an
            // extension fragment that deferred its `about` id takes it now.
            self.resolve_deferred_about_ids(&mut fragment);
            if let Some(stashed) = self.stash.get_mut(&data_id) {
                stashed.fragment = Some(fragment);
            }
        }

        // Mirrors `insertExplicitStartTag`: if the tag produced no element
        // (stripped/ignored), handle it as a deleted start tag.
        let inserted = modes::start_tag(
            &mut self.builder,
            &mut self.dispatcher,
            name,
            attrs,
            false,
            0,
            0,
        );
        if let Some(uid) = inserted {
            // Record the element identity → stash id mapping while the element
            // is still on the stack (needed later by the EndTag branch, which
            // only receives the `uid` after `modes::end_tag` has popped it).
            self.uid_to_data_id.insert(uid, data_id);
        } else {
            self.handle_deleted_start_tag(name, dp);
        }
    }

    /// Insert `td/tr/th` tag source or a placeholder meta (mirrors
    /// `TreeBuilderStage::handleDeletedStartTag`).
    fn handle_deleted_start_tag(&mut self, name: &str, dp: &TDataParsoid) {
        if dp.stx.as_deref() != Some("html") && matches!(name, "td" | "tr" | "th") {
            // A stripped wikitext-syntax table tag outside of a table. Re-insert
            // the original page source. PHP guards the `substr` on a non-empty
            // tsr with non-null offsets (`!empty( $dp->tsr ) && $dp->tsr->start
            // !== null && $dp->tsr->end !== null`); otherwise it falls back to
            // the literal tag character.
            let orig_txt = match &dp.tsr {
                Some(tsr) if tsr.start.is_some() => tsr.substr(&self.source).to_string(),
                _ => match name {
                    "td" => "|",
                    "tr" => "|-",
                    "th" => "!",
                    _ => "",
                }
                .to_string(),
            };
            if !orig_txt.is_empty() {
                self.emit_characters(&orig_txt);
            }
        } else {
            self.insert_placeholder_meta(name, dp, true);
        }
    }

    /// Flush a run of literal characters through the tree builder (mirrors the
    /// `$this->remexPipeline->dispatcher->characters( ... )` call).
    fn emit_characters(&mut self, text: &str) {
        modes::characters(
            &mut self.builder,
            &mut self.dispatcher,
            text,
            0,
            text.len(),
            0,
            0,
        );
    }

    /// Copy source data from a matched end tag onto its element's stashed node
    /// data. Faithful to the `EndTagTk` branch of PHP's
    /// `TreeBuilderStage::processToken`:
    ///   - `endTSR` ← the end tag's `tsr` (for `ComputeDSR`).
    ///   - `stx` ← transferred when present.
    ///   - `endTagSrc` ← when present and not a literal-HTML element.
    ///   - promote `autoInsertedStartToken`/`autoInsertedEndToken` to their
    ///     persistent `autoInsertedStart`/`autoInsertedEnd` forms.
    fn apply_end_tag_data(&mut self, uid: usize, dp: &TDataParsoid) {
        // Look up the element's stashed `data-object-id`. Explicit start tags are
        // recorded in `uid_to_data_id` at start-tag time; AFE-reconstructed
        // clones (which copy the original's attributes, including
        // `data-object-id`) are resolved from the tree handler instead.
        let data_id = self.uid_to_data_id.get(&uid).copied().or_else(|| {
            self.builder
                .handler
                .data_object_id(uid)
                .and_then(|v| v.parse::<usize>().ok())
        });

        let Some(data_id) = data_id else {
            return;
        };
        // Record that this *element* was ended by an explicit end tag (so it
        // does NOT get `autoInsertedEnd`). Keyed by `uid` so reconstructed
        // formatting-element clones are tracked independently of the shared
        // `data-object-id`.
        self.explicitly_ended.insert(uid);
        let Some(stashed) = self.stash.get_mut(&data_id) else {
            return;
        };
        let Some(node_dp) = stashed.dp.as_mut() else {
            return;
        };

        if !matches!(node_dp.stx.as_deref(), Some("html"))
            && let Some(end_tag_src) = &dp.end_tag_src
        {
            node_dp.end_tag_src = Some(end_tag_src.clone());
        }
        if let Some(stx) = &dp.stx {
            node_dp.stx = Some(stx.clone());
        }
        if let Some(tsr) = &dp.tsr {
            node_dp.tmp.end_tsr = Some(tsr.clone());
        }
    }

    fn process_selfclosing(&mut self, name: &str, attribs: &[KV], dp: &TDataParsoid) {
        let data_mw = Self::extract_data_mw(attribs);
        let mut was_inserted = false;

        if name == "mw:dom-fragment-token" {
            // Look up the pre-built sub-fragment, stash it, and emit an unfostered
            // `<span typeof="mw:DOMFragment">` placeholder carrying the id that
            // resolves back to it (unpacked by `UnpackDOMFragments` in finalize).
            let fragment_id = attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("data-fragment-id"))
                .and_then(|kv| kv.value.as_str())
                .and_then(|s| s.parse::<usize>().ok());
            let fragment = fragment_id.and_then(|id| self.fragments.remove(&id));
            let mut fragment = fragment.unwrap_or_else(Node::document);
            self.resolve_deferred_about_ids(&mut fragment);
            let id = self.stash_fragment(fragment, dp);
            let attrs = Attributes::from_pairs(vec![
                ("typeof".to_string(), "mw:DOMFragment".to_string()),
                (DATA_OBJECT_ATTR_NAME.to_string(), id.to_string()),
            ]);
            self.insert_unfostered_meta(attrs);
            return;
        }

        if name == "meta" {
            let should_not_foster = match_type_of(attribs).is_some();
            if should_not_foster {
                if let Some(ty) = match_transclusion(attribs) {
                    self.in_transclusion = ty == "mw:Transclusion";
                }
                let (attrs, _) = self.stash_data_attribs(attribs, dp, data_mw.clone());
                self.insert_unfostered_meta(attrs);
                was_inserted = true;
            }
        }

        if !was_inserted {
            let (attrs, _) = self.stash_data_attribs(attribs, dp, data_mw);
            let void = crate::html5::html_data::is_void_tag(name);
            let inserted = modes::start_tag(
                &mut self.builder,
                &mut self.dispatcher,
                name,
                attrs,
                void,
                0,
                0,
            );
            if inserted.is_some() {
                if !void {
                    modes::end_tag(&mut self.builder, &mut self.dispatcher, name, 0, 0);
                }
            } else {
                // The self-closing tag was stripped; insert a placeholder so it
                // round-trips (mirrors `insertPlaceholderMeta`).
                self.insert_placeholder_meta(name, dp, true);
            }
        }
    }

    /// Finalize into a `Node` document, resolving `data-object-id` into the
    /// stashed `data-parsoid`/`data-mw`.
    ///
    /// This performs only tree-building finalization. DOM-level passes that must
    /// respect ordering with p-wrapping (`encapsulate_transclusions`,
    /// `migrate_br_newlines`, `strip_marker_metas`) happen later via
    /// [`post_pwrap_transforms`], so p-wrapping runs before encapsulation
    /// (mirrors PHP's `NESTED_PIPELINE_DOM_TRANSFORMS` order).
    pub fn finalize(mut self) -> Node {
        // Mark `autoInsertedEnd` on each markable element that was closed
        // *implicitly* (its `uid` is not in `explicitly_ended`). Mirrors
        // `TreeMutationRelay::endTag`. Done per-element (rather than per
        // `data-object-id`) so reconstructed formatting-element clones each
        // get the correct flag.
        let explicitly_ended = std::mem::take(&mut self.explicitly_ended);
        self.builder
            .handler
            .mark_implicit_auto_inserted_end(&explicitly_ended);

        let mut doc = self.builder.handler.finish();
        resolve_data_ids(&mut doc, &self.stash, &mut std::collections::HashSet::new());
        // Promote transient autoInsertedStart/EndToken flags to their persistent
        // final form (mirrors `TreeBuilderStage::processToken` end-tag branch).
        promote_auto_inserted_flags(&mut doc);
        // Remove empty auto-inserted elements (mirrors
        // `ProcessTreeBuilderFixups::removeAutoInsertedEmptyTags`, which runs
        // after tree building and before DOM-level p-wrapping).
        remove_auto_inserted_empty_tags(&mut doc);
        // MarkFosteredContent: strip `mw:TransclusionShadow` bookkeeping metas
        // and (once foster-box emission is ported) mark fostered content. Runs
        // before `ComputeDSR` so the `fostered` flag yields zero-width ranges.
        crate::pipeline::mark_fostered_content::run(&mut doc);
        doc
    }
}

impl Default for Html5TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `typeof` matches `mw:Transclusion|mw:Param` (see `matchTypeOf`).
fn match_type_of(attribs: &[KV]) -> Option<String> {
    let v = attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("typeof"))
        .and_then(|kv| kv.value.as_str())?;
    for ty in v.split_whitespace() {
        if ty == "mw:Transclusion"
            || ty == "mw:Param"
            || ty.starts_with("mw:Transclusion/")
            || ty.starts_with("mw:Param/")
        {
            return Some(ty.to_string());
        }
    }
    None
}

/// Whether `typeof` starts with `mw:Transclusion` (see `matchTypeOf`).
fn match_transclusion(attribs: &[KV]) -> Option<String> {
    let v = attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("typeof"))
        .and_then(|kv| kv.value.as_str())?;
    v.split_whitespace()
        .find(|ty| ty.starts_with("mw:Transclusion"))
        .map(String::from)
}

/// Prepend a leading-wikitext string (a `recordTemplateInfo` `unwrappedWT`) to
/// the `data-mw.parts` array, producing a multi-template-content-block.
/// Mirrors PHP's `DOMRangeBuilder::recordTemplateInfo`, which prepends the
/// recovered `unwrappedWT` so that the leading source wikitext survives as a
/// literal `data-mw.parts` entry before the template object.
fn prepend_unwrapped_wt_part(data_mw: &str, unwrapped_wt: &str) -> String {
    let mut json: serde_json::Value = serde_json::from_str(data_mw)
        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(parts) = json.get_mut("parts").and_then(|p| p.as_array_mut())
        && !parts
            .first()
            .is_some_and(|f| f.as_str() == Some(unwrapped_wt))
    {
        parts.insert(0, serde_json::Value::String(unwrapped_wt.to_string()));
    }
    json.to_string()
}

/// Append a trailing-wikitext string to the `data-mw.parts` array, mirroring the
/// `encapsulateTemplates` trailing-wikitext step (the gap between the last
/// template's `dsr.end` and the range's `dsr.end`, recovered from `source`).
fn append_trailing_wt_part(data_mw: &str, trailing: &str) -> String {
    let mut json: serde_json::Value = serde_json::from_str(data_mw)
        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(parts) = json.get_mut("parts").and_then(|p| p.as_array_mut())
        && !parts.last().is_some_and(|f| f.as_str() == Some(trailing))
    {
        parts.push(serde_json::Value::String(trailing.to_string()));
    }
    json.to_string()
}

/// Build the compound `data-mw.parts` for an encapsulation target from its
/// transclusion start-meta: any leading `unwrappedWT`, the existing template
/// object(s), and any trailing wikitext recovered from the range's DSR tail.
/// Faithful to PHP `DOMRangeBuilder::recordTemplateInfo` +
/// `encapsulateTemplates` (leading/trailing-wikitext steps).
fn build_compound_data_mw(
    start_meta: &Node,
    source: Option<&str>,
    range_end: Option<usize>,
) -> Option<String> {
    let data_mw = start_meta.data_mw.clone()?;
    let dp = start_meta.dp.as_ref();

    let data_mw = match dp.and_then(|d| d.tmp.unwrapped_wt.as_deref()) {
        Some(wt) if !wt.is_empty() => prepend_unwrapped_wt_part(&data_mw, wt),
        _ => data_mw,
    };

    let tpl_end = dp.and_then(|d| d.dsr.as_ref().and_then(|r| r.end));
    let data_mw = match (tpl_end, range_end) {
        (Some(tpl_end), Some(range_end)) if range_end > tpl_end => {
            match source
                .and_then(|s| s.get(tpl_end..range_end))
                .filter(|t| !t.is_empty())
            {
                Some(t) => append_trailing_wt_part(&data_mw, t),
                None => data_mw,
            }
        }
        _ => data_mw,
    };

    Some(data_mw)
}

/// Nested transclusion ranges found inside a templated node's subtree, in
/// document order, as `(data-mw parts, dsr)` pairs.
///
/// A templated table's range typically *encloses* further transclusion ranges
/// (one per template-generated cell). PHP's `DOMRangeBuilder` collects them via
/// `recordTemplateInfo` into `compoundTpls` so the outermost node reports a
/// compound transclusion; rustoid has no range-merging pass, so the nested
/// ranges are recovered here from their `about`-stamped encapsulation targets.
type NestedTplParts = Vec<(
    Vec<serde_json::Value>,
    Option<crate::wikitext::tokens_v2::DomSourceRange>,
)>;

fn collect_nested_transclusion_parts(
    node: &Node,
    start_meta: &Node,
    _source: Option<&str>,
) -> NestedTplParts {
    let outer_about = start_meta.get_attr("about").map(str::to_string);
    let mut found: NestedTplParts = Vec::new();

    fn walk(node: &Node, outer_about: Option<&str>, found: &mut NestedTplParts) {
        for child in &node.children {
            let about = child.get_attr("about").map(str::to_string);
            let is_nested_tpl = !matches!(child.kind, NodeKind::Text(_))
                && child
                    .get_attr("typeof")
                    .is_some_and(|t| t.split_whitespace().any(|x| x == "mw:Transclusion"))
                && about.as_deref().is_some()
                && about.as_deref() != outer_about;

            if is_nested_tpl {
                let parts = child
                    .data_mw
                    .as_deref()
                    .and_then(|dm| serde_json::from_str::<serde_json::Value>(dm).ok())
                    .and_then(|v| v.get("parts").and_then(|p| p.as_array()).cloned())
                    .unwrap_or_default();
                if !parts.is_empty() {
                    let dsr = child.dp.as_ref().and_then(|d| d.dsr.clone());
                    found.push((parts, dsr));
                }
            }
            walk(child, outer_about, found);
        }
    }

    walk(node, outer_about.as_deref(), &mut found);
    found
}

/// Build the compound `data-mw` for a node whose range encloses `nested`
/// transclusion ranges, mirroring `DOMRangeBuilder::recordTemplateInfo`: each
/// template's parts are appended, with the intervening source wikitext between
/// one template's `dsr.end` and the next template's `dsr.start` inserted as a
/// string part (PHP's `$width = $dsr->start - $prevTplInfo->dsr->end`).
fn build_compound_data_mw_with_nested(
    start_meta: &Node,
    source: Option<&str>,
    nested: &NestedTplParts,
) -> Option<String> {
    let base = build_compound_data_mw(start_meta, source, None)?;
    let mut root: serde_json::Value = serde_json::from_str(&base).ok()?;
    let parts = root.get_mut("parts")?.as_array_mut()?;

    // The outermost template's end offset seeds the gap computation.
    let mut prev_end = start_meta
        .dp
        .as_ref()
        .and_then(|d| d.dsr.as_ref())
        .and_then(|r| r.end);

    for (nested_parts, dsr) in nested {
        let Some(dsr) = dsr.as_ref() else { continue };
        if let (Some(prev), Some(start)) = (prev_end, dsr.start)
            && prev < start
            && let Some(src) = source
            && let Some(gap) = src.get(prev..start)
            && !gap.is_empty()
        {
            parts.push(serde_json::Value::String(gap.to_string()));
        }
        for p in nested_parts {
            parts.push(p.clone());
        }
        prev_end = dsr.end;
    }

    Some(root.to_string())
}

/// Dissolve nested transclusion ranges whose `data-mw` was absorbed into an
/// enclosing compound transclusion, by unwrapping each absorbed span into its
/// children.
///
/// PHP's `DOMRangeBuilder` never leaves a separate element for such a range: its
/// post-tplwrap cells hold a bare text node (`align=center style="color:red;"|Foo`)
/// with no transclusion span, and the cell's DSR is clamped to the template's end.
/// rustoid encapsulates each inner range eagerly, before the enclosing range is
/// known, so the equivalent is to splice the absorbed span's children in its place.
///
/// The `about`/`typeof`/`data-mw` are dropped with the span: they now belong to
/// the enclosing compound transclusion, which is what stops `TableFixups`'
/// `hoistTransclusionInfo` from lifting a second id onto the cell.
/// Dissolve nested transclusion ranges whose `data-mw` was absorbed into an
/// enclosing compound transclusion, by unwrapping each absorbed span into its
/// children.
///
/// PHP's `DOMRangeBuilder` never leaves a separate element for such a range: its
/// post-tplwrap cells hold a bare text node (`align=center style="color:red;"|Foo`)
/// with no transclusion span, and the cell's DSR is clamped to the template's end.
/// rustoid encapsulates each inner range eagerly, before the enclosing range is
/// known, so the equivalent is to splice the absorbed range's children in place.
///
/// The `about`/`typeof`/`data-mw` go with the wrapper: they now belong to the
/// enclosing compound transclusion, which is what stops `TableFixups`'
/// `hoistTransclusionInfo` from lifting a second id onto the cell.
fn dissolve_absorbed_ranges(node: &mut Node, outer_about: Option<&str>) {
    let mut i = 0usize;
    while i < node.children.len() {
        let about = node.children[i].get_attr("about").map(str::to_string);
        let is_absorbed = about.as_deref().is_some()
            && about.as_deref() != outer_about
            && node.children[i]
                .get_attr("typeof")
                .is_some_and(|t| t.split_whitespace().any(|x| x == "mw:Transclusion"));

        if is_absorbed {
            let mut head = node.children.remove(i);
            let children = std::mem::take(&mut head.children);
            let head_is_span = matches!(&head.kind, NodeKind::Element(ElementKind::Span));
            let spliced = children.len();
            for (offset, child) in children.into_iter().enumerate() {
                node.children.insert(i + offset, child);
            }

            // The rest of the range follows as siblings carrying the same `about`.
            // Mirrors `migrateElements`: a `span` is dropped outright (it only kept
            // the range contiguous — PHP's "drop the newline span"), while any other
            // element keeps its content with `about` stripped.
            let mut j = i + spliced;
            while j < node.children.len() {
                let sibling_about = node.children[j].get_attr("about").map(str::to_string);
                if sibling_about.as_deref() != about.as_deref() {
                    break;
                }
                if matches!(&node.children[j].kind, NodeKind::Element(ElementKind::Span)) {
                    let mut wrapper = node.children.remove(j);
                    let inner = std::mem::take(&mut wrapper.children);
                    let inner_len = inner.len();
                    for (offset, child) in inner.into_iter().enumerate() {
                        node.children.insert(j + offset, child);
                    }
                    j += inner_len;
                } else {
                    node.children[j].attrs.retain(|a| a.key != "about");
                    j += 1;
                }
            }

            // A non-span head survives as a visited sibling and still needs its own
            // children examined; a span was fully unwrapped, so the spliced-in
            // children are re-examined at the same index instead.
            if !head_is_span && spliced > 0 {
                dissolve_absorbed_ranges(&mut node.children[i], outer_about);
                i += 1;
            }
            continue;
        }
        dissolve_absorbed_ranges(&mut node.children[i], outer_about);
        i += 1;
    }
}

/// Merge a transclusion's `data-mw` (its `parts` envelope) with a target's own
/// `data-mw` (e.g. a media container's `attribs`/`errors`). The transclusion
/// object is the base; the target's non-`parts` keys (and any `attribs`) are
/// carried over so the compound object retains both the transclusion metadata
/// and the media options. Mirrors PHP's `DOMRangeBuilder` encapsulation, which
/// attaches the transclusion `parts` onto the target without discarding the
/// target's existing `data-mw` fields.
fn merge_encap_data_mw(encap: Option<String>, target: Option<String>) -> Option<String> {
    let encap = encap?;
    let Some(target) = target else {
        return Some(encap);
    };
    // Both are `{"parts": … , …}` envelopes, so the merge is "keep the
    // transclusion's `parts`, then splice in the target's other keys". Done
    // textually because `serde_json` sorts an object's keys, and `data-mw` is
    // compared byte-for-byte: Parsoid writes `target` before `params` before
    // `i`, and a round trip through a `serde_json::Value` would reorder every
    // part on every encapsulated element.
    let extra = strip_data_mw_key(&target, "parts");
    if extra.is_empty() {
        return Some(encap);
    }
    let extra = extra.trim_matches(|c: char| c == '{' || c == '}' || c == ',' || c.is_whitespace());
    if extra.is_empty() {
        return Some(encap);
    }
    Some(splice_into_object(&encap, extra))
}

/// The value of `key` in a flat JSON object, or `None`. Used to pull `parts`
/// out of a `data-mw` envelope without reparsing and losing key order.
fn data_mw_key_span(json: &str, key: &str) -> Option<(usize, usize)> {
    let needle = format!("\"{key}\":");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start();
    let lead = json[start..].len() - rest.len();
    let start = start + lead;
    let bytes = json.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in json[start..].char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                if depth == 0 {
                    return Some((start, start + i));
                }
                depth -= 1;
            }
            ',' if depth == 0 => return Some((start, start + i)),
            _ => {}
        }
        // A bare scalar ends at the comma or the closing brace.
        if depth == 0 && bytes.get(start + i).is_some_and(|b| *b == b',') {
            return Some((start, start + i));
        }
    }
    Some((start, json.len()))
}

/// `json` with the value of `key` removed, leaving the other entries intact.
fn strip_data_mw_key(json: &str, key: &str) -> String {
    let Some((start, end)) = data_mw_key_span(json, key) else {
        return json.to_string();
    };
    // Also drop the separator that preceded the removed entry.
    let mut cut_from = json[..start].rfind(':').map(|c| c + 1).unwrap_or(0);
    if let Some(prev) = json[..cut_from].rfind(',') {
        cut_from = prev;
    }
    format!("{}{}", &json[..cut_from], &json[end..])
}

/// Insert `extra` (already a `"key":value` fragment) into a `{…}` object.
fn splice_into_object(json: &str, extra: &str) -> String {
    match json.rfind('}') {
        Some(close) => {
            let head = json[..close].trim_end().trim_end_matches(',');
            // An empty object needs no separator.
            let sep = if head.trim_end().ends_with('{') {
                ""
            } else {
                ","
            };
            format!("{head}{sep}{extra}{}", &json[close..])
        }
        None => json.to_string(),
    }
}

/// Walk the AST, resolving `data-object-id` attributes into stashed
/// `data-parsoid`/`data-mw`.
///
/// `seen` tracks stash ids already resolved earlier in the walk. A stash id
/// resolved by a *second* element is an AFE-reconstruction clone (a formatting
/// element reconstructed by the HTML5 "active formatting elements" algorithm),
/// which must not carry the original `src`/`tsr`: it is marked `autoInsertedStart`
/// (keeping `stx`, mirroring PHP's `TreeMutationRelay::insertElement`, which sets
/// `autoInsertedStart` on elements not matching an explicit start tag).
fn resolve_data_ids(
    node: &mut Node,
    stash: &HashMap<usize, StashedNodeData>,
    seen: &mut std::collections::HashSet<usize>,
) {
    let mut data_id: Option<usize> = None;
    for attr in &node.attrs {
        if attr.key == DATA_OBJECT_ATTR_NAME {
            data_id = attr.value.parse().ok();
        }
    }
    node.attrs.retain(|a| a.key != DATA_OBJECT_ATTR_NAME);
    // Preserve `autoInsertedEnd`/`autoInsertedStart` computed per-element by the
    // handler (see `mark_implicit_auto_inserted_end`) before we overwrite `dp`
    // with the stashed data.
    let pre_auto_inserted_end = node
        .dp
        .as_ref()
        .map(|d| d.auto_inserted_end)
        .unwrap_or(false);
    let pre_auto_inserted_start = node
        .dp
        .as_ref()
        .map(|d| d.auto_inserted_start)
        .unwrap_or(false);
    if let Some(id) = data_id
        && let Some(data) = stash.get(&id)
    {
        if seen.insert(id) {
            // First resolution: the explicit element gets the full stashed data.
            node.data_parsoid = data.data_parsoid.clone();
            node.dp = data.dp.clone();
            node.data_mw = data.data_mw.clone();
            if let Some(fragment) = &data.fragment {
                node.fragment = Some(Box::new(fragment.clone()));
            }
        } else {
            // AFE-reconstruction clone: keep `stx`, drop positional source info
            // (`src`/`tsr`), and mark auto-inserted start.
            let mut dp = data.dp.clone().unwrap_or_default();
            dp.tsr = None;
            dp.src = None;
            dp.auto_inserted_start = true;
            node.data_parsoid = dp.to_data_parsoid_json();
            node.dp = Some(dp);
            node.data_mw = data.data_mw.clone();
        }
    }
    // Re-apply the per-element auto-inserted flags computed by the handler.
    if let Some(dp) = node.dp.as_mut() {
        if pre_auto_inserted_end {
            dp.auto_inserted_end = true;
        }
        if pre_auto_inserted_start {
            dp.auto_inserted_start = true;
        }
    }
    // Keep the `data-parsoid` JSON in sync with the re-applied flags (consumers
    // such as `remove_auto_inserted_empty_tags` read the JSON string).
    if pre_auto_inserted_end || pre_auto_inserted_start {
        sync_auto_inserted_flags(node);
    }
    for child in &mut node.children {
        resolve_data_ids(child, stash, seen);
    }
}

/// Migrate newlines that immediately precede a blank-line `<br>` (LineBreak)
/// to *after* the `<br>`, matching Parsoid's rendered output.
///
/// The ParagraphWrapper emits `Nl <br> Nl` for a run of blank lines, but
/// Parsoid renders the blank-line `<br>` such that the newline for the blank
/// line is absorbed by the `<br>` itself; only a single newline follows it.
/// A whitespace-only text node ending in `\n` that directly precedes a
/// `LineBreak` has that trailing newline dropped (the text node is removed
/// when it becomes empty).
fn migrate_br_newlines(node: &mut Node) {
    for child in &mut node.children {
        if matches!(child.kind, NodeKind::Element(_)) {
            migrate_br_newlines(child);
        }
    }

    let mut out: Vec<Node> = Vec::with_capacity(node.children.len());
    let children = std::mem::take(&mut node.children);
    let n = children.len();
    let mut i = 0;
    while i < n {
        let mut cur = children[i].clone();
        // A text node ending in a newline immediately before a `<br>` drops
        // that trailing newline (the blank-line `<br>` absorbs it).
        if let NodeKind::Text(text) = &cur.kind
            && text.ends_with('\n')
            && i + 1 < n
            && matches!(
                children[i + 1].kind,
                NodeKind::Element(ElementKind::LineBreak)
            )
        {
            let trimmed = text.trim_end_matches('\n').to_string();
            if trimmed.is_empty() {
                // Drop the now-empty text node entirely, but keep the `<br>`.
                i += 1;
                cur = children[i].clone();
            } else {
                cur.kind = NodeKind::Text(trimmed);
            }
        }
        out.push(cur);
        i += 1;
    }
    node.children = out;
}

/// Whether a `Node` is a list item (`li`/`dd`/`dt`), used by the
/// `isNestedInListItem` ancestry check.
fn is_list_item_node(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::Element(
            ElementKind::ListItem
                | ElementKind::DefinitionTerm
                | ElementKind::DefinitionDescription
        )
    )
}

/// Whether `node`'s `data-mw` is absent or an empty JSON object/array, faithful
/// to `DOMDataUtils::getDataMw($node)->isEmpty()`.
fn data_mw_is_empty(node: &Node) -> bool {
    node.data_mw.as_deref().is_none_or(|json| {
        serde_json::from_str::<serde_json::Value>(json)
            .map(|v| match v {
                serde_json::Value::Object(o) => o.is_empty(),
                serde_json::Value::Array(a) => a.is_empty(),
                _ => false,
            })
            .unwrap_or(false)
    })
}

/// Whether `node` should be stripped by the marker-meta cleanup, faithful to
/// `CleanUp::stripMarkerMetas`. `in_list_item` is true when `node` has a
/// list-item ancestor (see `isNestedInListItem`).
fn should_strip_marker_meta(node: &Node, in_list_item: bool) -> bool {
    if !matches!(node.kind, NodeKind::Element(ElementKind::Other(ref name)) if name == "meta") {
        return false;
    }
    if crate::html::dom_utils::has_type_of(node, "mw:IndentPreWS") {
        // PHP's `stripMarkerMetas` keeps `mw:IndentPreWS` here (setting the
        // parent's `dsr.openWidth`) and strips it in `finalCleanup`. This
        // pipeline has no `finalCleanup`, so both removals are folded here.
        // The DSR open-width adjustment is handled by `ComputeDSR` (`mw:IndentPreWS`
        // resets `cs`/`ce`), so early removal is observationally equivalent.
        return true;
    }
    if crate::html::dom_utils::has_type_of(node, "mw:Placeholder/UnclosedComment") {
        return true;
    }
    // A non-template meta may carry the `mw:Transclusion` typeof without
    // data-mw; keep it only when it actually has data-mw.
    if data_mw_is_empty(node) {
        if crate::html::dom_utils::has_type_of(node, "mw:Placeholder/StrippedTag") && !in_list_item
        {
            return true;
        }
        if crate::html::dom_utils::has_type_of(node, "mw:Transclusion") {
            return true;
        }
    }
    false
}

/// Remove marker metas from the AST, faithful to PHP's
/// `CleanUp::stripMarkerMetas` (with `finalCleanup`'s `mw:IndentPreWS` removal
/// folded in, since this pipeline has no separate `finalCleanup` pass).
fn strip_marker_metas(node: &mut Node) {
    strip_marker_metas_rec(node, false);
}

fn strip_marker_metas_rec(node: &mut Node, in_list_item: bool) {
    for child in &mut node.children {
        let child_in_li = in_list_item || is_list_item_node(child);
        strip_marker_metas_rec(child, child_in_li);
    }
    node.children
        .retain(|child| !should_strip_marker_meta(child, in_list_item));
}

/// Whether a `data-parsoid` JSON property is present and truthy.
fn dp_bool(dp: &serde_json::Value, key: &str) -> bool {
    dp.get(key)
        .map(|v| v == &serde_json::Value::Bool(true))
        .unwrap_or(false)
}

/// Run the DOM transforms that must occur *after* p-wrapping, in PHP's
/// `NESTED_PIPELINE_DOM_TRANSFORMS` order (`pwrap` … `migrate-metas` …
/// `migrate-nls` … `dsr` … `tplwrap` … `strip-metas`). Called from the full-page
/// pipeline after `p_wrap::run`. `depths` is the `about → (start, end)` marker
/// depth map captured over the freshly-built DOM (before p-wrapping).
pub fn post_pwrap_transforms(
    node: &mut Node,
    depths: &std::collections::HashMap<String, (usize, usize)>,
    source: Option<&str>,
) {
    // Migrate transclusion marker metas toward a canonical position before
    // migrate-nls (mirrors PHP's `migrate-metas` … `migrate-nls` order).
    crate::pipeline::migrate_template_marker_metas::run(node, depths);
    // Hoist trailing newlines out of line-ending / auto-closed elements before
    // template encapsulation (mirrors PHP's `migrate-nls` … `tplwrap` order).
    crate::pipeline::migrate_trailing_nls::run(node);
    // ComputeDSR runs *after* `migrate-metas`/`migrate-nls` (which canonicalize
    // the DOM, e.g. merging the `{{!}}`-split table cells back toward a single
    // cell) and *before* `tplwrap` (mirrors PHP's NESTED_PIPELINE_DOM_TRANSFORMS
    // order `… migrate-metas … migrate-nls … dsr … tplwrap …`), so each node's
    // `dsr` is anchored against the post-migration DOM.
    if let Some(source) = source {
        crate::pipeline::compute_dsr::run(node, source);
    }
    // Encapsulate transclusion meta markers into wrapping `<span>` elements.
    encapsulate_transclusions(node, source);
    // Unpack `mw:DOMFragment` placeholders (extension/template sub-content)
    // into their stashed children. This runs *after* encapsulation (mirrors
    // PHP's `tplwrap` … `dom-unpack` order), so the transclusion range is first
    // computed over the opaque `mw:DOMFragment` placeholder and then transferred
    // onto the unpacked extension content.
    crate::pipeline::unpack_dom_fragments::run(node);
    // A blank-line `<br>` (generated by ParagraphWrapper for two or more
    // source newlines) absorbs the newline that precedes it, so the rendered
    // newline lands *after* the `<br>` rather than before it. Mirrors PHP's
    // `MigrateTrailingNLs`.
    migrate_br_newlines(node);
    // Strip internal marker metas (e.g. `<meta typeof="mw:IndentPreWS">`),
    // mirroring PHP's `CleanUp::stripMarkerMetas()`.
    strip_marker_metas(node);
}

/// Sync the structured `dp.autoInsertedStart`/`autoInsertedEnd` flags into the
/// `data-parsoid` JSON string (used by `remove_auto_inserted_empty_tags`).
fn sync_auto_inserted_flags(node: &mut Node) {
    let Some(dp) = node.dp.as_ref() else {
        return;
    };
    let start = dp.auto_inserted_start;
    let end = dp.auto_inserted_end;
    if let (true, Some(s)) = (node.data_parsoid.is_some(), node.data_parsoid.as_deref())
        && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(s)
        && let Some(obj) = json.as_object_mut()
    {
        if start {
            obj.insert(
                "autoInsertedStart".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        if end {
            obj.insert("autoInsertedEnd".to_string(), serde_json::Value::Bool(true));
        }
        node.data_parsoid = Some(json.to_string());
    }
}

/// Promote the transient `autoInsertedStartToken`/`autoInsertedEndToken` flags
/// to the persistent `autoInsertedStart`/`autoInsertedEnd` form, dropping the
/// token-stage fields. Mirrors `TreeBuilderStage::processToken`'s `EndTagTk`
/// branch (which promotes them onto the element when its end tag is seen).
fn promote_auto_inserted_flags(node: &mut Node) {
    if let Some(dp) = node.data_parsoid.as_deref()
        && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(dp)
        && let Some(obj) = json.as_object_mut()
    {
        if obj.remove("autoInsertedStartToken").is_some() {
            obj.insert(
                "autoInsertedStart".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        if obj.remove("autoInsertedEndToken").is_some() {
            obj.insert("autoInsertedEnd".to_string(), serde_json::Value::Bool(true));
        }
        node.data_parsoid = Some(json.to_string());
    }
    for child in &mut node.children {
        promote_auto_inserted_flags(child);
    }
}

/// Remove empty auto-inserted elements (those with both `autoInsertedStart` and
/// `autoInsertedEnd`, no non-whitespace content, and no `mw:DOMFragment`
/// typeof). Faithful port of
/// `ProcessTreeBuilderFixups::removeAutoInsertedEmptyTags`, migrating any
/// whitespace-only child out before removing the element.
fn remove_auto_inserted_empty_tags(node: &mut Node) {
    for child in &mut node.children {
        if let NodeKind::Element(_) = child.kind {
            remove_auto_inserted_empty_tags(child);
        }
    }

    let children = std::mem::take(&mut node.children);
    let mut out: Vec<Node> = Vec::with_capacity(children.len());
    for child in children {
        let remove = if let NodeKind::Element(_) = &child.kind {
            let has_dom_fragment = child.attrs.iter().any(|a| {
                a.key == "typeof" && a.value.split_whitespace().any(|t| t == "mw:DOMFragment")
            });
            if has_dom_fragment {
                false
            } else if let Some(dp) = child.data_parsoid.as_deref() {
                let json = serde_json::from_str::<serde_json::Value>(dp).ok();
                let dp = json.unwrap_or_default();
                let auto_start = dp_bool(&dp, "autoInsertedStart");
                let auto_end = dp_bool(&dp, "autoInsertedEnd");
                if auto_start && auto_end {
                    // Empty means no children, or a single non-element child that
                    // is whitespace-only text.
                    match child.children.as_slice() {
                        [] => true,
                        [only] if !matches!(only.kind, NodeKind::Element(_)) => {
                            matches!(&only.kind, NodeKind::Text(t) if t.trim().is_empty())
                                || matches!(only.kind, NodeKind::Comment(_))
                        }
                        _ => false,
                    }
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        if remove {
            // Migrate any whitespace-only child out before removing.
            if let [only] = child.children.as_slice()
                && !matches!(only.kind, NodeKind::Element(_))
            {
                out.push(only.clone());
            }
            // Otherwise, drop entirely.
        } else {
            out.push(child);
        }
    }
    node.children = out;
}

/// Whether a node is a transclusion/param *start* marker meta
/// (`typeof="mw:Transclusion"` or `"mw:Param"`, no `/End` suffix).
fn is_transclusion_start(node: &Node) -> bool {
    matches!(&node.kind, NodeKind::Element(ElementKind::Other(name)) if name == "meta")
        && node
            .get_attr("typeof")
            .is_some_and(|t| t == "mw:Transclusion" || t == "mw:Param")
}

/// The source offset at which a transclusion starts, from its marker's `dsr`.
///
/// Returns `None` when the marker carries no `dsr`, in which case the range
/// rules must fall back to nesting alone — an absent offset is not evidence that
/// the sibling is outside the range.
fn transclusion_start_offset(start_meta: &Node) -> Option<usize> {
    start_meta
        .dp
        .as_ref()
        .and_then(|dp| dp.dsr.as_ref())
        .and_then(|dsr| dsr.start)
}

/// Whether `node`'s source range reaches as far as `offset`.
///
/// An element whose `dsr.end` is at or before `offset` ends before the offset, so
/// it cannot contain anything occurring at that offset. Nodes without a `dsr`
/// reach it by default: this test exists to *exclude* a sibling on positive
/// evidence, and an unknown range is not evidence.
fn sibling_reaches_offset(node: &Node, offset: Option<usize>) -> bool {
    let Some(offset) = offset else {
        return true;
    };
    let Some(end) = node
        .dp
        .as_ref()
        .and_then(|dp| dp.dsr.as_ref())
        .and_then(|dsr| dsr.end)
    else {
        return true;
    };
    end > offset
}

/// Whether `node` begins before `offset` in the source.
///
/// An element starting before the transclusion cannot be part of it, even when
/// the transclusion's start marker is nested inside the element — the marker's
/// offset is the transclusion's, and the element's own range is evidence that it
/// predates it. Nodes without a `dsr` begin nowhere in particular and so are not
/// excluded on this ground.
fn sibling_starts_before(node: &Node, offset: Option<usize>) -> bool {
    let Some(offset) = offset else {
        return false;
    };
    node.dp
        .as_ref()
        .and_then(|dp| dp.dsr.as_ref())
        .and_then(|dsr| dsr.start)
        .is_some_and(|start| start < offset)
}

/// Whether a node is a transclusion/param *end* marker meta
/// (`typeof="mw:Transclusion/End"` or `"mw:Param/End"`).
fn is_transclusion_end(node: &Node) -> bool {
    matches!(&node.kind, NodeKind::Element(ElementKind::Other(name)) if name == "meta")
        && node
            .get_attr("typeof")
            .is_some_and(|t| t == "mw:Transclusion/End" || t == "mw:Param/End")
}

/// Whether a node is any transclusion/param marker meta (start or end).
fn is_transclusion_marker_meta(node: &Node) -> bool {
    is_transclusion_start(node) || is_transclusion_end(node)
}

/// Extract a v3 parser-function name from a transclusion start marker's `data-mw`.
/// For v3 parser functions (when `ParsoidExperimentalParserFunctionOutput` is on), the
/// `data-mw` envelope is `{"parts":[{"parserfunction":{"target":{"key":"<name>"}}}]}`.
/// Returns the `<name>` (e.g. `if`) if this is a v3 parser function, else `None`.
fn parser_function_name(start_meta: &Node) -> Option<String> {
    let data_mw_json: serde_json::Value =
        serde_json::from_str(start_meta.data_mw.as_deref()?).ok()?;
    let part = data_mw_json.get("parts")?.as_array()?.first()?;
    let pf = part.get("parserfunction")?;
    pf.get("target")?.get("key")?.as_str().map(str::to_string)
}

/// Encapsulate transclusion meta markers into wrapping `<span>` elements (the
/// common, non-fostered case of PHP's `DOMRangeBuilder::encapsulateTemplates`).
///
/// Each `<meta typeof="mw:Transclusion">` … `<meta typeof="mw:Transclusion/End">`
/// pair (with a matching `about`) is replaced by a `<span>` carrying `about`,
/// `typeof`, `data-parsoid`, and `data-mw`, wrapping the intervening siblings.
fn encapsulate_transclusions(node: &mut Node, source: Option<&str>) {
    // Recurse into element children first, then process direct children.
    for child in &mut node.children {
        if matches!(child.kind, NodeKind::Element(_)) {
            encapsulate_transclusions(child, source);
        }
    }

    let children = std::mem::take(&mut node.children);
    let children = wrap_transclusion_children(children, source, Some(node));
    node.children = wrap_flipped_children(children, source, Some(node));
}

/// Wrap transclusion ranges among a parent's direct children (the sibling case,
/// where both the start and end marker metas are direct children).
///
/// Faithful port of `DOMRangeBuilder::encapsulateTemplates` for the simple,
/// non-fostered case: `ensureElementsInRangeAndAddAboutIds` stamps `about` on
/// every element in the range (wrapping stray text in `about` spans),
/// `findEncapTarget` picks the first non-meta element, and `addTypeOf` + data-mw
/// transfer the `typeof`/`about`/metadata onto that target. The start and end
/// marker metas are then removed.
///
/// Nested transclusions (a marker pair fully contained within another) are
/// fused innermost-first: an inner range's markers are removed and its
/// `typeof`/metadata merged onto its target before the enclosing range is
/// processed, so two nested `mw:Transclusion` markers collapse to one.
fn wrap_transclusion_children(
    children: Vec<Node>,
    source: Option<&str>,
    parent: Option<&Node>,
) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(children.len());
    let mut i = 0;
    while i < children.len() {
        if !is_transclusion_start(&children[i]) {
            out.push(children[i].clone());
            i += 1;
            continue;
        }

        // Find the matching end meta, accounting for nesting.
        let mut depth = 0usize;
        let mut end_idx = None;
        for (j, child) in children.iter().enumerate().skip(i) {
            if is_transclusion_start(child) {
                depth += 1;
            } else if is_transclusion_end(child) {
                depth -= 1;
                if depth == 0 {
                    end_idx = Some(j);
                    break;
                }
            }
        }

        let Some(end) = end_idx else {
            // Unmatched start marker: leave as-is.
            out.push(children[i].clone());
            i += 1;
            continue;
        };

        let start_meta = children[i].clone();
        let about = start_meta.get_attr("about").map(str::to_string);
        let typeof_attr = start_meta.get_attr("typeof").map(str::to_string);

        // Fuse any *nested* ranges in the content first (innermost-first).
        let content: Vec<Node> = children[i + 1..end].to_vec();
        let content = wrap_transclusion_children(content, source, parent);

        // Stamp `about` on every element in the range and find the first
        // element (the encapsulation target), dropping deletable text and
        // wrapping non-whitespace text in `about` spans. Nested template-marker
        // metas are skipped as encapsulation targets (mirrors `findEncapTarget`
        // skipping `isTplMarkerMeta`), but still receive the `about` stamp.
        //
        // Whitespace-only text that sits *between* two non-marker elements is
        // significant (it preserves a block boundary inside the transclusion),
        // so it is wrapped in a single-space `about` span rather than dropped.
        let mut new_content: Vec<Node> = Vec::with_capacity(content.len());
        let mut encap_target = None;
        for (idx, child) in content.iter().enumerate() {
            match &child.kind {
                NodeKind::Element(_) => {
                    let mut child = child.clone();
                    if let Some(about) = &about {
                        child.set_attr("about", about.clone());
                    }
                    let is_marker = is_transclusion_marker_meta(&child);
                    if encap_target.is_none() && !is_marker {
                        encap_target = Some(new_content.len());
                    }
                    new_content.push(child);
                }
                NodeKind::Text(s) => {
                    if s.trim().is_empty() {
                        // Whitespace-only text inside a transclusion range.
                        // Mirror PHP `isDeletableNode`: drop it only in the
                        // narrowly-targeted cases (it separates a wikitext
                        // block from a following wikitext list/table, or it
                        // sits between two sol-transparent links); otherwise
                        // wrap it in a single-space `about` span so the range
                        // stays contiguous and editable.
                        if is_deletable_in_range(&content, idx, parent) {
                            continue;
                        }
                        // Span-wrap the newline (single space) to keep the
                        // transclusion boundary inside the paragraph.
                        let mut span = Node::element(ElementKind::Span);
                        if let Some(about) = &about {
                            span.set_attr("about", about.clone());
                        }
                        span.push_child(Node::text(" "));
                        span.data_parsoid = Some("{\"tmp\":{\"wrapper\":true}}".to_string());
                        new_content.push(span);
                        continue;
                    }
                    // Wrap non-whitespace text in an `about` span so the range
                    // is a contiguous chain of elements. The span becomes the
                    // encapsulation target if no earlier element exists. Mark it
                    // with the WRAPPER temp flag (mirrors PHP `addSpanWrappers`),
                    // on the structured `dp` so it survives the `data_parsoid`
                    // transfer onto the encapsulation target below.
                    let mut span = Node::element(ElementKind::Span);
                    if let Some(about) = &about {
                        span.set_attr("about", about.clone());
                    }
                    span.push_child(Node::text(s.clone()));
                    span.data_parsoid = Some("{\"tmp\":{\"wrapper\":true}}".to_string());
                    span.dp = Some(TDataParsoid {
                        tmp: crate::wikitext::tokens_v2::TempData {
                            wrapper: true,
                            ..Default::default()
                        },
                        ..TDataParsoid::default()
                    });
                    if encap_target.is_none() {
                        encap_target = Some(new_content.len());
                    }
                    new_content.push(span);
                }
                _ => new_content.push(child.clone()),
            }
        }

        // Transfer `typeof`/`about`/metadata onto the encapsulation target,
        // merging (rather than overwriting) any existing `typeof` so that an
        // extension `mw:Extension/pre` combines with `mw:Transclusion`
        // (mirrors `DOMUtils::addTypeOf`'s multivalue handling).
        if let Some(et) = encap_target {
            // If the encapsulation target is rendering-transparent (a
            // category/redirect/language link, comment, or non-HTML meta), it
            // cannot carry content; wrap it in a `<span class="mw-empty-elt">`
            // that becomes the encapsulation target instead (mirrors PHP
            // `DOMRangeBuilder::handleFirstRenderingTransparentNode`, which
            // stashes such nodes into an `mw-empty-elt` span at a range
            // boundary and moves the transclusion metadata onto the span).
            if crate::html::wts_utils::is_rendering_transparent_node(&new_content[et]) {
                let mut inner = new_content.remove(et);
                // The `about` id was stamped on the transparent node during
                // range stamping; move it onto the wrapper span.
                let about_id = inner.get_attr("about").map(str::to_string);
                inner.attrs.retain(|a| a.key != "about");
                let mut span = Node::element(ElementKind::Span);
                span.set_attr("class", "mw-empty-elt");
                if let Some(id) = about_id {
                    span.set_attr("about", id);
                }
                span.children.push(inner);
                new_content.insert(et, span);
            }

            if let Some(typeof_) = &typeof_attr {
                let existing = new_content[et].get_attr("typeof").map(str::to_string);
                let merged = match existing {
                    Some(existing) if !existing.split_whitespace().any(|t| t == typeof_) => {
                        // The `mw:Transclusion` type is *prepended* when the
                        // encapsulation target is a media container (`mw:File`),
                        // yielding `mw:Transclusion mw:File` (mirrors PHP's
                        // `AddMediaInfo`, which adds `mw:File` after the
                        // transclusion marker). For all other targets
                        // (extensions, `mw:Nowiki`, parser functions, …) the
                        // transclusion type is *appended* (`mw:Extension/pre
                        // mw:Transclusion`, etc.).
                        let is_media = existing
                            .split_whitespace()
                            .any(|t| t == "mw:File" || t.starts_with("mw:File/"));
                        if is_media {
                            format!("{typeof_} {existing}")
                        } else {
                            format!("{existing} {typeof_}")
                        }
                    }
                    Some(existing) => existing,
                    None => typeof_.clone(),
                };
                new_content[et].set_attr("typeof", merged);
            }
            // v3 parser functions add a `mw:ParserFunction/<name>` typeof
            // (mirrors `DOMRangeBuilder::encapsulateTemplates`, which adds it
            // when `TemplateInfo.type === 'parserfunction'`).
            if let Some(pf_name) = parser_function_name(&start_meta) {
                let existing = new_content[et].get_attr("typeof").map(str::to_string);
                let pf_typeof = format!("mw:ParserFunction/{pf_name}");
                let merged = match existing {
                    Some(existing) if !existing.split_whitespace().any(|t| t == pf_typeof) => {
                        format!("{existing} {pf_typeof}")
                    }
                    Some(existing) => existing,
                    None => pf_typeof,
                };
                new_content[et].set_attr("typeof", merged);
            }
            new_content[et].data_parsoid = start_meta.data_parsoid.clone();
            // The encapsulation target (e.g. a `<div>`/`<pre>` block produced by a
            // template argument) carries a *fragment-relative* `tsr`/`dsr` (offsets
            // into the template's own substituted source, e.g. `[0, 5]` for
            // `<div>`). The `mw:Transclusion` start meta carries the page-relative
            // `tsr`/`dsr` of the whole `{{…}}`; sync those onto the target (while
            // preserving the target's own fragment `src`/`dom_fragment_src`, which
            // are used to reconstruct the argument's tag) so ComputeDSR anchors the
            // transclusion at the correct page offset.
            if let (Some(mdp), Some(tdp)) = (start_meta.dp.as_ref(), new_content[et].dp.as_mut()) {
                tdp.tsr = mdp.tsr.clone();
                tdp.dsr = mdp.dsr.clone();
            }
            // Merge the transclusion's `data-mw` (its `parts` envelope) with any
            // `data-mw` already on the encapsulation target (e.g. a media
            // container's `attribs`/`errors`), rather than overwriting it. This
            // mirrors PHP, where `DOMDataUtils::setDataMw` on the encapsulation
            // target preserves the target's own `attribs` while attaching the
            // transclusion `parts`. Losing the target's `data-mw` here would
            // drop e.g. a `link=` option and break `AddMediaInfo`.
            new_content[et].data_mw = merge_encap_data_mw(
                build_compound_data_mw(&start_meta, source, None),
                new_content[et].data_mw.clone(),
            );
            // A templated *table* whose range encloses nested transclusion ranges
            // gets a compound `data-mw` listing every constituent template plus
            // the intervening wikitext (mirrors `DOMRangeBuilder::recordTemplateInfo`
            // collecting into `compoundTpls`). Without this the table reports a
            // single template, so `fromWellBalancedTemplate` (one part) matches and
            // `TableFixups` skips the whole table, leaving template-generated cell
            // attributes unparsed.
            if matches!(new_content[et].kind, NodeKind::Element(ElementKind::Table)) {
                let nested =
                    collect_nested_transclusion_parts(&new_content[et], &start_meta, source);
                if !nested.is_empty()
                    && let Some(merged) =
                        build_compound_data_mw_with_nested(&start_meta, source, &nested)
                {
                    new_content[et].data_mw =
                        merge_encap_data_mw(Some(merged), new_content[et].data_mw.clone());
                    // PHP's merge dissolves an absorbed inner range into plain
                    // content: its post-tplwrap cells hold a bare text node with
                    // no transclusion span at all. rustoid encapsulates inner
                    // ranges eagerly, so undo that here — unwrap each absorbed
                    // span into its children (keeping the content, dropping the
                    // now-redundant `about`/`typeof`/`data-mw`).
                    let outer_about = start_meta.get_attr("about").map(str::to_string);
                    dissolve_absorbed_ranges(&mut new_content[et], outer_about.as_deref());
                }
            }
            apply_encap_dp_fields(&mut new_content[et], &start_meta);
        } else {
            // Empty transclusion: the start and end markers are adjacent (no
            // content). PHP `DOMRangeBuilder::findEnclosingRange` inserts an
            // empty `<span>` before the end marker, which then becomes the
            // encapsulation target and receives `about`/`typeof`/metadata.
            // Recreate that so an empty template round-trips as an editable
            // (empty) transclusion rather than disappearing entirely.
            let mut span = Node::element(ElementKind::Span);
            if let Some(about) = &about {
                span.set_attr("about", about.clone());
            }
            if let Some(typeof_) = &typeof_attr {
                span.set_attr("typeof", typeof_.clone());
            }
            span.data_parsoid = start_meta.data_parsoid.clone();
            span.data_mw = build_compound_data_mw(&start_meta, source, None);
            apply_encap_dp_fields(&mut span, &start_meta);
            new_content.push(span);
        }

        out.extend(new_content);
        i = end + 1;
    }
    out
}

/// Whether a whitespace-only text node inside a transclusion range should be
/// deleted (rather than wrapped in a single-space `about` span). Faithful port
/// of PHP `DOMRangeBuilder::isDeletableNode`.
///
/// The first check is the important one for tables: a text node in a *fosterable*
/// position cannot have any rendering-relevant content (the HTML tree builder
/// would already have fostered it out), and `data-mw` captures the template's
/// output anyway, so it is always safe to drop. Everything else is the narrowly
/// targeted newline handling: deletable when it separates a wikitext block node
/// from a following wikitext list/table, or when it sits between two
/// sol-transparent links.
fn is_deletable_in_range(content: &[Node], idx: usize, parent: Option<&Node>) -> bool {
    // `DOMUtils::isFosterablePosition` keys off the *parent* element name.
    if parent.is_some_and(crate::html::dom_utils::is_fosterable_position_element) {
        return true;
    }

    let prev = idx.checked_sub(1).map(|p| &content[p]);
    let next = content.get(idx + 1);

    if let Some(prev) = prev
        && crate::html::dom_utils::is_wikitext_block_node(prev)
        && let Some(next) = next
        && matches!(&next.kind, NodeKind::Element(_))
    {
        let next_name = crate::html::wts_utils::node_name(next);
        if matches!(next_name.as_str(), "ul" | "ol" | "table") {
            return true;
        }
    }

    if let Some(prev) = prev
        && let Some(next) = next
        && crate::html::wts_utils::is_sol_transparent_link(prev)
        && crate::html::wts_utils::is_sol_transparent_link(next)
    {
        return true;
    }

    false
}

/// Wrap transclusion ranges whose start/end markers were not both emitted as
/// direct siblings (PHP's `DOMRangeBuilder`'s common-ancestor handling).
///
/// When the start marker meta and the element containing the end marker are
/// siblings under a common ancestor, the start marker's `about`/`typeof`/
/// `data-mw`/`data-parsoid` are transferred onto the first non-meta element of
/// the range, `about` is stamped on every other element in the range, and both
/// marker metas are removed.
///
/// This covers both:
///   - the end marker nested in a *following* sibling element (the common,
///     non-fostered case, e.g. `{{1x|*bar}}` → `<meta/> <ul>…</ul>`), and
///   - the "flipped" case where the end marker was fostered into a *preceding*
///     sibling element.
fn wrap_flipped_children(
    mut children: Vec<Node>,
    source: Option<&str>,
    parent: Option<&Node>,
) -> Vec<Node> {
    let mut i = 0;
    while i < children.len() {
        // The range start is either the marker meta itself (a direct sibling) or
        // the deepest element whose subtree holds a start marker whose end marker
        // lies outside that subtree. PHP's `findWrappableTemplateRangesRecursive`
        // pairs the two metas wherever they are in the tree, and
        // `findEnclosingRange` then lifts the range to their common ancestor, so
        // a template that opens inside one child and closes inside another is
        // encapsulated on the enclosing element of the opening marker (e.g. the
        // first of two sibling `<td>`s).
        let start_meta = if is_transclusion_start(&children[i]) {
            children[i].clone()
        } else if matches!(children[i].kind, NodeKind::Element(_))
            && let Some(found) = find_unmatched_start_meta(&children[i])
        {
            found.clone()
        } else {
            i += 1;
            continue;
        };

        let about: Option<String> = start_meta.get_attr("about").map(str::to_string);

        // Find the sibling element (in either direction, nearest first) whose
        // subtree contains the matching end marker.
        let mut target = None;
        for (j, child) in children.iter().enumerate().take(i).rev() {
            if matches!(child.kind, NodeKind::Element(_))
                && subtree_contains_end_meta(child, about.as_deref())
            {
                target = Some(j);
                break;
            }
        }
        if target.is_none() {
            for (j, child) in children.iter().enumerate().skip(i + 1) {
                if matches!(child.kind, NodeKind::Element(_))
                    && subtree_contains_end_meta(child, about.as_deref())
                {
                    target = Some(j);
                    break;
                }
            }
        }

        let Some(t) = target else {
            i += 1;
            continue;
        };

        // A target *before* the start marker would extend the range backwards,
        // which is only legitimate when the marker really is nested inside it.
        //
        // The marker's own `dsr.start` is the evidence that decides it: the
        // template begins at that offset in the source, so a sibling that ends
        // at or before it cannot be part of the range whatever the nesting says.
        // Checked per range, so it does not depend on how the paragraph wrapper
        // chose to nest things.
        if t < i && !sibling_reaches_offset(&children[t], transclusion_start_offset(&start_meta)) {
            i += 1;
            continue;
        }

        // Whether the range start is the sibling itself (`start_meta` is
        // `children[i]`) or a marker nested inside its subtree (PHP's
        // common-ancestor case).
        let start_is_sibling = is_transclusion_start(&children[i]);

        // Determine the contiguous sibling range [lo, hi] spanned by the
        // transclusion: from the start meta to the element holding the end
        // marker. Every element in that range gets the `about` id; the first
        // non-meta element becomes the encapsulation target.
        let (lo, hi) = if t < i { (t, i) } else { (i, t) };
        let mut encap_target = None;
        for (j, child) in children.iter_mut().enumerate().skip(lo).take(hi - lo + 1) {
            if matches!(child.kind, NodeKind::Element(_)) && !is_transclusion_start(child) {
                // A sibling that begins *before* the transclusion cannot be in
                // it, whatever the nesting says. This is the paragraph-wrapper
                // case: on `AAA{{If empty|<div>X</div>|b}}` the `<p>` holds the
                // start marker yet begins at offset 0, before the template at 3,
                // so stamping it with `about` gave the `<p>` the same id as the
                // `<div>` — two elements sharing one transclusion id, where the
                // live service serves one. Narrowing the *range* to drop the
                // element was tried and cost five fixtures: the range must stay
                // contiguous for the span-wrapping and deletability steps below,
                // so only the stamp is withheld here.
                if !sibling_starts_before(child, transclusion_start_offset(&start_meta)) {
                    child.set_attr("about", start_meta.get_attr("about").unwrap_or(""));
                }
                if encap_target.is_none() {
                    encap_target = Some(j);
                }
            }
        }

        // A text node inside the range cannot carry `about`, so it is span-wrapped
        // (mirrors `ensureElementsInRangeAndAddAboutIds`, which requires every node
        // in the range to be an Element so the chain stays contiguous and
        // editable). A newline between two block siblings, e.g. the one between the
        // `<dd>`s of `one\n::two`, is kept this way rather than dropped: PHP's
        // `isDeletableNode` only discards it in the two narrow cases mirrored by
        // `is_deletable_in_range`, and a `dd`/`dd` pair is neither.
        let range: Vec<Node> = children[lo..=hi].to_vec();
        let about_id = start_meta.get_attr("about").map(str::to_string);
        for (offset, node) in range.iter().enumerate() {
            if !matches!(&node.kind, NodeKind::Text(t) if t.trim().is_empty()) {
                continue;
            }
            if is_deletable_in_range(&range, offset, parent) {
                continue;
            }
            // A whitespace text node in a fosterable position is already dropped
            // by `is_deletable_in_range`, so this wrapping only ever sees nodes that
            // belong in the range (a newline between block siblings such as the
            // `<dd>`s of `one\n::two`).
            let NodeKind::Text(text) = &node.kind else {
                continue;
            };
            let mut span = Node::element(ElementKind::Span);
            if let Some(about) = &about_id {
                span.set_attr("about", about.clone());
            }
            span.push_child(Node::text(text.clone()));
            span.data_parsoid = Some("{\"tmp\":{\"wrapper\":true}}".to_string());
            children[lo + offset] = span;
        }

        // Adoption scenario (PHP `findWrappableTemplateRangesRecursive`): when
        // the transclusion content was fostered out of a `<table>`, that table
        // follows the element holding the end marker as a sibling and shares the
        // transclusion's source start. Stamp `about` on it (and any subsequent
        // sibling sharing that start) so the whole fostered block + table form a
        // single `about` chain.
        let range_start = start_meta
            .dp
            .as_ref()
            .and_then(|d| d.dsr.as_ref().and_then(|r| r.start));
        if let Some(range_start) = range_start {
            for child in children.iter_mut().skip(t + 1) {
                let same_start = child
                    .dp
                    .as_ref()
                    .and_then(|d| d.dsr.as_ref().and_then(|r| r.start))
                    == Some(range_start);
                if matches!(child.kind, NodeKind::Element(_)) && same_start {
                    child.set_attr("about", start_meta.get_attr("about").unwrap_or(""));
                }
            }
        }

        let Some(et) = encap_target else {
            i += 1;
            continue;
        };

        // Determine the range's end DSR by merging the start-meta's `dsr.end`
        // with any *following* sibling whose DSR extends past it (mirrors PHP's
        // `encapsulateTemplates` `$dp1DSR->end = $dp2DSR->end` merge). This
        // covers multi-template-content-blocks where the transclusion output
        // spills past the element holding the end marker (e.g. a fostered
        // `<p>` followed by a `<table>`), so the trailing wikitext part and the
        // range's `about` chain both include the whole block.
        let tpl_end = start_meta
            .dp
            .as_ref()
            .and_then(|d| d.dsr.as_ref().and_then(|r| r.end));
        let mut range_end = tpl_end;
        for child in children.iter().skip(i + 1) {
            if let Some(end) = child
                .dp
                .as_ref()
                .and_then(|d| d.dsr.as_ref().and_then(|r| r.end))
            {
                range_end = Some(range_end.map_or(end, |cur| cur.max(end)));
            }
        }

        // Transfer encapsulation data onto the target element and drop the
        // end marker from its subtree. The end marker lives under the sibling
        // element `t` (located via `subtree_contains_end_meta`), which may
        // differ from the encapsulation target `et` when an intervening
        // element precedes `t` (e.g. `<meta/> <p>..</p> <i><div>..</div><meta/End></i>`).
        //
        // Table fostering (PHP `getDOMRange`'s `getStartConsideringFosteredContent` +
        // `MAP_TBODY_TR` migration): when the encapsulation target is a `<table>`, the
        // transclusion content was fostered out of the table, so the encap data goes on
        // the table *body* (`<tbody>`/`<thead>`/`<tfoot>`) for a well-balanced
        // (single-template) transclusion, or on the first content *cell* (`<td>`/`<th>`)
        // when the range extends beyond the template (mixed template + wikitext content).
        //
        // Drop the end marker (a `<table>` child, sibling of the `<tbody>`) *before*
        // transferring onto the nested body, so the two `&mut` borrows don't alias.
        let well_balanced = tpl_end.is_none_or(|te| range_end.is_some_and(|re| re <= te));
        remove_end_meta(&mut children[t], about.as_deref());
        {
            let encap_node = table_body_content_target(&mut children[et], well_balanced);
            transfer_transclusion_to_element(encap_node, &start_meta, source, range_end);
        }
        // Remove the start marker meta: either the sibling element itself, or
        // the marker nested in the range-start element's subtree.
        if start_is_sibling {
            children.remove(i);
            // Do not advance `i`: the next sibling shifted into this index.
        } else {
            remove_start_meta(&mut children[i], about.as_deref());
            i += 1;
        }
    }
    children
}

/// Resolve the actual encapsulation target for a fostered table range.
///
/// When the range start is a `<table>` whose content was fostered (PHP
/// `getStartConsideringFosteredContent` + `MAP_TBODY_TR`), the transclusion
/// encapsulation goes on the table *body* (`<tbody>`/`<thead>`/`<tfoot>`) when
/// the transclusion is well-balanced (a single template produced the whole
/// table body — mirrors `DataMw::fromWellBalancedTemplate`), and on the first
/// content *cell* (`<td>`/`<th>`) when the range extends beyond the template
/// (mixed template + wikitext, e.g. `{{table_attribs_4}} ||a||b`). The sibling
/// cells receive the same `about` id via the range's `about` chain.
fn table_body_content_target(table: &mut Node, well_balanced: bool) -> &mut Node {
    if !matches!(table.kind, NodeKind::Element(ElementKind::Table)) {
        return table;
    }
    let Some(body_idx) = table.children.iter().position(|c| {
        matches!(&c.kind, NodeKind::Element(ElementKind::Other(name))
            if matches!(name.as_str(), "tbody" | "thead" | "tfoot"))
            || matches!(c.kind, NodeKind::Element(ElementKind::TableRow))
    }) else {
        return table;
    };
    let body = &mut table.children[body_idx];
    if well_balanced || !is_table_body_element(body) {
        // A well-balanced (whole-body) transclusion target is the body/row
        // itself; a direct `<tr>` child (no implicit `<tbody>`) is also the
        // body-level target.
        return body;
    }
    // Mixed content: descend `<tbody>`/`<thead>`/`<tfoot>` → `<tr>` → first cell.
    let Some(row_idx) = body
        .children
        .iter()
        .position(|c| matches!(c.kind, NodeKind::Element(ElementKind::TableRow)))
    else {
        return body;
    };
    let row = &mut body.children[row_idx];
    let Some(cell_idx) = row.children.iter().position(|c| {
        matches!(
            c.kind,
            NodeKind::Element(ElementKind::TableCell | ElementKind::TableHeader)
        )
    }) else {
        return row;
    };
    &mut row.children[cell_idx]
}

/// Is this node a table *body* wrapper element (`<tbody>`/`<thead>`/`<tfoot>`)?
fn is_table_body_element(node: &Node) -> bool {
    matches!(&node.kind, NodeKind::Element(ElementKind::Other(name))
        if matches!(name.as_str(), "tbody" | "thead" | "tfoot"))
}

/// Does this subtree contain a transclusion *end* marker with the given `about`?
fn subtree_contains_end_meta(node: &Node, about: Option<&str>) -> bool {
    if is_transclusion_end(node) && node.get_attr("about") == about {
        return true;
    }
    node.children
        .iter()
        .any(|c| subtree_contains_end_meta(c, about))
}

/// Find the first transclusion/param start marker meta in this subtree whose
/// end marker is *not* also inside the subtree. Used to pair marker metas that
/// live in different children of a common ancestor (PHP's
/// `DOMRangeBuilder::findEnclosingRange` common-ancestor case), e.g. a template
/// that opens in one `<td>` and closes in the next.
fn find_unmatched_start_meta(node: &Node) -> Option<&Node> {
    if is_transclusion_start(node) {
        let about = node.get_attr("about");
        if !node
            .children
            .iter()
            .any(|c| subtree_contains_end_meta(c, about))
        {
            return Some(node);
        }
    }
    node.children.iter().find_map(find_unmatched_start_meta)
}

/// Remove a transclusion *start* marker with the given `about` from a subtree.
/// Returns true if one was removed.
fn remove_start_meta(node: &mut Node, about: Option<&str>) -> bool {
    let mut found = false;
    let mut i = 0;
    while i < node.children.len() {
        if is_transclusion_start(&node.children[i]) && node.children[i].get_attr("about") == about {
            node.children.remove(i);
            found = true;
        } else {
            i += 1;
        }
    }
    for child in &mut node.children {
        if remove_start_meta(child, about) {
            found = true;
        }
    }
    found
}

/// Remove a transclusion *end* marker with the given `about` from a subtree.
/// Returns true if one was removed.
fn remove_end_meta(node: &mut Node, about: Option<&str>) -> bool {
    let mut found = false;
    let mut i = 0;
    while i < node.children.len() {
        if is_transclusion_end(&node.children[i]) && node.children[i].get_attr("about") == about {
            node.children.remove(i);
            found = true;
        } else {
            i += 1;
        }
    }
    for child in &mut node.children {
        if remove_end_meta(child, about) {
            found = true;
        }
    }
    found
}

/// Transfer the encapsulation data from a transclusion start marker meta onto
/// the target element (mirrors `encapsulateTemplates`' type/`about`/data-mw
/// transfer when the range start is a non-meta element).
fn transfer_transclusion_to_element(
    target: &mut Node,
    start_meta: &Node,
    source: Option<&str>,
    range_end: Option<usize>,
) {
    if let Some(about) = start_meta.get_attr("about") {
        target.set_attr("about", about);
    }
    if let Some(typeof_) = start_meta.get_attr("typeof") {
        target.set_attr("typeof", typeof_);
    }
    target.data_parsoid = start_meta.data_parsoid.clone();
    target.data_mw = build_compound_data_mw(start_meta, source, range_end);
    apply_encap_dp_fields(target, start_meta);
}

/// Apply `firstWikitextNode` and `pi` onto an encapsulation target, derived
/// from the transclusion start-meta's transient `TempData` (mirrors
/// `encapsulateTemplates`' `$encapDP->firstWikitextNode`/`$encapDP->pi`).
/// Both the structured `dp` and the serialized `data-parsoid` string are
/// updated (the serializer emits the string form).
fn apply_encap_dp_fields(target: &mut Node, start_meta: &Node) {
    let Some(src) = start_meta.dp.as_ref() else {
        return;
    };
    let first_wikitext_node = src.tmp.first_wikitext_node.clone();
    let pi = src.tmp.tplarginfo.clone().map(|inner| format!("[{inner}]"));

    if let Some(dp) = target.dp.as_mut() {
        dp.first_wikitext_node = first_wikitext_node.clone();
        dp.pi = pi.clone();
    }

    // Nothing to add to the serialized form: leave `data-parsoid` untouched.
    if first_wikitext_node.is_none() && pi.is_none() {
        return;
    }

    let mut json: serde_json::Value = target
        .data_parsoid
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(obj) = json.as_object_mut() {
        if let Some(fwn) = &first_wikitext_node {
            obj.insert(
                "firstWikitextNode".to_string(),
                serde_json::Value::String(fwn.clone()),
            );
        }
        if let Some(pi) = &pi
            && let Ok(pi_json) = serde_json::from_str::<serde_json::Value>(pi)
        {
            obj.insert("pi".to_string(), pi_json);
        }
    }
    target.data_parsoid = Some(json.to_string());
}

/// Run the HTML5 tree builder over a token stream.
pub fn token_stream_to_ast_html(tokens: &[Item]) -> Node {
    token_stream_to_ast_html_with_source(tokens, None)
}

/// Run the HTML5 tree builder over a token stream, with the page source
/// available for `tsr`-based source recovery in deleted-tag placeholders.
pub fn token_stream_to_ast_html_with_source(tokens: &[Item], source: Option<&str>) -> Node {
    token_stream_to_ast_html_with_fragments(tokens, source, HashMap::new(), None)
}

/// Like [`token_stream_to_ast_html_with_source`], but accepts pre-built
/// sub-fragments keyed by id (for `mw:dom-fragment-token` placeholders).
pub fn token_stream_to_ast_html_with_fragments(
    tokens: &[Item],
    source: Option<&str>,
    fragments: HashMap<usize, Node>,
    about_counter: Option<std::rc::Rc<std::cell::Cell<usize>>>,
) -> Node {
    let builder = Html5TreeBuilder::with_source_and_fragments(source.unwrap_or(""), fragments);
    let mut builder = match about_counter {
        Some(c) => builder.with_about_counter(c),
        None => builder,
    };
    builder.process_chunk(tokens);
    builder.process_token(&Item::Tok(ParsoidToken::Eof(
        crate::wikitext::tokens_v2::EOFTk,
    )));
    builder.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::{ElementKind, NodeKind};
    use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, SelfclosingTagTk, TagTk};

    fn tag(name: &str) -> Item {
        Item::Tok(ParsoidToken::Tag(TagTk::new(
            name,
            vec![],
            DataParsoid::default(),
        )))
    }
    fn end(name: &str) -> Item {
        Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            name,
            vec![],
            DataParsoid::default(),
        )))
    }
    fn txt(s: &str) -> Item {
        Item::Str(s.to_string())
    }

    #[test]
    fn test_prepend_unwrapped_wt_part() {
        // A single-template `data-mw.parts` gains a leading literal-wikitext part
        // for the recovered `unwrappedWT` (T322557 multi-template-content-block).
        let data_mw = r#"{"parts":[{"template":{"target":{"wt":"1x"}}}]}"#;
        let out = prepend_unwrapped_wt_part(data_mw, "{| <span>x</span> ");
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        let parts = json["parts"].as_array().unwrap();
        assert_eq!(
            parts[0],
            serde_json::Value::String("{| <span>x</span> ".into())
        );
        assert!(parts[1].get("template").is_some(), "{parts:?}");

        // Idempotent: an already-present leading part is not duplicated.
        let out2 = prepend_unwrapped_wt_part(&out, "{| <span>x</span> ");
        assert_eq!(out, out2);
    }

    #[test]
    fn test_build_compound_data_mw() {
        // A start-meta with `unwrappedWT` + a `dsr.end` and a following range-end
        // DSR produce all three parts: leading wikitext, template, trailing WT.
        let mut start = Node::element(ElementKind::Other("meta".to_string()));
        start.data_mw = Some(r#"{"parts":[{"template":{"target":{"wt":"1x"}}}]}"#.to_string());
        let mut dp = DataParsoid::default();
        dp.tmp.unwrapped_wt = Some("{| <span>x</span> ".to_string());
        dp.dsr = Some(crate::wikitext::tokens_v2::DomSourceRange {
            start: Some(0),
            end: Some(38),
            open_width: None,
            close_width: None,
            ..Default::default()
        });
        start.dp = Some(dp);

        // A 38-char prefix followed by `\n|}` so `source[38..41] == "\n|}"`.
        let source = format!("{:width$}\n|}}", "", width = 38);
        let out = build_compound_data_mw(&start, Some(&source), Some(41)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        let parts = json["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(
            parts[0],
            serde_json::Value::String("{| <span>x</span> ".into())
        );
        assert!(parts[1].get("template").is_some());
        assert_eq!(parts[2], serde_json::Value::String("\n|}".into()));
    }

    #[test]
    fn test_table_body_content_target() {
        // A well-balanced (single-template) transclusion targets the `<tbody>`.
        let mut table = Node::element(ElementKind::Table);
        let mut tbody = Node::element(ElementKind::Other("tbody".to_string()));
        let mut tr = Node::element(ElementKind::TableRow);
        tr.push_child(Node::element(ElementKind::TableCell));
        tbody.push_child(tr);
        table.push_child(tbody);
        assert_eq!(
            table_body_content_target(&mut table, true).kind,
            NodeKind::Element(ElementKind::Other("tbody".to_string()))
        );

        // A mixed-content transclusion targets the first `<td>`.
        let mut table = Node::element(ElementKind::Table);
        let mut tbody = Node::element(ElementKind::Other("tbody".to_string()));
        let mut tr = Node::element(ElementKind::TableRow);
        tr.push_child(Node::element(ElementKind::TableCell));
        tr.push_child(Node::element(ElementKind::TableCell));
        tbody.push_child(tr);
        table.push_child(tbody);
        assert_eq!(
            table_body_content_target(&mut table, false).kind,
            NodeKind::Element(ElementKind::TableCell)
        );
    }

    #[test]
    fn test_wrap_flipped_children_pairs_markers_across_siblings() {
        // A template that opens inside one `<td>` and closes inside the next
        // (T343874 / "Newline constraint after multi-node template"). PHP's
        // `findEnclosingRange` lifts the range to the common ancestor, so the
        // *first* cell becomes the encapsulation target and the second only
        // carries the `about` id.
        fn start_meta() -> Node {
            let mut m = Node::element(ElementKind::Other("meta".to_string()));
            m.set_attr("typeof", "mw:Transclusion");
            m.set_attr("about", "#mwt1");
            m
        }
        fn end_meta() -> Node {
            let mut m = Node::element(ElementKind::Other("meta".to_string()));
            m.set_attr("typeof", "mw:Transclusion/End");
            m.set_attr("about", "#mwt1");
            m
        }

        let mut td1 = Node::element(ElementKind::TableCell);
        td1.push_child(Node::text(" "));
        td1.push_child(start_meta());
        td1.push_child(Node::text("test"));

        let mut td2 = Node::element(ElementKind::TableCell);
        td2.push_child(Node::text(" 123"));
        td2.push_child(end_meta());

        let out = wrap_flipped_children(vec![td1, td2], None, None);

        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].get_attr("about"), Some("#mwt1"));
        assert_eq!(out[0].get_attr("typeof"), Some("mw:Transclusion"));
        assert_eq!(out[1].get_attr("about"), Some("#mwt1"));
        assert_eq!(out[1].get_attr("typeof"), None);
        // Both marker metas are gone; the first cell keeps its content.
        assert!(
            !out[0].children.iter().any(is_transclusion_marker_meta),
            "{out:?}"
        );
        assert!(
            !out[1].children.iter().any(is_transclusion_marker_meta),
            "{out:?}"
        );
    }

    #[test]
    fn test_newline_between_block_siblings_gets_about_span() {
        // `one\n::two` (Template:definition_list): the newline between the two
        // `<dd>`s cannot carry `about`, so it is span-wrapped to keep the range a
        // contiguous chain of elements (mirrors
        // `ensureElementsInRangeAndAddAboutIds`). PHP keeps it — `isDeletableNode`
        // only drops a newline in two narrow cases, and a `dd`/`dd` pair is
        // neither.
        fn meta(typeof_: &str) -> Node {
            let mut m = Node::element(ElementKind::Other("meta".to_string()));
            m.set_attr("typeof", typeof_);
            m.set_attr("about", "#mwt1");
            m
        }
        let dd = |text: &str| {
            let mut d = Node::element(ElementKind::Other("dd".to_string()));
            d.push_child(Node::text(text));
            d
        };
        let parent = Node::element(ElementKind::Other("dl".to_string()));

        let out = wrap_flipped_children(
            vec![meta("mw:Transclusion"), dd("one"), Node::text("\n"), {
                let mut d = dd("two");
                d.push_child(meta("mw:Transclusion/End"));
                d
            }],
            None,
            Some(&parent),
        );

        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0].get_attr("about"), Some("#mwt1"));
        // The middle node is a span wrapping the newline, carrying `about`.
        assert_eq!(crate::html::wts_utils::node_name(&out[1]), "span");
        assert_eq!(out[1].get_attr("about"), Some("#mwt1"));
        assert_eq!(
            out[1].children.first().map(|c| match &c.kind {
                NodeKind::Text(t) => t.as_str(),
                _ => "",
            }),
            Some("\n")
        );
        assert_eq!(out[2].get_attr("about"), Some("#mwt1"));
    }

    #[test]
    fn test_nested_dl_table_blocks() {
        // `:{| ...|}` then `:::{| ...|}` — two dl blocks, the second nested 3
        // deep, separated by a blank line. Both must survive.
        let doc = token_stream_to_ast_html(&[
            tag("dl"),
            tag("dd"),
            tag("table"),
            txt("\n"),
            tag("tbody"),
            tag("tr"),
            tag("td"),
            txt("foo\n"),
            tag("p"),
            txt("bar"),
            end("p"),
            end("td"),
            end("tr"),
            end("tbody"),
            end("table"),
            end("dd"),
            end("dl"),
            txt("\n\n"),
            tag("dl"),
            tag("dd"),
            tag("dl"),
            tag("dd"),
            tag("dl"),
            tag("dd"),
            tag("table"),
            txt("\n"),
            tag("tbody"),
            tag("tr"),
            tag("td"),
            txt("foo\n"),
            tag("p"),
            txt("bar"),
            end("p"),
            end("td"),
            end("tr"),
            end("tbody"),
            end("table"),
            end("dd"),
            end("dl"),
            end("dd"),
            end("dl"),
            end("dd"),
            end("dl"),
        ]);
        fn count_tables(n: &Node) -> usize {
            let mut c = if matches!(&n.kind, NodeKind::Element(ElementKind::Table)) {
                1
            } else {
                0
            };
            for ch in &n.children {
                c += count_tables(ch);
            }
            c
        }
        assert_eq!(count_tables(&doc), 2, "expected 2 tables: {doc:?}");
    }

    #[test]
    fn test_div_roundtrip() {
        let doc = token_stream_to_ast_html(&[tag("div"), txt("foo"), end("div")]);
        assert!(contains_kind(&doc, &ElementKind::Div), "{doc:?}");
        assert!(contains_text(&doc, "foo"), "{doc:?}");
    }

    #[test]
    fn test_heading_roundtrip() {
        let doc = token_stream_to_ast_html(&[tag("h2"), txt("H2"), end("h2")]);
        assert!(contains_kind(&doc, &ElementKind::Heading(2)), "{doc:?}");
        assert!(contains_text(&doc, "H2"), "{doc:?}");
        // There should not be a spurious placeholder meta.
        assert!(!contains_data_parsoid_name(&doc, "h2"), "{doc:?}");
    }

    #[test]
    fn test_heading_via_stage() {
        // Run the heading tokens through the full TT3 stage, which is what the
        // Parser does, to see whether a stage handler introduces a placeholder.
        let stage = crate::pipeline::tree_builder_stage::TreeBuilderStage::new(false);
        let cfg = crate::mock::MockSiteConfig::new();
        let doc = stage.to_ast(vec![tag("h2"), txt("H2"), end("h2")], &cfg);
        assert!(contains_kind(&doc, &ElementKind::Heading(2)), "{doc:?}");
        assert!(!contains_data_parsoid_name(&doc, "h2"), "{doc:?}");
    }

    #[test]
    fn test_heading_end_tag_with_tsr() {
        // The real tokenizer gives the heading end tag a TSR; this must not
        // cause the tree builder to treat it as a stripped tag.
        let mut end_tag = EndTagTk::new("h2", vec![], DataParsoid::default());
        end_tag.data_parsoid.tsr = Some(crate::wikitext::tokens_v2::SourceRange::new(7, 9));
        let doc = token_stream_to_ast_html_with_source(
            &[
                tag("h2"),
                txt("H2"),
                Item::Tok(ParsoidToken::EndTag(end_tag)),
            ],
            Some("== H2 =="),
        );
        assert!(contains_kind(&doc, &ElementKind::Heading(2)), "{doc:?}");
        assert!(!contains_data_parsoid_name(&doc, "h2"), "{doc:?}");
    }

    fn contains_data_parsoid_name(node: &Node, name: &str) -> bool {
        if let Some(dp) = &node.data_parsoid
            && dp.contains(&format!("\"name\":\"{name}\""))
        {
            return true;
        }
        node.children
            .iter()
            .any(|c| contains_data_parsoid_name(c, name))
    }

    #[test]
    fn test_table_basic() {
        let doc = token_stream_to_ast_html(&[
            tag("table"),
            tag("tbody"),
            tag("tr"),
            tag("td"),
            txt("cell"),
            end("td"),
            end("tr"),
            end("tbody"),
            end("table"),
        ]);
        assert!(contains_kind(&doc, &ElementKind::Table), "{doc:?}");
        assert!(contains_kind(&doc, &ElementKind::TableRow), "{doc:?}");
        assert!(contains_kind(&doc, &ElementKind::TableCell), "{doc:?}");
        assert!(contains_text(&doc, "cell"), "{doc:?}");
    }

    #[test]
    fn test_data_parsoid_stash() {
        let dp = DataParsoid::with_tsr(0, 5);
        let mut meta = SelfclosingTagTk::new("meta", vec![], dp);
        meta.add_attribute_str("typeof", "mw:Transclusion");
        let items = vec![
            Item::Tok(ParsoidToken::Tag(TagTk::new(
                "p",
                vec![],
                DataParsoid::default(),
            ))),
            Item::Tok(ParsoidToken::SelfclosingTag(meta)),
            txt("x"),
            Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                "p",
                vec![],
                DataParsoid::default(),
            ))),
        ];
        let doc = token_stream_to_ast_html(&items);
        assert!(contains_data_parsoid(&doc), "{doc:?}");
    }

    #[test]
    fn test_placeholder_meta() {
        // A stripped end tag `</div>` carries its source and name into an
        // `mw:Placeholder/StrippedTag` meta, unless fostered out.
        let dp = DataParsoid {
            src: Some("</div>".to_string()),
            ..DataParsoid::default()
        };

        let mut builder = Html5TreeBuilder::with_source("");
        assert!(!builder.is_fosterable_position());
        builder.insert_placeholder_meta("div", &dp, false);
        let doc = builder.finalize();

        let placeholder = find_placeholder(&doc).expect("expected a placeholder meta");
        assert_eq!(
            placeholder.get_attr("typeof"),
            Some("mw:Placeholder/StrippedTag")
        );
        if let Some(dp_json) = &placeholder.data_parsoid {
            assert!(dp_json.contains("\"src\":\"</div>\""), "{dp_json}");
            assert!(dp_json.contains("\"name\":\"div\""), "{dp_json}");
        } else {
            panic!("placeholder missing data-parsoid");
        }
    }

    #[test]
    fn test_unmatched_pre_end_tag_dropped() {
        // An unmatched `</pre>` end tag is dropped (no placeholder), per the
        // `<nowiki> and <pre> preference` fixture (Parsoid drops unmatched
        // closing pre tags).
        let dp = DataParsoid {
            src: Some("</pre>".to_string()),
            stx: Some("html".to_string()),
            ..DataParsoid::default()
        };

        let mut builder = Html5TreeBuilder::with_source("");
        builder.insert_placeholder_meta("pre", &dp, false);
        let doc = builder.finalize();

        assert!(find_placeholder(&doc).is_none(), "got: {doc:?}");
    }

    #[test]
    fn test_stripped_end_tag_placeholder() {
        // `</foo>` (literal HTML, no matching open element) is stripped and
        // becomes an `mw:Placeholder/StrippedTag` meta with `src`/`name`.
        let dp = DataParsoid {
            stx: Some("html".to_string()),
            ..DataParsoid::default()
        };
        let items = vec![Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "foo",
            vec![],
            dp,
        )))];
        let doc = token_stream_to_ast_html(&items);

        let placeholder = find_placeholder(&doc).expect("expected a placeholder meta");
        assert_eq!(
            placeholder.get_attr("typeof"),
            Some("mw:Placeholder/StrippedTag")
        );
        if let Some(dp_json) = &placeholder.data_parsoid {
            assert!(dp_json.contains("\"name\":\"foo\""), "{dp_json}");
            assert!(dp_json.contains("\"src\":\"</foo>\""), "{dp_json}");
        } else {
            panic!("placeholder missing data-parsoid");
        }
    }

    #[test]
    fn test_dom_fragment_injection() {
        // A `mw:dom-fragment-token` carrying a pre-built sub-fragment must be
        // spliced into the tree by `UnpackDOMFragments` during finalize.
        let sub = Node::text("fragment-body");
        let mut fragments = HashMap::new();
        fragments.insert(7usize, sub);

        let mut frag_tok =
            SelfclosingTagTk::new("mw:dom-fragment-token", vec![], DataParsoid::default());
        frag_tok.attribs.push(crate::wikitext::tokens_v2::KV {
            key: crate::wikitext::tokens_v2::KeyValue::Str("data-fragment-id".to_string()),
            value: crate::wikitext::tokens_v2::KeyValue::Str("7".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });

        let items = vec![
            tag("pre"),
            Item::Tok(ParsoidToken::SelfclosingTag(frag_tok)),
            end("pre"),
        ];
        let mut doc = token_stream_to_ast_html_with_fragments(&items, None, fragments, None);
        // `finalize` no longer unpacks fragments; the full-page pipeline runs
        // `unpack_dom_fragments` after p-wrapping/encapsulation via
        // `post_pwrap_transforms`. Run the unpack here directly.
        crate::pipeline::unpack_dom_fragments::run(&mut doc);
        assert!(
            contains_text(&doc, "fragment-body"),
            "fragment not spliced: {doc:?}"
        );
    }

    #[test]
    fn test_pre_end_tag_no_placeholder() {
        // `<pre>foo</pre>` must produce a single `<pre>` element and no
        // spurious `mw:Placeholder/StrippedTag` meta for the matched end tag
        // (regression test for the `pop_all_up_to_name` peek-before-pop fix).
        let items = vec![tag("pre"), txt("foo"), end("pre")];
        let doc = token_stream_to_ast_html(&items);
        assert!(
            find_placeholder(&doc).is_none(),
            "unexpected placeholder: {doc:?}"
        );
        assert!(contains_kind(&doc, &ElementKind::Preformatted), "{doc:?}");
    }

    #[test]
    fn test_adoption_agency_misnested_bold() {
        // `<b>a<b>b</b>c</b>` exercises the adoption agency for the inner
        // `</b>`; it must terminate and preserve all text.
        let items = vec![
            tag("b"),
            txt("a"),
            tag("b"),
            txt("b"),
            end("b"),
            txt("c"),
            end("b"),
        ];
        let doc = token_stream_to_ast_html(&items);
        for needle in ["a", "b", "c"] {
            assert!(contains_text(&doc, needle), "missing {needle}: {doc:?}");
        }
    }

    #[test]
    fn test_resolve_data_ids_marks_afe_clone_auto_inserted() {
        // A stash id resolved by a second element is an AFE-reconstruction clone:
        // it must keep `stx` but drop `src`/`tsr` and gain `autoInsertedStart`.
        let mut stash = std::collections::HashMap::new();
        stash.insert(
            1usize,
            StashedNodeData {
                data_parsoid: Some(
                    "{\"src\":\"<code>\",\"tsr\":[0,6],\"stx\":\"html\"}".to_string(),
                ),
                dp: Some(crate::wikitext::tokens_v2::DataParsoid {
                    src: Some("<code>".to_string()),
                    tsr: Some(crate::wikitext::tokens_v2::SourceRange::new(0, 6)),
                    stx: Some("html".to_string()),
                    ..crate::wikitext::tokens_v2::DataParsoid::default()
                }),
                data_mw: None,
                fragment: None,
            },
        );

        let mut root = Node::element(ElementKind::Other("html".to_string()));
        let mut first = Node::element(ElementKind::Other("code".to_string()));
        first.set_attr("data-object-id", "1");
        let mut dup = Node::element(ElementKind::Other("code".to_string()));
        dup.set_attr("data-object-id", "1");
        root.push_child(first);
        root.push_child(dup);

        resolve_data_ids(&mut root, &stash, &mut std::collections::HashSet::new());

        // First element keeps the full data.
        assert_eq!(
            root.children[0].data_parsoid.as_deref(),
            Some("{\"src\":\"<code>\",\"tsr\":[0,6],\"stx\":\"html\"}")
        );
        // Duplicate (AFE clone) drops src/tsr and gains autoInsertedStart.
        let dp2 = root.children[1].dp.as_ref().expect("dp");
        assert!(dp2.auto_inserted_start, "should be autoInsertedStart");
        assert!(dp2.src.is_none(), "src should be dropped");
        assert!(dp2.tsr.is_none(), "tsr should be dropped");
        assert_eq!(dp2.stx.as_deref(), Some("html"), "stx should be preserved");
    }

    #[test]
    fn test_style_raw_text_content() {
        // `<style>p{}</style>` must be a raw-text element: the text `p{}` is
        // consumed as its content (not emitted as a separate text node), and
        // the `<style>` is not self-closing.
        let items = vec![tag("style"), txt("p{}"), end("style")];
        let doc = token_stream_to_ast_html(&items);
        // The style element must contain the text `p{}`.
        assert!(contains_text(&doc, "p{}"), "style content lost: {doc:?}");
    }

    #[test]
    fn test_strip_marker_metas() {
        let mut doc = Node::document();
        let mut pre = Node::element(ElementKind::Preformatted);
        let mut meta = Node::element(ElementKind::Other("meta".to_string()));
        meta.set_attr("typeof", "mw:IndentPreWS");
        pre.push_child(meta);
        pre.push_child(Node::text("asdf"));
        doc.push_child(pre);

        strip_marker_metas(&mut doc);

        let pre = &doc.children[0];
        assert_eq!(pre.children.len(), 1);
        assert!(matches!(&pre.children[0].kind, NodeKind::Text(s) if s == "asdf"));
    }

    #[test]
    fn test_strip_marker_metas_stripped_tag() {
        // A stray/unmatched closing tag (`</code>`) becomes a
        // `mw:Placeholder/StrippedTag` meta with no data-mw; it must be
        // stripped unless nested in a list item (T66025).
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        let mut meta = Node::element(ElementKind::Other("meta".to_string()));
        meta.set_attr("typeof", "mw:Placeholder/StrippedTag");
        meta.data_parsoid = Some("{\"name\":\"code\",\"src\":\"</code>\"}".to_string());
        p.push_child(meta);
        doc.push_child(p);

        strip_marker_metas(&mut doc);

        assert!(doc.children[0].children.is_empty(), "{:?}", doc.children[0]);
    }

    #[test]
    fn test_strip_marker_metas_stripped_tag_kept_in_list_item() {
        // The same marker nested inside a `<li>` must be kept (see
        // `ComputeDSR` width handling for markers that stay in the DOM).
        let mut doc = Node::document();
        let mut li = Node::element(ElementKind::ListItem);
        let mut meta = Node::element(ElementKind::Other("meta".to_string()));
        meta.set_attr("typeof", "mw:Placeholder/StrippedTag");
        li.push_child(meta);
        doc.push_child(li);

        strip_marker_metas(&mut doc);

        assert_eq!(doc.children[0].children.len(), 1, "{:?}", doc.children[0]);
    }

    #[test]
    fn test_strip_marker_metas_keeps_stripped_tag_with_data_mw() {
        // A StrippedTag marker carrying data-mw is a template artifact and
        // must be retained.
        let mut doc = Node::document();
        let mut meta = Node::element(ElementKind::Other("meta".to_string()));
        meta.set_attr("typeof", "mw:Placeholder/StrippedTag");
        meta.data_mw = Some("{\"name\":\"code\"}".to_string());
        doc.push_child(meta);

        strip_marker_metas(&mut doc);

        assert_eq!(doc.children.len(), 1, "{:?}", doc);
    }

    #[test]
    fn test_forward_transclusion_encapsulation_onto_list() {
        // `{{1x|*bar}}` post-ListHandler: the start marker meta directly
        // precedes a `<ul>` whose subtree holds the end marker meta. The
        // encapsulation must transfer `about`/`typeof` onto the `<ul>` and
        // drop both marker metas (the faithful forward case of
        // `wrap_flipped_children`).
        let mut start = Node::element(ElementKind::Other("meta".to_string()));
        start.set_attr("typeof", "mw:Transclusion");
        start.set_attr("about", "#mwt1");

        let mut end = Node::element(ElementKind::Other("meta".to_string()));
        end.set_attr("typeof", "mw:Transclusion/End");
        end.set_attr("about", "#mwt1");

        let mut li = Node::element(ElementKind::Other("li".to_string()));
        li.push_child(Node::text("bar"));
        li.push_child(end);
        let mut ul = Node::element(ElementKind::Other("ul".to_string()));
        ul.push_child(li);

        let mut doc = Node::document();
        doc.push_child(start);
        doc.push_child(ul);

        encapsulate_transclusions(&mut doc, None);

        // The `<meta>` start marker is gone; only the `<ul>` remains.
        assert_eq!(doc.children.len(), 1, "{doc:?}");
        let ul = &doc.children[0];
        assert_eq!(ul.get_attr("typeof"), Some("mw:Transclusion"));
        assert_eq!(ul.get_attr("about"), Some("#mwt1"));
        // The end marker meta inside `<li>` is removed; `bar` survives.
        assert!(contains_text(ul, "bar"), "{doc:?}");
        assert!(!contains_transclusion_end(ul), "{doc:?}");
    }

    fn contains_transclusion_end(node: &Node) -> bool {
        if node.get_attr("about") == Some("#mwt1") && is_transclusion_end(node) {
            return true;
        }
        node.children.iter().any(contains_transclusion_end)
    }

    #[test]
    fn test_flipped_range_adopts_following_table() {
        // The T322557 adoption scenario: the transclusion content is fostered
        // out of a `<table>` into a preceding `<p>`, and the (fostered) `<table>`
        // follows as a sibling sharing the transclusion's `dsr.start`. The range
        // merge must stamp `about` onto the `<table>` too, so both the `<p>` and
        // the `<table>` form a single `about` chain.
        let mut start = Node::element(ElementKind::Other("meta".to_string()));
        start.set_attr("typeof", "mw:Transclusion");
        start.set_attr("about", "#mwt2");
        start.data_parsoid = Some("{}".to_string());
        start.dp = Some(DataParsoid {
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(0),
                end: Some(38),
                open_width: None,
                close_width: None,
                ..Default::default()
            }),
            ..DataParsoid::default()
        });

        let mut end = Node::element(ElementKind::Other("meta".to_string()));
        end.set_attr("typeof", "mw:Transclusion/End");
        end.set_attr("about", "#mwt2");

        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("v"));
        p.push_child(end);

        let mut table = Node::element(ElementKind::Table);
        table.set_attr("about", "#mwt3");
        table.set_attr("typeof", "mw:ExpandedAttrs");
        table.dp = Some(DataParsoid {
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(0),
                end: Some(41),
                open_width: None,
                close_width: None,
                ..Default::default()
            }),
            ..DataParsoid::default()
        });

        let mut doc = Node::document();
        doc.push_child(start);
        doc.push_child(p);
        doc.push_child(table);

        encapsulate_transclusions(&mut doc, None);

        // Start marker is gone; both the `<p>` and the `<table>` remain and
        // share the transclusion `about` id.
        assert_eq!(doc.children.len(), 2, "{doc:?}");
        assert_eq!(doc.children[0].get_attr("about"), Some("#mwt2"));
        assert_eq!(doc.children[1].get_attr("about"), Some("#mwt2"));
    }

    #[test]
    fn test_empty_transclusion_encapsulated_as_span() {
        // `{{blank}}` (empty template) post-expansion is a pair of adjacent
        // transclusion marker metas with no content. Faithful Parsoid keeps this
        // as an empty `<span about=... typeof="mw:Transclusion">` (mirrors
        // `DOMRangeBuilder::findEnclosingRange`'s empty-content branch) rather
        // than dropping both markers entirely.
        let mut start = Node::element(ElementKind::Other("meta".to_string()));
        start.set_attr("typeof", "mw:Transclusion");
        start.set_attr("about", "#mwt1");
        start.data_parsoid = Some("{\"src\":\"{{blank}}\"}".to_string());

        let mut end = Node::element(ElementKind::Other("meta".to_string()));
        end.set_attr("typeof", "mw:Transclusion/End");
        end.set_attr("about", "#mwt1");

        let mut doc = Node::document();
        doc.push_child(start);
        doc.push_child(end);

        encapsulate_transclusions(&mut doc, None);

        // Both metas are gone, replaced by a single empty transclusion span.
        assert_eq!(doc.children.len(), 1, "{doc:?}");
        let span = &doc.children[0];
        assert_eq!(span.get_attr("about"), Some("#mwt1"));
        assert_eq!(span.get_attr("typeof"), Some("mw:Transclusion"));
        assert!(span.children.is_empty(), "{span:?}");
    }

    #[test]
    fn test_rendering_transparent_encap_target_wrapped_in_mw_empty_elt() {
        // `{{1x|[[Category:Foo]]}}` encapsulates a rendering-transparent
        // category `<link>`. The transclusion metadata must move onto a
        // `<span class="mw-empty-elt">` wrapper rather than directly onto the
        // `<link>` (mirrors PHP `handleRenderingTransparentEltsBetweenBlocks`).
        let mut start = Node::element(ElementKind::Other("meta".to_string()));
        start.set_attr("typeof", "mw:Transclusion");
        start.set_attr("about", "#mwt1");
        start.data_mw = Some("{\"parts\":[{}]}".to_string());

        let mut link = Node::element(ElementKind::Other("link".to_string()));
        link.set_attr("rel", "mw:PageProp/Category");
        link.set_attr("href", "./Category:Foo");

        let mut end = Node::element(ElementKind::Other("meta".to_string()));
        end.set_attr("typeof", "mw:Transclusion/End");
        end.set_attr("about", "#mwt1");

        let mut doc = Node::document();
        doc.push_child(start);
        doc.push_child(link);
        doc.push_child(end);

        encapsulate_transclusions(&mut doc, None);

        assert_eq!(doc.children.len(), 1, "{doc:?}");
        let span = &doc.children[0];
        assert_eq!(span.get_attr("class"), Some("mw-empty-elt"), "{span:?}");
        assert_eq!(span.get_attr("typeof"), Some("mw:Transclusion"), "{span:?}");
        assert_eq!(span.get_attr("about"), Some("#mwt1"), "{span:?}");
        // The category link is preserved inside the wrapper, minus the moved
        // about id.
        assert_eq!(span.children.len(), 1, "{span:?}");
        let inner = &span.children[0];
        assert_eq!(inner.get_attr("rel"), Some("mw:PageProp/Category"));
        assert_eq!(inner.get_attr("about"), None, "{inner:?}");
    }

    #[test]
    fn test_transclusion_trailing_newline_span_wrapped() {
        // A newline-only text inside a transclusion range (e.g. the trailing
        // `\n` of `{{1x|<div/>\n}}`) must be span-wrapped as a single-space
        // `about` span (WRAPPER flag), not dropped — mirrors PHP
        // `isDeletableNode` (a newline between a block and a following
        // non-list/table sibling is *not* deletable).
        let mut start = Node::element(ElementKind::Other("meta".to_string()));
        start.set_attr("typeof", "mw:Transclusion");
        start.set_attr("about", "#mwt1");

        let mut div = Node::element(ElementKind::Div);
        div.push_child(Node::text("x"));

        let mut end = Node::element(ElementKind::Other("meta".to_string()));
        end.set_attr("typeof", "mw:Transclusion/End");
        end.set_attr("about", "#mwt1");

        let mut doc = Node::document();
        doc.push_child(start);
        doc.push_child(div);
        doc.push_child(Node::text("\n"));
        doc.push_child(end);

        encapsulate_transclusions(&mut doc, None);

        // The trailing newline is preserved as a single-space wrapper span
        // (a sibling of the encapsulated div, since it is part of the same
        // transclusion range).
        assert_eq!(doc.children.len(), 2, "{doc:?}");
        let div = &doc.children[0];
        assert_eq!(div.get_attr("typeof"), Some("mw:Transclusion"), "{doc:?}");
        let wrapper = &doc.children[1];
        assert_eq!(wrapper.get_attr("about"), Some("#mwt1"), "{doc:?}");
        assert!(
            wrapper
                .data_parsoid
                .as_deref()
                .is_some_and(|d| d.contains("wrapper")),
            "{doc:?}"
        );
        assert_eq!(wrapper.children.len(), 1);
        assert!(matches!(
            &wrapper.children[0].kind,
            NodeKind::Text(t) if t == " "
        ));
    }

    #[test]
    fn test_is_deletable_in_range() {
        // A newline between a div and a table is deletable (T370751).
        assert!(is_deletable_in_range(
            &[
                Node::element(ElementKind::Div),
                Node::text("\n"),
                Node::element(ElementKind::Table),
            ],
            1,
            None
        ));

        // A newline between a div and a paragraph is NOT deletable.
        assert!(!is_deletable_in_range(
            &[
                Node::element(ElementKind::Div),
                Node::text("\n"),
                Node::element(ElementKind::Paragraph),
            ],
            1,
            None
        ));

        // A newline between two sol-transparent links is deletable (T407798).
        let mut l1 = Node::element(ElementKind::Other("link".to_string()));
        l1.set_attr("rel", "mw:PageProp/Category");
        let mut l2 = Node::element(ElementKind::Other("link".to_string()));
        l2.set_attr("rel", "mw:PageProp/Category");
        assert!(is_deletable_in_range(&[l1, Node::text("\n"), l2], 1, None));

        // Anything in a fosterable position is deletable outright — the tree
        // builder would already have fostered out any rendering-relevant
        // content, and `data-mw` captures the template's output.
        let tr = Node::element(ElementKind::TableRow);
        assert!(is_deletable_in_range(
            &[Node::element(ElementKind::Div), Node::text(" ")],
            1,
            Some(&tr)
        ));
    }

    #[test]
    fn test_parser_function_name() {
        // A v3 parser-function start marker carries a "parserfunction" parts
        // entry whose `target.key` is the function name.
        let mut start = Node::element(ElementKind::Other("meta".to_string()));
        start.data_mw =
            Some("{\"parts\":[{\"parserfunction\":{\"target\":{\"key\":\"if\"}}}]}".to_string());
        assert_eq!(parser_function_name(&start).as_deref(), Some("if"));

        // A v2 (old) parser-function marker uses "template", not "parserfunction".
        let mut v2 = Node::element(ElementKind::Other("meta".to_string()));
        v2.data_mw =
            Some("{\"parts\":[{\"template\":{\"target\":{\"function\":\"if\"}}}]}".to_string());
        assert_eq!(parser_function_name(&v2), None);

        // No data-mw at all → None.
        let empty = Node::element(ElementKind::Other("meta".to_string()));
        assert_eq!(parser_function_name(&empty), None);
    }

    fn find_placeholder(node: &Node) -> Option<&Node> {
        if node
            .get_attr("typeof")
            .map(|t| t == "mw:Placeholder/StrippedTag")
            .unwrap_or(false)
        {
            return Some(node);
        }
        node.children.iter().find_map(find_placeholder)
    }

    fn contains_text(node: &Node, needle: &str) -> bool {
        if let NodeKind::Text(t) = &node.kind
            && t == needle
        {
            return true;
        }
        node.children.iter().any(|c| contains_text(c, needle))
    }

    fn contains_kind(node: &Node, kind: &ElementKind) -> bool {
        if let NodeKind::Element(k) = &node.kind
            && k == kind
        {
            return true;
        }
        node.children.iter().any(|c| contains_kind(c, kind))
    }

    fn contains_data_parsoid(node: &Node) -> bool {
        if node.data_parsoid.is_some() {
            return true;
        }
        node.children.iter().any(contains_data_parsoid)
    }
}
