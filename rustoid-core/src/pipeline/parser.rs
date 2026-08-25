//! Public `Parser` facade — ties the V2 token pipeline together into a single
//! entry point for wikitext → HTML.
//!
//! This is the Rust port of PHP Parsoid's top-level `Parser`/`Wikitext` entry
//! points, using the V2 token pipeline (`PegTokenizer` → TT2 handlers →
//! `TreeBuilderStage`).

use crate::dom::node::Node;
use crate::error::Result;
use crate::options::ParserOptions;
use crate::pipeline::frame::Frame;
use crate::pipeline::template_encapsulator::{TemplateEncapsulator, template_info_from};
use crate::pipeline::template_handler::{TemplateHandler, resolve_template_target};
use crate::pipeline::tree_builder_stage::TreeBuilderStage;
use crate::title::TitleParser;
use crate::traits::{DataSource, SiteConfig};
use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};
use crate::wikitext::tokens_v2::{Either, Item, ParsoidToken};

type ResolvedTarget = crate::pipeline::template_handler::ResolvedTarget;

/// Locate the `<body>` element in the tree-builder output and wrap its children
/// in `<section>` wrappers (see `pipeline::section_wrapper`).
///
/// No-op when `wrap_sections` is false (the fragment-rendering case).
fn wrap_sections_in_ast(ast: &mut Node, wrap_sections: bool) {
    use crate::dom::node::{ElementKind, NodeKind};

    if !wrap_sections {
        return;
    }

    // The tree builder runs in fragment mode: it produces a synthetic `<html>`
    // whose children are the body content (no `<head>`/`<body>` wrappers). Wrap
    // those children in sections.
    for html in &mut ast.children {
        if let NodeKind::Element(ElementKind::Other(tag)) = &html.kind
            && tag == "html"
        {
            crate::pipeline::section_wrapper::wrap_sections(html);
            return;
        }
    }
}

/// The wikitext parser, bound to a site configuration.
pub struct Parser<'a, C: SiteConfig> {
    config: &'a C,
}

impl<'a, C: SiteConfig> Parser<'a, C> {
    pub fn new(config: &'a C) -> Self {
        Self { config }
    }

    /// Tokenize raw wikitext into the V2 `Item` stream.
    fn tokenize(&self, wikitext: &str) -> Result<Vec<Item>> {
        let mut options = TokenizerOptions {
            magic_links: crate::wikitext::tokenizer_v2::MagicLinkConfig {
                rfc: self.config.magic_link_enabled("RFC"),
                pmid: self.config.magic_link_enabled("PMID"),
                isbn: self.config.magic_link_enabled("ISBN"),
            },
            ..TokenizerOptions::default()
        };
        // Localized synonyms for the `redirect` magic word (each including the
        // leading `#`), mirroring PHP's `getMagicWordMatcher( 'redirect' )`.
        if let Some(entry) = self.config.magic_words().get("redirect") {
            options.redirect_words = entry.aliases.clone();
        }
        options.ext_tags = self.config.extension_tags().to_vec();
        let mut tokenizer = PegTokenizer::new(wikitext, &options);
        let chunks = tokenizer.tokenize()?;
        Ok(chunks
            .into_iter()
            .map(|e| match e {
                Either::Left(s) => Item::Str(s),
                Either::Right(t) => Item::Tok(t),
            })
            .collect())
    }

    fn new_about_id(&self, counter: &std::cell::Cell<usize>) -> String {
        // PHP Parsoid numbers transclusion `about` ids starting from 1.
        let id = counter.get() + 1;
        counter.set(id);
        format!("#mwt{id}")
    }

    /// Expand `wikilink` self-closing tokens into `<a>`/`<link>` tag sequences
    /// (mirrors the TT2 `WikiLinkHandler`, whose rendering path lives in
    /// `pipeline::wiki_link_render`).
    fn render_links(&self, tokens: Vec<Item>) -> Vec<Item> {
        use crate::pipeline::wiki_link_render::{
            WikiLinkContext, get_wiki_link_target_info, render_redirect,
            render_wiki_link_dispatched,
        };

        let mut ctx = WikiLinkContext::new(self.config);
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = &item else {
                out.push(item);
                continue;
            };

            // `mw:redirect` is handled separately: it becomes a single
            // `<link rel="mw:PageProp/redirect" .../>` token.
            if stt.name == "mw:redirect" {
                out.extend(render_redirect(
                    &mut ctx,
                    &ParsoidToken::SelfclosingTag(stt.clone()),
                ));
                continue;
            }

            if stt.name != "wikilink" {
                out.push(item);
                continue;
            }

            let href = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .and_then(|kv| kv.value.as_str())
                .unwrap_or("")
                .to_string();
            let href_src = href.clone();
            let target = get_wiki_link_target_info(&ctx, &href, &href_src).unwrap_or_else(|_| {
                crate::pipeline::wiki_link_render::WikiLinkTargetInfo {
                    href: href.clone(),
                    href_src: href_src.clone(),
                    title: Some(crate::title::Title::new_main(href.clone())),
                    interwiki: None,
                    language: None,
                    local_prefix: None,
                    from_colon_escaped_text: false,
                    prefix: None,
                }
            });
            let rendered = render_wiki_link_dispatched(
                &mut ctx,
                &ParsoidToken::SelfclosingTag(stt.clone()),
                &target,
                false,
            );
            out.extend(rendered);
        }

        out
    }

    /// Expand `extlink`/`urllink` self-closing tokens into `<a>`/`<img>` tag
    /// sequences (mirrors the TT2 `ExternalLinkHandler`).
    fn render_external_links(&self, tokens: Vec<Item>) -> Vec<Item> {
        use crate::pipeline::external_link_handler::{on_ext_link, on_url_link};

        let mut out: Vec<Item> = Vec::new();
        for item in tokens {
            let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = &item else {
                out.push(item);
                continue;
            };

            let clean = |href: &str| {
                crate::sanitizer::clean_url(href, "external", |proto| {
                    // `split_url` keeps the `//` when present, and no `//` for
                    // scheme-only protocols like `mailto:`/`tel:`.
                    matches!(
                        proto,
                        "http://"
                            | "https://"
                            | "ftp://"
                            | "ftps://"
                            | "mailto:"
                            | "news:"
                            | "irc:"
                            | "ircs:"
                            | "gopher://"
                            | "mms://"
                            | "tel:"
                            | "nntp://"
                            | "//"
                    )
                })
            };

            match stt.name.as_str() {
                "extlink" => {
                    let Some(rendered) = on_ext_link(
                        &ParsoidToken::SelfclosingTag(stt.clone()),
                        clean,
                        self.config.relative_link_prefix(),
                    ) else {
                        out.push(item);
                        continue;
                    };
                    out.extend(rendered);
                }
                "urllink" => {
                    let content_href = stt
                        .attribs
                        .iter()
                        .find(|kv| kv.key.as_str() == Some("href"))
                        .and_then(|kv| kv.value.as_str())
                        .unwrap_or("")
                        .to_string();
                    let Some(rendered) = on_url_link(
                        &ParsoidToken::SelfclosingTag(stt.clone()),
                        &content_href,
                        clean,
                    ) else {
                        out.push(item);
                        continue;
                    };
                    out.extend(rendered);
                }
                _ => out.push(item),
            }
        }
        out
    }

    /// Expand `behavior-switch` tokens into `mw:PageProp` metas (mirrors the
    /// TT2 `BehaviorSwitchHandler`).
    fn render_behavior_switches(&self, tokens: Vec<Item>) -> Vec<Item> {
        crate::pipeline::behavior_switch_handler::BehaviorSwitchHandler.run(tokens)
    }

    /// Convert wikitext to the format-agnostic AST (no template expansion).
    ///
    /// When `wrap_sections` is true, heading content is wrapped in `<section>`
    /// elements (matching PHP's `WrapSections` DOM post-processor in document
    /// mode). Fragment rendering leaves it false.
    pub fn wikitext_to_ast(&self, wikitext: &str, wrap_sections: bool) -> Result<Node> {
        let tokens = self.tokenize(wikitext)?;
        let tokens = self.render_links(tokens);
        let tokens = self.render_external_links(tokens);
        let tokens = self.render_behavior_switches(tokens);
        let stage = TreeBuilderStage::new(false);
        let mut ast = stage.to_ast_with_source(tokens, Some(wikitext));
        crate::pipeline::p_wrap::run(&mut ast);
        crate::pipeline::headings::gen_anchors(&mut ast);
        wrap_sections_in_ast(&mut ast, wrap_sections);
        Ok(ast)
    }

    /// Convert wikitext to an HTML string (no native template expansion).
    pub fn wikitext_to_html(&self, wikitext: &str, options: &ParserOptions) -> Result<String> {
        let ast = self.wikitext_to_ast(wikitext, options.wrap_sections)?;
        let serializer = crate::html::serialize::HtmlSerializer::new(options.clone());
        serializer.serialize(&ast)
    }

    /// Convert wikitext to an HTML string with native template expansion.
    pub async fn wikitext_to_html_expanded(
        &self,
        wikitext: &str,
        source: &dyn DataSource,
        options: &ParserOptions,
    ) -> Result<String> {
        let tokens = self.tokenize(wikitext)?;
        let about_counter = std::cell::Cell::new(0usize);
        let ast = self
            .build_ast(
                tokens,
                Some(source),
                &options.page_title,
                &about_counter,
                wikitext,
                options.wrap_sections,
            )
            .await;
        let serializer = crate::html::serialize::HtmlSerializer::new(options.clone());
        serializer.serialize(&ast)
    }

    /// Run the TT2 stage (template/parser-function/magic-variable expansion)
    /// over a token stream, then the TT3 tree-building stage, producing an AST.
    async fn build_ast(
        &self,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        page_title: &str,
        about_counter: &std::cell::Cell<usize>,
        page_source: &str,
        wrap_sections: bool,
    ) -> Node {
        let title = TitleParser::parse(page_title, self.config);
        let frame = Frame::new(title, vec![]);

        let tokens = self
            .expand_templates(&frame, tokens, source, about_counter)
            .await;
        let tokens = self.render_links(tokens);
        let tokens = self.render_external_links(tokens);
        let tokens = self.render_behavior_switches(tokens);

        let stage = TreeBuilderStage::new(false);
        let mut ast = stage.to_ast_with_source(tokens, Some(page_source));
        crate::pipeline::p_wrap::run(&mut ast);
        crate::pipeline::headings::gen_anchors(&mut ast);
        wrap_sections_in_ast(&mut ast, wrap_sections);
        ast
    }

    /// Expand `template`/`templatearg` tokens in-place.
    async fn expand_templates(
        &self,
        frame: &Frame,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        let mut out = Vec::new();
        for item in tokens {
            let Item::Tok(tok) = &item else {
                out.push(item);
                continue;
            };
            let ParsoidToken::SelfclosingTag(stt) = tok else {
                out.push(item);
                continue;
            };

            if stt.name == "template" || stt.name == "template3" {
                let about_id = self.new_about_id(about_counter);
                let params = crate::pipeline::parser_functions::Params::new(stt.attribs.clone());

                let target_str = stt
                    .attribs
                    .first()
                    .and_then(|kv| kv.key.as_str())
                    .unwrap_or("")
                    .to_string();

                match resolve_template_target(self.config, &target_str) {
                    Some(ResolvedTarget::Template { name, title }) => {
                        let expanded = self
                            .expand_one_template(
                                source,
                                frame,
                                &name,
                                &title,
                                &params,
                                about_id,
                                tok,
                                about_counter,
                            )
                            .await;
                        out.extend(expanded);
                    }
                    _ => {
                        let expanded =
                            TemplateHandler.process(self.config, frame, about_counter, vec![item]);
                        out.extend(expanded);
                    }
                }
                continue;
            }

            if stt.name == "templatearg" {
                let about_id = self.new_about_id(about_counter);
                let name = stt
                    .attribs
                    .first()
                    .and_then(|kv| kv.key.as_str())
                    .unwrap_or("");
                let src = format!("{{{{{name}}}}}");
                let expanded =
                    TemplateHandler.handle_template_arg(frame, &src, about_id, tok, true);
                out.extend(expanded);
                continue;
            }

            out.push(item);
        }
        out
    }

    /// Fetch, expand, and recursively re-process a single template. Mirrors the
    /// native template expansion path (`fetchTemplateAndTitle` +
    /// `processTemplateSource`), then recursively runs the fetched+substituted
    /// source through template expansion with a child frame carrying the
    /// template's arguments.
    #[allow(clippy::too_many_arguments)]
    async fn expand_one_template(
        &self,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        name: &str,
        title: &crate::title::Title,
        params: &crate::pipeline::parser_functions::Params,
        about_id: String,
        token: &ParsoidToken,
        about_counter: &std::cell::Cell<usize>,
    ) -> Vec<Item> {
        const MAX_TEMPLATE_DEPTH: usize = 40;

        // Enforce loop / depth constraints.
        if let Some(err) = crate::pipeline::template_handler::enforce_template_constraints(
            frame,
            name,
            title,
            MAX_TEMPLATE_DEPTH,
            false,
        ) {
            return err;
        }

        // Without a data source, a template becomes a redlink.
        let Some(src) = source else {
            let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
            let info = template_info_from(None, Some(name), vec![]);
            return encap.encap_tokens(
                vec![crate::pipeline::template_handler::template_to_wikilink(
                    name,
                )],
                &info,
            );
        };

        let fetched = src.get_template(title).await.ok().flatten();
        let Some(template_src) = fetched else {
            let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
            let info = template_info_from(None, Some(name), vec![]);
            return encap.encap_tokens(
                vec![crate::pipeline::template_handler::template_to_wikilink(
                    name,
                )],
                &info,
            );
        };

        // Build a child frame carrying the template's arguments (params[1..]).
        let child_args: Vec<crate::wikitext::tokens_v2::KV> =
            params.args.iter().skip(1).cloned().collect();
        let child_frame = frame.new_child(title.clone(), child_args);

        // Substitute the template's arguments into its source.
        use crate::expand::transclusion::TemplateInvocation;
        use crate::wikitext::token_utils::key_value_to_string;
        let mut positional_args = Vec::new();
        let mut named_args = std::collections::HashMap::new();
        for kv in params.args.iter().skip(1) {
            let k = key_value_to_string(&kv.key);
            let v = key_value_to_string(&kv.value);
            if k.trim().is_empty() {
                positional_args.push(v);
            } else {
                named_args.insert(k.trim().to_string(), v);
            }
        }
        let invocation = TemplateInvocation {
            name: name.to_string(),
            positional_args,
            named_args,
        };
        let substituted = crate::expand::transclusion::substitute_args(
            &template_src,
            &invocation.to_template_args(),
            40,
        )
        .unwrap_or(template_src);

        // Re-tokenize and recursively expand the substituted source with the
        // child frame.
        let items = crate::pipeline::template_handler::tokenize_wikitext_to_items(
            &substituted,
            /* in_template */ true,
        );
        let expanded =
            Box::pin(self.expand_templates(&child_frame, items, Some(src), about_counter)).await;

        let encap = TemplateEncapsulator::new("mw:Transclusion", about_id, token);
        let mut info = template_info_from(None, Some(name), vec![]);
        info.param_infos = crate::pipeline::template_encapsulator::prepare_tpl_param_infos(params);
        encap.encap_tokens(expanded, &info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;
    use crate::options::ParserOptions;

    #[test]
    fn test_wikitext_to_html_heading() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("== Heading ==\n", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<h2"), "got: {html}");
        assert!(html.contains("Heading"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_nowiki() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        // `<nowiki>` renders as `<span typeof="mw:Nowiki">` with raw escaped text.
        let html = parser
            .wikitext_to_html("<nowiki>hi</nowiki>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Nowiki\""), "got: {html}");
        assert!(html.contains(">hi</span>"), "got: {html}");

        // The nested `</pre>` is escaped, not treated as a tag.
        let html = parser
            .wikitext_to_html("<nowiki></pre></nowiki>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("&lt;/pre>"), "got: {html}");
        assert!(!html.contains("<pre>"), "got: {html}");
    }

    #[test]
    fn test_attr_sanitization() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        // `onmouseover` is not in the `<pre>` whitelist and must be dropped;
        // `width` is allowed.
        let html = parser
            .wikitext_to_html(
                "<pre width=\"8\" onmouseover=\"alert()\">x</pre>",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("width=\"8\""), "got: {html}");
        assert!(!html.contains("onmouseover"), "got: {html}");
    }

    #[test]
    fn test_pre_entity_and_style() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        // Entities inside `<pre>` decode to plain text (no `mw:Entity` span).
        let html = parser
            .wikitext_to_html("<pre>&lt;</pre>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<pre"), "got: {html}");
        assert!(html.contains("&lt;"), "got: {html}");
        assert!(!html.contains("mw:Entity"), "got: {html}");

        // Insecure `style` is replaced by a marker comment, not dropped.
        let html = parser
            .wikitext_to_html(
                "<pre style=\"border-width: expression(alert())\">x</pre>",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("/* insecure input */"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_bold() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("'''bold'''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<b>"), "got: {html}");
        assert!(html.contains("bold"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_wikilink() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("[[Main Page]]", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<a"), "got: {html}");
        assert!(html.contains("rel=\"mw:WikiLink\""), "got: {html}");
        assert!(html.contains("Main Page"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_extlink() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "[https://example.com Example]",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<a"), "got: {html}");
        assert!(html.contains("rel=\"mw:ExtLink\""), "got: {html}");
        assert!(html.contains("https://example.com"), "got: {html}");
        assert!(html.contains("Example"), "got: {html}");
        // The structural `<html>` wrapper must appear exactly once (the
        // tree-builder fragment must not be nested inside another wrapper).
        assert_eq!(html.matches("<html").count(), 1, "got: {html}");
        assert_eq!(html.matches("<body").count(), 1, "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_bare_url() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "See https://example.com now",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("rel=\"mw:ExtLink\""), "got: {html}");
        assert!(html.contains("https://example.com"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_behavior_switch() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("__TOC__", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("mw:PageProp/toc"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_italic() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("''italic''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<i>"), "got: {html}");
        assert!(html.contains("italic"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_bold_italic() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("'''''both'''''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<b>"), "got: {html}");
        assert!(html.contains("<i>"), "got: {html}");
        assert!(html.contains("both"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_unordered_list() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("* one\n* two", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<ul"), "got: {html}");
        assert!(html.contains("<li"), "got: {html}");
        assert!(html.contains("one"), "got: {html}");
        assert!(html.contains("two"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_paragraph_break() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("First\n\nSecond", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("First"), "got: {html}");
        assert!(html.contains("Second"), "got: {html}");
        assert!(html.matches("<p>").count() >= 2, "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_definition_list() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(";term:definition", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<dl"), "got: {html}");
        assert!(html.contains("<dt"), "got: {html}");
        assert!(html.contains("<dd"), "got: {html}");
        assert!(html.contains("term"), "got: {html}");
        assert!(html.contains("definition"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_nested_list() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("* a\n** b", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<ul"), "got: {html}");
        assert!(html.contains("a"), "got: {html}");
        assert!(html.contains("b"), "got: {html}");
        // Nested bullet means two nested <ul> elements.
        assert!(html.matches("<ul").count() >= 2, "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_ordered_list() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("# one\n# two", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<ol"), "got: {html}");
        assert!(html.contains("<li"), "got: {html}");
        assert!(html.contains("one"), "got: {html}");
        assert!(html.contains("two"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_multi_line_dl() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(";term\n:definition", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<dl"), "got: {html}");
        assert!(html.contains("<dt"), "got: {html}");
        assert!(html.contains("<dd"), "got: {html}");
        assert!(html.contains("term"), "got: {html}");
        assert!(html.contains("definition"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_heading_with_link() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("== See [[Main Page]] ==", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<h2"), "got: {html}");
        assert!(html.contains("<a"), "got: {html}");
        assert!(html.contains("rel=\"mw:WikiLink\""), "got: {html}");
        assert!(html.contains("Main Page"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_wikitext_table() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("{|\n|-\n| cell\n|}", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<table"), "got: {html}");
        assert!(html.contains("<tr"), "got: {html}");
        assert!(html.contains("<td"), "got: {html}");
        assert!(html.contains("cell"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_html_table() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "<table><tr><td>cell</td></tr></table>",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<table"), "got: {html}");
        assert!(html.contains("<tr"), "got: {html}");
        assert!(html.contains("<td"), "got: {html}");
        assert!(html.contains("cell"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_table_header() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("{|\n! header\n|}", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<table"), "got: {html}");
        assert!(html.contains("<th"), "got: {html}");
        assert!(html.contains("header"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_table_multi_cell() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("{|\n| a || b\n|}", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<table"), "got: {html}");
        assert!(html.contains("a"), "got: {html}");
        assert!(html.contains("b"), "got: {html}");
        assert!(html.matches("<td").count() >= 2, "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_redirect() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("#redirect [[Target]]", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(
            html.contains(r#"rel="mw:PageProp/redirect""#),
            "got: {html}"
        );
        assert!(html.contains(r#"href="./Target""#), "got: {html}");
        assert!(!html.contains("<mw:redirect"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_redirect_piped_target() {
        // The redirect target is the part before the `|`; the link label is
        // ignored (matches PHP, which renders the target only).
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "#REDIRECT [[Target|label]]",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains(r#"href="./Target""#), "got: {html}");
        assert!(!html.contains("label"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_table_caption() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "{|\n|+ A caption\n|-\n| cell\n|}",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<caption"), "got: {html}");
        assert!(html.contains("A caption"), "got: {html}");
        assert!(html.contains("cell"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_table_cell_attrs() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "{|\n|-\n| style=\"color:red\" | cell\n|}",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<td"), "got: {html}");
        assert!(html.contains("color:red"), "got: {html}");
        assert!(html.contains("cell"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_hr() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("----", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<hr"), "got: {html}");
    }

    #[test]
    fn test_wikitext_literal_html_tag_stx() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("<div>foo</div>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<div"), "got: {html}");
        // Literal HTML tags carry stx:"html" in data-parsoid.
        assert!(
            html.contains("\"stx\":\"html\""),
            "expected stx:html in: {html}"
        );
    }

    #[tokio::test]
    async fn test_wikitext_to_html_template() {
        use crate::mock::MockDataSource;

        let source = MockDataSource::new();
        source.add_template("Template:Foo", "Hello {{{1}}}!");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        let html = parser
            .wikitext_to_html_expanded("{{Foo|world}}", &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        assert!(html.contains("Hello world"), "got: {html}");
        // The transclusion should carry a `data-mw` marker.
        assert!(html.contains("data-mw"), "expected data-mw in: {html}");
        // The transclusion is encapsulated in a `<span about=... typeof="mw:Transclusion">`.
        assert!(
            html.contains("typeof=\"mw:Transclusion\""),
            "expected mw:Transclusion span in: {html}"
        );
        assert!(
            html.contains("about=\"#mwt1\""),
            "expected about=#mwt1 in: {html}"
        );
    }

    #[tokio::test]
    async fn test_template_expands_to_list_encapsulation() {
        use crate::mock::MockDataSource;
        let source = MockDataSource::new();
        source.add_template("Template:1x", "{{{1}}}");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        // A template expanding to list syntax gets fostered out of the list;
        // the transclusion `about`/`typeof` must be transferred onto the `<ul>`.
        let html = parser
            .wikitext_to_html_expanded("{{1x|*bar}}", &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        assert!(
            html.contains("<ul about=\"#mwt1\" typeof=\"mw:Transclusion\""),
            "got: {html}"
        );
        assert!(html.contains("<li"), "got: {html}");
        assert!(html.contains(">bar</li>"), "got: {html}");
        assert!(!html.contains("mw:Transclusion/End"), "got: {html}");
    }

    #[tokio::test]
    async fn test_wikitext_nested_template() {
        use crate::mock::MockDataSource;

        let source = MockDataSource::new();
        source.add_template("Template:Outer", "{{Inner|world}}");
        source.add_template("Template:Inner", "Hello {{{1}}}!");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        let html = parser
            .wikitext_to_html_expanded("{{Outer}}", &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        // The nested `{{Inner|world}}` should expand to "Hello world!".
        assert!(html.contains("Hello world"), "got: {html}");
    }

    #[tokio::test]
    async fn test_wikitext_self_referential_template() {
        use crate::mock::MockDataSource;

        let source = MockDataSource::new();
        // A self-referential template would infinitely recurse without a
        // loop/depth guard.
        source.add_template("Template:Loop", "{{Loop}}");
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);

        let html = parser
            .wikitext_to_html_expanded("{{Loop}}", &source, &ParserOptions::for_page("Test"))
            .await
            .unwrap();
        // The loop is detected and an error is emitted rather than hanging.
        assert!(
            html.contains("Template loop detected") || html.contains("limit exceeded"),
            "got: {html}"
        );
    }

    #[test]
    fn test_wikitext_to_html_entity_named() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("A &amp; B", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
        // The decoded `&` is HTML-escaped as `&amp;` inside the span.
        assert!(
            html.contains("> &amp;</span>") || html.contains(">&amp;</span>"),
            "got: {html}"
        );
        // The entity span carries both the raw and decoded source in
        // data-parsoid (src and srcContent), mirroring PHP.
        assert!(html.contains("\"src\":\"&amp;amp;\""), "got: {html}");
        assert!(html.contains("\"srcContent\":\"&amp;\""), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_numeric() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&#169;", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
        assert!(html.contains("©"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_hex() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&#x1F600;", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
        assert!(html.contains("😀"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_unknown_left_literal() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&foo;", &ParserOptions::for_page("Test"))
            .unwrap();
        // Unknown named entities are not wrapped in an mw:Entity span.
        assert!(!html.contains("mw:Entity"), "got: {html}");
        assert!(html.contains("&amp;foo;"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_accented() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&Aacute;", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
        assert!(html.contains("Á"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_entity_two_codepoints() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("&acE;", &ParserOptions::for_page("Test"))
            .unwrap();
        // acE decodes to two codepoints (U+223E U+0333), still wrapped.
        assert!(html.contains("typeof=\"mw:Entity\""), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_magic_link_rfc() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("See RFC 1234 here", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("rel=\"mw:ExtLink\""), "got: {html}");
        assert!(
            html.contains("https://datatracker.ietf.org/doc/html/rfc1234"),
            "got: {html}"
        );
        assert!(html.contains("RFC 1234"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_magic_link_pmid() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("PMID 1234", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("rel=\"mw:ExtLink\""), "got: {html}");
        assert!(
            html.contains("//www.ncbi.nlm.nih.gov/pubmed/1234?dopt=Abstract"),
            "got: {html}"
        );
    }

    #[test]
    fn test_wikitext_to_html_magic_link_isbn() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("ISBN 978-0-123456-47-2", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("rel=\"mw:WikiLink\""), "got: {html}");
        assert!(
            html.contains("Special:BookSources/9780123456472"),
            "got: {html}"
        );
    }

    #[test]
    fn test_wikitext_to_html_section_wrapping() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let mut opts = ParserOptions::for_page("Test");
        opts.wrap_sections = true;
        let html = parser
            .wikitext_to_html("lead\n== Heading ==\nbody\n", &opts)
            .unwrap();
        // There is always a lead section, and each wikitext heading is wrapped.
        assert!(html.contains("data-mw-section-id=\"0\""), "got: {html}");
        assert!(html.contains("data-mw-section-id=\"1\""), "got: {html}");
        assert!(html.contains("<section"), "got: {html}");
        assert!(html.contains("<h2"), "got: {html}");
        assert!(html.contains("Heading"), "got: {html}");
    }

    #[test]
    fn test_wikitext_to_html_nested_section() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let mut opts = ParserOptions::for_page("Test");
        opts.wrap_sections = true;
        let html = parser
            .wikitext_to_html("== A ==\n=== B ===\n", &opts)
            .unwrap();
        assert!(html.contains("data-mw-section-id=\"1\""), "got: {html}");
        assert!(html.contains("data-mw-section-id=\"2\""), "got: {html}");
    }
}
