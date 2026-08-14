//! Parsoid-compatible token types.
//!
//! These mirror the PHP Parsoid token hierarchy in `src/Tokens/`:
//! - `TagTk` — opening tag (e.g., `<p>`, `<b>`, `<h2>`, `<table>`)
//! - `EndTagTk` — closing tag (e.g., `</p>`, `</b>`, `</h2>`)
//! - `SelfclosingTagTk` — self-closing tag (e.g., `<br/>`, `<mw:redirect/>`)
//! - `CommentTk` — HTML comment
//! - `NlTk` — newline token
//! - `EOFTk` — end of file
//! - `EmptyLineTk` — empty line (with comments)
//!
//! Tokens can carry attribute key-value pairs (`KV`), source range info
//! (`SourceRange`), and `DataParsoid` / `DataMw` metadata.
//!
//! The tokenizer emits a flat array of `ParsoidToken | String`, where
//! strings represent plain text content.

use std::fmt;

/// A source range (analogous to PHP's SourceRange).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRange {
    pub start: usize,
    pub end: usize,
}

impl SourceRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn length(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Extract the substring from the source input.
    pub fn substr<'a>(&self, input: &'a str) -> &'a str {
        let end = self.end.min(input.len());
        let start = self.start.min(end);
        &input[start..end]
    }
}

/// A key-value pair for token attributes (analogous to PHP's KV).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KV {
    pub key: KeyValue,
    pub value: KeyValue,
    /// Source range covering the entire key=value (optional).
    pub src_offsets: Option<KVSourceRange>,
    /// Raw source string for the key (for round-tripping).
    pub ksrc: Option<String>,
    /// Raw source string for the value (for round-tripping).
    pub vsrc: Option<String>,
}

/// Value for a KV attribute key or value — can be a plain string or token array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyValue {
    Str(String),
    Tokens(Vec<ParsoidToken>),
}

impl KeyValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            KeyValue::Str(s) => Some(s),
            KeyValue::Tokens(_) => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            KeyValue::Str(s) => s.is_empty(),
            KeyValue::Tokens(t) => t.is_empty(),
        }
    }
}

impl fmt::Display for KeyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyValue::Str(s) => write!(f, "{s}"),
            KeyValue::Tokens(tokens) => {
                for t in tokens {
                    write!(f, "{t}")?;
                }
                Ok(())
            }
        }
    }
}

/// Source range for a key-value pair (analogous to PHP's KVSourceRange).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KVSourceRange {
    pub key_start: usize,
    pub key_end: usize,
    pub value_start: usize,
    pub value_end: usize,
}

/// DataParsoid — per-token metadata used for round-tripping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataParsoid {
    /// Token source range.
    pub tsr: Option<SourceRange>,
    /// Raw wikitext source for this token.
    pub src: Option<String>,
    /// Magic word source text.
    pub magic_src: Option<String>,
    /// Whether this is an auto-inserted start token.
    pub auto_inserted_start: bool,
    /// Whether this is an auto-inserted end token.
    pub auto_inserted_end: bool,
    /// Self-closing flag.
    pub self_close: Option<bool>,
    /// Whether the element has no explicit close tag.
    pub no_close: bool,
    /// Start tag source variation (e.g. `{|` vs default for table).
    pub start_tag_src: Option<String>,
    /// End tag source variation.
    pub end_tag_src: Option<String>,
    /// First pipe source for wikilinks.
    pub first_pipe_src: Option<String>,
    /// Style flag for table cells.
    pub stx: Option<String>,
    /// Extra dashes for horizontal rule.
    pub extra_dashes: Option<usize>,
    /// Source content for HTML entities.
    pub src_content: Option<String>,
    /// Whether the token represents an unclosed comment.
    pub unclosed_comment: Option<bool>,
    /// Whether the token has line content (for hr).
    pub line_content: Option<bool>,
    /// DOM fragment source range.
    pub dom_fragment_src: Option<String>,
    /// Extension tag offsets.
    pub ext_tag_offsets: Option<DomSourceRange>,
    /// Link token for redirects.
    pub link_tk: Option<Box<ParsoidToken>>,
}

impl DataParsoid {
    /// Create a DataParsoid with the given source range.
    pub fn with_tsr(start: usize, end: usize) -> Self {
        Self {
            tsr: Some(SourceRange::new(start, end)),
            ..Default::default()
        }
    }

    /// Create a DataParsoid with the given source range (already built).
    pub fn with_tsr_range(tsr: SourceRange) -> Self {
        Self {
            tsr: Some(tsr),
            ..Default::default()
        }
    }
}

/// DOM source range with additional metadata (analogous to PHP's DomSourceRange).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomSourceRange {
    pub start: usize,
    pub end: usize,
    pub open_width: usize,
    pub close_width: usize,
}

impl DomSourceRange {
    pub fn inner_start(&self) -> usize {
        self.start + self.open_width
    }

    pub fn inner_end(&self) -> usize {
        self.end - self.close_width
    }
}

/// DataMw — metadata about MediaWiki-specific attributes (analogous to PHP's DataMw).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataMw {
    /// Extension attributes map.
    pub attrs: Vec<(String, String)>,
    /// Source for include tags.
    pub src: Option<String>,
}

/// A single Parsoid token — mirrors the PHP token hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsoidToken {
    /// Opening tag: `<name attrs...>`
    Tag(TagTk),
    /// Closing tag: `</name>`
    EndTag(EndTagTk),
    /// Self-closing tag: `<name attrs.../>`
    SelfclosingTag(SelfclosingTagTk),
    /// HTML/XML comment: `<!-- ... -->`
    Comment(CommentTk),
    /// Newline token.
    Nl(NlTk),
    /// End of file marker.
    Eof(EOFTk),
    /// Empty line token (may contain comments).
    EmptyLine(EmptyLineTk),
    /// Indent-pre compound token (nested tokens incl. `<pre>`/`</pre>`).
    IndentPre(IndentPreTk),
    /// List compound token (nested tokens; `list_type` is ul/ol/dl).
    List(ListTk),
}

impl ParsoidToken {
    /// Return the token's name (tag name for TagTk/EndTagTk/SelfclosingTagTk, "" otherwise).
    pub fn get_name(&self) -> &str {
        match self {
            ParsoidToken::Tag(t) => &t.name,
            ParsoidToken::EndTag(t) => &t.name,
            ParsoidToken::SelfclosingTag(t) => &t.name,
            _ => "",
        }
    }

    /// Get a mutable reference to the DataParsoid if available.
    pub fn data_parsoid(&self) -> Option<&DataParsoid> {
        match self {
            ParsoidToken::Tag(t) => Some(&t.data_parsoid),
            ParsoidToken::EndTag(t) => Some(&t.data_parsoid),
            ParsoidToken::SelfclosingTag(t) => Some(&t.data_parsoid),
            ParsoidToken::Comment(t) => Some(&t.data_parsoid),
            ParsoidToken::Nl(t) => Some(&t.data_parsoid),
            ParsoidToken::EmptyLine(t) => Some(&t.data_parsoid),
            ParsoidToken::IndentPre(t) => Some(&t.data_parsoid),
            ParsoidToken::List(t) => Some(&t.data_parsoid),
            ParsoidToken::Eof(_) => None,
        }
    }

    /// Get a mutable reference to the DataParsoid if available.
    pub fn data_parsoid_mut(&mut self) -> Option<&mut DataParsoid> {
        match self {
            ParsoidToken::Tag(t) => Some(&mut t.data_parsoid),
            ParsoidToken::EndTag(t) => Some(&mut t.data_parsoid),
            ParsoidToken::SelfclosingTag(t) => Some(&mut t.data_parsoid),
            ParsoidToken::Comment(t) => Some(&mut t.data_parsoid),
            ParsoidToken::Nl(t) => Some(&mut t.data_parsoid),
            ParsoidToken::EmptyLine(t) => Some(&mut t.data_parsoid),
            ParsoidToken::IndentPre(t) => Some(&mut t.data_parsoid),
            ParsoidToken::List(t) => Some(&mut t.data_parsoid),
            ParsoidToken::Eof(_) => None,
        }
    }

    /// Get the DataMw if available.
    pub fn data_mw(&self) -> Option<&DataMw> {
        match self {
            ParsoidToken::Tag(t) => t.data_mw.as_ref(),
            ParsoidToken::SelfclosingTag(t) => t.data_mw.as_ref(),
            _ => None,
        }
    }

    /// Set the attributes on the token.
    pub fn set_attribs(&mut self, attrs: Vec<KV>) {
        match self {
            ParsoidToken::Tag(t) => t.attribs = attrs,
            ParsoidToken::EndTag(t) => t.attribs = attrs,
            ParsoidToken::SelfclosingTag(t) => t.attribs = attrs,
            _ => {}
        }
    }

    /// Get the attributes.
    pub fn get_attribs(&self) -> &[KV] {
        match self {
            ParsoidToken::Tag(t) => &t.attribs,
            ParsoidToken::EndTag(t) => &t.attribs,
            ParsoidToken::SelfclosingTag(t) => &t.attribs,
            _ => &[],
        }
    }

    /// Get the value of a named attribute.
    pub fn get_attribute(&self, name: &str) -> Option<&KeyValue> {
        self.get_attribs()
            .iter()
            .find(|kv| kv.key.as_str() == Some(name))
            .map(|kv| &kv.value)
    }

    /// Get the string value of a named attribute.
    pub fn get_attribute_v(&self, name: &str) -> Option<&str> {
        self.get_attribute(name).and_then(|v| v.as_str())
    }

    /// Get a named attribute KV (analogous to PHP's getAttributeKV).
    pub fn get_attribute_kv(&self, name: &str) -> Option<&crate::wikitext::tokens_v2::KV> {
        self.get_attribs()
            .iter()
            .find(|kv| kv.key.as_str() == Some(name))
    }

    /// Get a mutable named attribute KV.
    pub fn get_attribute_kv_mut(
        &mut self,
        name: &str,
    ) -> Option<&mut crate::wikitext::tokens_v2::KV> {
        match self {
            ParsoidToken::Tag(t) => t
                .attribs
                .iter_mut()
                .find(|kv| kv.key.as_str() == Some(name)),
            ParsoidToken::EndTag(t) => t
                .attribs
                .iter_mut()
                .find(|kv| kv.key.as_str() == Some(name)),
            ParsoidToken::SelfclosingTag(t) => t
                .attribs
                .iter_mut()
                .find(|kv| kv.key.as_str() == Some(name)),
            _ => None,
        }
    }

    /// Set (replace or append) a string attribute.
    pub fn set_attribute(&mut self, name: &str, value: &str) {
        if let Some(kv) = self.get_attribute_kv_mut(name) {
            kv.value = KeyValue::Str(value.to_string());
        } else {
            self.push_string_attr(name, value);
        }
    }

    /// Add a space-separated string attribute value (append if present).
    pub fn add_space_separated_attribute(&mut self, name: &str, value: &str) {
        if let Some(existing) = self.get_attribute_v(name).map(|v| v.to_string()) {
            let combined = format!("{existing} {value}");
            self.set_attribute(name, &combined);
        } else {
            self.push_string_attr(name, value);
        }
    }

    /// Remove a named attribute.
    pub fn remove_attribute(&mut self, name: &str) {
        match self {
            ParsoidToken::Tag(t) => t.attribs.retain(|kv| kv.key.as_str() != Some(name)),
            ParsoidToken::EndTag(t) => t.attribs.retain(|kv| kv.key.as_str() != Some(name)),
            ParsoidToken::SelfclosingTag(t) => t.attribs.retain(|kv| kv.key.as_str() != Some(name)),
            _ => {}
        }
    }

    /// Push a string attribute (internal helper).
    fn push_string_attr(&mut self, name: &str, value: &str) {
        match self {
            ParsoidToken::Tag(t) => t.add_attribute_str(name, value),
            ParsoidToken::EndTag(t) => t.add_attribute_str(name, value),
            ParsoidToken::SelfclosingTag(t) => t.add_attribute_str(name, value),
            _ => {}
        }
    }
}

impl fmt::Display for ParsoidToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParsoidToken::Tag(t) => write!(f, "<{}>", t.name),
            ParsoidToken::EndTag(t) => write!(f, "</{}>", t.name),
            ParsoidToken::SelfclosingTag(t) => write!(f, "<{}/>", t.name),
            ParsoidToken::Comment(_) => write!(f, "<!-- ... -->"),
            ParsoidToken::Nl(_) => write!(f, "\\n"),
            ParsoidToken::Eof(_) => write!(f, "EOF"),
            ParsoidToken::EmptyLine(_) => write!(f, "[empty-line]"),
            ParsoidToken::IndentPre(_) => write!(f, "[indent-pre]"),
            ParsoidToken::List(_) => write!(f, "[list]"),
        }
    }
}

// ---- Concrete token types ----

/// Opening tag token: `<name attrs...>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagTk {
    pub name: String,
    pub attribs: Vec<KV>,
    pub data_parsoid: DataParsoid,
    pub data_mw: Option<DataMw>,
}

impl TagTk {
    pub fn new(name: impl Into<String>, attribs: Vec<KV>, dp: DataParsoid) -> Self {
        Self {
            name: name.into(),
            attribs,
            data_parsoid: dp,
            data_mw: None,
        }
    }

    /// Add a string attribute.
    pub fn add_attribute_str(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attribs.push(KV {
            key: KeyValue::Str(key.into()),
            value: KeyValue::Str(value.into()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
    }
}

/// Closing tag token: `</name>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndTagTk {
    pub name: String,
    pub attribs: Vec<KV>,
    pub data_parsoid: DataParsoid,
}

impl EndTagTk {
    pub fn new(name: impl Into<String>, attribs: Vec<KV>, dp: DataParsoid) -> Self {
        Self {
            name: name.into(),
            attribs,
            data_parsoid: dp,
        }
    }

    /// Add a string attribute.
    pub fn add_attribute_str(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attribs.push(KV {
            key: KeyValue::Str(key.into()),
            value: KeyValue::Str(value.into()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
    }
}

/// Self-closing tag token: `<name attrs.../>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfclosingTagTk {
    pub name: String,
    pub attribs: Vec<KV>,
    pub data_parsoid: DataParsoid,
    pub data_mw: Option<DataMw>,
}

impl SelfclosingTagTk {
    pub fn new(name: impl Into<String>, attribs: Vec<KV>, dp: DataParsoid) -> Self {
        Self {
            name: name.into(),
            attribs,
            data_parsoid: dp,
            data_mw: None,
        }
    }

    /// Add a string attribute.
    pub fn add_attribute_str(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attribs.push(KV {
            key: KeyValue::Str(key.into()),
            value: KeyValue::Str(value.into()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
    }
}

/// Comment token: `<!-- ... -->`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentTk {
    pub value: String,
    pub data_parsoid: DataParsoid,
}

impl CommentTk {
    pub fn new(value: impl Into<String>, dp: DataParsoid) -> Self {
        Self {
            value: value.into(),
            data_parsoid: dp,
        }
    }
}

/// Newline token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NlTk {
    pub data_parsoid: DataParsoid,
}

impl NlTk {
    pub fn new(tsr: SourceRange) -> Self {
        Self {
            data_parsoid: DataParsoid::with_tsr_range(tsr),
        }
    }
}

/// End of file marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EOFTk;

/// Empty line token (possibly with comments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyLineTk {
    pub tokens: Vec<ParsoidToken>,
    pub data_parsoid: DataParsoid,
}

impl EmptyLineTk {
    pub fn new(tokens: Vec<ParsoidToken>, dp: DataParsoid) -> Self {
        Self {
            tokens,
            data_parsoid: dp,
        }
    }
}

/// Indent-pre compound token (nested tokens incl. `<pre>`/`</pre>`).
/// Analogous to PHP's `IndentPreTk` (which extends `CompoundTk`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndentPreTk {
    /// The nested tokens making up the indent-pre block.
    pub nested_tokens: Vec<Item>,
    pub data_parsoid: DataParsoid,
}

impl IndentPreTk {
    pub fn new() -> Self {
        Self {
            nested_tokens: Vec::new(),
            data_parsoid: DataParsoid::default(),
        }
    }

    /// Add a nested token.
    pub fn add_token(&mut self, token: Item) {
        self.nested_tokens.push(token);
    }

    /// Get the nested tokens.
    pub fn get_nested_tokens(&self) -> &[Item] {
        &self.nested_tokens
    }

    /// Set the nested tokens.
    pub fn set_nested_tokens(&mut self, tokens: Vec<Item>) {
        self.nested_tokens = tokens;
    }

    /// Does this token implicitly induce an end-of-line context?
    /// (True, per PHP `IndentPreTk::setsEOLContext()`.)
    pub fn sets_eol_context(&self) -> bool {
        true
    }
}

impl Default for IndentPreTk {
    fn default() -> Self {
        Self::new()
    }
}

/// List compound token (nested tokens; `list_type` is ul/ol/dl).
/// Analogous to PHP's `ListTk` (which extends `CompoundTk`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListTk {
    pub nested_tokens: Vec<Item>,
    pub data_parsoid: DataParsoid,
    pub list_type: Option<String>,
}

impl ListTk {
    pub fn new() -> Self {
        Self {
            nested_tokens: Vec::new(),
            data_parsoid: DataParsoid::default(),
            list_type: None,
        }
    }

    pub fn add_token(&mut self, token: Item) {
        self.nested_tokens.push(token);
    }

    pub fn add_tokens(&mut self, tokens: Vec<Item>) {
        self.nested_tokens.extend(tokens);
    }

    pub fn get_nested_tokens(&self) -> &[Item] {
        &self.nested_tokens
    }

    pub fn sets_eol_context(&self) -> bool {
        true
    }

    /// Is this a dl list containing only dd items (per PHP `isDLDDList`).
    pub fn is_dl_dd_list(&self) -> bool {
        if self.list_type.as_deref() != Some("dl") {
            return false;
        }
        let n = self.nested_tokens.len();
        if n == 0 {
            return false;
        }
        let mut i = 0;
        loop {
            // nested_tokens[i+1] must be a <dd>.
            let is_dd = matches!(
                self.nested_tokens.get(i + 1),
                Some(Item::Tok(ParsoidToken::Tag(t))) if t.name == "dd"
            );
            if !is_dd {
                return false;
            }
            i += 2;
            if i >= n {
                break;
            }
            let is_dl = matches!(
                self.nested_tokens.get(i),
                Some(Item::Tok(ParsoidToken::Tag(t))) if t.name == "dl"
            );
            if !is_dl {
                break;
            }
        }
        true
    }
}

impl Default for ListTk {
    fn default() -> Self {
        Self::new()
    }
}

/// A token stream item: either a plain text string or a ParsoidToken.
/// Used by compound tokens (IndentPre, List) to store nested tokens.
// `ParsoidToken` is large enough that boxing `Tok` would avoid bloat, but
// `Item` is part of the public API and boxing would ripple through the
// codebase, so we accept the size cost rather than breaking callers.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Str(String),
    Tok(ParsoidToken),
}

/// Helper: flatten a list of `ParsoidToken | String` into a single Vec.
pub fn flatten_token_list(
    items: Vec<Either<String, ParsoidToken>>,
) -> Vec<Either<String, ParsoidToken>> {
    items
}

/// An item in a token stream — either a token or a plain text string.
#[derive(Debug, Clone, PartialEq)]
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

impl<L: fmt::Display, R: fmt::Display> fmt::Display for Either<L, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Either::Left(l) => write!(f, "{l}"),
            Either::Right(r) => write!(f, "{r}"),
        }
    }
}

/// A chunk of output is a sequence of tokens and plain text strings.
pub type TokenChunk = Vec<Either<String, ParsoidToken>>;

/// Flatten a recursive array of token chunks (analogous to `TokenizerUtils::flattenIfArray`).
pub fn flatten_if_array(
    items: Vec<Either<String, ParsoidToken>>,
) -> Vec<Either<String, ParsoidToken>> {
    items
}

/// Flatten a list of strings and tokens into a single token chunk.
pub fn flatten_stringlist(
    items: Vec<Either<String, ParsoidToken>>,
) -> Vec<Either<String, ParsoidToken>> {
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tag_token() {
        let mut dp = DataParsoid::default();
        dp.tsr = Some(SourceRange::new(0, 3));
        let tag = TagTk::new("p", vec![], dp);
        let token = ParsoidToken::Tag(tag);
        assert_eq!(token.get_name(), "p");
    }

    #[test]
    fn test_selfclosing_token() {
        let mut dp = DataParsoid::default();
        dp.tsr = Some(SourceRange::new(0, 5));
        let tk = SelfclosingTagTk::new("br", vec![], dp);
        let token = ParsoidToken::SelfclosingTag(tk);
        assert_eq!(token.get_name(), "br");
    }

    #[test]
    fn test_kv_str() {
        let kv = KV {
            key: KeyValue::Str("href".to_string()),
            value: KeyValue::Str("/wiki/Foo".to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        };
        assert_eq!(kv.key.as_str(), Some("href"));
        assert_eq!(kv.value.as_str(), Some("/wiki/Foo"));
    }
}
