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
    DataParsoid, EndTagTk, Item, KV, ParsoidToken, SelfclosingTagTk, TagTk,
};

use super::wiki_link_handler::{build_link_attrs, string_kv};

/// A lightweight analogue of PHP's `Env` — wraps a `SiteConfig` plus a small
/// amount of per-parse state (the about-id counter used for transclusions).
pub struct WikiLinkContext<'a> {
    pub config: &'a dyn SiteConfig,
    about_id_counter: usize,
    metadata: MetadataCollector,
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
        }
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

    // Capture the (decoded) title before handling colon escape.
    let title_decoded = decode_uri_component(&href);

    if href.trim_start().starts_with(':') {
        from_colon_escaped_text = true;
        href = href.trim_start().strip_prefix(':').unwrap().to_string();
    }
    if href.starts_with(':') {
        // Multiple colons — caught by caller as an invalid title.
        return Err("Multiple colons prefixing href.".to_string());
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

        if let Some(ns_id) = ns_id {
            // Namespace prefix → local title.
            title = Some(Title::new(ns_id, title_part.to_string()));
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
        // No colon → plain mainspace title.
        title = Some(Title::new_main(title_decoded.clone()));
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
        // No explicit link text; derive it from the title.
        let morecontent = target
            .title
            .as_ref()
            .map(|t| t.get_prefixed_text())
            .unwrap_or_else(|| target.href.clone());

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
        let href = make_link(title, ctx.config);
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

    // Add title unless it's just a fragment.
    if target.href.is_empty() || !target.href.starts_with('#') {
        let mut title_attr = format!("{}:", target.prefix.clone().unwrap_or_default());
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
                .map(|t| ctx.config.get_upload_url(&t.get_full_db_key()))
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
) -> Vec<Item> {
    use super::media_options::{MediaOpts, get_format, get_option_info, get_wrapper_info};

    let title = target.title.as_ref().expect("file title");

    // Extract option strings from mw:maybeContent (pipe-separated).
    let mut opts = MediaOpts::default();
    if let Some(content) = token.get_attribute_v("mw:maybeContent") {
        for part in content.split('|') {
            if let Some(info) = get_option_info(ctx.config, part) {
                match info.ck.as_str() {
                    "format" => opts.format = Some(info.v),
                    "manualthumb" => opts.manualthumb = Some(info.v),
                    "halign" => opts.halign = Some(info.v),
                    "valign" => opts.valign = Some(info.v),
                    "border" => opts.border = Some(info.v),
                    "upright" => opts.upright = Some(info.v),
                    "width" => {
                        // Parse WxH (separated by 'x').
                        if let Some((w, h)) = info.v.split_once('x') {
                            opts.width = Some(w.to_string());
                            opts.height = Some(h.to_string());
                        } else {
                            opts.width = Some(info.v);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let format = get_format(&opts);
    let (classes, is_inline) = get_wrapper_info(&opts);

    // rdfa type and container.
    let rdfa_type = match format.as_deref() {
        Some("manualthumb") | Some("thumbnail") => "mw:File/Thumb",
        Some("framed") => "mw:File/Frame",
        Some("frameless") => "mw:File/Frameless",
        _ => "mw:File",
    };

    let container_name = if is_inline { "span" } else { "figure" };

    let mut container_attribs = vec![crate::pipeline::wiki_link_handler::string_kv(
        "typeof", rdfa_type,
    )];
    if !classes.is_empty() {
        container_attribs.insert(
            0,
            crate::pipeline::wiki_link_handler::string_kv("class", &classes.join(" ")),
        );
    }
    let container = TagTk::new(container_name, container_attribs, DataParsoid::default());

    // Anchor wraps the file element.
    let mut anchor = TagTk::new("a", vec![], DataParsoid::default());
    anchor.add_attribute_str("href", ctx.config.get_upload_url(&title.get_full_db_key()));
    anchor.add_attribute_str("class", "new");
    anchor.add_attribute_str("title", title.get_prefixed_text());

    // Inner span (broken media placeholder) with resource/lang/data-* attrs.
    let mut span = TagTk::new("span", vec![], DataParsoid::default());
    span.add_attribute_str("class", "mw-file-element mw-broken-media");
    span.add_attribute_str("resource", make_link(title, ctx.config));
    if let Some(width) = &opts.width {
        span.add_attribute_str("data-width", width);
    }
    if let Some(height) = &opts.height {
        span.add_attribute_str("data-height", height);
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

    // For block formats, add a figcaption (empty) then close the figure.
    if !is_inline {
        out.push(Item::Tok(ParsoidToken::Tag(TagTk::new(
            "figcaption",
            vec![],
            DataParsoid::default(),
        ))));
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
                return render_file(ctx, token, target);
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

    let rendered = render_wiki_link_dispatched(ctx, &wikilink_token, &target, true);

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
            assert_eq!(href, Some("https://en.wikipedia.org/wiki/Foo"));
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

        let out = render_file(&mut ctx, &token, &target);

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

        let out = render_file(&mut ctx, &token, &target);

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
}
