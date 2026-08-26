//! Minimal serializer environment.
//!
//! A small stand-in for PHP's `Config\Env` restricted to what the html2wt
//! serializer needs: `trace`/`log` (no-ops in this codebase, which has no
//! tracing infrastructure yet) and `isValidLinkTarget`. It carries the
//! `SiteConfig` (protocol/extension-tag lookups) and a context title for
//! relative/fragment link resolution.

use crate::title::Title;
use crate::traits::SiteConfig;

/// Serializer-facing environment. Mirrors the subset of `Env` the
/// `WikitextEscapeHandlers`/serializer depends on.
pub struct SerializerEnv<'a> {
    config: &'a dyn SiteConfig,
    context_title: &'a Title,
}

impl<'a> SerializerEnv<'a> {
    pub fn new(config: &'a dyn SiteConfig, context_title: &'a Title) -> Self {
        Self {
            config,
            context_title,
        }
    }

    pub fn get_site_config(&self) -> &'a dyn SiteConfig {
        self.config
    }

    pub fn context_title(&self) -> &'a Title {
        self.context_title
    }

    /// No-op trace (Parsoid emits `trace/$prefix` when tracing is enabled;
    /// this codebase has no tracing subsystem yet).
    pub fn trace(&self, _prefix: &str, _msg: impl std::fmt::Display) {}

    /// No-op log.
    pub fn log(&self, _prefix: &str, _msg: impl std::fmt::Display) {}

    /// Whether an href attribute value could be a valid local link target.
    ///
    /// Mirrors PHP `Env::isValidLinkTarget` loosely: reject values that begin
    /// with a valid protocol (those are external links), and reject titles
    /// containing characters MediaWiki disallows in page titles. Full fidelity
    /// requires `Title::newFromText` validation (which our infallible
    /// `TitleParser` doesn't yet model), so the character-class check is a
    /// documented approximation.
    pub fn is_valid_link_target(&self, href: &str) -> bool {
        if self.config.has_valid_protocol(href) {
            return false;
        }
        // Strip a leading fragment (`#…`) — fragments are resolved against the
        // context title and are always valid link targets.
        let decoded = crate::util::decode_uri_component(href);
        let title_part = decoded.split('#').next().unwrap_or("");
        if title_part.is_empty() {
            // A pure fragment (`#section`) is a valid target.
            return !href.trim().is_empty();
        }
        !title_part
            .chars()
            .any(|c| matches!(c, '[' | ']' | '{' | '}' | '|' | '<' | '>'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    #[test]
    fn test_is_valid_link_target() {
        let config = MockSiteConfig::new();
        let title = Title::new_main("Test Page");
        let env = SerializerEnv::new(&config, &title);

        assert!(env.is_valid_link_target("Foo"));
        assert!(env.is_valid_link_target("Foo/bar"));
        assert!(env.is_valid_link_target("#section"));
        // External protocol targets are not local links.
        assert!(!env.is_valid_link_target("https://example.com"));
        // MediaWiki-illegal title characters are rejected.
        assert!(!env.is_valid_link_target("Foo[bar"));
        assert!(!env.is_valid_link_target("Foo|bar"));
    }
}
