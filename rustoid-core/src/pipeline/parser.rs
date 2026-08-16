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
        let options = TokenizerOptions::default();
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
        let id = counter.get();
        counter.set(id + 1);
        format!("#mwt{id}")
    }

    /// Convert wikitext to the format-agnostic AST (no template expansion).
    pub fn wikitext_to_ast(&self, wikitext: &str) -> Result<Node> {
        let tokens = self.tokenize(wikitext)?;
        let stage = TreeBuilderStage::new(false);
        Ok(stage.to_ast(tokens))
    }

    /// Convert wikitext to an HTML string (no native template expansion).
    pub fn wikitext_to_html(&self, wikitext: &str, options: &ParserOptions) -> Result<String> {
        let ast = self.wikitext_to_ast(wikitext)?;
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
            .build_ast(tokens, Some(source), &options.page_title, &about_counter)
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
    ) -> Node {
        let title = TitleParser::parse(page_title, self.config);
        let frame = Frame::new(title, vec![]);

        let tokens = self
            .expand_templates(&frame, tokens, source, about_counter)
            .await;

        let stage = TreeBuilderStage::new(false);
        stage.to_ast(tokens)
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
        assert!(html.contains("<h2>"), "got: {html}");
        assert!(html.contains("Heading"), "got: {html}");
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
}
