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
    fn stash_data_attribs(
        &mut self,
        attribs: &[KV],
        dp: &TDataParsoid,
        data_mw: Option<String>,
    ) -> Attributes {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for kv in attribs {
            if let (Some(k), Some(v)) = (kv.key.as_str(), kv.value.as_str())
                && k != "data-parsoid"
                && k != "data-mw"
                && k != DATA_OBJECT_ATTR_NAME
            {
                pairs.push((k.to_string(), v.to_string()));
            }
        }
        let id = self.stash(dp, data_mw);
        pairs.push((DATA_OBJECT_ATTR_NAME.to_string(), id.to_string()));
        Attributes::from_pairs(pairs)
    }

    /// Extract `data-mw` string attribute from a token's attribs.
    fn extract_data_mw(attribs: &[KV]) -> Option<String> {
        attribs
            .iter()
            .find(|kv| kv.key.as_str() == Some("data-mw"))
            .and_then(|kv| kv.value.as_str())
            .map(String::from)
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
            let attrs = self.stash_data_attribs(
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
        for token in tokens {
            self.process_token(token);
        }
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
        let attrs = self.stash_data_attribs(attribs, dp, data_mw);

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
        if inserted.is_none() {
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
        // Read the element's `data-object-id` (a regular attribute, added by
        // `stash_data_attribs`) from the stack while it's still there.
        let data_id = self
            .builder
            .stack
            .item_by_uid(uid)
            .and_then(|elt| elt.attrs.get(DATA_OBJECT_ATTR_NAME))
            .and_then(|v| v.parse::<usize>().ok());

        let Some(data_id) = data_id else {
            return;
        };
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
                let attrs = self.stash_data_attribs(attribs, dp, data_mw.clone());
                self.insert_unfostered_meta(attrs);
                was_inserted = true;
            }
        }

        if !was_inserted {
            let attrs = self.stash_data_attribs(attribs, dp, data_mw);
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
    pub fn finalize(self) -> Node {
        let mut doc = self.builder.handler.finish();
        resolve_data_ids(&mut doc, &self.stash);
        // Promote transient autoInsertedStart/EndToken flags to their persistent
        // final form (mirrors `TreeBuilderStage::processToken` end-tag branch).
        promote_auto_inserted_flags(&mut doc);
        // Remove empty auto-inserted elements (mirrors
        // `ProcessTreeBuilderFixups::removeAutoInsertedEmptyTags`, which runs
        // after tree building and before DOM-level p-wrapping).
        remove_auto_inserted_empty_tags(&mut doc);
        // Unpack `mw:DOMFragment` placeholders (extension/template sub-content)
        // into their stashed children. Mirrors PHP's `UnpackDOMFragments` DOM
        // handler, which runs after tree building.
        crate::pipeline::unpack_dom_fragments::run(&mut doc);
        // Encapsulate transclusion meta markers into wrapping `<span>` elements.
        encapsulate_transclusions(&mut doc);
        // A blank-line `<br>` (generated by ParagraphWrapper for two or more
        // source newlines) absorbs the newline that precedes it, so the rendered
        // newline lands *after* the `<br>` rather than before it. Mirrors the
        // DOM-level newline migration in Parsoid's `MigrateTrailingNLs`.
        migrate_br_newlines(&mut doc);
        // Strip internal marker metas (e.g. `<meta typeof="mw:IndentPreWS">`),
        // mirroring PHP's `CleanUp::stripMarkerMetas()` which runs after tree
        // building and before serialization.
        strip_marker_metas(&mut doc);
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

/// Walk the AST, resolving `data-object-id` attributes into stashed
/// `data-parsoid`/`data-mw`.
fn resolve_data_ids(node: &mut Node, stash: &HashMap<usize, StashedNodeData>) {
    let mut data_id: Option<usize> = None;
    for attr in &node.attrs {
        if attr.key == DATA_OBJECT_ATTR_NAME {
            data_id = attr.value.parse().ok();
        }
    }
    node.attrs.retain(|a| a.key != DATA_OBJECT_ATTR_NAME);
    if let Some(id) = data_id
        && let Some(data) = stash.get(&id)
    {
        node.data_parsoid = data.data_parsoid.clone();
        node.dp = data.dp.clone();
        node.data_mw = data.data_mw.clone();
        if let Some(fragment) = &data.fragment {
            node.fragment = Some(Box::new(fragment.clone()));
        }
    }
    for child in &mut node.children {
        resolve_data_ids(child, stash);
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

/// Remove marker metas (`<meta typeof="mw:IndentPreWS">`) from the AST.
/// These are internal bookkeeping placeholders inserted by the PreHandler;
/// PHP strips them in `CleanUp::stripMarkerMetas()`.
fn strip_marker_metas(node: &mut Node) {
    node.children.retain(|child| {
        let is_marker_meta = matches!(&child.kind, NodeKind::Element(kind) if {
            match kind {
                ElementKind::Other(name) => {
                    name == "meta"
                        && child
                            .attrs
                            .iter()
                            .any(|a| a.key == "typeof" && a.value == "mw:IndentPreWS")
                }
                _ => false,
            }
        });
        !is_marker_meta
    });
    for child in &mut node.children {
        strip_marker_metas(child);
    }
}

/// Whether a `data-parsoid` JSON property is present and truthy.
fn dp_bool(dp: &serde_json::Value, key: &str) -> bool {
    dp.get(key)
        .map(|v| v == &serde_json::Value::Bool(true))
        .unwrap_or(false)
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
fn encapsulate_transclusions(node: &mut Node) {
    // Recurse into element children first, then process direct children.
    for child in &mut node.children {
        if matches!(child.kind, NodeKind::Element(_)) {
            encapsulate_transclusions(child);
        }
    }

    let children = std::mem::take(&mut node.children);
    let children = wrap_transclusion_children(children);
    node.children = wrap_flipped_children(children);
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
fn wrap_transclusion_children(children: Vec<Node>) -> Vec<Node> {
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
        let content = wrap_transclusion_children(content);

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
                        // Whitespace-only: drop unless it separates two elements
                        // (marks a block boundary inside the transclusion).
                        let prev_is_elem =
                            idx > 0 && matches!(content[idx - 1].kind, NodeKind::Element(_));
                        let next_is_elem = idx + 1 < content.len()
                            && matches!(content[idx + 1].kind, NodeKind::Element(_));
                        if prev_is_elem && next_is_elem {
                            let mut span = Node::element(ElementKind::Span);
                            if let Some(about) = &about {
                                span.set_attr("about", about.clone());
                            }
                            span.push_child(Node::text(" "));
                            span.data_parsoid = Some("{\"tmp\":{\"wrapper\":true}}".to_string());
                            new_content.push(span);
                        }
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
            if let Some(typeof_) = &typeof_attr {
                let existing = new_content[et].get_attr("typeof").map(str::to_string);
                let merged = match existing {
                    Some(existing) if !existing.split_whitespace().any(|t| t == typeof_) => {
                        format!("{existing} {typeof_}")
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
            new_content[et].data_mw = start_meta.data_mw.clone();
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
            span.data_mw = start_meta.data_mw.clone();
            new_content.push(span);
        }

        out.extend(new_content);
        i = end + 1;
    }
    out
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
fn wrap_flipped_children(mut children: Vec<Node>) -> Vec<Node> {
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

        let Some(et) = encap_target else {
            i += 1;
            continue;
        };

        // Transfer encapsulation data onto the target element and drop the
        // end marker from its subtree.
        transfer_transclusion_to_element(&mut children[et], &start_meta);
        remove_end_meta(&mut children[et], about.as_deref());
        // Remove the start marker meta.
        children.remove(i);
        // Do not advance `i`: the next sibling shifted into this index.
    }
    children
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
fn transfer_transclusion_to_element(target: &mut Node, start_meta: &Node) {
    if let Some(about) = start_meta.get_attr("about") {
        target.set_attr("about", about);
    }
    if let Some(typeof_) = start_meta.get_attr("typeof") {
        target.set_attr("typeof", typeof_);
    }
    target.data_parsoid = start_meta.data_parsoid.clone();
    target.data_mw = start_meta.data_mw.clone();
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
        let doc = token_stream_to_ast_html_with_fragments(&items, None, fragments);
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

        encapsulate_transclusions(&mut doc);

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

        encapsulate_transclusions(&mut doc);

        // Both metas are gone, replaced by a single empty transclusion span.
        assert_eq!(doc.children.len(), 1, "{doc:?}");
        let span = &doc.children[0];
        assert_eq!(span.get_attr("about"), Some("#mwt1"));
        assert_eq!(span.get_attr("typeof"), Some("mw:Transclusion"));
        assert!(span.children.is_empty(), "{span:?}");
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
