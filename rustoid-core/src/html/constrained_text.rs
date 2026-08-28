//! Constrained-text chunks for wikitext serialization (base classes).
//!
//! Faithful port of PHP Parsoid's
//! `src/Html2Wt/ConstrainedText/{ConstrainedText,State,Result}.php`. A
//! `ConstrainedText` is a chunk of output wikitext plus a pointer to the DOM
//! node that generated it, plus optional `prefix`/`suffix` escape strings that
//! subclasses supply to keep wikitext boundaries safe.
//!
//! The `escapeLine` entry point threads left/right context through a line of
//! chunks so each chunk can decide whether to emit `<nowiki>`-style escapes.

use crate::html::dom_tree::{DomTree, NodeId};
use crate::html::env::SerializerEnv;

/// The escape behavior of a [`ConstrainedText`] chunk, encoding which of PHP's
/// `ConstrainedText` subclasses the chunk instantiates. The `RegExpConstrainedText`
/// subclasses (`WikiLinkText`, `ExtLinkText`, `AutoURLLinkText`, `MagicLinkText`)
/// add `<nowiki/>` prefixes/suffixes when the surrounding context would merge
/// with the chunk to form a different token; the base `ConstrainedText`
/// (`Plain`) adds none.
#[derive(Debug, Clone)]
pub enum ConstrainedTextKind {
    /// Base `ConstrainedText` — no boundary escaping.
    Plain,
    /// `WikiLinkText` — `[[…]]` links, with optional link-prefix/trail guards.
    WikiLink {
        /// Match link trails greedily when present (consumes any link-prefix
        /// characters of an adjacent wikilink).
        greedy: bool,
        /// Compile `<nowiki/>` when the left context matches the bad-prefix
        /// (link-prefix characters immediately before the link).
        bad_prefix: Option<regex::Regex>,
        /// Compile `<nowiki/>` when the right context starts a link trail.
        bad_suffix: Option<regex::Regex>,
    },
    /// `ExtLinkText` — `[http://…]` links (no boundary escaping).
    ExtLink,
    /// `AutoURLLinkText` — bare `http://…` autolinks, with a word-boundary
    /// prefix guard and a trailing-punctuation suffix guard.
    AutoUrl { bad_prefix: regex::Regex },
    /// `MagicLinkText` — `RFC`/`ISBN`/`PMID` magic links, with `\w` guards on
    /// both sides.
    MagicLink {
        bad_prefix: regex::Regex,
        bad_suffix: regex::Regex,
    },
    /// `LanguageVariantText` — `-{ … }-` language-variant markup, with a `\|`
    /// bad-prefix guard (vertical bars immediately preceding cause problems in
    /// tables).
    LanguageVariant,
}

/// Result of escaping a single chunk: the (possibly escaped) text plus optional
/// prefix/suffix strings. Mirrors PHP's `ConstrainedText\Result`.
#[derive(Debug, Clone, Default)]
pub struct Result {
    pub text: String,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    /// Whether this chunk matches greedily (protects the left context from the
    /// next chunk's prefix).
    pub greedy: bool,
}

impl Result {
    pub fn new(text: impl Into<String>, prefix: Option<String>, suffix: Option<String>) -> Self {
        Self {
            text: text.into(),
            prefix,
            suffix,
            greedy: false,
        }
    }
}

/// Per-line escape context threaded through [`ConstrainedText::escape_line`].
/// Mirrors PHP's `ConstrainedText\State`.
#[derive(Debug, Clone)]
pub struct State {
    /// The fully-escaped text of chunks to the left (fed back as `leftContext`).
    pub left_context: String,
    /// The remaining raw text of chunks to the right.
    pub right_context: String,
    /// The position of the chunk currently being escaped.
    pub pos: usize,
}

impl State {
    pub fn new(line: &[ConstrainedText]) -> Self {
        let right_context: String = line.iter().map(|c| c.text.as_str()).collect();
        Self {
            left_context: String::new(),
            right_context,
            pos: 0,
        }
    }
}

/// A chunk of wikitext output. Mirrors PHP's `ConstrainedText\ConstrainedText`.
///
/// `node` is the `NodeId` (into the navigation `DomTree`) of the DOM node that
/// produced this chunk; PHP stores a `Node` reference directly, but our tree is
/// navigated by stable id.
#[derive(Debug, Clone)]
pub struct ConstrainedText {
    pub text: String,
    pub node: NodeId,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    /// The subclass that determines this chunk's `escape()` behavior.
    pub kind: ConstrainedTextKind,
    /// Whether this chunk came from selective serialization (selser).
    pub selser: bool,
    /// Suppress separator emission before this chunk.
    pub no_sep: bool,
}

impl ConstrainedText {
    pub fn new(
        text: impl Into<String>,
        node: NodeId,
        prefix: Option<String>,
        suffix: Option<String>,
    ) -> Self {
        Self {
            text: text.into(),
            node,
            prefix,
            suffix,
            kind: ConstrainedTextKind::Plain,
            selser: false,
            no_sep: false,
        }
    }

    /// Coerce `text` (already a `ConstrainedText`, or a plain string) into a
    /// `ConstrainedText`. Mirrors `ConstrainedText::cast`.
    pub fn cast(text: impl Into<String>, node: NodeId) -> Self {
        Self::new(text, node, None, None)
    }

    // ------------------------------------------------------------------
    // Concrete `ConstrainedText` subclasses (the terminal link emitters).
    // ------------------------------------------------------------------

    /// `WikiLinkText` — an `[[…]]` link. `greedy` is true when the link trail
    /// should be matched greedily. Faithful to `WikiLinkText::__construct`.
    pub fn wiki_link(
        text: impl Into<String>,
        node: NodeId,
        greedy: bool,
        bad_prefix: Option<regex::Regex>,
        bad_suffix: Option<regex::Regex>,
    ) -> Self {
        Self {
            text: text.into(),
            node,
            prefix: None,
            suffix: None,
            kind: ConstrainedTextKind::WikiLink {
                greedy,
                bad_prefix,
                bad_suffix,
            },
            selser: false,
            no_sep: false,
        }
    }

    /// `ExtLinkText` — an `[http://…]` link (no boundary escaping).
    pub fn ext_link(text: impl Into<String>, node: NodeId) -> Self {
        Self {
            text: text.into(),
            node,
            prefix: None,
            suffix: None,
            kind: ConstrainedTextKind::ExtLink,
            selser: false,
            no_sep: false,
        }
    }

    /// `AutoURLLinkText` — a bare `http://…` autolink. The trailing-punctuation
    /// suffix guard is computed from the URL (an open paren changes the set that
    /// would be absorbed into the autolink).
    pub fn auto_url_link(text: impl Into<String>, node: NodeId) -> Self {
        Self {
            text: text.into(),
            node,
            prefix: None,
            suffix: None,
            kind: ConstrainedTextKind::AutoUrl {
                bad_prefix: regex::Regex::new(r"\w$").unwrap(),
            },
            selser: false,
            no_sep: false,
        }
    }

    /// `MagicLinkText` — an `RFC`/`ISBN`/`PMID` magic link.
    pub fn magic_link(text: impl Into<String>, node: NodeId) -> Self {
        Self {
            text: text.into(),
            node,
            prefix: None,
            suffix: None,
            kind: ConstrainedTextKind::MagicLink {
                bad_prefix: regex::Regex::new(r"\w$").unwrap(),
                bad_suffix: regex::Regex::new(r"^\w").unwrap(),
            },
            selser: false,
            no_sep: false,
        }
    }

    /// `LanguageVariantText` — a `-{ … }-` language-variant chunk, with a `|`
    /// bad-prefix guard (the `badPrefix` is `^\|$`, matching a bare vertical bar
    /// as the left context).
    pub fn language_variant(text: impl Into<String>, node: NodeId) -> Self {
        Self {
            text: text.into(),
            node,
            prefix: None,
            suffix: None,
            kind: ConstrainedTextKind::LanguageVariant,
            selser: false,
            no_sep: false,
        }
    }

    /// Determine the escape prefix/suffix for this chunk given the line context.
    /// Faithful to the `ConstrainedText::escape` override of each subclass.
    pub fn escape(&self, state: &State) -> Result {
        let mut result = Result::new(self.text.clone(), self.prefix.clone(), self.suffix.clone());
        match &self.kind {
            ConstrainedTextKind::Plain => {}
            ConstrainedTextKind::ExtLink => {}
            ConstrainedTextKind::WikiLink {
                bad_prefix,
                bad_suffix,
                greedy,
                ..
            } => {
                if let Some(re) = bad_prefix
                    && re.is_match(&state.left_context)
                {
                    result.prefix = Some("<nowiki/>".to_string());
                }
                if let Some(re) = bad_suffix
                    && re.is_match(&state.right_context)
                {
                    result.suffix = Some("<nowiki/>".to_string());
                }
                result.greedy = *greedy;
            }
            ConstrainedTextKind::AutoUrl { bad_prefix, .. } => {
                // `RegExpConstrainedText::escape`: prefix guard on the left.
                if bad_prefix.is_match(&state.left_context) {
                    result.prefix = Some("<nowiki/>".to_string());
                }
                // Suffix guard is computed from the URL (paren presence) — see
                // `AutoURLLinkText::badSuffix`; the trailing-punctuation set is
                // tested against the right context below.
                if auto_url_bad_suffix_matches(&self.text, &state.right_context) {
                    result.suffix = Some("<nowiki/>".to_string());
                }
                // `escape()` special case: if the text ends with an incomplete
                // entity and the right context completes it, protect the suffix.
                if result.suffix.is_none()
                    && regex::Regex::new(r"&[#0-9a-zA-Z]*$")
                        .unwrap()
                        .is_match(&result.text)
                    && regex::Regex::new(r"^[#0-9a-zA-Z]*;")
                        .unwrap()
                        .is_match(&state.right_context)
                {
                    result.suffix = Some("<nowiki/>".to_string());
                }
            }
            ConstrainedTextKind::MagicLink {
                bad_prefix,
                bad_suffix,
            } => {
                if bad_prefix.is_match(&state.left_context) {
                    result.prefix = Some("<nowiki/>".to_string());
                }
                if bad_suffix.is_match(&state.right_context) {
                    result.suffix = Some("<nowiki/>".to_string());
                }
            }
            ConstrainedTextKind::LanguageVariant => {
                // `LanguageVariantText` badPrefix is `/^\|$/D`: a bare vertical
                // bar immediately preceding the variant.
                if state.left_context == "|" {
                    result.prefix = Some("<nowiki/>".to_string());
                }
            }
        }
        result
    }

    pub fn equals(&self, other: &ConstrainedText) -> bool {
        self.text == other.text
    }

    /// Escape a line of chunks, threading left/right context so each chunk can
    /// insert `prefix`/`suffix` boundary escapes. Mirrors `escapeLine`.
    pub fn escape_line(line: &[ConstrainedText]) -> String {
        let mut state = State::new(line);
        let mut safe_left = String::new();
        while state.pos < line.len() {
            let chunk = &line[state.pos];
            // Drop this chunk's raw text from the right context.
            state.right_context = state
                .right_context
                .chars()
                .skip(chunk.text.chars().count())
                .collect();
            let escaped = chunk.escape(&state);
            if let Some(prefix) = &escaped.prefix {
                state.left_context.push_str(prefix);
            }
            state.left_context.push_str(&escaped.text);
            if let Some(suffix) = &escaped.suffix {
                state.left_context.push_str(suffix);
            }
            if escaped.greedy {
                safe_left.push_str(&state.left_context);
                state.left_context.clear();
            }
            state.pos += 1;
        }
        safe_left.push_str(&state.left_context);
        safe_left
    }
}

/// Options for [`from_sel_ser`], mirroring the `$opts` array passed to PHP's
/// `ConstrainedText::fromSelSer` (`ignorePrefix`/`ignoreSuffix`).
#[derive(Debug, Clone, Copy, Default)]
pub struct FromSelSerOpts {
    pub ignore_prefix: bool,
    pub ignore_suffix: bool,
}

/// SelSer support: partition an unmodified node's wikitext (`text`) into
/// `ConstrainedText` chunks, deferring the leftmost/rightmost child to the
/// specialized subclass (wikilink/extlink/autourl/magiclink/language-variant)
/// when its DSR bounds coincide with the node's, so boundary escapes are
/// preserved. Faithful to `ConstrainedText::fromSelSer` + `fromSelSerImpl`.
///
/// Operates on the `DomTree`/`NodeId` navigation arena so the per-child
/// recursion can look up the first/last non-deleted child by id.
pub fn from_sel_ser(
    tree: &DomTree,
    id: NodeId,
    text: &str,
    env: Option<SerializerEnv>,
    opts: FromSelSerOpts,
) -> Vec<ConstrainedText> {
    // Main dispatch: try each concrete subclass (backwards, so subtypes are
    // checked before the base `ConstrainedText` itself), then fall through to
    // the base `fromSelSerImpl` which handles everything that reaches it.
    for kind in [
        ConstrainedTextKind::LanguageVariant,
        ConstrainedTextKind::MagicLink {
            bad_prefix: regex::Regex::new(r"\w$").unwrap(),
            bad_suffix: regex::Regex::new(r"^\w").unwrap(),
        },
        ConstrainedTextKind::AutoUrl {
            bad_prefix: regex::Regex::new(r"\w$").unwrap(),
        },
        ConstrainedTextKind::ExtLink,
        ConstrainedTextKind::WikiLink {
            greedy: false,
            bad_prefix: None,
            bad_suffix: None,
        },
    ] {
        if let Some(chunks) = from_sel_ser_impl_kind(tree, id, text, env, &kind) {
            // Tag these chunks as coming from selser.
            return chunks;
        }
    }
    // Base case (should never be reached, but be non-panicking).
    from_sel_ser_impl_base(tree, id, text, env, opts)
}

/// Dispatch to the relevant `fromSelSerImpl` for a concrete `kind`.
fn from_sel_ser_impl_kind(
    tree: &DomTree,
    id: NodeId,
    text: &str,
    env: Option<SerializerEnv>,
    kind: &ConstrainedTextKind,
) -> Option<Vec<ConstrainedText>> {
    let node = tree.node(id);
    let dp = node.dp.as_ref();
    let stx = dp.and_then(|d| d.stx.as_deref()).unwrap_or("");
    let name = crate::html::wts_utils::node_name(node);
    let rel = node.get_attr("rel");

    let chunk = match kind {
        ConstrainedTextKind::LanguageVariant => {
            if crate::html::dom_utils::has_type_of(node, "mw:LanguageVariant") {
                ConstrainedText::language_variant(text, id)
            } else {
                return None;
            }
        }
        ConstrainedTextKind::MagicLink { .. } => {
            if stx == "magiclink" {
                ConstrainedText::magic_link(text, id)
            } else {
                return None;
            }
        }
        ConstrainedTextKind::AutoUrl { .. } => {
            let link_type = dp.and_then(|d| d.link_type.as_deref());
            if (name == "a" && stx == "url") || (name == "img" && link_type == Some("extlink")) {
                ConstrainedText::auto_url_link(text, id)
            } else {
                return None;
            }
        }
        ConstrainedTextKind::ExtLink => {
            if crate::html::dom_utils::has_rel(node, "mw:ExtLink")
                && stx != "simple"
                && stx != "piped"
            {
                ConstrainedText::ext_link(text, id)
            } else {
                return None;
            }
        }
        ConstrainedTextKind::WikiLink { .. } => {
            let is_wiki_rel = rel
                .map(|r| {
                    r.split_whitespace()
                        .any(|t| t == "mw:WikiLink" || t == "mw:WikiLink/Interwiki")
                })
                .unwrap_or(false);
            if is_wiki_rel && (stx == "simple" || stx == "piped") {
                // Build the WikiLink chunk with its link-prefix/trail guards.
                from_wiki_link_chunk(tree, id, text, env)
            } else {
                return None;
            }
        }
        ConstrainedTextKind::Plain => {
            return None;
        }
    };

    let mut ct = chunk;
    ct.selser = true;
    Some(vec![ct])
}

/// Construct the `WikiLinkText` chunk, faithfully replicating its constructor's
/// `noTrails`/`badPrefix`/`badSuffix`/`greedy` logic.
fn from_wiki_link_chunk(
    tree: &DomTree,
    id: NodeId,
    text: &str,
    env: Option<SerializerEnv>,
) -> ConstrainedText {
    let node = tree.node(id);
    let rel = node.get_attr("rel").unwrap_or("");
    // category links/external links/images don't use link trails or prefixes
    let no_trails = rel
        .split_whitespace()
        .all(|t| t != "mw:WikiLink" && t != "mw:WikiLink/Interwiki");

    let link_prefix_regex = env.and_then(|e| e.get_site_config().link_prefix_regex());
    let link_trail_regex = env.and_then(|e| e.get_site_config().link_trail_regex());

    // Default bad prefix: `(^|[^\[])(\[\[)*\[$`.
    let default_bad_prefix = r"(^|[^\[])(\[\[)*\[$";
    let bad_prefix = if !no_trails && let Some(lp) = link_prefix_regex {
        // PHP combines the link-prefix regex with the default via alternation:
        // `(linkPrefixRegex)|((^|[^\[])(\[\[)*\[$)`.
        regex::Regex::new(&format!("({lp})|({default_bad_prefix})")).unwrap()
    } else {
        regex::Regex::new(default_bad_prefix).unwrap()
    };
    let bad_suffix = if no_trails {
        None
    } else {
        link_trail_regex.and_then(|lt| regex::Regex::new(lt).ok())
    };
    let greedy = !(no_trails || text.ends_with(']'));

    ConstrainedText {
        text: text.to_string(),
        node: id,
        prefix: None,
        suffix: None,
        kind: ConstrainedTextKind::WikiLink {
            greedy,
            bad_prefix: Some(bad_prefix),
            bad_suffix,
        },
        selser: true,
        no_sep: false,
    }
}

/// The base-case `fromSelSerImpl`: partition the text around the leftmost/
/// rightmost non-deleted children whose DSR bounds coincide with the node's,
/// tagging the whole result as selser chunks with `noSep` on all but the first.
fn from_sel_ser_impl_base(
    tree: &DomTree,
    id: NodeId,
    text: &str,
    env: Option<SerializerEnv>,
    opts: FromSelSerOpts,
) -> Vec<ConstrainedText> {
    let node = tree.node(id);
    let node_dsr = crate::html::wts_utils::get_dsr(node);
    let mut text = text.to_string();
    let mut prefix_chunks: Vec<ConstrainedText> = Vec::new();
    let mut suffix_chunks: Vec<ConstrainedText> = Vec::new();

    // First non-deleted child.
    let first_child = crate::html::dom_tree::first_non_deleted_child(tree, id);
    let last_child = crate::html::dom_tree::last_non_deleted_child(tree, id);

    if !opts.ignore_prefix
        && let Some(fc) = first_child
        && let Some(fc_dsr) = crate::html::wts_utils::get_dsr(tree.node(fc))
        && crate::html::dsr::is_valid_dsr(Some(&fc_dsr), false)
        && node_dsr.as_ref().and_then(|d| d.start) == fc_dsr.start
    {
        let len = fc_dsr.length();
        if len <= text.len() {
            prefix_chunks = from_sel_ser(
                tree,
                fc,
                &text[..len],
                env,
                FromSelSerOpts {
                    ignore_suffix: true,
                    ignore_prefix: false,
                },
            );
            text = text[len..].to_string();
        }
    }

    if !opts.ignore_suffix
        && let (Some(fc), Some(lc)) = (first_child, last_child)
        && fc != lc
        && let Some(lc_dsr) = crate::html::wts_utils::get_dsr(tree.node(lc))
        && crate::html::dsr::is_valid_dsr(Some(&lc_dsr), false)
        && node_dsr.as_ref().and_then(|d| d.end) == lc_dsr.end
    {
        let len = lc_dsr.length();
        if len <= text.len() {
            suffix_chunks = from_sel_ser(
                tree,
                lc,
                &text[text.len() - len..],
                env,
                FromSelSerOpts {
                    ignore_prefix: true,
                    ignore_suffix: false,
                },
            );
            text = text[..text.len() - len].to_string();
        }
    }

    // Glue together prefixChunks, the middle text, and suffixChunks.
    let mut chunks: Vec<ConstrainedText> = prefix_chunks;
    chunks.push(ConstrainedText::cast(text, id));
    chunks.extend(suffix_chunks);

    // Top-level chunks only.
    if !(opts.ignore_prefix || opts.ignore_suffix) {
        // Ensure the first chunk belongs to `node` for correct separator
        // emission before `node`.
        if chunks.first().is_none_or(|c| c.node != id) {
            chunks.insert(0, ConstrainedText::cast("", id));
        }
        // Set `no_sep` on all but the first chunk.
        for (i, t) in chunks.iter_mut().enumerate() {
            if i > 0 {
                t.no_sep = true;
            }
        }
    }

    // Tag all chunks as selser.
    for c in &mut chunks {
        c.selser = true;
    }

    chunks
}

/// Whether the character is in the legacy parser's `EXT_LINK_URL_CLASS` — the
/// set of characters that terminate a free external link. Faithful to
/// `AutoURLLinkText::EXT_LINK_URL_CLASS` (negated `^` in the PHP source, so
/// these are the chars that are *not* part of a link).
fn is_ext_link_url_class(c: char) -> bool {
    matches!(c, '[' | ']' | '<' | '>' | '"')
        || matches!(c, '\u{00}'..='\u{20}' | '\u{7F}')
        || matches!(
            c,
            '\u{00A0}' | '\u{1680}' | '\u{180E}' | '\u{2000}'
                ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
        )
}

/// Whether the character is in `TRAILING_PUNCT = ',;\\\\.:!?'` (the comma,
/// semicolon, backslash, dot, colon, exclamation, question mark).
fn is_trailing_punct(c: char) -> bool {
    matches!(c, ',' | ';' | '\\' | '.' | ':' | '!' | '?')
}

/// Whether `ctx[i..]` begins with a `&lt;`/`&gt;`/`&nbsp;`/numeric form of those
/// entities (the `NOT_LTGTNBSP` negative-lookahead assertion in the PHP regex).
fn starts_with_protected_entity(ctx: &str) -> bool {
    // `&(lt|gt|nbsp|#x0*(3[CcEe]|[Aa]0)|#0*(60|62|160));`
    let rest = match ctx.strip_prefix('&') {
        Some(r) => r,
        None => return false,
    };
    let name = rest.split(';').next().unwrap_or(rest);
    let lower = name.to_ascii_lowercase();
    if lower == "lt" || lower == "gt" || lower == "nbsp" {
        return true;
    }
    // Numeric forms: #x0*3[ce], #x0*a0, #0*60, #0*62, #0*160.
    if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        let hex = hex.trim_start_matches('0');
        return matches!(hex.to_ascii_lowercase().as_str(), "3c" | "3e" | "a0");
    }
    if let Some(dec) = name.strip_prefix('#') {
        let dec = dec.trim_start_matches('0');
        return matches!(dec, "60" | "62" | "160");
    }
    false
}

/// Whether the right context requires a `<nowiki/>` suffix for an autolink,
/// mirroring `AutoURLLinkText::escape`'s suffix guard. `url_has_paren` selects
/// whether `)` is treated as trailing-punctuation (it is *not* when the URL
/// contains an open paren).
///
/// The regex is anchored at the start of `right_context`:
/// `^(?!&...;)(?!'')[tpun]*[urlclass tpun]`.
fn auto_url_bad_suffix_matches(url: &str, right_context: &str) -> bool {
    let bytes = right_context.as_bytes();
    let mut i = 0;
    // Negative-lookahead 1: no protected entity at position 0.
    if starts_with_protected_entity(right_context) {
        return false;
    }
    // Negative-lookahead 2: no `''` at position 0.
    if right_context.starts_with("''") {
        return false;
    }
    let url_has_paren = url.contains('(');
    // Consume zero-or-more trailing punctuation (plus `)` when the URL has no
    // open paren).
    while i < bytes.len() {
        let c = right_context[i..].chars().next().unwrap();
        let is_trailing = is_trailing_punct(c) || (!url_has_paren && c == ')');
        if !is_trailing {
            break;
        }
        i += c.len_utf8();
    }
    // One-or-more chars from URL-class ∪ trailing-punct (∪ `)` when no paren).
    if i >= bytes.len() {
        return false;
    }
    let c = right_context[i..].chars().next().unwrap();
    is_ext_link_url_class(c) || is_trailing_punct(c) || (!url_has_paren && c == ')')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cast() {
        let ct = ConstrainedText::cast("hello", 3);
        assert_eq!(ct.text, "hello");
        assert_eq!(ct.node, 3);
        assert_eq!(ct.prefix, None);
        assert_eq!(ct.suffix, None);
        assert!(!ct.selser);
        assert!(!ct.no_sep);
    }

    #[test]
    fn test_escape_line_no_subclass_escapes() {
        // With only base chunks (no prefix/suffix), escape_line is a plain
        // concatenation.
        let line = vec![
            ConstrainedText::cast("foo", 1),
            ConstrainedText::cast("bar", 2),
            ConstrainedText::cast("baz", 3),
        ];
        assert_eq!(ConstrainedText::escape_line(&line), "foobarbaz");
    }

    #[test]
    fn test_state_builds_right_context() {
        let line = vec![
            ConstrainedText::cast("ab", 1),
            ConstrainedText::cast("cde", 2),
        ];
        let state = State::new(&line);
        assert_eq!(state.right_context, "abcde");
        assert_eq!(state.left_context, "");
        assert_eq!(state.pos, 0);
    }

    #[test]
    fn test_escape_line_strips_past_chunk_from_right_context() {
        // A chunk with a prefix exercises the prefix/suffix path (base `escape`
        // returns the prefix set on the chunk). Verify the right-context
        // advancement and prefix emission.
        let mut a = ConstrainedText::cast("ab", 1);
        a.prefix = Some("<nowiki>".to_string());
        let line = vec![a, ConstrainedText::cast("cd", 2)];
        assert_eq!(ConstrainedText::escape_line(&line), "<nowiki>abcd");
    }

    #[test]
    fn test_wikilink_trail_suffix() {
        // A wikilink followed by a word char (the enwiki link trail `[a-z]+`)
        // needs a `<nowiki/>` suffix to prevent the word being absorbed.
        let trail = Some(regex::Regex::new("[a-z]+").unwrap());
        let link = ConstrainedText::wiki_link("[[Foo]]", 1, true, None, trail);
        // Right context starting with a trail char.
        let state = State {
            left_context: "".to_string(),
            right_context: "bar rest".to_string(),
            pos: 0,
        };
        let r = link.escape(&state);
        assert_eq!(r.suffix.as_deref(), Some("<nowiki/>"));
        assert!(r.greedy);
    }

    #[test]
    fn test_wikilink_plain_no_escape() {
        // No trail regex (category/external/image link) → no suffix.
        let link = ConstrainedText::wiki_link("[[Foo]]", 1, false, None, None);
        let state = State {
            left_context: "".to_string(),
            right_context: "bar".to_string(),
            pos: 0,
        };
        let r = link.escape(&state);
        assert_eq!(r.suffix, None);
    }

    #[test]
    fn test_auto_url_prefix_word_boundary() {
        // An autolink preceded by a word char needs a `<nowiki/>` prefix (the
        // `\w$` bad-prefix guard).
        let link = ConstrainedText::auto_url_link("https://example.com", 1);
        let state = State {
            left_context: "see".to_string(),
            right_context: "".to_string(),
            pos: 0,
        };
        let r = link.escape(&state);
        assert_eq!(r.prefix.as_deref(), Some("<nowiki/>"));
    }

    #[test]
    fn test_magic_link_both_guards() {
        let link = ConstrainedText::magic_link("RFC 1234", 1);
        let state = State {
            left_context: "x".to_string(),
            right_context: "y".to_string(),
            pos: 0,
        };
        let r = link.escape(&state);
        assert_eq!(r.prefix.as_deref(), Some("<nowiki/>"));
        assert_eq!(r.suffix.as_deref(), Some("<nowiki/>"));
    }

    #[test]
    fn test_language_variant_escape() {
        let lv = ConstrainedText::language_variant("-{x}-", 1);
        let state = State {
            left_context: "|".to_string(),
            right_context: "".to_string(),
            pos: 0,
        };
        let r = lv.escape(&state);
        assert_eq!(r.prefix.as_deref(), Some("<nowiki/>"));
        // Not preceded by a bare pipe → no prefix.
        let state2 = State {
            left_context: "x".to_string(),
            right_context: "".to_string(),
            pos: 0,
        };
        assert_eq!(lv.escape(&state2).prefix, None);
    }

    #[test]
    fn test_from_sel_ser_base_plain() {
        use crate::dom::node::{ElementKind, Node};

        // A plain paragraph with no link children and no DSR → single plain
        // selser chunk owned by the element.
        let mut doc = Node::document();
        let mut p = Node::element(ElementKind::Paragraph);
        p.push_child(Node::text("hello"));
        doc.push_child(p);
        let tree = DomTree::new(doc);
        let p_id = tree.first_child(tree.root()).unwrap();

        let chunks = from_sel_ser(&tree, p_id, "hello", None, FromSelSerOpts::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello");
        assert_eq!(chunks[0].node, p_id);
        assert!(chunks[0].selser);
    }

    #[test]
    fn test_from_sel_ser_detects_magic_link() {
        use crate::dom::node::{ElementKind, Node};

        // An `<a>` element with `stx=magiclink` produces a MagicLink chunk.
        let mut doc = Node::document();
        let mut a = Node::element(ElementKind::ExtLink);
        let dp = crate::wikitext::tokens_v2::DataParsoid {
            stx: Some("magiclink".to_string()),
            ..Default::default()
        };
        a.dp = Some(dp);
        a.set_attr("rel", "mw:ExtLink");
        doc.push_child(a);
        let tree = DomTree::new(doc);
        let a_id = tree.first_child(tree.root()).unwrap();

        let chunks = from_sel_ser(&tree, a_id, "RFC 1234", None, FromSelSerOpts::default());
        assert_eq!(chunks.len(), 1);
        assert!(matches!(
            chunks[0].kind,
            ConstrainedTextKind::MagicLink { .. }
        ));
        assert!(chunks[0].selser);
    }
}
