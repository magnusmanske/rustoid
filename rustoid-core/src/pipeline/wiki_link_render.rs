//! WikiLinkHandler (rendering path) — port of the link-target classification
//! and `<a>` emission from PHP Parsoid's `src/Wt2Html/TT/WikiLinkHandler.php`.
//!
//! Renders a `wikilink` self-closing token (emitted by the tokenizer for
//! `[[Target|text]]`) into an `<a rel="mw:WikiLink">` tag sequence. This
//! module covers the common local-title case; interwiki/language/category/file/
//! media handling is layered on top once the corresponding site-config data
//! (interwiki `url`, namespace flags) is fully wired.

use crate::title::{Title, TitleParser, make_link};
use crate::traits::SiteConfig;
use crate::wikitext::tokens_v2::{
    DataMwAttrib, DataMwValue, DataParsoid, EndTagTk, Item, KV, ParsoidToken, SelfclosingTagTk,
    TagTk,
};

/// A callback that builds an inline DOM fragment document from an
/// already-tokenized (and optionally template-expanded) token stream, for
/// tunneled media captions (mirrors PHP's `processContentInPipeline` with
/// `inlineContext => true`). The caller (the `Parser`) wires this to its
/// `renderLinks` + `renderExternalLinks` + inline tree-builder pipeline.
pub type CaptionFragmentBuilder<'a> = &'a mut dyn FnMut(Vec<Item>) -> crate::dom::node::Node;

use super::wiki_link_handler::{build_link_attrs, string_kv};

/// The default thumbnail width (MediaWiki's `$wgThumbLimits[0]`, 180px), applied
/// when a `thumb`/`frameless` format has no explicit size (mirrors
/// `SiteConfig::widthOption()`).
const DEFAULT_THUMB_WIDTH: u32 = 180;

/// A lightweight analogue of PHP's `Env` — wraps a `SiteConfig` plus a small
/// amount of per-parse state (the about-id counter used for transclusions).
pub struct WikiLinkContext<'a> {
    pub config: &'a dyn SiteConfig,
    about_id_counter: usize,
    metadata: MetadataCollector,
    /// Suppress media format options (`thumb`/`frame`/`framed`/`frameless`/
    /// `manualthumb`), treating them as `bogus` so the media renders as a bare
    /// `mw:File` rather than `mw:File/Thumb` etc. Set in gallery context (mirrors
    /// PHP `renderMedia`'s `suppressMediaFormats` → `renderFile`'s
    /// `extTagOpts['suppressMediaFormats']`).
    suppress_media_formats: bool,
}

/// A lightweight `ContentMetadataCollector` analogue, tracking categories and
/// language links emitted during link rendering.
#[derive(Debug, Default, Clone)]
pub struct MetadataCollector {
    /// Categories as (title, sort-key) pairs.
    pub categories: Vec<(crate::title::Title, String)>,
    /// Language link titles.
    pub language_links: Vec<crate::title::Title>,
}

impl MetadataCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a category membership (mirrors `addCategory`).
    pub fn add_category(&mut self, title: &crate::title::Title, sort_key: &str) {
        self.categories.push((title.clone(), sort_key.to_string()));
    }

    /// Record a language link (mirrors `addLanguageLink`).
    pub fn add_language_link(&mut self, title: &crate::title::Title) {
        self.language_links.push(title.clone());
    }
}

impl<'a> WikiLinkContext<'a> {
    pub fn new(config: &'a dyn SiteConfig) -> Self {
        Self {
            config,
            about_id_counter: 0,
            metadata: MetadataCollector::new(),
            suppress_media_formats: false,
        }
    }

    /// Enable suppression of media format options (gallery context).
    pub fn set_suppress_media_formats(&mut self) {
        self.suppress_media_formats = true;
    }

    /// Whether media format options are suppressed (gallery context).
    pub fn suppress_media_formats(&self) -> bool {
        self.suppress_media_formats
    }

    /// Generate a fresh about id (mirrors `Env::newAboutId`). In PHP these are
    /// global DOM ids; we approximate with a per-parse counter.
    pub fn new_about_id(&mut self) -> String {
        self.about_id_counter += 1;
        format!("#mwt{}", self.about_id_counter)
    }

    /// Resolve a URL-decoded title string to a Title (mirrors
    /// `Env::makeTitleFromURLDecodedStr`).
    pub fn make_title(&self, decoded: &str) -> Option<Title> {
        // Use the existing TitleParser (handles namespace + interwiki).
        let title = TitleParser::parse(decoded, self.config);
        Some(title)
    }

    /// Mutable access to the metadata collector (mirrors `Env::getMetadata`).
    pub fn metadata_mut(&mut self) -> &mut MetadataCollector {
        &mut self.metadata
    }
}

/// The result of classifying a wikilink target. Mirrors PHP's `stdClass`
/// returned by `getWikiLinkTargetInfo`.
#[derive(Debug, Clone)]
pub struct WikiLinkTargetInfo {
    pub href: String,
    pub href_src: String,
    /// A title object, if this is a local title link.
    pub title: Option<Title>,
    /// Reserved for interwiki/language info (not yet wired).
    pub interwiki: Option<crate::traits::InterwikiInfo>,
    pub language: Option<crate::traits::InterwikiInfo>,
    pub local_prefix: Option<String>,
    pub from_colon_escaped_text: bool,
    pub prefix: Option<String>,
}

/// Normalize and analyze a wikilink target. Mirrors PHP's
/// `getWikiLinkTargetInfo` for the title, interwiki, and language cases.
pub fn get_wiki_link_target_info(
    ctx: &WikiLinkContext,
    href: &str,
    href_src: &str,
) -> Result<WikiLinkTargetInfo, String> {
    use crate::util::decode_uri_component;

    let mut href = href.to_string();
    let mut from_colon_escaped_text = false;
    let mut local_prefix: Option<String> = None;
    let mut prefix: Option<String> = None;

    // Capture the (decoded) title before handling colon escape. The wikilink
    // target is entity-decoded here as well (`&#45;` → `-`), mirroring the PEG
    // grammar's entity rules which are preserved in the title string.
    let mut title_decoded =
        crate::html::wts_utils::decode_wt_entities_all(&decode_uri_component(&href));

    if href.trim_start().starts_with(':') {
        from_colon_escaped_text = true;
        href = href.trim_start().strip_prefix(':').unwrap().to_string();
    }
    if href.starts_with(':') {
        // Multiple colons — caught by caller as an invalid title.
        return Err("Multiple colons prefixing href.".to_string());
    }

    // The decoded title used for (re-)parsing must not carry the leading colon
    // escape, or `TitleParser` would treat it as force-mainspace.
    if from_colon_escaped_text {
        title_decoded = title_decoded
            .strip_prefix(':')
            .unwrap_or(&title_decoded)
            .to_string();
    }

    let href_bits = crate::pipeline::wiki_link_handler::href_parts(&href);

    let mut title: Option<Title> = None;
    let mut interwiki: Option<crate::traits::InterwikiInfo> = None;
    let mut language: Option<crate::traits::InterwikiInfo> = None;

    if let Some((ns_prefix, title_part)) = href_bits {
        let ns_prefix = ns_prefix.to_string();
        prefix = Some(ns_prefix.clone());

        let normalized = crate::util::normalize_namespace_name(ns_prefix.trim());
        let ns_id = namespace_id(ctx.config, &normalized);
        let interwiki_info = ctx.config.interwiki_map().get(&normalized).cloned();

        if ns_id.is_some() {
            // Namespace prefix → local title. Re-parse the decoded full string
            // (rather than `Title::new`) so first-letter capitalization is
            // applied for case-insensitive namespaces (mirrors
            // `makeTitleFromURLDecodedStr`).
            title = Some(TitleParser::parse(&title_decoded, ctx.config));
        } else if let Some(info) = &interwiki_info {
            if info.localinterwiki == Some(true) {
                // Local interwiki: empty title means main page (T66167).
                if title_part.is_empty() {
                    title = Some(Title::new_main(String::new()));
                } else {
                    href = if title_part.contains(':') {
                        format!(":{title_part}")
                    } else {
                        title_part.to_string()
                    };
                    title = Some(TitleParser::parse(&href, ctx.config));
                    local_prefix = Some(match local_prefix {
                        Some(existing) => format!("{ns_prefix}:{existing}"),
                        None => ns_prefix.clone(),
                    });
                }
            } else if !info.url.is_empty() {
                href = title_part.to_string();
                if from_colon_escaped_text
                    || (info.language.is_none() && info.extralanglink != Some(true))
                {
                    // Interwiki link.
                    interwiki = Some(info.clone());
                    if href.trim_start().starts_with(':') {
                        href = href.trim_start().strip_prefix(':').unwrap().to_string();
                    }
                } else {
                    // Language link.
                    language = Some(info.clone());
                }
            } else {
                // Unrecognized prefix → treat whole string as title.
                title = Some(TitleParser::parse(&title_decoded, ctx.config));
            }
        } else {
            // No namespace or interwiki prefix → plain title.
            title = Some(TitleParser::parse(&title_decoded, ctx.config));
        }
    } else {
        // No colon → plain mainspace title. Use `TitleParser::parse` (rather
        // than `Title::new_main`) so the URL fragment is split off and
        // first-letter capitalization is applied (mirrors
        // `makeTitleFromURLDecodedStr`).
        title = Some(TitleParser::parse(&title_decoded, ctx.config));
    }

    // A title that (after URL-decoding) still carries a percent-encoding
    // sequence (`%hh`) or an entity reference (`&…;`) is invalid: it cannot be
    // round-tripped consistently (mirrors `Title::newFromText`'s
    // `getTitleInvalidRegex`, which `makeTitleFromURLDecodedStr` enforces). The
    // caller bails the link to plain text.
    if let Some(t) = &title
        && crate::title::has_invalid_chars(&t.text)
    {
        return Err("Invalid characters in title.".to_string());
    }
    if interwiki.is_some() && crate::title::has_invalid_chars(&href) {
        return Err("Invalid characters in title.".to_string());
    }

    Ok(WikiLinkTargetInfo {
        href,
        href_src: href_src.to_string(),
        title,
        interwiki,
        language,
        local_prefix,
        from_colon_escaped_text,
        prefix,
    })
}

/// Resolve a (possibly localized/canonical) namespace name to its id.
/// `name` is already first-letter-lowercased (`normalizeNamespaceName`); the
/// comparison stays case-sensitive so an interwiki prefix like `wikipedia` does
/// not collide with a namespace alias like `Wikipedia`.
fn namespace_id(config: &dyn SiteConfig, name: &str) -> Option<i32> {
    for (&id, ns) in config.namespaces() {
        if ns.canonical == name {
            return Some(id);
        }
        if ns.aliases.iter().any(|a| a == name) {
            return Some(id);
        }
    }
    None
}

/// Extract link text and build attributes for a wikilink, returning the new
/// `<a>` tag attributes and the content tokens. Faithful port of
/// `WikiLinkHandler::addLinkAttributesAndGetContent` for the simple/simple-piped
/// cases.
pub fn add_link_attributes_and_get_content(
    _ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
) -> (Vec<KV>, Vec<Item>, DataParsoid) {
    let attribs = token.get_attribs().to_vec();
    let data_parsoid = token.data_parsoid().cloned().unwrap_or_default();

    let link_attrs = [string_kv("rel", "mw:WikiLink")];
    let new_attr_data = build_link_attrs(&attribs, true, None, Some(&link_attrs));
    let content_kvs = new_attr_data.content_kvs;

    // If there's link text (piped), extract it; otherwise compute auto text.
    if !content_kvs.is_empty() {
        let mut out: Vec<Item> = Vec::new();
        for (i, kv) in content_kvs.iter().enumerate() {
            let toks = match &kv.value {
                crate::wikitext::tokens_v2::KeyValue::Str(s) => {
                    vec![Item::Str(s.clone())]
                }
                crate::wikitext::tokens_v2::KeyValue::Tokens(t) => t.clone(),
            };
            out.extend(toks);
            if i < content_kvs.len() - 1 {
                out.push(Item::Str("|".to_string()));
            }
        }

        // Carries the original token's dataParsoid (src cleared, stx='piped'),
        // faithfully mirroring PHP's `addLinkAttributesAndGetContent`.
        let mut dp = data_parsoid.clone();
        dp.src = None;
        dp.stx = Some("piped".to_string());
        (new_attr_data.attribs, out, dp)
    } else {
        // No explicit link text; derive it from the (decoded) target href,
        // which carries the fragment and namespace (mirrors PHP's
        // `addLinkAttributesAndGetContent`: `decodeURIComponent($target->href)`,
        // plus interwiki/local prefix prepending).
        let mut morecontent = crate::util::decode_uri_component(&target.href);
        if target.interwiki.is_some()
            && let Some(ref p) = target.prefix
        {
            morecontent = format!("{p}:{morecontent}");
        }
        if let Some(ref lp) = target.local_prefix {
            morecontent = format!("{lp}:{morecontent}");
        }

        let mut dp = data_parsoid.clone();
        dp.src = None;
        dp.stx = Some("simple".to_string());
        (new_attr_data.attribs, vec![Item::Str(morecontent)], dp)
    }
}

/// Render a plain wiki link into a sequence of items: `<a rel="mw:WikiLink">`
/// ... content ... `</a>`. Faithful port of `WikiLinkHandler::renderWikiLink`.
pub fn render_wiki_link(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
) -> Vec<Item> {
    let (attribs, content, dp) = add_link_attributes_and_get_content(ctx, token, target);

    let mut a_tag = TagTk::new("a", attribs, dp);

    // href = makeLink(title), title = getPrefixedText().
    // `addNormalizedAttribute('href', normalized, src)` records the source href
    // as `sa.href` and the normalized href as `a.href` (for ComputeDSR).
    if let Some(title) = &target.title {
        let mut href = make_link(title, ctx.config);
        // `makeLink(title)` omits the fragment; append it so `[[Main Page#section]]`
        // renders `./Main_Page#section` (mirrors PHP, where the anchor href
        // carries the title's fragment).
        if let Some(fragment) = &title.fragment {
            href.push('#');
            href.push_str(fragment);
        }
        let prefixed = title.get_prefixed_text();
        a_tag.add_attribute_str("href", &href);
        a_tag.add_attribute_str("title", &prefixed);
        a_tag.data_parsoid.set_sa("href", &target.href_src);
        a_tag.data_parsoid.set_a("href", &href);
    } else {
        a_tag.add_attribute_str("href", &target.href);
        a_tag.data_parsoid.set_sa("href", &target.href_src);
        a_tag.data_parsoid.set_a("href", &target.href);
    }

    let mut out = vec![Item::Tok(ParsoidToken::Tag(a_tag))];
    out.extend(content);
    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        "a",
        vec![],
        DataParsoid::default(),
    ))));
    out
}

/// Render an interwiki link into `<a rel="mw:WikiLink/Interwiki">...</a>`.
/// Mirrors `WikiLinkHandler::renderInterwikiLink`.
pub fn render_interwiki_link(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
) -> Vec<Item> {
    use crate::sanitizer::sanitize_title_uri;
    use crate::util::decode_uri_component;

    let info = target.interwiki.as_ref().expect("interwiki info");

    let (attribs, content, dp) = add_link_attributes_and_get_content(ctx, token, target);
    let mut new_tk = TagTk::new("a", attribs, dp);

    let is_local = info.local;
    let trimmed_href = target.href.trim();
    let title = sanitize_title_uri(&decode_uri_component(trimmed_href), !is_local);
    let mut abs_href = info.url.replace("$1", &title);
    if info.protorel == Some(true) {
        abs_href = abs_href
            .strip_prefix("http:")
            .or_else(|| abs_href.strip_prefix("https:"))
            .map(|s| s.to_string())
            .unwrap_or(abs_href);
    }
    new_tk.add_attribute_str("href", &abs_href);
    new_tk.data_parsoid.set_sa("href", &target.href_src);
    new_tk.data_parsoid.set_a("href", &abs_href);

    // Replace the rel attribute value with mw:WikiLink/Interwiki.
    if let Some(kv) = new_tk
        .attribs
        .iter_mut()
        .find(|kv| kv.key.as_str() == Some("rel"))
    {
        kv.value = crate::wikitext::tokens_v2::KeyValue::Str("mw:WikiLink/Interwiki".to_string());
    }

    // Add title unless it's just a fragment (and trim off fragment).
    // The title prefix is the *canonical* interwiki map key (`$target->interwiki['prefix']`
    // in PHP), not the raw input prefix (which may differ in case).
    if target.href.is_empty() || !target.href.starts_with('#') {
        let prefix = info.prefix.clone().unwrap_or_default();
        let mut title_attr = format!("{prefix}:");
        let stripped_fragment = trimmed_href.split('#').next().unwrap_or(trimmed_href);
        title_attr.push_str(&decode_uri_component(&stripped_fragment.replace('_', " ")));
        new_tk.add_attribute_str("title", &title_attr);
    }

    let mut out = vec![Item::Tok(ParsoidToken::Tag(new_tk))];
    out.extend(content);
    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        "a",
        vec![],
        DataParsoid::default(),
    ))));
    out
}

/// Render a language link into `<link rel="mw:PageProp/Language">`.
/// Mirrors `WikiLinkHandler::renderLanguageLink`.
pub fn render_language_link(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
) -> Vec<Item> {
    use crate::sanitizer::sanitize_title_uri;
    use crate::util::decode_uri_component;

    let info = target.language.as_ref().expect("language info");

    let (attribs, _content, dp) = add_link_attributes_and_get_content(ctx, token, target);
    let mut new_tk = crate::wikitext::tokens_v2::SelfclosingTagTk::new("link", attribs, dp);

    // Set absolute link to the article in the other language.
    let title = sanitize_title_uri(&decode_uri_component(&target.href), false);
    let mut abs_href = info.url.replace("$1", &title);
    if info.protorel == Some(true) {
        abs_href = abs_href
            .strip_prefix("http:")
            .or_else(|| abs_href.strip_prefix("https:"))
            .map(|s| s.to_string())
            .unwrap_or(abs_href);
    }
    new_tk.add_attribute_str("href", &abs_href);
    new_tk.data_parsoid.set_sa("href", &target.href_src);
    new_tk.data_parsoid.set_a("href", &abs_href);

    // Change rel to mw:PageProp/Language.
    if let Some(kv) = new_tk
        .attribs
        .iter_mut()
        .find(|kv| kv.key.as_str() == Some("rel"))
    {
        kv.value = crate::wikitext::tokens_v2::KeyValue::Str("mw:PageProp/Language".to_string());
    }

    // Record the language link in metadata (PHP uses the decoded title).
    let meta_title =
        crate::title::TitleParser::parse(&decode_uri_component(&target.href), ctx.config);
    ctx.metadata_mut().add_language_link(&meta_title);

    vec![Item::Tok(ParsoidToken::SelfclosingTag(new_tk))]
}

/// Render a category membership into `<link rel="mw:PageProp/Category">`.
/// Mirrors `WikiLinkHandler::renderCategory` for the simple sort-key case.
pub fn render_category(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
) -> Vec<Item> {
    use crate::sanitizer::sanitize_title_uri;

    let (attribs, content, dp) = add_link_attributes_and_get_content(ctx, token, target);
    let mut new_tk = crate::wikitext::tokens_v2::SelfclosingTagTk::new("link", attribs, dp);

    // Change rel to mw:PageProp/Category.
    if let Some(kv) = new_tk
        .attribs
        .iter_mut()
        .find(|kv| kv.key.as_str() == Some("rel"))
    {
        kv.value = crate::wikitext::tokens_v2::KeyValue::Str("mw:PageProp/Category".to_string());
    }

    // href = makeLink(title).
    if let Some(title) = &target.title {
        let mut href = make_link(title, ctx.config);

        // Compute sort key from content (strip newlines).
        let category_sort = token_utils_tokens_to_string(&content);
        let category_sort = category_sort.replace('\n', "");
        if !category_sort.is_empty() && category_sort != target.href {
            // Append '#sortkey' to href, encoding '#' as %23.
            let encoded = sanitize_title_uri(&category_sort, false).replace('#', "%23");
            href.push('#');
            href.push_str(&encoded);
        }

        new_tk.add_attribute_str("href", &href);

        // Record the category in metadata.
        ctx.metadata_mut().add_category(title, &category_sort);
    }

    vec![Item::Tok(ParsoidToken::SelfclosingTag(new_tk))]
}

/// Convert content items to a single string for sort-key computation.
fn token_utils_tokens_to_string(items: &[Item]) -> String {
    use crate::wikitext::token_utils::tokens_to_string;
    tokens_to_string(items)
}

/// Re-tokenize a media caption string as full wikitext (quotes, entities,
/// links, nowiki, magic links, …), returning the inline token stream. Mirrors
/// PHP `processContentInPipeline` with `inlineContext => true` for the caption
/// (which re-parses the caption with the inline grammar). The main pipeline's
/// TT3 handlers (QuoteTransformer, etc.) then turn `mw-quote`/`mw:Entity` into
/// `<i>`/`mw:Entity` spans during tree-building.
fn tokenize_caption(caption: &str, config: &dyn SiteConfig) -> Vec<Item> {
    use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};

    let options = TokenizerOptions {
        magic_links: crate::wikitext::tokenizer_v2::MagicLinkConfig {
            rfc: config.magic_link_enabled("RFC"),
            pmid: config.magic_link_enabled("PMID"),
            isbn: config.magic_link_enabled("ISBN"),
        },
        ext_tags: config.extension_tags().to_vec(),
        protocols: config.protocols().iter().map(|s| s.to_string()).collect(),
        lang_conv_enabled: config.lang_converter_enabled(),
        ..TokenizerOptions::default()
    };
    let mut tokenizer = PegTokenizer::new(caption, &options);
    tokenizer
        .tokenize()
        .unwrap_or_default()
        .into_iter()
        .map(|e| match e {
            crate::wikitext::tokens_v2::Either::Left(s) => Item::Str(s),
            crate::wikitext::tokens_v2::Either::Right(t) => Item::Tok(t),
        })
        .collect()
}

/// Re-tokenize caption *items* as full wikitext. Unlike `tokenize_caption`, this
/// walks a mixed token array (text chunks + transclusion markers produced by
/// `expandAttributes`) and re-tokenizes only the text chunks, passing through
/// the `mw:Transclusion` meta markers untouched (so templates in captions render
/// as `mw:Transclusion` spans rather than flattening to text).
///
/// A caption may carry `language-variant` tokens (`-{…}-`) interleaved with text;
/// because `tokenize_link_content` only recognizes `{{…}}`/`-{…}-` directives, a
/// nested `[[…]]` wikilink surrounding a `-{…}-` is split across several items
/// (e.g. `[[File:…|alt=`, `language-variant`, `|`, `language-variant`, `]]`). To
/// re-assemble such a wikilink, we reconstruct the full caption source (using
/// each directive's `data-parsoid.src`) and re-tokenize it as one span, rather
/// than tokenizing each text chunk in isolation.
fn tokenize_caption_items(items: &[Item], config: &dyn SiteConfig) -> Vec<Item> {
    // Only reconstruct when a directive token is present that may have split a
    // surrounding wikilink across several items (`tokenize_link_content` only
    // recognizes `{{…}}`/`-{…}-`/`<nowiki>` directives, so `[[A|…directive…]]` is
    // fragmented); otherwise keep the per-chunk path to preserve transclusion
    // markers. `extension` (nowiki) and `language-variant` tokens carry their exact
    // wikitext in `data-parsoid.src`, so the full caption source can be re-assembled.
    let needs_reconstruction = items.iter().any(|item| {
        matches!(
            item,
            Item::Tok(ParsoidToken::SelfclosingTag(tk))
                if tk.name == "language-variant" || tk.name == "extension"
        )
    });
    if !needs_reconstruction {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if let Item::Str(s) = item {
                out.extend(tokenize_caption(s, config));
            } else {
                out.push(item.clone());
            }
        }
        return out;
    }

    // Reconstruct the full caption source, then tokenize it whole. `extension`/
    // `language-variant`/`template`/meta tokens carry their exact wikitext in `src`;
    // text chunks are already literal.
    let mut src = String::new();
    for item in items {
        match item {
            Item::Str(s) => src.push_str(s),
            Item::Tok(tok) => {
                let dp_src = tok.data_parsoid().and_then(|d| d.src.clone());
                if let Some(s) = dp_src {
                    src.push_str(&s);
                }
            }
        }
    }
    tokenize_caption(&src, config)
}

/// Split a media option string on *top-level* pipes, respecting nested
/// `[[…]]`/`{{…}}` (so a `|` inside a piped link or template does not split the
/// options). Mirrors `wikilink_content`'s balanced-bracket pipe handling,
/// including the `{| … |}` table block, whose internal pipes are cell/row
/// markers (not option separators) and must stay glued to the caption. A
/// `<nowiki>` (self-closing or paired) is likewise opaque: a `|` inside it (e.g.
/// a bogus attribute `bogus="attri|bute"`) is literal, not a separator.
pub(crate) fn split_media_options(content: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut bracket = 0i32;
    let mut braces = 0i32;
    let mut table = 0i32;
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // A `<nowiki>` (self-closing or paired) is opaque to pipe splitting.
        if c == b'<'
            && content[i..].to_ascii_lowercase().starts_with("<nowiki")
            && content[i + 7..].starts_with(|c: char| c == '>' || c == '/' || c.is_whitespace())
            && let Some((tag_end, self_closing)) =
                crate::wikitext::tokenizer_v2::nowiki_start_tag_end(content, i)
        {
            current.push_str(&content[i..tag_end]);
            i = tag_end;
            if !self_closing {
                // Paired `<nowiki>…</nowiki>`: consume to just past the close tag.
                if let Some(close_rel) = content[i..].to_ascii_lowercase().find("</nowiki") {
                    let close_end = i + close_rel + "</nowiki>".len();
                    let gt = content[close_end..].find('>').map(|g| g + 1).unwrap_or(0);
                    current.push_str(&content[i..close_end + gt]);
                    i = close_end + gt;
                } else {
                    // Unclosed nowiki: the rest is literal.
                    current.push_str(&content[i..]);
                    i = bytes.len();
                }
            }
            continue;
        }
        if c == b'[' && bytes.get(i + 1) == Some(&b'[') {
            bracket += 1;
            current.push_str("[[");
            i += 2;
            continue;
        }
        if c == b']' && bytes.get(i + 1) == Some(&b']') {
            bracket = bracket.saturating_sub(1);
            current.push_str("]]");
            i += 2;
            continue;
        }
        if c == b'{' && bytes.get(i + 1) == Some(&b'{') {
            braces += 1;
            current.push_str("{{");
            i += 2;
            continue;
        }
        if c == b'}' && bytes.get(i + 1) == Some(&b'}') {
            braces = braces.saturating_sub(1);
            current.push_str("}}");
            i += 2;
            continue;
        }
        // A `{| … |}` table block is a single balanced atom: its internal `|`
        // (cell/row markers) must not split the option/caption list.
        if c == b'{' && bytes.get(i + 1) == Some(&b'|') && bracket == 0 && braces == 0 {
            table += 1;
            current.push_str("{|");
            i += 2;
            continue;
        }
        if c == b'|' && bytes.get(i + 1) == Some(&b'}') && table > 0 {
            table -= 1;
            current.push_str("|}");
            i += 2;
            continue;
        }
        if c == b'|' && bracket == 0 && braces == 0 && table == 0 {
            parts.push(std::mem::take(&mut current));
            i += 1;
            continue;
        }
        // Advance one UTF-8 code point (structural delimiters `|`/`[`/`{` are
        // single-byte ASCII, so the structural branches above are byte-safe).
        let ch_len = content[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        current.push_str(&content[i..i + ch_len]);
        i += ch_len;
    }
    if !current.is_empty() || parts.is_empty() {
        parts.push(current);
    }
    parts
}

/// Render a `[[Media:Foo]]` link (a direct media link). Mirrors
/// `WikiLinkHandler::renderMedia` + `linkToMedia` for the no-file-info case.
pub fn render_media(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
) -> Vec<Item> {
    link_to_media(ctx, token, target, None)
}

/// Render a media link (shared by `renderMedia`). `info` is optional file info
/// (not yet fetched; the no-info case uses the upload URL). Mirrors
/// `WikiLinkHandler::linkToMedia`.
pub fn link_to_media(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
    info: Option<&crate::traits::FileInfo>,
) -> Vec<Item> {
    let (attribs, content, dp) = add_link_attributes_and_get_content(ctx, token, target);
    let mut link = TagTk::new("a", attribs, dp);

    // imgHref = info.url or upload URL.
    let img_href = info
        .map(|i| i.file_url.clone())
        .or_else(|| {
            target
                .title
                .as_ref()
                .map(|t| ctx.config.get_upload_url(&t.get_dbkey()))
        })
        .unwrap_or_default();

    // rel = mw:MediaLink.
    if let Some(kv) = link
        .attribs
        .iter_mut()
        .find(|kv| kv.key.as_str() == Some("rel"))
    {
        kv.value = crate::wikitext::tokens_v2::KeyValue::Str("mw:MediaLink".to_string());
    }

    link.add_attribute_str("href", &img_href);

    // resource = makeLink(title).
    if let Some(title) = &target.title {
        let resource = make_link(title, ctx.config);
        link.add_attribute_str("resource", &resource);

        // Normalize file name (strip path). `getDBkey` is the raw title text.
        let normalized = info
            .map(|i| {
                i.file_url
                    .rsplit('/')
                    .next()
                    .unwrap_or(&i.title)
                    .to_string()
            })
            .unwrap_or_else(|| title.text.clone());
        link.add_attribute_str("title", normalized.replace('_', " "));
    }

    let mut out = vec![Item::Tok(ParsoidToken::Tag(link))];
    out.extend(content);
    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        "a",
        vec![],
        DataParsoid::default(),
    ))));
    out
}

/// Render a file link, parsing image options. Mirrors `WikiLinkHandler::renderFile`
/// for the common simple-option and width cases (full media info fetching and
/// complex option stringification are deferred).
pub fn render_file(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
    fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
    next_id: &mut usize,
    build_fragment: CaptionFragmentBuilder,
) -> Vec<Item> {
    use super::media_options::{MediaOpts, get_format, get_wrapper_info};

    let title = target.title.as_ref().expect("file title");

    // Extract options from the pipe-separated `mw:maybeContent` KVs (the
    // tokenizer emits one KV per top-level `|`-separated segment). Each part is
    // stringified for option recognition, but the caption part keeps its raw
    // token array so transclusions/tables round-trip (mirrors PHP `renderFile`'s
    // `buildLinkAttrs` content-KV loop).
    let mut opts = MediaOpts::default();
    let mut opt_list: Vec<crate::wikitext::tokens_v2::OptListEntry> = Vec::new();
    let mut caption: Option<Vec<Item>> = None;
    let mut caption_ak: Option<String> = None;
    let mut caption_pos: usize = 0;
    for kv in token.get_attribs() {
        if kv.key.as_str() != Some("mw:maybeContent") {
            continue;
        }
        // Stringify this part for option recognition, stripping transclusion/
        // param meta markers (`strip_meta_tags` with `wrap_templates`), keeping
        // the raw tokens for a caption.
        let raw_items = crate::pipeline::attribute_transform_manager::key_value_to_items(&kv.value);
        // Whether this part's raw source is a token array (i.e. it contained a
        // template/entity, not a plain string). Used to detect `mw:ExpandedAttrs`
        // on *options* (not captions), mirroring PHP's `is_array($origOptSrc)`.
        let is_token_array = matches!(kv.value, crate::wikitext::tokens_v2::KeyValue::Tokens(_));
        // The raw source wikitext for round-tripping (`$oContent->vsrc` in PHP).
        let vsrc = kv.vsrc.clone();
        let part = match &kv.value {
            crate::wikitext::tokens_v2::KeyValue::Str(s) => s.clone(),
            crate::wikitext::tokens_v2::KeyValue::Tokens(items) => {
                let stripped = crate::pipeline::attribute_expander::strip_meta_tags(items, true);
                crate::wikitext::token_utils::tokens_to_string(&stripped.value)
            }
        };
        // A template that expanded to a top-level `|`-separated string yields
        // multiple option parts (no editing support). Split and process each;
        // the container is marked `mw:Placeholder` (mirrors PHP `renderFile`'s
        // `explode('|', $oText)` + `$dataParsoid->uneditable = true`). Pipes in
        // table syntax (`{| … |}`) or nested `{{…}}`/`[[…]]` within a caption are
        // NOT top-level option separators, so they must not trigger the split
        // (mirrors PHP, where an array `$oText` skips `explode`).
        let pipe_parts = split_media_options(&part);
        if is_token_array && pipe_parts.len() > 1 {
            opts.placeholder = true;
            for sub in pipe_parts {
                // The pipe-split pieces are plain strings (a template expanded to
                // a `|`-separated option string), so each is a *non*-array option
                // source: no `mw:ExpandedAttrs` for them (mirrors PHP, where the
                // `explode('|', $oText)` path `continue`s before `$expOpt`).
                if !record_media_option(ctx, &mut opts, &mut opt_list, &sub, &sub, false, false) {
                    // Unrecognized sub-part ⇒ caption (last one wins). A previous
                    // caption becomes a `bogus` optList entry at its position
                    // (mirrors PHP's `array_splice` bogus-marker for displaced captions).
                    if caption.is_some() {
                        let pos = caption_pos.min(opt_list.len());
                        opt_list.insert(pos, bogus_opt(&sub));
                        caption_pos = pos + 1;
                    } else {
                        caption_pos = opt_list.len();
                    }
                    caption_ak = Some(sub.clone());
                    caption = Some(vec![Item::Str(sub.clone())]);
                }
            }
            continue;
        }
        let has_transclusion = if is_token_array {
            contains_transclusion(&raw_items)
        } else {
            false
        };
        if !record_media_option(
            ctx,
            &mut opts,
            &mut opt_list,
            &part,
            vsrc.as_deref().unwrap_or(&part),
            is_token_array,
            has_transclusion,
        ) {
            // Unrecognized ⇒ caption (last one wins). Keep the raw tokens.
            if caption.is_some() {
                let pos = caption_pos.min(opt_list.len());
                let src = vsrc.clone().unwrap_or_else(|| part.clone());
                opt_list.insert(pos, bogus_opt(&src));
                caption_pos = pos + 1;
            } else {
                caption_pos = opt_list.len();
            }
            caption_ak = Some(vsrc.unwrap_or_else(|| part.clone()));
            caption = Some(raw_items);
        }
    }

    let format = get_format(&opts);
    let (classes, is_inline) = get_wrapper_info(&opts);

    // A `thumb`/`frameless` format with no explicit size defaults to the site's
    // default thumbnail width (180px), stamped as `data-width` on the broken
    // span (mirrors `renderFile`'s default-size handling). `framed`/`manualthumb`
    // are unscaled, so they get no default. This runs *after* `getWrapperInfo`
    // so the `mw-default-size` class is still added (the default is not an
    // explicit size). `upright` scales the default by the given factor (bare
    // `upright` → 0.75) and rounds to the nearest 10px.
    let mut upright_factor: Option<f64> = None;
    if matches!(format.as_deref(), Some("thumbnail") | Some("frameless"))
        && opts.width.is_none()
        && opts.height.is_none()
    {
        let mut default_width: f64 = DEFAULT_THUMB_WIDTH as f64;
        if let Some(u) = &opts.upright {
            let factor = if u == "upright" {
                0.75
            } else {
                u.parse::<f64>().unwrap_or(0.0)
            };
            upright_factor = Some(factor);
            default_width *= factor;
            default_width = 10.0 * (default_width / 10.0).round();
        }
        opts.width = Some(default_width.to_string());
    }

    // rdfa type and container.
    let mut rdfa_type = match format.as_deref() {
        Some("manualthumb") | Some("thumbnail") => "mw:File/Thumb",
        Some("framed") => "mw:File/Frame",
        Some("frameless") => "mw:File/Frameless",
        _ => "mw:File",
    }
    .to_string();
    // An expanded (rich-markup) attribute value marks the container so the
    // attribute can be round-tripped via `data-mw.attribs` (mirrors
    // `$container->addSpaceSeparatedAttribute('typeof', 'mw:ExpandedAttrs')`).
    if opts.expanded_attrs {
        rdfa_type.push_str(" mw:ExpandedAttrs");
    }
    // A template that expanded to a `|`-separated option string has no editing
    // support; mark the container `mw:Placeholder` (mirrors `renderFile`'s
    // `$dataParsoid->uneditable` + `$rdfaType .= ' mw:Placeholder'`).
    if opts.placeholder {
        rdfa_type.push_str(" mw:Placeholder");
    }

    let container_name = if is_inline { "span" } else { "figure" };

    let mut container_attribs = vec![crate::pipeline::wiki_link_handler::string_kv(
        "typeof", &rdfa_type,
    )];
    if !classes.is_empty() {
        container_attribs.insert(
            0,
            crate::pipeline::wiki_link_handler::string_kv("class", &classes.join(" ")),
        );
    }

    // Non-`getUsed()` options (`link`, `alt`, `manualthumb`, `page`, `class`,
    // etc.) are stored in `data-mw.attribs` so `AddMediaInfo` can apply them
    // after file-info retrieval (mirrors `renderFile`'s `dataMw->attribs`).
    let data_mw_attribs: Vec<DataMwAttrib> = [
        ("link", opts.link.as_ref()),
        ("alt", opts.alt.as_ref()),
        ("manualthumb", opts.manualthumb.as_ref()),
        ("page", opts.page.as_ref()),
    ]
    .into_iter()
    .filter_map(|(key, val)| {
        val.map(|v| {
            DataMwAttrib::new(
                DataMwValue::Str(key.to_string()),
                DataMwValue::Object {
                    txt: Some(v.clone()),
                    html: None,
                    uneditable: false,
                },
            )
        })
    })
    .collect();

    let mut container = TagTk::new(container_name, container_attribs, DataParsoid::default());

    // Build the container's `data-parsoid` (mirrors PHP `renderFile` attaching
    // `$dataParsoid` — the optList — to the container token). Insert the caption
    // entry at its recorded position (preceding captions become `bogus`).
    {
        let mut dp = DataParsoid::default();
        if caption.is_some() {
            if caption_pos > opt_list.len() {
                caption_pos = opt_list.len();
            }
            opt_list.insert(
                caption_pos,
                crate::wikitext::tokens_v2::OptListEntry {
                    ck: Some("caption".to_string()),
                    ak: caption_ak.clone(),
                    v: None,
                },
            );
        }
        dp.opt_list = Some(opt_list);
        container.data_parsoid = dp;
    }
    if !data_mw_attribs.is_empty() || (is_inline && caption.is_some()) {
        let mut obj = serde_json::Map::new();
        if !data_mw_attribs.is_empty() {
            let json =
                crate::pipeline::attribute_expander::serialize_data_mw_attribs(&data_mw_attribs);
            obj.insert(
                "attribs".to_string(),
                serde_json::from_str(&json).unwrap_or(serde_json::Value::Array(vec![])),
            );
        }
        // Inline-media captions are stored in `data-mw.caption` (mirrors PHP's
        // `$dataMw->caption`, a serialized DOM fragment of the re-parsed caption).
        if is_inline && let Some(cap) = &caption {
            // Stringify the caption tokens (inline media has no rendered block
            // structure). The raw tokens preserve transclusion markers for
            // round-tripping when present.
            obj.insert(
                "caption".to_string(),
                serde_json::Value::String(crate::wikitext::token_utils::tokens_to_string(cap)),
            );
        }
        container.add_attribute_str("data-mw", serde_json::Value::Object(obj).to_string());
    }

    // Anchor wraps the file element.
    let mut anchor = TagTk::new("a", vec![], DataParsoid::default());
    anchor.add_attribute_str("href", ctx.config.get_upload_url(&title.get_dbkey()));
    anchor.add_attribute_str("class", "new");
    anchor.add_attribute_str("title", title.get_prefixed_text());

    // Inner span (broken media placeholder) with resource/lang/data-* attrs.
    // The `resource` attribute carries normalized (`a`) and source (`sa`) shadow
    // info so html2wt can round-trip the title (mirrors PHP's
    // `$span->addNormalizedAttribute('resource', $opts['title']['v'], $opts['title']['src'])`).
    let mut span_dp = DataParsoid::default();
    span_dp.set_a("resource", &make_link(title, ctx.config));
    span_dp.set_sa("resource", &target.href_src);
    let mut span = TagTk::new("span", vec![], span_dp);
    span.add_attribute_str("class", "mw-file-element mw-broken-media");
    span.add_attribute_str("resource", make_link(title, ctx.config));
    if let Some(width) = &opts.width {
        span.add_attribute_str("data-width", width);
    }
    if let Some(height) = &opts.height {
        span.add_attribute_str("data-height", height);
    }
    // `upright` factor is stamped as `data-upright` on the broken span (mirrors
    // `renderFile`); `AddMediaInfo` reads it to add the `mw-file-upright` class
    // and `--mw-file-upright` style on the final `<img>`.
    if let Some(factor) = upright_factor {
        span.add_attribute_str("data-upright", factor.to_string());
    }
    // `lang=` is applied to the broken span (mirrors `renderFile`'s
    // `$span->addNormalizedAttribute('lang', ...)`); `AddMediaInfo` reads it back
    // to build the `?lang=` description-link query.
    if let Some(lang) = &opts.lang {
        span.add_attribute_str("lang", lang);
    }

    let mut out = vec![
        Item::Tok(ParsoidToken::Tag(container)),
        Item::Tok(ParsoidToken::Tag(anchor)),
        Item::Tok(ParsoidToken::Tag(span)),
        Item::Str(title.get_prefixed_text()),
        Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "span",
            vec![],
            DataParsoid::default(),
        ))),
        Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "a",
            vec![],
            DataParsoid::default(),
        ))),
    ];

    // For block formats, add a figcaption holding the caption (or empty).
    if !is_inline {
        out.push(Item::Tok(ParsoidToken::Tag(TagTk::new(
            "figcaption",
            vec![],
            DataParsoid::default(),
        ))));
        if let Some(cap) = &caption {
            // The caption is re-tokenized as full wikitext (quotes/entities/
            // links/tables/…) while preserving transclusion markers, then
            // tunneled through an inline sub-pipeline via an
            // `mw:dom-fragment-token` placeholder, mirroring PHP's
            // `getDOMFragmentToken($optsCaption['v'], …, ['inlineContext' => true])`.
            // The inline context disables the ParagraphWrapper so newlines stay
            // literal rather than breaking the caption into `<p>` runs.
            let items = tokenize_caption_items(cap, ctx.config);
            let frag = build_fragment(items);
            out.push(dom_fragment_token(frag, token, fragments, next_id));
        }
        out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
            "figcaption",
            vec![],
            DataParsoid::default(),
        ))));
    }

    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        container_name,
        vec![],
        DataParsoid::default(),
    ))));

    out
}

/// Whether a token array contains a transclusion start marker (a self-closing
/// token whose `typeof` space-separated list includes `mw:Transclusion`).
/// Mirrors PHP `WikiLinkHandler::hasTransclusion`, used to gate `link` options on
/// `is_array($origOptSrc) && hasTransclusion($origOptSrc)`.
fn contains_transclusion(items: &[Item]) -> bool {
    items.iter().any(|item| {
        if let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = item {
            tk.attribs.iter().any(|kv| {
                kv.key.as_str() == Some("typeof")
                    && kv
                        .value
                        .as_str()
                        .is_some_and(|ty| ty.split_whitespace().any(|t| t == "mw:Transclusion"))
            })
        } else {
            false
        }
    })
}

/// Resolve a single option string, apply it to `opts`, and record its
/// round-trip entry in `opt_list` (the `data-parsoid.optList`). Returns `true`
/// when the string is a recognized option (so the caller treats `false` as a
/// caption). Faithful to the option-dispatch loop of PHP `renderFile`.
///
/// `vsrc` is the raw source wikitext for the option (`$oContent->vsrc`), used
/// as the `ak` (aliased key) so whitespace/entities round-trip; `part` is the
/// stringified option text used for recognition.
fn record_media_option(
    ctx: &WikiLinkContext,
    opts: &mut super::media_options::MediaOpts,
    opt_list: &mut Vec<crate::wikitext::tokens_v2::OptListEntry>,
    part: &str,
    vsrc: &str,
    is_token_array: bool,
    has_transclusion: bool,
) -> bool {
    use super::media_options::{
        get_option_info, has_wikitext_markup, is_valid_internal_lang, strip_quote_markers,
    };

    let Some(info) = get_option_info(ctx.config, part) else {
        return false;
    };
    // Whether this option is "expanded" (marks the container `mw:ExpandedAttrs`
    // and stashes its `html` in `data-mw.attribs`). Mirrors PHP `renderFile`:
    //   for `link`, `$expOpt = is_array($origOptSrc) && hasTransclusion(...)`
    //   otherwise, `$expOpt = is_array($origOptSrc)`
    // (the stricter `link` test avoids treating a mere autourl/entity in the link
    // target as an editable expansion).
    let exp_opt = if info.ck == "link" {
        is_token_array && has_transclusion
    } else {
        is_token_array
    };
    if exp_opt {
        opts.expanded_attrs = true;
    }

    // The optList `ck` is the short canonical name for simple options and the
    // group key for prefix options; `ak` is the source wikitext (mirrors the
    // `$opt = ['ck' => …, 'ak' => …]` construction in PHP `renderFile`).
    let opt_ck = if info.s {
        info.v.clone()
    } else {
        info.ck.clone()
    };
    let opt_ak = vsrc.to_string();

    // First-wins / last-wins dispatch (mirrors the PHP `isset($opts[$ck])` guard
    // plus the `format`/`manualthumb` joint guard and `width`'s last-wins rule).
    //
    // `suppressMediaFormats` (gallery context) makes a `format`/`manualthumb`
    // option `bogus` unless a format was already set (mirrors PHP `renderFile`).
    if ctx.suppress_media_formats()
        && matches!(info.ck.as_str(), "format" | "manualthumb")
        && opts.format.is_none()
        && opts.manualthumb.is_none()
    {
        opt_list.push(crate::wikitext::tokens_v2::OptListEntry {
            ck: Some("bogus".to_string()),
            ak: Some(opt_ak),
            v: None,
        });
        return true;
    }

    match info.ck.as_str() {
        "format" if opts.format.is_none() && opts.manualthumb.is_none() => {
            opts.format = Some(info.v);
        }
        "manualthumb" if opts.manualthumb.is_none() && opts.format.is_none() => {
            opts.manualthumb = Some(info.v);
        }
        "halign" if opts.halign.is_none() => opts.halign = Some(info.v),
        "valign" if opts.valign.is_none() => opts.valign = Some(info.v),
        "border" if opts.border.is_none() => opts.border = Some(info.v),
        "upright" if opts.upright.is_none() => opts.upright = Some(info.v),
        "link" if opts.link.is_none() => {
            let resolved = resolve_wikilink_option(&info.v, true);
            opts.link = Some(strip_quote_markers(&resolved));
        }
        "alt" => {
            opts.expanded_attrs |= has_wikitext_markup(&info.v);
            if opts.alt.is_none() {
                let resolved = resolve_wikilink_option(&info.v, false);
                opts.alt = Some(strip_quote_markers(&resolved));
            }
        }
        "class" if opts.class.is_none() => opts.class = Some(info.v),
        "page" if opts.page.is_none() => opts.page = Some(info.v),
        "lang" if opts.lang.is_none() && is_valid_internal_lang(&info.v) => {
            opts.lang = Some(info.v);
        }
        "width" => {
            // `width` is "last wins" (mirrors PHP's special case): a previous
            // `width` entry becomes bogus.
            for entry in opt_list.iter_mut() {
                if entry.ck.as_deref() == Some("width") {
                    entry.ck = Some("bogus".to_string());
                    break;
                }
            }
            if let Some((w, h)) = info.v.split_once('x') {
                opts.width = Some(w.to_string());
                opts.height = Some(h.to_string());
            } else {
                opts.width = Some(info.v);
            }
        }
        // A duplicate (or invalid-lang) option becomes a `bogus` optList entry.
        _ => {
            opt_list.push(crate::wikitext::tokens_v2::OptListEntry {
                ck: Some("bogus".to_string()),
                ak: Some(opt_ak),
                v: None,
            });
            return true;
        }
    }

    opt_list.push(crate::wikitext::tokens_v2::OptListEntry {
        ck: Some(opt_ck),
        ak: Some(opt_ak),
        v: None,
    });
    true
}

/// A `bogus` optList entry carrying the given source (`ak`) — used to mark
/// displaced captions/options for faithful serialization (mirrors PHP's
/// `['ck' => 'bogus', 'ak' => …]` entries).
fn bogus_opt(ak: &str) -> crate::wikitext::tokens_v2::OptListEntry {
    crate::wikitext::tokens_v2::OptListEntry {
        ck: Some("bogus".to_string()),
        ak: Some(ak.to_string()),
        v: None,
    }
}

/// Resolve wikilink syntax (`[[target|display]]`) inside a `link`/`alt` option
/// value to a plain string: the target for `link` (`is_link`), the display text
/// for `alt`. Non-wikilink values are returned unchanged. Faithful to the
/// `mw:WikiLink`/`mw:WikiLink/Interwiki` branches of PHP's
/// `stringifyOptionTokens` (which, for a *local* wikilink, capture the content).
pub(crate) fn resolve_wikilink_option(value: &str, is_link: bool) -> String {
    let trimmed = value.trim();
    if !trimmed.starts_with("[[") || !trimmed.ends_with("]]") {
        return value.to_string();
    }
    let inner = &trimmed[2..trimmed.len() - 2];
    // A piped link: `[[target|display]]` → target (link) or display (alt).
    if let Some((target, display)) = inner.split_once('|') {
        if is_link {
            target.trim().to_string()
        } else {
            display.trim().to_string()
        }
    } else {
        inner.trim().to_string()
    }
}

/// Register a pre-built inline caption fragment in `fragments` and return the
/// `mw:dom-fragment-token` placeholder that the tree builder splices in its
/// place (mirrors `PipelineUtils::getDOMFragmentToken` + `tunnelDOMThroughTokens`).
/// Wrap a fragment into an `mw:dom-fragment-token` self-closing token.
/// Mirrors PHP's `PipelineUtils::getDOMFragmentToken`.
pub(crate) fn dom_fragment_token(
    frag: crate::dom::node::Node,
    token: &ParsoidToken,
    fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
    next_id: &mut usize,
) -> Item {
    let id = *next_id;
    *next_id += 1;
    fragments.insert(id, frag);

    let dp = token.data_parsoid().cloned().unwrap_or_default();
    let mut frag_tok = SelfclosingTagTk::new("mw:dom-fragment-token", vec![], dp);
    frag_tok.attribs.push(KV {
        key: crate::wikitext::tokens_v2::KeyValue::Str("data-fragment-id".to_string()),
        value: crate::wikitext::tokens_v2::KeyValue::Str(id.to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });
    Item::Tok(ParsoidToken::SelfclosingTag(frag_tok))
}

/// Dispatch a wikilink token to the correct renderer based on the target's
/// namespace. Mirrors `WikiLinkHandler::wikiLinkHandler`.
///
/// When `is_redirect` is set, media/file/category targets are forced to render
/// as a plain wikilink (mirrors the `redirect=true` token flag in PHP, which
/// short-circuits those namespace special-cases).
pub fn render_wiki_link_dispatched(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
    is_redirect: bool,
    fragments: &mut std::collections::HashMap<usize, crate::dom::node::Node>,
    next_id: &mut usize,
    build_fragment: CaptionFragmentBuilder,
) -> Vec<Item> {
    if let Some(title) = &target.title {
        if is_redirect {
            return render_wiki_link(ctx, token, target);
        }

        let media_ns = ctx.config.canonical_namespace_id("Media");
        let file_ns = ctx.config.canonical_namespace_id("File");
        let category_ns = ctx.config.canonical_namespace_id("Category");

        if Some(title.namespace_id) == media_ns {
            return render_media(ctx, token, target);
        }
        if !target.from_colon_escaped_text && !target.href.starts_with('#') {
            if Some(title.namespace_id) == file_ns {
                return render_file(ctx, token, target, fragments, next_id, build_fragment);
            }
            if Some(title.namespace_id) == category_ns {
                return render_category(ctx, token, target);
            }
        }
        return render_wiki_link(ctx, token, target);
    }

    if target.interwiki.is_some() {
        return render_interwiki_link(ctx, token, target);
    }
    if target.language.is_some() {
        return render_language_link(ctx, token, target);
    }

    render_wiki_link(ctx, token, target)
}

/// Render a `mw:redirect` self-closing token into a
/// `<link rel="mw:PageProp/redirect" href="..."/>` token. Faithful port of
/// `WikiLinkHandler::onRedirect`.
///
/// The redirect token carries the raw target in its `href` attribute (set by
/// the tokenizer). We render an embedded wikilink against that target to
/// obtain the normalized href (e.g. `./Target`), then emit the `link` token
/// with `rel=mw:PageProp/redirect` and that normalized href.
pub fn render_redirect(ctx: &mut WikiLinkContext, token: &ParsoidToken) -> Vec<Item> {
    use crate::wikitext::token_utils::key_value_to_string;

    let href = token
        .get_attribs()
        .iter()
        .find(|kv| kv.key.as_str() == Some("href"))
        .map(|kv| key_value_to_string(&kv.value))
        .unwrap_or_default();
    let target =
        get_wiki_link_target_info(ctx, &href, &href).unwrap_or_else(|_| WikiLinkTargetInfo {
            href: href.clone(),
            href_src: href.clone(),
            title: Some(crate::title::Title::new_main(href.clone())),
            interwiki: None,
            language: None,
            local_prefix: None,
            from_colon_escaped_text: false,
            prefix: None,
        });

    // An empty redirect target (e.g. `#REDIRECT [[]]`) is invalid. Mirror PHP's
    // `onRedirect` bail-out: re-emit the redirect source as a `#` list item
    // (the leading `#` becomes the list bullet), so it renders as
    // <ol><li>REDIRECT [[]]</li></ol>.
    if href.trim().is_empty() {
        return bail_redirect_as_list_item(token);
    }

    // Synthesize the embedded wikilink token. It carries the same attributes
    // (notably `href`) as the redirect token; the `redirect` flag is handled
    // separately by the renderer via the `is_redirect` parameter.
    let wikilink = SelfclosingTagTk::new(
        "wikilink",
        token.get_attribs().to_vec(),
        DataParsoid::default(),
    );
    let wikilink_token = ParsoidToken::SelfclosingTag(wikilink);

    let rendered = render_wiki_link_dispatched(
        ctx,
        &wikilink_token,
        &target,
        true,
        &mut std::collections::HashMap::new(),
        &mut 0usize,
        &mut |_| crate::dom::node::Node::document(),
    );

    // Extract the normalized href from the rendered first token (an `<a>` or
    // `<link>`), mirroring PHP's `$da->a['href']`.
    let normalized_href = rendered
        .first()
        .and_then(|item| match item {
            Item::Tok(t) => t.get_attribute_v("href").map(|s| s.to_string()),
            Item::Str(_) => None,
        })
        .unwrap_or_else(|| href.clone());

    // Build the `<link rel="mw:PageProp/redirect" href="..."/>` token,
    // preserving the redirect token's data-parsoid and any `about`/`typeof`
    // attributes added by attribute expansion (`mw:ExpandedAttrs`).
    let dp = token.data_parsoid().cloned().unwrap_or_default();
    let mut link = SelfclosingTagTk::new("link", vec![], dp);

    // Retain `about` and `typeof` from the (templated) redirect token, in the
    // order PHP emits them (about, typeof, then rel, then href).
    if let Some(about) = token.get_attribute_v("about").map(|s| s.to_string()) {
        link.add_attribute_str("about", &about);
    }
    if let Some(type_of) = token.get_attribute_v("typeof").map(|s| s.to_string()) {
        link.add_attribute_str("typeof", &type_of);
    }
    link.add_attribute_str("rel", "mw:PageProp/redirect");
    link.add_attribute_str("href", &normalized_href);

    vec![Item::Tok(ParsoidToken::SelfclosingTag(link))]
}

/// Reconstruct an invalid redirect as a `#` list item, mirroring PHP's
/// `WikiLinkHandler::onRedirect` bail-out. The redirect token's `src` is the
/// redirect word (e.g. `#REDIRECT `); we strip the leading `#` and re-append the
/// original (empty) wikilink target to reconstruct `REDIRECT [[]]` content.
fn bail_redirect_as_list_item(token: &ParsoidToken) -> Vec<Item> {
    let dp = token.data_parsoid().cloned().unwrap_or_default();
    let src = dp.src.clone().unwrap_or_default();

    // Reconstruct the list-item content: the redirect word minus the leading
    // `#`, plus `[[<target>]]`.
    let word = src.strip_prefix('#').unwrap_or(&src);
    let content = format!("{word}[[]]");

    // A `listItem` with the `#` bullet.
    let mut li = TagTk::new("listItem", vec![], DataParsoid::default());
    li.add_attribute_str("bullets", "#");

    vec![Item::Tok(ParsoidToken::Tag(li)), Item::Str(content)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;
    use crate::wikitext::tokens_v2::{KeyValue, SelfclosingTagTk};

    /// Build a `wikilink` self-closing token with `href` and optional content.
    fn wikilink_token(href: &str, maybe_content: Option<&str>) -> ParsoidToken {
        let mut tk = SelfclosingTagTk::new("wikilink", vec![], DataParsoid::default());
        tk.attribs.push(KV {
            key: KeyValue::Str("href".to_string()),
            value: KeyValue::Str(href.to_string()),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
        if let Some(content) = maybe_content {
            tk.attribs.push(KV {
                key: KeyValue::Str("mw:maybeContent".to_string()),
                value: KeyValue::Str(content.to_string()),
                src_offsets: None,
                ksrc: None,
                vsrc: None,
            });
        }
        ParsoidToken::SelfclosingTag(tk)
    }

    fn config_static() -> &'static dyn SiteConfig {
        // Leak a single shared config.
        static CONFIG: once_cell::sync::Lazy<MockSiteConfig> =
            once_cell::sync::Lazy::new(MockSiteConfig::new);
        &*CONFIG
    }

    #[test]
    fn test_simple_link_renders_a_tag() {
        let mut ctx = WikiLinkContext::new(config_static());
        let token = wikilink_token("Foo", None);
        let target = get_wiki_link_target_info(&ctx, "Foo", "Foo").unwrap();

        let out = render_wiki_link(&mut ctx, &token, &target);

        // First item is <a> with rel/href/title.
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "a"));
        assert!(matches!(
            out.last(),
            Some(Item::Tok(ParsoidToken::EndTag(t))) if t.name == "a"
        ));

        if let Item::Tok(ParsoidToken::Tag(t)) = &out[0] {
            let href = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .and_then(|kv| kv.value.as_str());
            let title = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("title"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(href, Some("./Foo"));
            assert_eq!(title, Some("Foo"));
        }
    }

    #[test]
    fn test_namespaced_link() {
        let ctx = WikiLinkContext::new(config_static());
        let target = get_wiki_link_target_info(&ctx, "Template:Foo", "Template:Foo").unwrap();

        assert!(target.title.is_some());
        let title = target.title.unwrap();
        assert_eq!(title.namespace_id, 10);
        assert_eq!(title.text, "Foo");
    }

    #[test]
    fn test_interwiki_classification() {
        let ctx = WikiLinkContext::new(config_static());
        // "wikipedia" is a non-language interwiki (no `language` field).
        let target = get_wiki_link_target_info(&ctx, "wikipedia:Foo", "wikipedia:Foo").unwrap();
        assert!(target.interwiki.is_some());
        assert!(target.language.is_none());

        // "de" is a language prefix (has `language` field).
        let target = get_wiki_link_target_info(&ctx, "de:Foo", "de:Foo").unwrap();
        assert!(target.language.is_some());
        assert!(target.interwiki.is_none());
    }

    #[test]
    fn test_render_interwiki_link() {
        let mut ctx = WikiLinkContext::new(config_static());
        let token = wikilink_token("wikipedia:Foo", None);
        let target = get_wiki_link_target_info(&ctx, "wikipedia:Foo", "wikipedia:Foo").unwrap();
        let out = render_interwiki_link(&mut ctx, &token, &target);

        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "a"));
        if let Item::Tok(ParsoidToken::Tag(t)) = &out[0] {
            let href = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(href, Some("http://en.wikipedia.org/wiki/Foo"));
        }
    }

    #[test]
    fn test_render_language_link() {
        let mut ctx = WikiLinkContext::new(config_static());
        let token = wikilink_token("de:Foo", None);
        let target = get_wiki_link_target_info(&ctx, "de:Foo", "de:Foo").unwrap();
        let out = render_language_link(&mut ctx, &token, &target);

        assert!(matches!(&out[0], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "link"));
        if let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &out[0] {
            let href = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .and_then(|kv| kv.value.as_str());
            // Language links are protocol-relative (protorel strips the scheme).
            assert_eq!(href, Some("//de.wikipedia.org/wiki/Foo"));
        }
        // Language link should be recorded in metadata.
        assert_eq!(ctx.metadata_mut().language_links.len(), 1);
    }

    #[test]
    fn test_render_category_link() {
        let mut ctx = WikiLinkContext::new(config_static());
        let token = wikilink_token("Category:People", None);
        let target = get_wiki_link_target_info(&ctx, "Category:People", "Category:People").unwrap();

        assert_eq!(target.title.as_ref().unwrap().namespace_id, 14);

        let out = render_category(&mut ctx, &token, &target);

        assert!(matches!(&out[0], Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "link"));
        if let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &out[0] {
            let rel = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("rel"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(rel, Some("mw:PageProp/Category"));

            let href = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(href, Some("./Category:People"));
        }
        // Category should be recorded in metadata.
        assert_eq!(ctx.metadata_mut().categories.len(), 1);
    }

    #[test]
    fn test_render_category_with_sort_key() {
        let mut ctx = WikiLinkContext::new(config_static());
        let token = wikilink_token("Category:People", Some("A sort key"));
        let target = get_wiki_link_target_info(&ctx, "Category:People", "Category:People").unwrap();
        let out = render_category(&mut ctx, &token, &target);

        if let Item::Tok(ParsoidToken::SelfclosingTag(t)) = &out[0] {
            let href = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .and_then(|kv| kv.value.as_str());
            // The sort key should be appended as '#A%20sort%20key'.
            assert_eq!(href, Some("./Category:People#A%20sort%20key"));
        }
    }

    #[test]
    fn test_media_namespace_classification() {
        let ctx = WikiLinkContext::new(config_static());
        assert_eq!(ctx.config.canonical_namespace_id("Media"), Some(-2));
        assert_eq!(ctx.config.canonical_namespace_id("File"), Some(6));
        assert_eq!(ctx.config.canonical_namespace_id("Category"), Some(14));
    }

    #[test]
    fn test_render_media_link() {
        let mut ctx = WikiLinkContext::new(config_static());
        let token = wikilink_token("Media:Foo.jpg", None);
        let target = get_wiki_link_target_info(&ctx, "Media:Foo.jpg", "Media:Foo.jpg").unwrap();
        assert_eq!(target.title.as_ref().unwrap().namespace_id, -2);

        let out = render_media(&mut ctx, &token, &target);

        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "a"));
        if let Item::Tok(ParsoidToken::Tag(t)) = &out[0] {
            let rel = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("rel"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(rel, Some("mw:MediaLink"));
        }
    }

    #[test]
    fn test_render_file_link_with_thumb() {
        let mut ctx = WikiLinkContext::new(config_static());
        let token = wikilink_token("File:Example.jpg", Some("thumb"));
        let target =
            get_wiki_link_target_info(&ctx, "File:Example.jpg", "File:Example.jpg").unwrap();
        assert_eq!(target.title.as_ref().unwrap().namespace_id, 6);

        let out = render_file(
            &mut ctx,
            &token,
            &target,
            &mut std::collections::HashMap::new(),
            &mut 0usize,
            &mut |_| crate::dom::node::Node::document(),
        );

        // With 'thumb' (thumbnail format), the container should be a <figure>
        // with typeof='mw:File/Thumb'.
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "figure"));
        if let Item::Tok(ParsoidToken::Tag(t)) = &out[0] {
            let type_of = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(type_of, Some("mw:File/Thumb"));
        }
        // Block formats include a figcaption.
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "figcaption"))
        );
    }

    #[test]
    fn test_render_file_link() {
        let mut ctx = WikiLinkContext::new(config_static());
        let token = wikilink_token("File:Example.jpg", None);
        let target =
            get_wiki_link_target_info(&ctx, "File:Example.jpg", "File:Example.jpg").unwrap();
        assert_eq!(target.title.as_ref().unwrap().namespace_id, 6);

        let out = render_file(
            &mut ctx,
            &token,
            &target,
            &mut std::collections::HashMap::new(),
            &mut 0usize,
            &mut |_| crate::dom::node::Node::document(),
        );

        // Container is <span typeof="mw:File" class="mw-default-size">.
        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "span"));
        if let Item::Tok(ParsoidToken::Tag(t)) = &out[0] {
            let type_of = t
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("typeof"))
                .and_then(|kv| kv.value.as_str());
            assert_eq!(type_of, Some("mw:File"));
        }
    }

    #[test]
    fn test_render_redirect_empty_target_bails() {
        // An empty redirect target (#REDIRECT [[]]) must bail out to a
        // listItem rather than render as a <link>.
        let mut ctx = WikiLinkContext::new(config_static());
        let mut tk = SelfclosingTagTk::new("mw:redirect", vec![], DataParsoid::default());
        tk.data_parsoid.src = Some("#REDIRECT ".to_string());
        tk.add_attribute_str("href", "");

        let out = render_redirect(&mut ctx, &ParsoidToken::SelfclosingTag(tk));

        assert!(matches!(&out[0], Item::Tok(ParsoidToken::Tag(t)) if t.name == "listItem"));
        assert!(matches!(&out[1], Item::Str(s) if s == "REDIRECT [[]]"));
    }

    #[test]
    fn test_tokenize_caption_plain() {
        // A plain caption with no wikitext constructs re-tokenizes as a single
        // text run.
        let out = tokenize_caption("Caption text", config_static());
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Item::Str(s) if s == "Caption text"));
    }

    #[test]
    fn test_tokenize_caption_quotes() {
        // `''two''` must become `mw-quote` tokens so QuoteTransformer renders
        // `<i>two</i>`.
        let out = tokenize_caption("one ''two'' three", config_static());
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Tok(ParsoidToken::SelfclosingTag(t)) if t.name == "mw-quote")),
            "expected mw-quote tokens: {out:?}"
        );
    }

    #[test]
    fn test_tokenize_caption_entity() {
        // `&#x7C;` must become an `mw:Entity` span.
        let out = tokenize_caption("a &#x7C; b", config_static());
        assert!(
            out.iter()
                .any(|it| matches!(it, Item::Tok(ParsoidToken::Tag(t)) if t.name == "span")),
            "expected mw:Entity span: {out:?}"
        );
    }

    #[test]
    fn test_split_media_options_nested_pipe() {
        // A `|` inside a piped link must not split the options.
        let parts = split_media_options("thumb|text with a [[MeatBall:Link|link]] in it");
        assert_eq!(
            parts,
            vec!["thumb", "text with a [[MeatBall:Link|link]] in it"]
        );
    }

    #[test]
    fn test_split_media_options_simple() {
        let parts = split_media_options("right|Caption text");
        assert_eq!(parts, vec!["right", "Caption text"]);
    }

    #[test]
    fn test_split_media_options_skips_nowiki_pipe() {
        // A `|` inside a `<nowiki>` attribute or body is literal, not a separator.
        let parts = split_media_options("thumb|Test <nowiki bogus=\"attri|bute\"/> 123");
        assert_eq!(
            parts,
            vec!["thumb", "Test <nowiki bogus=\"attri|bute\"/> 123"]
        );
        let parts = split_media_options("thumb|caption <nowiki>a|b</nowiki> tail");
        assert_eq!(parts, vec!["thumb", "caption <nowiki>a|b</nowiki> tail"]);
    }
}
