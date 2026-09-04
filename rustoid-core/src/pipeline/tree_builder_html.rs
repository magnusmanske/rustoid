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
use crate::wikitext::tokens_v2::{DataParsoid as TDataParsoid, Item, KV, ParsoidToken};

/// The attribute name used to smuggle the stashed node-data id through the tree
/// builder (mirrors `DOMDataUtils::DATA_OBJECT_ATTR_NAME`).
const DATA_OBJECT_ATTR_NAME: &str = "data-object-id";

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

    /// Stash a pre-built sub-fragment node and return its id.
    fn stash_fragment(&mut self, fragment: Node) -> usize {
        let id = self.next_data_id;
        self.next_data_id += 1;
        self.stash.insert(
            id,
            StashedNodeData {
                data_parsoid: None,
                dp: None,
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
        let templated = data_mw
            .as_deref()
            .map(templated_attrib_keys)
            .unwrap_or_default();
        let mut pairs: Vec<(String, String)> = Vec::new();
        for kv in attribs {
            if let (Some(k), Some(v)) = (kv.key.as_str(), kv.value.as_str())
                && k != "data-parsoid"
                && k != "data-mw"
                && k != DATA_OBJECT_ATTR_NAME
                && k != "data-fragment-id"
                && !templated.contains(k)
            {
                pairs.push((k.to_string(), v.to_string()));
            }
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

    /// Insert a `<meta>` tag *unfostered* (mirrors `insertUnfosteredMeta` +
    /// `InHead::startTag`).
    fn insert_unfostered_meta(&mut self, attrs: Attributes) {
        self.dispatcher.flush_table_text(&mut self.builder);
        modes::start_tag(
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
        let data_mw = Self::extract_data_mw(attribs);
        let (attrs, data_id) = self.stash_data_attribs(attribs, dp, data_mw);

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
            && let Some(fragment) = self.fragments.remove(&fragment_id)
            && let Some(stashed) = self.stash.get_mut(&data_id)
        {
            stashed.fragment = Some(fragment);
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
            // the original page source.
            let orig_txt = if let Some(tsr) = &dp.tsr {
                tsr.substr(&self.source).to_string()
            } else {
                match name {
                    "td" => "|",
                    "tr" => "|-",
                    "th" => "!",
                    _ => "",
                }
                .to_string()
            };
            if !orig_txt.is_empty() {
                modes::characters(
                    &mut self.builder,
                    &mut self.dispatcher,
                    &orig_txt,
                    0,
                    orig_txt.len(),
                    0,
                    0,
                );
            }
        } else {
            self.insert_placeholder_meta(name, dp, true);
        }
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
            let id = self.stash_fragment(fragment.unwrap_or_else(Node::document));
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

/// Collect the plain-text attribute keys recorded in a `data-mw.attribs`
/// envelope, so [`Html5TreeBuilder::stash_data_attribs`] can drop those
/// templated attributes from the rendered element. Mirrors PHP's
/// `TreeBuilderStage` (which keeps only non-templated attributes on the DOM
/// node once `data-mw.attribs` captures the templated ones).
///
/// `data-mw.attribs` is a flat `[k, v]` pair list; each `k` is a plain string
/// or a `{ txt, html, uneditable }` object whose `txt` names the attribute.
fn templated_attrib_keys(data_mw: &str) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    let Ok(json) = serde_json::from_str::<serde_json::Value>(data_mw) else {
        return keys;
    };
    let Some(attribs) = json.get("attribs").and_then(|a| a.as_array()) else {
        return keys;
    };
    for pair in attribs {
        let Some(k) = pair.get(0) else { continue };
        match k {
            serde_json::Value::String(s) => {
                keys.insert(s.clone());
            }
            serde_json::Value::Object(obj) => {
                if let Some(txt) = obj.get("txt").and_then(|t| t.as_str()) {
                    keys.insert(txt.to_string());
                }
            }
            _ => {}
        }
    }
    keys
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

/// Merge a transclusion's `data-mw` (its `parts` envelope) with a target's own
/// `data-mw` (e.g. a media container's `attribs`/`errors`). The transclusion
/// object is the base; the target's non-`parts` keys (and any `attribs`) are
/// carried over so the compound object retains both the transclusion metadata
/// and the media options. Mirrors PHP's `DOMRangeBuilder` encapsulation, which
/// attaches the transclusion `parts` onto the target without discarding the
/// target's existing `data-mw` fields.
fn merge_encap_data_mw(encap: Option<String>, target: Option<String>) -> Option<String> {
    let encap: serde_json::Value = serde_json::from_str(encap.as_deref()?).ok()?;
    let target: serde_json::Value = match target {
        Some(t) => match serde_json::from_str::<serde_json::Value>(&t) {
            Ok(v) => v,
            Err(_) => return Some(encap.to_string()),
        },
        None => return Some(encap.to_string()),
    };

    let Some(mut obj) = encap.as_object().cloned() else {
        return Some(encap.to_string());
    };
    if let Some(tobj) = target.as_object() {
        for (k, v) in tobj {
            // `parts` is the transclusion's own envelope; every other key
            // (attribs, errors, caption, …) belongs to the target and must be
            // preserved.
            if k != "parts" {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    Some(serde_json::Value::Object(obj).to_string())
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
    let children = wrap_transclusion_children(children, source);
    node.children = wrap_flipped_children(children, source);
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
fn wrap_transclusion_children(children: Vec<Node>, source: Option<&str>) -> Vec<Node> {
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
        let content = wrap_transclusion_children(content, source);

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
                        if is_deletable_in_range(&content, idx) {
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
                    // encapsulation target if no earlier element exists.
                    let mut span = Node::element(ElementKind::Span);
                    if let Some(about) = &about {
                        span.set_attr("about", about.clone());
                    }
                    span.push_child(Node::text(s.clone()));
                    span.data_parsoid = Some("{\"tmp\":{\"wrapper\":true}}".to_string());
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

/// Whether a newline-only text node inside a transclusion range should be
/// deleted (rather than wrapped in a single-space `about` span). Faithful port
/// of PHP `DOMRangeBuilder::isDeletableNode` (minus the fosterable-position
/// case, which is handled by `fostered` flags elsewhere): a newline is
/// deletable when it separates a wikitext block node from a following wikitext
/// list/table, or when it sits between two sol-transparent links. Otherwise it
/// must be preserved (span-wrapped) so the range stays contiguous.
fn is_deletable_in_range(content: &[Node], idx: usize) -> bool {
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
fn wrap_flipped_children(mut children: Vec<Node>, source: Option<&str>) -> Vec<Node> {
    let mut i = 0;
    while i < children.len() {
        if !is_transclusion_start(&children[i]) {
            i += 1;
            continue;
        }

        let about: Option<String> = children[i].get_attr("about").map(str::to_string);

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

        let start_meta = children[i].clone();

        // Determine the contiguous sibling range [lo, hi] spanned by the
        // transclusion: from the start meta to the element holding the end
        // marker. Every element in that range gets the `about` id; the first
        // non-meta element becomes the encapsulation target.
        let (lo, hi) = if t < i { (t, i) } else { (i, t) };
        let mut encap_target = None;
        for (j, child) in children.iter_mut().enumerate().skip(lo).take(hi - lo + 1) {
            if matches!(child.kind, NodeKind::Element(_)) && !is_transclusion_start(child) {
                child.set_attr("about", start_meta.get_attr("about").unwrap_or(""));
                if encap_target.is_none() {
                    encap_target = Some(j);
                }
            }
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
        // the table *body* (`<tbody>`/`<thead>`/`<tfoot>`/`<tr>`), not the `<table>`.
        //
        // Drop the end marker (a `<table>` child, sibling of the `<tbody>`) *before*
        // transferring onto the nested body, so the two `&mut` borrows don't alias.
        remove_end_meta(&mut children[t], about.as_deref());
        {
            let encap_node = table_body_content_target(&mut children[et]);
            transfer_transclusion_to_element(encap_node, &start_meta, source, range_end);
        }
        // Remove the start marker meta.
        children.remove(i);
        // Do not advance `i`: the next sibling shifted into this index.
    }
    children
}

/// Resolve the actual encapsulation target for a fostered table range.
///
/// When the range start is a `<table>` whose content was fostered (PHP
/// `getStartConsideringFosteredContent` + `MAP_TBODY_TR`), the transclusion
/// encapsulation goes on the table *body* (`<tbody>`/`<thead>`/`<tfoot>`/`<tr>`)
/// rather than the `<table>` element itself. The body is the table's first
/// element child whose tag is a table-body/row tag.
fn table_body_content_target(table: &mut Node) -> &mut Node {
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
    &mut table.children[body_idx]
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
    token_stream_to_ast_html_with_fragments(tokens, source, HashMap::new())
}

/// Like [`token_stream_to_ast_html_with_source`], but accepts pre-built
/// sub-fragments keyed by id (for `mw:dom-fragment-token` placeholders).
pub fn token_stream_to_ast_html_with_fragments(
    tokens: &[Item],
    source: Option<&str>,
    fragments: HashMap<usize, Node>,
) -> Node {
    let mut builder = Html5TreeBuilder::with_source_and_fragments(source.unwrap_or(""), fragments);
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
    fn test_templated_attrib_keys() {
        // A `data-mw.attribs` envelope names the templated attributes (both as
        // plain strings and as `{ txt, html, uneditable }` objects) that must be
        // dropped from the rendered element's plain attributes.
        let data_mw = r#"{"attribs":[[{"txt":"a","html":"<span>x</span>"},{"html":""}],[{"txt":"k","html":"k","uneditable":true},""]]}"#;
        let keys = templated_attrib_keys(data_mw);
        assert!(keys.contains("a"), "{keys:?}");
        assert!(keys.contains("k"), "{keys:?}");
        assert!(!keys.contains("style"), "{keys:?}");

        // Plain-string keys are also collected.
        let data_mw = r#"{"attribs":[["style","color:red"],["title","hi"]]}"#;
        let keys = templated_attrib_keys(data_mw);
        assert!(keys.contains("style") && keys.contains("title"), "{keys:?}");
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
        let mut doc = token_stream_to_ast_html_with_fragments(&items, None, fragments);
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
            1
        ));

        // A newline between a div and a paragraph is NOT deletable.
        assert!(!is_deletable_in_range(
            &[
                Node::element(ElementKind::Div),
                Node::text("\n"),
                Node::element(ElementKind::Paragraph),
            ],
            1
        ));

        // A newline between two sol-transparent links is deletable (T407798).
        let mut l1 = Node::element(ElementKind::Other("link".to_string()));
        l1.set_attr("rel", "mw:PageProp/Category");
        let mut l2 = Node::element(ElementKind::Other("link".to_string()));
        l2.set_attr("rel", "mw:PageProp/Category");
        assert!(is_deletable_in_range(&[l1, Node::text("\n"), l2], 1));
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
