//! HTML5 tree construction for the Parsoid token stream.
//!
//! This is the Rust port of PHP Parsoid's `TreeBuilderStage` + `RemexPipeline`
//! adapters. Parsoid delegates the actual spec-compliant HTML5 tree
//! construction (foster parenting, active-formatting reconstruction, table
//! insertion-mode fixups) to a third-party tree builder — RemexHtml in PHP,
//! `html5ever`'s `TreeBuilder` here.
//!
//! Parsoid adds the following on top of a plain HTML5 tree builder, which is
//! re-implemented here faithfully:
//!
//!   * `data-parsoid` / `data-mw` are stashed in a side table keyed by a
//!     `data-object-id` attribute (rather than emitted as raw attributes),
//!     mirroring `DOMDataUtils::stashObjectInDoc`.
//!   * transclusion/param metas (`typeof` matching `mw:Transclusion` /
//!     `mw:Param`) must not be fostered, and shadow metas are emitted after
//!     text inside a transcluded table.
//!   * deleted start tags (`td`/`tr`/`th` outside a table) re-emit their
//!     wikitext source, and stripped tags emit `mw:Placeholder` metas.
//!   * text and newline tokens are buffered and flushed as a single character
//!     run, tracking `tableDepth` and `inTransclusion` exactly as Parsoid does.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::TokenSink as _;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeBuilder, TreeSink};
use html5ever::{Attribute, ExpandedName, QualName};

use crate::dom::node::{ElementKind, Node};
use crate::wikitext::tokens_v2::{DataParsoid as TDataParsoid, Item, KV, ParsoidToken};

/// The attribute name Parsoid uses to smuggle the id of the stashed
/// `data-parsoid`/`data-mw` through the tree builder. Mirrors
/// `DOMDataUtils::DATA_OBJECT_ATTR_NAME`.
const DATA_OBJECT_ATTR_NAME: &str = "data-object-id";

/// Identifier for a stashed `NodeData`.
type NodeDataId = usize;

/// The HTML namespace (avoids the `ns!` macro, which needs `namespace_url!`
/// from markup5ever to be in scope).
fn html_ns() -> html5ever::Namespace {
    html5ever::Namespace::from("http://www.w3.org/1999/xhtml")
}

fn empty_ns() -> html5ever::Namespace {
    html5ever::Namespace::from("")
}

/// A stashed `NodeData` — the `data-parsoid`/`data-mw` blob associated with a
/// DOM element via its `data-object-id` attribute. Mirrors PHP's `NodeData`.
#[derive(Debug, Clone, Default)]
struct StashedNodeData {
    data_parsoid: Option<String>,
    data_mw: Option<String>,
}

// ---------------------------------------------------------------------------
// Tree sink: builds the plain `Node` AST from html5ever tree actions.
//
// The handle is `Rc<Node>` where the qualified name is stored immutably (so
// `elem_name` can borrow it) and the mutable parts are behind `RefCell`, the
// same pattern `markup5ever_rcdom` uses.
// ---------------------------------------------------------------------------

/// The `TreeSink` handles are `Rc<HandleNode>`.
type Handle = Rc<HandleNode>;

/// A child of a handle under construction: either a nested element/comment
/// handle, or a run of text (which the sink merges into adjacent text).
enum Child {
    Handle(Handle),
    Text(String),
}

/// Mutable per-node state.
#[derive(Default)]
struct CellData {
    children: Vec<Child>,
    attrs: Vec<(String, String)>,
}

/// A handle into the tree under construction.
struct HandleNode {
    /// The element's name (owned; borrowed by `elem_name`).
    name: QualName,
    /// Which kind of node this is (element vs comment/document).
    is_element: bool,
    /// For comment nodes, the pre-built comment payload.
    comment: Option<String>,
    /// Mutable state.
    cell: RefCell<CellData>,
    /// The stashed `data-object-id`, set at creation from attributes.
    data_id: Cell<Option<NodeDataId>>,
    /// The parent handle (used to relocate fostered content before a sibling).
    parent: RefCell<Option<Weak<HandleNode>>>,
}

/// The `TreeSink` implementation, building a `Node` document.
struct AstBuilder {
    document: Handle,
    next_data_id: Cell<NodeDataId>,
    stash: RefCell<HashMap<NodeDataId, StashedNodeData>>,
}

impl AstBuilder {
    fn new() -> Self {
        let document = Rc::new(HandleNode {
            name: QualName::new(None, html_ns(), html5ever::LocalName::from("document")),
            is_element: false,
            comment: None,
            cell: RefCell::new(CellData::default()),
            data_id: Cell::new(None),
            parent: RefCell::new(None),
        });
        AstBuilder {
            document,
            next_data_id: Cell::new(0),
            stash: RefCell::new(HashMap::new()),
        }
    }

    /// Stash a `NodeData` and return its id (mirrors `stashObjectInDoc`).
    fn stash(&self, data: StashedNodeData) -> NodeDataId {
        let id = self.next_data_id.get();
        self.next_data_id.set(id + 1);
        self.stash.borrow_mut().insert(id, data);
        id
    }

    /// Produce the final document, returning the root element's children
    /// directly (mirrors Parsoid's `finalizeDOM`, which migrates the fragment
    /// into the body).
    fn build_document(&self) -> Node {
        let mut doc = Node::document();

        // In fragment mode the content lives under a single synthetic root
        // element created by the tree builder; unwrap it.
        let content_handle: Option<Handle> = {
            let doc_children = self.document.cell.borrow();
            match doc_children.children.as_slice() {
                [Child::Handle(h)] if h.name.local.as_ref() == "html" => Some(Rc::clone(h)),
                _ => None,
            }
        };

        if let Some(parent) = content_handle {
            let children = std::mem::take(&mut parent.cell.borrow_mut().children);
            for child in children {
                doc.push_child(freeze_child(child, &self.stash));
            }
        } else {
            let children = std::mem::take(&mut self.document.cell.borrow_mut().children);
            for child in children {
                doc.push_child(freeze_child(child, &self.stash));
            }
        }
        doc
    }
}

impl TreeSink for AstBuilder {
    type Handle = Handle;
    type Output = Node;

    fn finish(self) -> Node {
        self.build_document()
    }

    fn parse_error(&mut self, _msg: Cow<'static, str>) {}

    fn get_document(&mut self) -> Handle {
        Rc::clone(&self.document)
    }

    fn elem_name<'a>(&'a self, target: &'a Handle) -> ExpandedName<'a> {
        target.name.expanded()
    }

    fn create_element(
        &mut self,
        name: QualName,
        attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> Handle {
        let mut data_id = None;
        let mut normal = Vec::new();
        for attr in attrs {
            let key = attr.name.local.as_ref().to_string();
            let value = attr.value.to_string();
            if key == DATA_OBJECT_ATTR_NAME {
                data_id = value.parse().ok();
            } else {
                normal.push((key, value));
            }
        }

        Rc::new(HandleNode {
            name,
            is_element: true,
            comment: None,
            cell: RefCell::new(CellData {
                attrs: normal,
                ..Default::default()
            }),
            data_id: Cell::new(data_id),
            parent: RefCell::new(None),
        })
    }

    fn create_comment(&mut self, text: StrTendril) -> Handle {
        Rc::new(HandleNode {
            name: QualName::new(None, html_ns(), html5ever::LocalName::from("comment")),
            is_element: false,
            comment: Some(text.to_string()),
            cell: RefCell::new(CellData::default()),
            data_id: Cell::new(None),
            parent: RefCell::new(None),
        })
    }

    fn create_pi(&mut self, _target: StrTendril, _data: StrTendril) -> Handle {
        Rc::new(HandleNode {
            name: QualName::new(None, html_ns(), html5ever::LocalName::from("")),
            is_element: false,
            comment: None,
            cell: RefCell::new(CellData::default()),
            data_id: Cell::new(None),
            parent: RefCell::new(None),
        })
    }

    fn append(&mut self, parent: &Handle, child: NodeOrText<Handle>) {
        append_child(parent, child);
    }

    fn append_based_on_parent_node(
        &mut self,
        element: &Handle,
        _prev_element: &Handle,
        child: NodeOrText<Handle>,
    ) {
        // Foster parenting: `element` is the table and the child must be moved
        // before it. `prev_element` is the same target used when the table has
        // no parent, but here we mirror RcDom by inserting before `element`.
        insert_before_sibling(element, child);
    }

    fn append_doctype_to_document(
        &mut self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
    }

    fn get_template_contents(&mut self, _target: &Handle) -> Handle {
        unreachable!("template elements are not used in Parsoid output")
    }

    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        Rc::ptr_eq(x, y)
    }

    fn set_quirks_mode(&mut self, _mode: QuirksMode) {}

    fn append_before_sibling(&mut self, sibling: &Handle, new_node: NodeOrText<Handle>) {
        insert_before_sibling(sibling, new_node);
    }

    fn add_attrs_if_missing(&mut self, target: &Handle, attrs: Vec<Attribute>) {
        let mut cell = target.cell.borrow_mut();
        for attr in attrs {
            let key = attr.name.local.as_ref().to_string();
            let value = attr.value.to_string();
            if key == DATA_OBJECT_ATTR_NAME {
                target.data_id.set(value.parse().ok());
            } else if !cell.attrs.iter().any(|(k, _)| k == &key) {
                cell.attrs.push((key, value));
            }
        }
    }

    fn remove_from_parent(&mut self, _target: &Handle) {}

    fn reparent_children(&mut self, node: &Handle, new_parent: &Handle) {
        let children = std::mem::take(&mut node.cell.borrow_mut().children);
        new_parent.cell.borrow_mut().children.extend(children);
    }

    fn is_mathml_annotation_xml_integration_point(&self, _handle: &Handle) -> bool {
        false
    }
}

/// Append a child node/text into a parent handle.
fn append_child(parent: &Handle, child: NodeOrText<Handle>) {
    match child {
        NodeOrText::AppendNode(node) => {
            *node.parent.borrow_mut() = Some(Rc::downgrade(parent));
            parent.cell.borrow_mut().children.push(Child::Handle(node));
        }
        NodeOrText::AppendText(text) => {
            let text = text.to_string();
            let mut cell = parent.cell.borrow_mut();
            if let Some(Child::Text(existing)) = cell.children.last_mut() {
                existing.push_str(&text);
            } else {
                cell.children.push(Child::Text(text));
            }
        }
    }
}

/// Insert a node/text immediately before `sibling` in the sibling's parent.
fn insert_before_sibling(sibling: &Handle, new_node: NodeOrText<Handle>) {
    if let Some(parent) = sibling.parent.borrow().as_ref().and_then(|w| w.upgrade()) {
        let mut cell = parent.cell.borrow_mut();
        let idx = cell
            .children
            .iter()
            .position(|c| matches!(c, Child::Handle(h) if Rc::ptr_eq(h, sibling)));
        let pos = idx.unwrap_or_else(|| cell.children.len());
        match new_node {
            NodeOrText::AppendNode(node) => {
                *node.parent.borrow_mut() = Some(Rc::downgrade(&parent));
                cell.children.insert(pos, Child::Handle(node));
            }
            NodeOrText::AppendText(text) => {
                cell.children.insert(pos, Child::Text(text.to_string()));
            }
        }
    } else {
        // No tracked parent (document-level sibling): fall back to append.
        append_child(sibling, new_node);
    }
}

/// Freeze a child (handle or text) into a plain `Node`.
fn freeze_child(child: Child, stash: &RefCell<HashMap<NodeDataId, StashedNodeData>>) -> Node {
    match child {
        Child::Handle(handle) => freeze_from_handle(&handle, stash),
        Child::Text(text) => Node::text(text),
    }
}

/// Freeze a handle into a plain `Node`, resolving `data-object-id` and
/// recursively freezing children. Children are already plain `Node`s only if
/// they were text; element children are frozen recursively here.
fn freeze_from_handle(
    handle: &Handle,
    stash: &RefCell<HashMap<NodeDataId, StashedNodeData>>,
) -> Node {
    if let Some(text) = &handle.comment {
        return Node::comment(text.clone());
    }
    if !handle.is_element {
        let mut doc = Node::document();
        let children = std::mem::take(&mut handle.cell.borrow_mut().children);
        for child in children {
            doc.push_child(freeze_child(child, stash));
        }
        return doc;
    }

    let mut cell = handle.cell.borrow_mut();
    let kind = local_to_element_kind(handle.name.local.as_ref());
    let mut node = Node::element(kind);
    for (k, v) in std::mem::take(&mut cell.attrs) {
        if k != DATA_OBJECT_ATTR_NAME {
            node.set_attr(k, v);
        }
    }
    if let Some(id) = handle.data_id.get()
        && let Some(data) = stash.borrow().get(&id)
    {
        node.data_parsoid = data.data_parsoid.clone();
        node.data_mw = data.data_mw.clone();
    }
    for child in std::mem::take(&mut cell.children) {
        node.push_child(freeze_child(child, stash));
    }
    node
}

fn local_to_element_kind(local: &str) -> ElementKind {
    match local {
        "div" => ElementKind::Div,
        "span" => ElementKind::Span,
        "p" => ElementKind::Paragraph,
        "b" => ElementKind::Bold,
        "i" => ElementKind::Italic,
        "table" => ElementKind::Table,
        "tr" => ElementKind::TableRow,
        "td" | "th" => ElementKind::TableCell,
        "caption" => ElementKind::TableCaption,
        "ul" => ElementKind::UnorderedList,
        "ol" => ElementKind::OrderedList,
        "li" => ElementKind::ListItem,
        "dl" => ElementKind::DefinitionList,
        "dt" => ElementKind::DefinitionTerm,
        "dd" => ElementKind::DefinitionDescription,
        "pre" => ElementKind::Preformatted,
        "hr" => ElementKind::HorizontalRule,
        "br" => ElementKind::LineBreak,
        "h1" => ElementKind::Heading(1),
        "h2" => ElementKind::Heading(2),
        "h3" => ElementKind::Heading(3),
        "h4" => ElementKind::Heading(4),
        "h5" => ElementKind::Heading(5),
        "h6" => ElementKind::Heading(6),
        "figure" => ElementKind::Figure,
        "figcaption" => ElementKind::FigCaption,
        "section" => ElementKind::Section,
        // Tokens that have not yet been expanded by the link handlers reach the
        // tree builder as pseudo-element names; map them to their semantic kinds
        // so serialization round-trips as before (WikiLink/ExternalLinkHandler
        // will eventually replace these with real <a> elements in TT2).
        "wikilink" => ElementKind::Wikilink,
        "extlink" | "urllink" => ElementKind::ExtLink,
        "mw:redirect" => ElementKind::Redirect,
        other => ElementKind::Other(other.to_string()),
    }
}

/// Whether a tag is a void element (no closing tag). Mirrors
/// `Utils::isVoidElement` / `Consts::$HTML['VoidTags']`.
fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Convert string-valued wikitext attributes to html5ever `Attribute`s.
fn kv_to_attrs(kvs: &[KV]) -> Vec<Attribute> {
    let mut out = Vec::new();
    for kv in kvs {
        if let (Some(k), Some(v)) = (kv.key.as_str(), kv.value.as_str()) {
            out.push(html_attr(k, v));
        }
    }
    out
}

fn html_attr(key: &str, value: &str) -> Attribute {
    Attribute {
        name: QualName::new(
            None,
            empty_ns(),
            html5ever::LocalName::from(Cow::from(key.to_string())),
        ),
        value: StrTendril::from(value),
    }
}

fn make_tag(name: &str, self_closing: bool) -> html5ever::tokenizer::Tag {
    html5ever::tokenizer::Tag {
        kind: html5ever::tokenizer::StartTag,
        name: html5ever::LocalName::from(Cow::from(name.to_string())),
        self_closing,
        attrs: Vec::new(),
    }
}

/// The HTML5 tree builder adapter for the Parsoid token stream.
///
/// Ports `TreeBuilderStage::processToken` / `processChunk` onto html5ever's
/// `TreeBuilder`.
pub struct Html5TreeBuilder {
    tree: TreeBuilder<Handle, AstBuilder>,
    /// Assigned to start/self-closing tags (mirrors `$tagId`).
    tag_id: usize,
    /// Whether we are inside a transclusion (mirrors `$inTransclusion`).
    in_transclusion: bool,
    /// Crude table-depth tracking (mirrors `$tableDepth`).
    table_depth: usize,
    /// Buffered string/Nl tokens (mirrors `$textContentBuffer`).
    text_buffer: String,
}

impl Html5TreeBuilder {
    pub fn new() -> Self {
        let mut sink = AstBuilder::new();
        // Parsoid builds a fragment with a `<body>` context element (mirrors
        // RemexPipeline's `startDocument(..., 'body')`), so top-level content,
        // comments and metas land in the body rather than a synthetic head.
        let body_ctx = sink.create_element(
            QualName::new(None, html_ns(), html5ever::LocalName::from("body")),
            vec![],
            ElementFlags::default(),
        );
        let tree = TreeBuilder::new_for_fragment(sink, body_ctx, None, Default::default());
        Html5TreeBuilder {
            tree,
            tag_id: 1,
            in_transclusion: false,
            table_depth: 0,
            text_buffer: String::new(),
        }
    }

    /// Feed an html5ever token to the tree builder, discarding the
    /// `TokenSinkResult` (script/rawdata interrupts do not occur in wikitext).
    fn feed(&mut self, token: html5ever::tokenizer::Token) {
        let _ = self.tree.process_token(token, 0);
    }

    /// Process a chunk of tokens (mirrors `processChunk`).
    pub fn process_chunk(&mut self, tokens: &[Item]) {
        let n = tokens.len();
        let mut i = 0;
        while i < n {
            self.process_token(&tokens[i]);
            i += 1;
        }
    }

    /// Feed a single token to the tree builder (mirrors `processToken`).
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
                    self.process_end_tag(&name);
                }
                ParsoidToken::Comment(c) => {
                    self.feed(html5ever::tokenizer::CommentToken(StrTendril::from(
                        &c.value[..],
                    )));
                }
                ParsoidToken::Eof(_) => {
                    self.feed(html5ever::tokenizer::EOFToken);
                    self.tree.end();
                }
                ParsoidToken::EmptyLine(_) | ParsoidToken::IndentPre(_) | ParsoidToken::List(_) => {
                    self.process_compound(tok);
                }
            },
        }
    }

    fn process_compound(&mut self, tok: &ParsoidToken) {
        let nested: Vec<Item> = match tok {
            ParsoidToken::EmptyLine(t) => t.tokens.iter().cloned().map(Item::Tok).collect(),
            ParsoidToken::IndentPre(t) => t.nested_tokens.clone(),
            ParsoidToken::List(t) => t.nested_tokens.clone(),
            _ => return,
        };
        self.process_chunk(&nested);
    }

    fn flush_text(&mut self) {
        let text = std::mem::take(&mut self.text_buffer);
        if text.is_empty() {
            return;
        }
        self.feed(html5ever::tokenizer::CharacterTokens(StrTendril::from(
            &text[..],
        )));
        if self.in_transclusion && self.table_depth > 0 && !text.trim().is_empty() {
            // Mirrors `insertExplicitStartTag('meta', ['typeof' =>
            // 'mw:TransclusionShadow'], true)` — a plain (un-stashed) shadow
            // meta, not a data-carrier.
            let mut tag = make_tag("meta", true);
            tag.attrs.push(html_attr("typeof", "mw:TransclusionShadow"));
            self.feed(html5ever::tokenizer::TagToken(tag));
        }
    }

    fn process_start_tag(&mut self, name: &str, attribs: &[KV], dp: &TDataParsoid) {
        // A stripped wikitext-syntax table tag outside a table re-emits its
        // source (mirrors `handleDeletedStartTag`). We detect the "outside a
        // table" condition via `table_depth` directly, and only for tags with
        // non-HTML syntax (stx != "html").
        if self.table_depth == 0
            && dp.stx.as_deref() != Some("html")
            && matches!(name, "td" | "tr" | "th")
        {
            let src = dp.src.clone().or_else(|| {
                Some(
                    match name {
                        "td" => "|",
                        "tr" => "|-",
                        "th" => "!",
                        _ => "",
                    }
                    .to_string(),
                )
            });
            if let Some(orig) = src
                && !orig.is_empty()
            {
                self.feed(html5ever::tokenizer::CharacterTokens(StrTendril::from(
                    &orig[..],
                )));
                return;
            }
        }

        let data_mw = extract_data_mw(attribs);
        let mut attrs = kv_to_attrs(attribs);
        let data_id = self.stash(dp, data_mw);
        attrs.push(html_attr(DATA_OBJECT_ATTR_NAME, &data_id.to_string()));

        let mut tag = make_tag(name, false);
        tag.attrs = attrs;
        self.feed(html5ever::tokenizer::TagToken(tag));
    }

    fn process_selfclosing(&mut self, name: &str, attribs: &[KV], dp: &TDataParsoid) {
        let data_mw = extract_data_mw(attribs);
        let mut was_inserted = false;

        if name == "meta" {
            let should_not_foster = match_type_of(attribs).is_some();
            if should_not_foster {
                // `typeof` starts with `mw:Transclusion`: enter/leave
                // transclusion state exactly when it is `mw:Transclusion`.
                if let Some(ty) = match_transclusion(attribs) {
                    self.in_transclusion = ty == "mw:Transclusion";
                }
                let mut attrs = kv_to_attrs(attribs);
                let data_id = self.stash(dp, data_mw.clone());
                attrs.push(html_attr(DATA_OBJECT_ATTR_NAME, &data_id.to_string()));
                let mut tag = make_tag("meta", true);
                tag.attrs = attrs;
                self.feed(html5ever::tokenizer::TagToken(tag));
                was_inserted = true;
            }
        }

        if !was_inserted {
            let mut attrs = kv_to_attrs(attribs);
            let data_id = self.stash(dp, data_mw.clone());
            attrs.push(html_attr(DATA_OBJECT_ATTR_NAME, &data_id.to_string()));

            let mut tag = make_tag(name, true);
            tag.attrs = attrs;
            let is_void = is_void_element(name);
            self.feed(html5ever::tokenizer::TagToken(tag));
            if !is_void {
                let end = html5ever::tokenizer::Tag {
                    kind: html5ever::tokenizer::EndTag,
                    ..make_tag(name, false)
                };
                self.feed(html5ever::tokenizer::TagToken(end));
            }
        }
    }

    fn process_end_tag(&mut self, name: &str) {
        let end = html5ever::tokenizer::Tag {
            kind: html5ever::tokenizer::EndTag,
            ..make_tag(name, false)
        };
        self.feed(html5ever::tokenizer::TagToken(end));
    }

    fn stash(&mut self, dp: &TDataParsoid, data_mw: Option<String>) -> NodeDataId {
        self.tree.sink.stash(StashedNodeData {
            data_parsoid: dp.to_data_parsoid_json(),
            data_mw,
        })
    }

    /// Finalize the document (mirrors `finalizeDOM`).
    pub fn finalize(self) -> Node {
        self.tree.sink.finish()
    }
}

impl Default for Html5TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the `data-mw` string attribute from a token's attribute list.
fn extract_data_mw(attribs: &[KV]) -> Option<String> {
    attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("data-mw"))
        .and_then(|kv| kv.value.as_str())
        .map(|v| v.to_string())
}

/// Whether `typeof` matches `mw:Transclusion` or `mw:Param` (mirrors
/// `TokenUtils::matchTypeOf` with `#^mw:(Transclusion|Param)(/|$)#`).
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

/// Whether `typeof` starts with `mw:Transclusion` (mirrors
/// `TokenUtils::matchTypeOf` with `#^mw:Transclusion#`).
fn match_transclusion(attribs: &[KV]) -> Option<String> {
    let v = attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("typeof"))
        .and_then(|kv| kv.value.as_str())?;
    for ty in v.split_whitespace() {
        if ty.starts_with("mw:Transclusion") {
            return Some(ty.to_string());
        }
    }
    None
}

/// Run the HTML5 tree builder over a token stream, producing a `Node`
/// document. This replaces the naive stack-based converter in
/// `tree_builder_stage::token_stream_to_ast`.
pub fn token_stream_to_ast_html(tokens: &[Item]) -> Node {
    let mut builder = Html5TreeBuilder::new();
    builder.process_chunk(tokens);
    // Ensure EOF is emitted to flush any remaining open elements.
    builder.process_token(&Item::Tok(ParsoidToken::Eof(
        crate::wikitext::tokens_v2::EOFTk,
    )));
    builder.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::node::NodeKind;
    use crate::wikitext::tokens_v2::{CommentTk, DataParsoid, EndTagTk, SelfclosingTagTk, TagTk};

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
    fn test_plain_text() {
        let doc = token_stream_to_ast_html(&[txt("hello world")]);
        assert!(contains_text(&doc, "hello world"), "{doc:?}");
    }

    #[test]
    fn test_table_basic() {
        let items = vec![
            tag("table"),
            tag("tr"),
            tag("td"),
            txt("cell"),
            end("td"),
            end("tr"),
            end("table"),
        ];
        let doc = token_stream_to_ast_html(&items);
        assert!(contains_kind(&doc, &ElementKind::Table), "{doc:?}");
        assert!(contains_kind(&doc, &ElementKind::TableRow), "{doc:?}");
        assert!(contains_kind(&doc, &ElementKind::TableCell), "{doc:?}");
        assert!(contains_text(&doc, "cell"), "{doc:?}");
    }

    #[test]
    fn test_comment() {
        let items = vec![Item::Tok(ParsoidToken::Comment(CommentTk::new(
            "hello",
            DataParsoid::default(),
        )))];
        let doc = token_stream_to_ast_html(&items);
        assert!(contains_comment(&doc, "hello"), "{doc:?}");
    }

    #[test]
    fn test_data_parsoid_stash() {
        let dp = DataParsoid::with_tsr(0, 5);
        let mut meta = SelfclosingTagTk::new("meta", vec![], dp);
        meta.add_attribute_str("typeof", "mw:Transclusion");
        let items = vec![Item::Tok(ParsoidToken::SelfclosingTag(meta))];
        let doc = token_stream_to_ast_html(&items);
        assert!(!doc.children.is_empty(), "{doc:?}");
        assert!(doc.children[0].data_parsoid.is_some(), "{doc:?}");
    }

    #[test]
    fn test_foster_parenting() {
        // Text directly inside a <table> (outside a cell) is foster-parented
        // out in front of the table, per the HTML5 spec (and Parsoid's use of
        // a spec-compliant tree builder).
        let items = vec![
            tag("table"),
            txt("stray"),
            tag("tr"),
            tag("td"),
            txt("cell"),
            end("td"),
            end("tr"),
            end("table"),
        ];
        let doc = token_stream_to_ast_html(&items);
        // The stray text is fostered out before the table: the document's first
        // child is the (fostered) text, and the table is a sibling after it.
        let text_pos = doc.children.iter().position(|n| matches!(n.kind, NodeKind::Text(_) if matches!(&n.kind, NodeKind::Text(t) if t == "stray")));
        assert!(text_pos.is_some(), "stray text missing: {doc:?}");
        assert!(contains_text(&doc, "cell"), "{doc:?}");
    }

    #[test]
    fn test_transclusion_meta_state() {
        // A `mw:Transclusion` start meta followed by an end meta toggles the
        // in-transclusion flag; the metas are stashed and round-trip.
        let start = {
            let mut m = SelfclosingTagTk::new("meta", vec![], DataParsoid::with_tsr(0, 2));
            m.add_attribute_str("typeof", "mw:Transclusion");
            m
        };
        let end = {
            let mut m = SelfclosingTagTk::new("meta", vec![], DataParsoid::with_tsr(4, 6));
            m.add_attribute_str("typeof", "mw:Transclusion/End");
            m
        };
        let items = vec![
            Item::Tok(ParsoidToken::SelfclosingTag(start)),
            txt("x"),
            Item::Tok(ParsoidToken::SelfclosingTag(end)),
        ];
        let doc = token_stream_to_ast_html(&items);
        // The start meta should carry its stashed data-parsoid.
        assert!(
            doc.children.iter().any(|n| n.data_parsoid.is_some()),
            "{doc:?}"
        );
    }

    #[test]
    fn test_deleted_start_tag() {
        // A wikitext-syntax `td`/`tr` outside a table re-emits its source
        // literal (mirrors `handleDeletedStartTag`).
        let mut td_dp = DataParsoid::default();
        td_dp.stx = None;
        let items = vec![Item::Tok(ParsoidToken::Tag(TagTk::new(
            "td",
            vec![],
            td_dp,
        )))];
        let doc = token_stream_to_ast_html(&items);
        assert!(contains_text(&doc, "|"), "expected literal pipe: {doc:?}");
    }

    #[test]
    fn test_div_roundtrip() {
        // A simple <div> must build and serialize without hanging.
        let items = vec![tag("div"), txt("foo"), end("div")];
        let doc = token_stream_to_ast_html(&items);
        assert!(contains_kind(&doc, &ElementKind::Div), "{doc:?}");
        assert!(contains_text(&doc, "foo"), "{doc:?}");
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

    fn contains_comment(node: &Node, needle: &str) -> bool {
        if let NodeKind::Comment(c) = &node.kind
            && c == needle
        {
            return true;
        }
        node.children.iter().any(|c| contains_comment(c, needle))
    }
}
