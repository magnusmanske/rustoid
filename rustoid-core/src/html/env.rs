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
#[derive(Clone, Copy)]
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
    ///
    /// `ignoreFragment = true` returns `getPrefixedDBKey()` (underscores in both
    /// the namespace and the title), the default `false` returns the
    /// `getFullDBKey()` (which appends the fragment).
    ///
    /// Mirrors PHP's `makeTitle(..., $noExceptions = true)`: `Title::newFromText`
    /// throws `TitleException` for illegal characters (`&name;`, `%hh`, …), which
    /// this surfaces as `None`. The title is resolved (relative `#`/`/`/`../`
    /// references) *before* parsing, and a relative reference adopts the context
    /// title's namespace as its default.
    pub fn normalized_title_key(&self, str_: &str, ignore_fragment: bool) -> Option<String> {
        let title = self.make_title_from_url_decoded_str(str_, true)?;
        if title.text.is_empty() && title.namespace_id == 0 && title.interwiki.is_none() {
            return None;
        }
        if ignore_fragment {
            Some(title.get_prefixed_db_key())
        } else {
            Some(title.get_full_db_key())
        }
    }

    /// `Env::makeTitleFromURLDecodedStr( $str, 0, $noExceptions )` — url-decode,
    /// resolve relative references, then parse. `None` stands for PHP's
    /// `TitleException` when `noExceptions` is set.
    pub fn make_title_from_url_decoded_str(
        &self,
        str_: &str,
        no_exceptions: bool,
    ) -> Option<Title> {
        let decoded = crate::util::decode_uri_component(str_);
        self.make_title(&decoded, no_exceptions)
    }

    /// `Env::makeTitle` — resolve then parse, honoring the relative-reference
    /// namespace rule.
    fn make_title(&self, text: &str, no_exceptions: bool) -> Option<Title> {
        // A relative reference (`#`, `/`, `../`) takes the context namespace.
        let resolved = self.resolve_title(text);
        let title = crate::title::TitleParser::try_parse(&resolved, self.config);
        if title.is_none() && !no_exceptions {
            // PHP rethrows; callers that pass `noExceptions = false` expect a
            // hard failure. rustoid has no exception channel, so returning
            // `None` keeps the signature total (see the `TitleParser` docs).
            return None;
        }
        title
    }

    /// `Env::resolveTitle` — resolve a possibly-relative title reference
    /// (`#fragment`, `/absolute-subpage`, `../relative`) against the context
    /// title. Faithful to `Config\Env::resolveTitle` (the `$resolveOnly` variant
    /// is not used by the serializer, which always normalizes).
    pub fn resolve_title(&self, str_: &str) -> String {
        let orig_name = str_;
        let s = str_.trim();
        let title = self.context_title();

        // Resolve lonely fragments (important if the current page is a subpage,
        // otherwise the relative link will be wrong).
        if !s.is_empty() && s.starts_with('#') {
            return format!("{}{s}", title.get_prefixed_text());
        }

        let mut title_key = s.to_string();
        if self
            .get_site_config()
            .namespace_has_subpages(title.namespace_id)
        {
            let mut re_normalize = false;
            if let Some(rel_up_len) = count_leading_dotdot_slashes(s) {
                // Levels are indicated by `../`.
                let levels = rel_up_len / 3;
                let prefixed = title.get_prefixed_text();
                let title_bits: Vec<&str> = prefixed.split('/').collect();
                if title_bits.first().is_some_and(|b| b.is_empty()) {
                    // FIXME: Punt on subpages of titles starting with "/" for now.
                    return orig_name.to_string();
                }
                if title_bits.len() <= levels {
                    // Too many levels -- invalid relative link.
                    return orig_name.to_string();
                }
                let mut new_bits: Vec<String> = title_bits[..title_bits.len() - levels]
                    .iter()
                    .map(|b| (*b).to_string())
                    .collect();
                if s.len() > rel_up_len {
                    let last_bit = &s[rel_up_len..];
                    if last_bit.starts_with('#') {
                        // Fragments should not get appended after a trailing "/".
                        let n = new_bits.len();
                        new_bits[n - 1].push_str(last_bit);
                    } else {
                        new_bits.push(last_bit.to_string());
                    }
                }
                title_key = new_bits.join("/");
                re_normalize = true;
            } else if !s.is_empty() && s.starts_with('/') {
                // Resolve absolute subpage links.
                title_key = format!("{}{s}", title.get_prefixed_text());
                re_normalize = true;
            }

            if re_normalize {
                // Remove final slashes if present, then normalize the title key.
                title_key = title_key.trim_end_matches('/').to_string();
                title_key = self
                    .normalized_title_key(&title_key, false)
                    .unwrap_or(title_key);
            }
        }

        // Strip a leading ':'.
        if let Some(stripped) = title_key.strip_prefix(':') {
            title_key = stripped.to_string();
        }
        title_key
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

/// The byte length of a leading run of `../` (levels are indicated by `../`),
/// or `None` when `s` does not start with `../`. Mirrors the
/// `preg_match( '!^(?:\.\./)+!', $str, $relUp )` capture length.
fn count_leading_dotdot_slashes(s: &str) -> Option<usize> {
    if !s.starts_with("../") {
        return None;
    }
    let mut n = 0;
    while s[n..].starts_with("../") {
        n += 3;
    }
    Some(n)
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
        // `ignoreFragment` yields the prefixed DB key (underscores); otherwise
        // the full DB key, which appends the fragment.
        assert_eq!(
            env.normalized_title_key("Foo Bar", true).as_deref(),
            Some("Foo_Bar")
        );
        assert_eq!(
            env.normalized_title_key("Foo Bar", false).as_deref(),
            Some("Foo_Bar")
        );
        assert_eq!(
            env.normalized_title_key("Foo Bar#Sec", true).as_deref(),
            Some("Foo_Bar")
        );
        assert_eq!(
            env.normalized_title_key("Foo Bar#Sec", false).as_deref(),
            Some("Foo_Bar#Sec")
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
        // A lone fragment resolves against the context title's prefixed text.
        assert_eq!(env.resolve_title("#Section"), "Test Page#Section");
        assert_eq!(env.resolve_title("Foo"), "Foo");
        assert_eq!(env.resolve_title("  Foo  "), "Foo");
    }

    #[test]
    fn test_resolve_title_relative_subpage() {
        let mut config = MockSiteConfig::new();
        config.enable_subpages_for_ns(0);
        let title = Title::new_main("Subpage test/1/2/3/4");
        let env = SerializerEnv::new(&config, &title);
        // Two `../` levels drop `3/4`, appending the trailing part.
        assert_eq!(
            env.resolve_title("../../subpage/"),
            "Subpage_test/1/2/subpage"
        );
        assert_eq!(
            env.resolve_title("../../subpage"),
            "Subpage_test/1/2/subpage"
        );
        // An absolute subpage resolves against the full prefixed text.
        assert_eq!(
            env.resolve_title("/subpage"),
            "Subpage_test/1/2/3/4/subpage"
        );
    }
}
