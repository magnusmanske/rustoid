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
    pub interwiki: Option<String>,
    pub language: Option<String>,
    pub local_prefix: Option<String>,
    pub from_colon_escaped_text: bool,
    pub prefix: Option<String>,
}

/// Normalize and analyze a wikilink target. Mirrors PHP's
/// `getWikiLinkTargetInfo` for the title and colon-escape cases.
pub fn get_wiki_link_target_info(
    ctx: &WikiLinkContext,
    href: &str,
    href_src: &str,
) -> Result<WikiLinkTargetInfo, String> {
    let mut href = href.to_string();
    let mut from_colon_escaped_text = false;

    // Capture the title to resolve before handling colon escape.
    if href.trim_start().starts_with(':') {
        from_colon_escaped_text = true;
        href = href.trim_start().strip_prefix(':').unwrap().to_string();
    }
    if href.starts_with(':') {
        // Multiple colons — caught by the caller as an invalid title.
        return Err("Multiple colons prefixing href.".to_string());
    }

    // Try to classify prefix (namespace or interwiki) then title.
    let title = if let Some((prefix, _)) = crate::pipeline::wiki_link_handler::href_parts(&href) {
        let prefix = prefix.to_string();
        let normalized = prefix.trim().to_string();
        let ns_id = namespace_id(ctx.config, &normalized);
        let interwiki = ctx.config.interwiki_map().get(&normalized);

        if let Some(ns_id) = ns_id {
            // Namespace prefix: build title with that namespace.
            let after = href.strip_prefix(&prefix).and_then(|s| s.strip_prefix(':'));
            let text = after.unwrap_or(&href).to_string();
            Some(Title::new(ns_id, text))
        } else if interwiki.is_some() {
            // Interwiki/language link (not a local title). We don't fully
            // render these yet; signal with a title from the remaining text.
            let after = href.strip_prefix(&prefix).and_then(|s| s.strip_prefix(':'));
            let text = after.unwrap_or(&href).to_string();
            Some(Title::new_main(text))
        } else {
            // No recognized namespace/interwiki prefix — plain mainspace title.
            Some(Title::new_main(href.clone()))
        }
    } else {
        Some(Title::new_main(href.clone()))
    };

    Ok(WikiLinkTargetInfo {
        href,
        href_src: href_src.to_string(),
        title,
        interwiki: None,
        language: None,
        local_prefix: None,
        from_colon_escaped_text,
        prefix: None,
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
}
