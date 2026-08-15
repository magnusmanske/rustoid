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
use crate::wikitext::tokens_v2::{DataParsoid, EndTagTk, Item, KV, ParsoidToken, TagTk};

use super::wiki_link_handler::{build_link_attrs, string_kv};

/// A lightweight analogue of PHP's `Env` — wraps a `SiteConfig` plus a small
/// amount of per-parse state (the about-id counter used for transclusions).
pub struct WikiLinkContext<'a> {
    pub config: &'a dyn SiteConfig,
    about_id_counter: usize,
}

impl<'a> WikiLinkContext<'a> {
    pub fn new(config: &'a dyn SiteConfig) -> Self {
        Self {
            config,
            about_id_counter: 0,
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
) -> (Vec<KV>, Vec<Item>, String) {
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
                crate::wikitext::tokens_v2::KeyValue::Tokens(t) => {
                    t.iter().cloned().map(Item::Tok).collect()
                }
            };
            out.extend(toks);
            if i < content_kvs.len() - 1 {
                out.push(Item::Str("|".to_string()));
            }
        }

        // stx = 'piped' for round-tripping.
        let mut dp = data_parsoid.clone();
        dp.stx = Some("piped".to_string());
        (new_attr_data.attribs, out, "piped".to_string())
    } else {
        // No explicit link text; derive it from the title.
        let morecontent = target
            .title
            .as_ref()
            .map(|t| t.get_prefixed_text())
            .unwrap_or_else(|| target.href.clone());

        let mut dp = data_parsoid.clone();
        dp.stx = Some("simple".to_string());
        (
            new_attr_data.attribs,
            vec![Item::Str(morecontent)],
            "simple".to_string(),
        )
    }
}

/// Render a plain wiki link into a sequence of items: `<a rel="mw:WikiLink">`
/// ... content ... `</a>`. Faithful port of `WikiLinkHandler::renderWikiLink`.
pub fn render_wiki_link(
    ctx: &mut WikiLinkContext,
    token: &ParsoidToken,
    target: &WikiLinkTargetInfo,
) -> Vec<Item> {
    let (attribs, content, _stx) = add_link_attributes_and_get_content(ctx, token, target);

    let mut a_tag = TagTk::new("a", attribs, DataParsoid::default());

    // href = makeLink(title), title = getPrefixedText().
    if let Some(title) = &target.title {
        let href = make_link(title, ctx.config);
        let prefixed = title.get_prefixed_text();
        // Set href and title (normalized attribute semantics approximated).
        a_tag.add_attribute_str("href", &href);
        a_tag.add_attribute_str("title", &prefixed);
    } else {
        a_tag.add_attribute_str("href", &target.href);
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

    let (attribs, content, _stx) = add_link_attributes_and_get_content(ctx, token, target);
    let mut new_tk = TagTk::new("a", attribs, DataParsoid::default());

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

    let (attribs, _content, _stx) = add_link_attributes_and_get_content(ctx, token, target);
    let mut new_tk =
        crate::wikitext::tokens_v2::SelfclosingTagTk::new("link", attribs, DataParsoid::default());

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

    // Change rel to mw:PageProp/Language.
    if let Some(kv) = new_tk
        .attribs
        .iter_mut()
        .find(|kv| kv.key.as_str() == Some("rel"))
    {
        kv.value = crate::wikitext::tokens_v2::KeyValue::Str("mw:PageProp/Language".to_string());
    }

    vec![Item::Tok(ParsoidToken::SelfclosingTag(new_tk))]
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
            assert_eq!(href, Some("https://de.wikipedia.org/wiki/Foo"));
        }
    }
}
