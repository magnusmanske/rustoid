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

/// Extract the raw template target (the text between `{{` and the first
/// top-level `|` or the closing `}}`) from a template token's source string.
///
/// This is the *unprocessed* target as written in the wikitext, before the
/// PHP preprocessor strips comments. Used to detect a comment in the target
/// (`{{f<!---->oo}}`), which suppresses `mw:Transclusion` encapsulation.
fn raw_template_target(src: &str) -> Option<&str> {
    let inner = src.strip_prefix("{{")?.strip_suffix("}}")?;
    Some(inner.split_once('|').map_or(inner, |(t, _)| t))
}

/// If `item` is a `<pre format="wikitext">` extension token, return the
/// self-closing token; otherwise `None`.
fn wikitext_pre_target(item: &Item) -> Option<&crate::wikitext::tokens_v2::SelfclosingTagTk> {
    let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = item else {
        return None;
    };
    if stt.name != "extension" {
        return None;
    }
    let name = stt
        .attribs
        .iter()
        .find(|a| a.key.as_str() == Some("name"))
        .and_then(|a| a.value.as_str());
    let attrs = crate::pipeline::extension_handler::extension_kv_attrs(stt);
    let format = attrs
        .iter()
        .find(|kv| kv.key.as_str() == Some("format"))
        .and_then(|kv| kv.value.as_str());
    if name == Some("pre") && format == Some("wikitext") {
        Some(stt)
    } else {
        None
    }
}

/// Extract the raw body source from a `<pre format="wikitext">` extension token.
fn extension_body(stt: &crate::wikitext::tokens_v2::SelfclosingTagTk) -> String {
    let ext_src = stt
        .attribs
        .iter()
        .find(|a| a.key.as_str() == Some("source"))
        .and_then(|a| a.value.as_str())
        .unwrap_or("");
    crate::pipeline::extension_handler::extract_ext_body(stt, ext_src)
}

/// Emit the `<pre typeof="mw:Extension/pre">` + `mw:dom-fragment-token` +
/// `</pre>` placeholder sequence for a `<pre format="wikitext">` extension,
/// referencing the sub-fragment by `id`.
fn emit_pre_placeholder(
    stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    id: usize,
    out: &mut Vec<Item>,
) {
    use crate::wikitext::tokens_v2::{EndTagTk, KeyValue, SelfclosingTagTk, TagTk};

    let attrs: Vec<crate::wikitext::tokens_v2::KV> =
        crate::pipeline::extension_handler::extension_kv_attrs(stt);
    let sanitized = crate::sanitizer::sanitize_tag_attrs("pre", attrs, |_proto| true);
    let mut dp = stt.data_parsoid.clone();
    dp.src = None;
    dp.src_content = None;
    dp.ext_tag_offsets = None;
    dp.stx = Some("html".to_string());
    let mut pre = TagTk::new("pre", sanitized, dp);
    pre.data_mw = None;
    pre.add_attribute_str("typeof", "mw:Extension/pre");

    let mut frag = SelfclosingTagTk::new("mw:dom-fragment-token", vec![], stt.data_parsoid.clone());
    frag.attribs.push(crate::wikitext::tokens_v2::KV {
        key: KeyValue::Str("data-fragment-id".to_string()),
        value: KeyValue::Str(id.to_string()),
        src_offsets: None,
        ksrc: None,
        vsrc: None,
    });

    out.push(Item::Tok(ParsoidToken::Tag(pre)));
    out.push(Item::Tok(ParsoidToken::SelfclosingTag(frag)));
    out.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
        "pre",
        vec![],
        crate::wikitext::tokens_v2::DataParsoid::default(),
    ))));
}

/// Extract the body-content children from a tree-builder document (`<html>`
/// wrapped), returning them as a fragment document. Mirrors the `body`
/// extraction in `HtmlSerializer::split_structure`.
fn extract_fragment_children(ast: &Node) -> Node {
    for child in &ast.children {
        if let crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::Other(tag)) =
            &child.kind
            && tag == "html"
        {
            let mut frag = crate::dom::node::Node::document();
            frag.children = child.children.clone();
            return frag;
        }
    }
    // No `<html>` wrapper (e.g. a plain text body returned as a text node).
    ast.clone()
}

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
        use crate::wikitext::token_utils::key_value_to_string;

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
                let href = stt
                    .attribs
                    .iter()
                    .find(|kv| kv.key.as_str() == Some("href"))
                    .map(|kv| key_value_to_string(&kv.value))
                    .unwrap_or_default();
                // A redirect to a `<nowiki>` target cannot be rendered as a clean
                // link; mirror PHP's `onRedirect`/`bailTokens` bail-out. A
                // templated target is expanded upstream (AttributeExpander), so
                // by the time we get here any remaining `{{` indicates a failure
                // to expand and must bail.
                if href.contains("<nowiki") {
                    out.extend(self.bail_dirty_redirect(stt, &href));
                } else {
                    out.extend(render_redirect(
                        &mut ctx,
                        &ParsoidToken::SelfclosingTag(stt.clone()),
                    ));
                }
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
                .map(|kv| key_value_to_string(&kv.value))
                .unwrap_or_default();
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

    /// Reconstruct an invalid redirect (target contains `<nowiki>` or a
    /// template) as a `#` list item, mirroring PHP's `onRedirect` bail + `bailTokens`.
    /// The redirect word (minus the leading `#`) is followed by the re-tokenized
    /// wikilink source with its leading `[` restored.
    fn bail_dirty_redirect(
        &self,
        stt: &crate::wikitext::tokens_v2::SelfclosingTagTk,
        href: &str,
    ) -> Vec<Item> {
        // The redirect word source (e.g. `#REDIRECT `).
        let src = stt.data_parsoid.src.clone().unwrap_or_default();
        let word = src.strip_prefix('#').unwrap_or(&src).to_string();

        // Re-tokenize the wikilink inner (`[{href}]]`, mirroring PHP's
        // `bailTokens` which strips the first `[`) and expand `<nowiki>`.
        let re_src = format!("[{href}]]");
        let tokens = self.tokenize(&re_src).unwrap_or_default();
        let mut fragments = std::collections::HashMap::new();
        let mut next_id = 0usize;
        let expanded = crate::pipeline::extension_handler::run(
            tokens,
            self.config,
            &mut fragments,
            &mut next_id,
        );

        let mut li = crate::wikitext::tokens_v2::TagTk::new(
            "listItem",
            vec![],
            crate::wikitext::tokens_v2::DataParsoid::default(),
        );
        li.add_attribute_str("bullets", "#");

        let mut out = vec![
            Item::Tok(ParsoidToken::Tag(li)),
            Item::Str(word),
            Item::Str("[".to_string()),
        ];
        out.extend(expanded);
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
                    self.config.has_valid_protocol(proto)
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

    /// Run an inline sub-pipeline over an extension-tag body and return the
    /// body-content children as a fragment document (mirrors
    /// `PipelineUtils::processContentInPipeline` with `pipelineType
    /// = 'wikitext-to-fragment'` + `inlineContext`).
    ///
    /// When a data source and frame are supplied, nested templates/parser
    /// functions in the body are expanded first (mirroring the
    /// `expandTemplates` parse option of the PHP fragment pipeline).
    async fn process_fragment_body(
        &self,
        body: &str,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> Node {
        let mut tokens = match self.tokenize(body) {
            Ok(t) => t,
            Err(_) => return crate::dom::node::Node::document(),
        };
        // Expand nested templates/parser functions when a data source is
        // available (the synchronous `wikitext_to_ast` path has none).
        if source.is_some() {
            tokens = self
                .expand_templates(frame, tokens, source, about_counter, true)
                .await;
            tokens = self
                .expand_attributes(frame, tokens, source, about_counter, None)
                .await;
        }
        // The quote transformer flushes pending quotes only on a newline or EOF
        // token; append a synthetic EOF so quotes (`'''bold'''`) flush.
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        self.fragment_from_tokens(tokens)
    }

    /// Build an inline fragment document from an already-tokenized (and
    /// optionally template-expanded) token stream.
    fn fragment_from_tokens(&self, tokens: Vec<Item>) -> Node {
        let tokens = self.render_links(tokens);
        let tokens = self.render_external_links(tokens);
        let tokens = self.render_behavior_switches(tokens);
        let stage = TreeBuilderStage::new(true);
        let mut frag = extract_fragment_children(&stage.to_ast(tokens, self.config));
        // Inline fragments have no transclusion encapsulation or p-wrapping, but
        // `mw:DOMFragment` placeholders (nested `format="wikitext"` content) must
        // still be unpacked (mirrors the nested sub-pipeline's `dom-unpack`).
        crate::pipeline::unpack_dom_fragments::run(&mut frag);
        frag
    }

    /// Serialize an attribute key/value source (a token array or plain string)
    /// into a DOM-fragment HTML string, for the `html` field of a
    /// `data-mw.attribs` entry. Mirrors PHP's `PipelineUtils::
    /// expandAttrValueToDOM` (which pipes the value through the
    /// `expanded-tokens-to-fragment` pipeline in inline context and serializes).
    ///
    /// The result carries `data-parsoid`/`data-mw`/`about`/`typeof` intact
    /// (matching Parsoid's round-trippable attribute fragments); the caller
    /// HTML-escapes it when embedding in the `data-mw` JSON envelope.
    fn value_to_dom_html(&self, kv: &crate::wikitext::tokens_v2::KeyValue) -> String {
        use crate::pipeline::attribute_transform_manager::key_value_to_items;

        let items = key_value_to_items(kv);
        let frag = self.fragment_from_tokens(items);
        let serializer =
            crate::html::serialize::HtmlSerializer::new(crate::options::ParserOptions {
                body_only: true,
                ..crate::options::ParserOptions::for_page("")
            });
        serializer.serialize(&frag).unwrap_or_default()
    }

    /// Build an inline fragment document from raw body wikitext, without
    /// template expansion (used by the synchronous `wikitext_to_ast` path).
    fn fragment_from_body(&self, body: &str) -> Node {
        let mut tokens = match self.tokenize(body) {
            Ok(t) => t,
            Err(_) => return crate::dom::node::Node::document(),
        };
        tokens.push(Item::Tok(ParsoidToken::Eof(
            crate::wikitext::tokens_v2::EOFTk,
        )));
        self.fragment_from_tokens(tokens)
    }

    /// Expand `<pre format="wikitext">` extension tokens in place: emit the
    /// `<pre typeof="mw:Extension/pre">` wrapper, and tunnel the body through
    /// the inline sub-pipeline as a `mw:dom-fragment-token` placeholder. Returns
    /// the token stream and a map of fragment id → pre-built sub-`Node`.
    async fn expand_wikitext_pre(
        &self,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        self.expand_wikitext_pre_with(source, frame, about_counter, tokens)
            .await
    }

    /// Synchronous variant of [`expand_wikitext_pre`] for the `wikitext_to_ast`
    /// path, which has no data source and therefore performs no nested-template
    /// expansion.
    fn expand_wikitext_pre_sync(
        &self,
        tokens: Vec<Item>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        let mut fragments = std::collections::HashMap::new();
        let mut next_id = 0usize;
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Some(pre_stt) = wikitext_pre_target(&item) else {
                out.push(item);
                continue;
            };
            let body = extension_body(pre_stt);
            let sub = self.fragment_from_body(&body);
            let id = next_id;
            next_id += 1;
            fragments.insert(id, sub);
            emit_pre_placeholder(pre_stt, id, &mut out);
        }

        (out, fragments)
    }

    /// Shared driver for [`expand_wikitext_pre`] / [`expand_wikitext_pre_sync`]:
    /// route `format="wikitext"` `<pre>` extension bodies through the inline
    /// sub-pipeline, emitting `<pre>` + `mw:dom-fragment-token` placeholders.
    async fn expand_wikitext_pre_with(
        &self,
        source: Option<&dyn DataSource>,
        frame: &Frame,
        about_counter: &std::cell::Cell<usize>,
        tokens: Vec<Item>,
    ) -> (Vec<Item>, std::collections::HashMap<usize, Node>) {
        let mut fragments = std::collections::HashMap::new();
        let mut next_id = 0usize;
        let mut out: Vec<Item> = Vec::new();

        for item in tokens {
            let Some(pre_stt) = wikitext_pre_target(&item) else {
                out.push(item);
                continue;
            };
            let body = extension_body(pre_stt);
            let sub = self
                .process_fragment_body(&body, source, frame, about_counter)
                .await;
            let id = next_id;
            next_id += 1;
            fragments.insert(id, sub);
            emit_pre_placeholder(pre_stt, id, &mut out);
        }

        (out, fragments)
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
        let (tokens, fragments) = self.expand_wikitext_pre_sync(tokens);
        let stage = TreeBuilderStage::new(false);
        let mut ast = stage.to_ast_with_fragments(tokens, Some(wikitext), self.config, fragments);
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
        crate::pipeline::compute_dsr::run(&mut ast, wikitext);
        crate::pipeline::p_wrap::run(&mut ast);
        crate::pipeline::tree_builder_html::post_pwrap_transforms(
            &mut ast,
            &depths,
            Some(wikitext),
        );
        crate::pipeline::cleanup::run(&mut ast);
        crate::pipeline::headings::gen_anchors(&mut ast);
        crate::pipeline::add_link_attributes::run(&mut ast, self.config);
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
        let page_title_prefixed = title.get_prefixed_text();
        let frame = Frame::new(title, vec![]);

        let tokens = self
            .expand_templates(&frame, tokens, source, about_counter, false)
            .await;
        let tokens = self
            .expand_attributes(&frame, tokens, source, about_counter, Some(page_source))
            .await;
        let tokens = self.render_links(tokens);
        let tokens = self.render_external_links(tokens);
        let tokens = self.render_behavior_switches(tokens);
        // Route `format="wikitext"` extension bodies through the inline
        // sub-pipeline, producing `mw:dom-fragment-token` placeholders + their
        // pre-built sub-fragments.
        let (tokens, fragments) = self
            .expand_wikitext_pre(tokens, source, &frame, about_counter)
            .await;

        let stage = TreeBuilderStage::new(false);
        let mut ast =
            stage.to_ast_with_fragments(tokens, Some(page_source), self.config, fragments);
        // Capture transclusion marker depth map over the freshly-built DOM
        // (before p-wrapping restructures it), mirroring PHP's
        // `transclusionMetaTagDepthMap` recorded at tree-build time.
        let depths = crate::pipeline::migrate_template_marker_metas::collect_depths(&ast);
        crate::pipeline::compute_dsr::run(&mut ast, page_source);
        // DOM-level p-wrapping runs before transclusion encapsulation (mirrors
        // PHP's `pwrap` … `tplwrap` order).
        crate::pipeline::p_wrap::run(&mut ast);
        crate::pipeline::tree_builder_html::post_pwrap_transforms(
            &mut ast,
            &depths,
            Some(page_source),
        );
        crate::pipeline::cleanup::run(&mut ast);
        crate::pipeline::headings::gen_anchors(&mut ast);
        crate::pipeline::add_link_attributes::run(&mut ast, self.config);
        // AddRedLinks: resolve which wikilink targets exist, marking missing
        // ones as red links. Gather the relevant page titles, batch-check their
        // existence via the data source, then apply the pass.
        let mut titles = Vec::new();
        crate::pipeline::add_red_links::collect_wikilink_titles(&ast, &mut titles);
        if !titles.is_empty() {
            let mut known = std::collections::HashSet::new();
            if let Some(source) = source {
                for t in &titles {
                    if let Ok(Some(_)) = source
                        .get_page_content(&crate::title::Title::new_main(t.clone()))
                        .await
                    {
                        known.insert(t.clone());
                    }
                }
            }
            crate::pipeline::add_red_links::run(&mut ast, &known, &page_title_prefixed);
        }
        wrap_sections_in_ast(&mut ast, wrap_sections);
        ast
    }

    /// Expand `template`/`templatearg` tokens in-place.
    ///
    /// `in_template` mirrors PHP's `wrapTemplates = !$options['inTemplate']`:
    /// when true (nested template / extension-content context), expanded
    /// templates are returned *without* `mw:Transclusion` encapsulation.
    async fn expand_templates(
        &self,
        frame: &Frame,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        in_template: bool,
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

                // A comment in the template *target* (`{{f<!---->oo}}`) is
                // stripped by the PHP preprocessor before target resolution,
                // so the template still expands, but Parsoid does not wrap the
                // expansion in a `mw:Transclusion` wrapper (the target is no
                // longer cleanly stringifiable). Detect this from the raw
                // template source so the expansion matches PHP's
                // `convertToString(..., /* expandTemplates */ true)` path,
                // which emits the expansion unencapsulated.
                let target_has_comment = stt
                    .data_parsoid
                    .src
                    .as_deref()
                    .and_then(raw_template_target)
                    .map(|t| t.contains("<!--"))
                    .unwrap_or(false);

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
                                in_template,
                                target_has_comment,
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
                    TemplateHandler.handle_template_arg(frame, &src, about_id, tok, !in_template);
                out.extend(expanded);
                continue;
            }

            out.push(item);
        }
        out
    }

    /// Expand templated attribute keys/values on `Tag`/`SelfclosingTag` tokens,
    /// then run `buildExpandedAttrs` to finalize the attributes (reparse-KV,
    /// `mw:ExpandedAttrs` marking). Mirrors the TT2 `AttributeExpander` handler
    /// (`onAny` → `processComplexAttributes`).
    async fn expand_attributes(
        &self,
        frame: &Frame,
        tokens: Vec<Item>,
        source: Option<&dyn DataSource>,
        about_counter: &std::cell::Cell<usize>,
        page_source: Option<&str>,
    ) -> Vec<Item> {
        use crate::wikitext::tokens_v2::{KV, KeyValue};

        let mut out = Vec::new();
        for item in tokens {
            let Item::Tok(tok) = &item else {
                out.push(item);
                continue;
            };
            if !matches!(tok, ParsoidToken::Tag(_) | ParsoidToken::SelfclosingTag(_)) {
                out.push(item);
                continue;
            }
            if tok.get_name() == "mw:dom-fragment-token" {
                out.push(item);
                continue;
            }

            let attribs = tok.get_attribs().to_vec();
            let has_tokens = attribs.iter().any(|kv| {
                matches!(kv.key, KeyValue::Tokens(_)) || matches!(kv.value, KeyValue::Tokens(_))
            });
            if !has_tokens {
                out.push(item);
                continue;
            }

            // Expand each templated key/value (template args and templates).
            let mut expanded_attrs: Vec<KV> = Vec::with_capacity(attribs.len());
            for kv in &attribs {
                let new_key = if let KeyValue::Tokens(toks) = &kv.key {
                    let expanded = self
                        .expand_templates(frame, toks.clone(), source, about_counter, false)
                        .await;
                    crate::pipeline::attribute_transform_manager::items_to_key_value(expanded)
                } else {
                    kv.key.clone()
                };
                let new_value = if let KeyValue::Tokens(toks) = &kv.value {
                    let expanded = self
                        .expand_templates(frame, toks.clone(), source, about_counter, false)
                        .await;
                    crate::pipeline::attribute_transform_manager::items_to_key_value(expanded)
                } else {
                    kv.value.clone()
                };
                expanded_attrs.push(KV {
                    key: new_key,
                    value: new_value,
                    src_offsets: kv.src_offsets.clone(),
                    ksrc: kv.ksrc.clone(),
                    vsrc: kv.vsrc.clone(),
                });
            }

            let result = crate::pipeline::attribute_expander::build_expanded_attrs(
                tok.clone(),
                &attribs,
                expanded_attrs,
                about_counter,
                false,
                &|kv| self.value_to_dom_html(kv),
                page_source,
            );
            out.extend(result);
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
        in_template: bool,
        target_has_comment: bool,
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
            if in_template {
                return vec![crate::pipeline::template_handler::template_to_wikilink(
                    name,
                )];
            }
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
            if in_template {
                return vec![crate::pipeline::template_handler::template_to_wikilink(
                    name,
                )];
            }
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
        // child frame. Extension tags must be registered so their bodies are
        // captured as `extension` tokens (not expanded as templates/list/etc.).
        let items = crate::pipeline::template_handler::tokenize_wikitext_to_items(
            &substituted,
            /* in_template */ true,
            self.config.extension_tags(),
        );
        let expanded =
            Box::pin(self.expand_templates(&child_frame, items, Some(src), about_counter, true))
                .await;

        if in_template || target_has_comment {
            // Nested/extension-content context, or a comment in the template
            // target (`{{f<!---->oo}}`): no `mw:Transclusion` wrapping.
            return expanded;
        }

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
        // A plain-text `<nowiki>` (no decodable entities) renders as bare text,
        // with no `mw:Nowiki` wrapper (matching Parsoid's rendered output).
        let html = parser
            .wikitext_to_html("<nowiki>hi</nowiki>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains(">hi</p>"), "got: {html}");
        assert!(!html.contains("mw:Nowiki"), "got: {html}");

        // The nested `</pre>` is escaped, not treated as a tag.
        let html = parser
            .wikitext_to_html("<nowiki></pre></nowiki>", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("&lt;/pre>"), "got: {html}");
        assert!(!html.contains("<pre>"), "got: {html}");
    }

    #[test]
    fn test_process_fragment_body_direct() {
        use crate::dom::node::{ElementKind, NodeKind};
        fn has_bold(n: &Node) -> bool {
            matches!(n.kind, NodeKind::Element(ElementKind::Bold))
                || n.children.iter().any(has_bold)
        }
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let sub = parser.fragment_from_body("'''bold'''");
        assert!(has_bold(&sub), "sub-fragment missing bold: {sub:?}");
        // Inline context must not introduce a <p> wrapper around inline content.
        assert!(!matches!(
            sub.children[0].kind,
            NodeKind::Element(ElementKind::Paragraph)
        ));
    }

    #[test]
    fn test_pre_format_wikitext_body() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "<pre format=\"wikitext\">'''bold'''</pre>",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<pre"), "got: {html}");
        assert!(html.contains("<b>bold</b>"), "got: {html}");
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
    fn test_value_to_dom_html_plain_string() {
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        // A plain string value serializes as itself (no <p> wrapper in inline
        // context).
        let kv = crate::wikitext::tokens_v2::KeyValue::Str("color:red".to_string());
        let html = parser.value_to_dom_html(&kv);
        assert_eq!(html, "color:red", "got: {html:?}");
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
        assert!(html.contains("rel=\"mw:ExtLink nofollow\""), "got: {html}");
        assert!(html.contains("class=\"external text\""), "got: {html}");
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
        assert!(html.contains("rel=\"mw:ExtLink nofollow\""), "got: {html}");
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
    fn test_auto_inserted_empty_bold_stripped() {
        // `''foo''''bar''` (the "annoying" misnested case) produces an
        // auto-inserted `<b></b>` at end of line in the legacy parser, but
        // Parsoid strips it via `ProcessTreeBuilderFixups::removeAutoInsertedEmptyTags`.
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html("''foo''''bar''", &ParserOptions::for_page("Test"))
            .unwrap();
        assert!(html.contains("<i>foo'<b>bar</b></i>"), "got: {html}");
        // No stray empty `<b></b>` should remain.
        assert!(!html.contains("<b></b>"), "got: {html}");
        assert!(!html.contains("<i></i>"), "got: {html}");
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
    fn test_wikitext_to_html_redirect_nowiki_bail() {
        // A redirect target containing `<nowiki>` cannot be rendered as a link;
        // it bails to `<ol><li>REDIRECT [[…]]</li></ol>`. The nowiki content is
        // plain text, so it merges directly (no `mw:Nowiki` span).
        let config = MockSiteConfig::new();
        let parser = Parser::new(&config);
        let html = parser
            .wikitext_to_html(
                "#REDIRECT [[<nowiki>[[Bar]]</nowiki>]]",
                &ParserOptions::for_page("Test"),
            )
            .unwrap();
        assert!(html.contains("<ol><li>"), "got: {html}");
        assert!(html.contains("REDIRECT [[[[Bar]]]]"), "got: {html}");
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
        assert!(html.contains("rel=\"mw:ExtLink nofollow\""), "got: {html}");
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
        assert!(html.contains("rel=\"mw:ExtLink nofollow\""), "got: {html}");
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
