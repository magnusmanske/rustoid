//! TokenHandlerPipeline — faithful port of PHP Parsoid's
//! `src/Wt2Html/TokenHandlerPipeline.php`.
//!
//! A token transformation manager that holds an ordered list of `TokenHandler`
//! transformers and pushes each token chunk through them in sequence. This is
//! the TokenTransform2 stage's driver.
//!
//! PHP's `TokenHandler` hierarchy has three concrete shapes (Universal,
//! XMLTag-based, Line-based); for the port we model a single trait because the
//! already-ported handlers share a `process(&PipelineContext, Vec<Item>) ->
//! Vec<Item>` signature.

use crate::traits::SiteConfig;
use crate::wikitext::tokens_v2::Item;

use super::frame::Frame;

/// Shared context threaded through every token transformation stage.
pub struct PipelineContext<'a> {
    pub site_config: &'a dyn SiteConfig,
    pub frame: &'a Frame,
    /// Whether to expand templates encountered (mirrors `expandTemplates`).
    pub expand_templates: bool,
    /// Whether we're processing template content (mirrors `inTemplate`).
    pub in_template: bool,
    /// A monotonically increasing counter for `about` ids (`env->newAboutId`).
    pub about_counter: std::cell::Cell<usize>,
}

impl<'a> PipelineContext<'a> {
    pub fn new_about_id(&self) -> String {
        let id = self.about_counter.get();
        self.about_counter.set(id + 1);
        format!("#mwt{id}")
    }
}

/// A single token transformation handler. Mirrors PHP's `TokenHandler`.
pub trait TokenHandler {
    fn process(&self, ctx: &PipelineContext, tokens: Vec<Item>) -> Vec<Item>;
}

/// The token handler pipeline (mirrors `TokenHandlerPipeline`).
pub struct TokenHandlerPipeline<'a> {
    transformers: Vec<Box<dyn TokenHandler + 'a>>,
}

impl<'a> TokenHandlerPipeline<'a> {
    pub fn new() -> Self {
        Self {
            transformers: Vec::new(),
        }
    }

    pub fn add_transformer(&mut self, t: Box<dyn TokenHandler + 'a>) {
        self.transformers.push(t);
    }

    /// Process a chunk of tokens through all registered transformers, in order.
    /// Mirrors `TokenHandlerPipeline::processChunk`.
    pub fn process_chunk(&self, ctx: &PipelineContext, tokens: Vec<Item>) -> Vec<Item> {
        let mut out = tokens;
        for transformer in &self.transformers {
            if out.is_empty() {
                break;
            }
            out = transformer.process(ctx, out);
        }
        out
    }
}

impl<'a> Default for TokenHandlerPipeline<'a> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_pipeline_identity() {
        let config = crate::mock::MockSiteConfig::new();
        let title = crate::title::TitleParser::parse("Template:Foo", &config);
        let frame = Frame::new(title, vec![]);
        let ctx = PipelineContext {
            site_config: &config,
            frame: &frame,
            expand_templates: true,
            in_template: false,
            about_counter: std::cell::Cell::new(0),
        };

        let pipeline = TokenHandlerPipeline::new();
        let out = pipeline.process_chunk(&ctx, vec![Item::Str("hello".to_string())]);
        assert_eq!(out, vec![Item::Str("hello".to_string())]);
    }

    #[test]
    fn test_about_id_counter() {
        let config = crate::mock::MockSiteConfig::new();
        let title = crate::title::TitleParser::parse("Template:Foo", &config);
        let frame = Frame::new(title, vec![]);
        let ctx = PipelineContext {
            site_config: &config,
            frame: &frame,
            expand_templates: true,
            in_template: false,
            about_counter: std::cell::Cell::new(0),
        };
        assert_eq!(ctx.new_about_id(), "#mwt0");
        assert_eq!(ctx.new_about_id(), "#mwt1");
    }
}
