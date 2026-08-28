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

    /// `Env::makeTitleFromText` — decode percent-encoding, resolve relative/
    /// fragment references against the context title, then parse to a `Title`
    /// (infallible, since our `TitleParser` doesn't throw on invalid titles).
    pub fn make_title_from_text(&self, str_: &str) -> Title {
        let decoded = crate::util::decode_uri_component(str_);
        crate::title::TitleParser::parse(&decoded, self.config)
    }

    /// `Env::normalizedTitleKey` (with `ignoreFragment` defaulting to `false`) —
    /// the normalized DB key of a url-decoded title string. Returns `None` for
    /// titles that resolve to an empty key.
    pub fn normalized_title_key(&self, str_: &str, ignore_fragment: bool) -> Option<String> {
        let title = self.make_title_from_text(str_);
        if title.text.is_empty() && title.namespace_id == 0 && title.interwiki.is_none() {
            return None;
        }
        if ignore_fragment {
            Some(title.get_prefixed_text())
        } else {
            Some(title.get_full_db_key())
        }
    }

    /// `Env::resolveTitle` (fragment and absolute-subpage handling only; the
    /// `../` relative-subpage resolution is not needed by the link serializer).
    pub fn resolve_title(&self, str_: &str) -> String {
        let trimmed = str_.trim();
        // Lonely fragments resolve against the context title.
        if let Some(fragment) = trimmed.strip_prefix('#') {
            return format!("{}{}", self.context_title.get_prefixed_text(), fragment);
        }
        trimmed.to_string()
    }

    /// No-op trace (Parsoid emits `trace/$prefix` when tracing is enabled;
    /// this codebase has no tracing subsystem yet).
    pub fn trace(&self, _prefix: &str, _msg: impl std::fmt::Display) {}

    /// No-op log.
    pub fn log(&self, _prefix: &str, _msg: impl std::fmt::Display) {}

    /// Whether an href attribute value could be a valid local link target.
    ///
    /// Faithful port of `Env::isValidLinkTarget`: percent-decode, resolve
    /// fragments, then check `normalizedTitleKey(...) !== null`. NOTE: this
    /// replaces the earlier character-class approximation.
    pub fn is_valid_link_target(&self, href: &str) -> bool {
        if self.config.has_valid_protocol(href) {
            return false;
        }
        let decoded = crate::util::decode_uri_component(href);
        let resolved = self.resolve_title(&decoded);
        self.normalized_title_key(&resolved, true).is_some()
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
    }

    #[test]
    fn test_normalized_title_key() {
        let config = MockSiteConfig::new();
        let title = Title::new_main("Test Page");
        let env = SerializerEnv::new(&config, &title);
        // `ignoreFragment` yields the prefixed (space) form; otherwise the DB key.
        assert_eq!(
            env.normalized_title_key("Foo Bar", true).as_deref(),
            Some("Foo Bar")
        );
        assert_eq!(
            env.normalized_title_key("Foo Bar", false).as_deref(),
            Some("Foo_Bar")
        );
        assert_eq!(
            env.normalized_title_key("Template:Foo", false).as_deref(),
            Some("Template:Foo")
        );
    }

    #[test]
    fn test_resolve_title_fragment() {
        let config = MockSiteConfig::new();
        let title = Title::new_main("Test Page");
        let env = SerializerEnv::new(&config, &title);
        assert_eq!(env.resolve_title("#Section"), "Test PageSection");
        assert_eq!(env.resolve_title("Foo"), "Foo");
    }
}
