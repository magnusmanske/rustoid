/// Page title handling with namespace-aware parsing and comparison.
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::traits::SiteConfig;

/// A MediaWiki page title with optional namespace, interwiki prefix, and fragment.
///
/// Examples:
/// - `"Main Page"` — namespace 0 (main), title `"Main Page"`.
/// - `"Template:Foo"` — namespace 10 (Template), title `"Foo"`.
/// - `":Category:People"` — force main-namespace page literally named `"Category:People"`.
/// - `"de:Foo"` — interwiki prefix `"de"`, title `"Foo"`.
/// - `"Foo#Section"` — title `"Foo"`, fragment `"Section"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Title {
    /// The interwiki prefix, if any (e.g. `"de"`, `"wiktionary"`).
    pub interwiki: Option<String>,
    /// The namespace ID (0 = mainspace, 10 = Template, 14 = Category, etc.).
    pub namespace_id: i32,
    /// The page title text (without namespace prefix, with underscores converted to spaces).
    pub text: String,
    /// The URL fragment (section anchor), if any.
    pub fragment: Option<String>,
}

impl Title {
    /// Create a Title in the main namespace (ID 0).
    pub fn new_main(text: impl Into<String>) -> Self {
        Self {
            interwiki: None,
            namespace_id: 0,
            text: text.into(),
            fragment: None,
        }
    }

    /// Create a simple Title with the given namespace ID and text.
    pub fn new(namespace_id: i32, text: impl Into<String>) -> Self {
        Self {
            interwiki: None,
            namespace_id,
            text: text.into(),
            fragment: None,
        }
    }

    /// The full page name including namespace prefix, resolved via SiteConfig.
    pub fn full_text(&self) -> String {
        let prefix = namespace_prefix(self.namespace_id);
        if self.namespace_id != 0 && !prefix.is_empty() {
            format!("{prefix}:{}", self.text)
        } else {
            self.text.clone()
        }
    }

    /// The full page name using canonical namespace names for display.
    pub fn full_text_with_config(&self, config: &dyn SiteConfig) -> String {
        if self.namespace_id == 0 {
            return self.text.clone();
        }
        if let Some(ns) = config.namespaces().get(&self.namespace_id) {
            format!("{}:{}", ns.canonical, self.text)
        } else {
            self.text.clone()
        }
    }

    /// The prefixed title with spaces (for display). Mirrors PHP's
    /// `Title::getPrefixedText()`.
    pub fn get_prefixed_text(&self) -> String {
        let text = self.text.replace('_', " ");
        let prefix = namespace_prefix(self.namespace_id);
        if self.namespace_id != 0 && !prefix.is_empty() {
            format!("{prefix}:{text}")
        } else {
            text
        }
    }

    /// The prefixed title with underscores (DB key). Mirrors PHP's
    /// `Title::getPrefixedDBKey()` / `getFullDBKey()`.
    pub fn get_full_db_key(&self) -> String {
        let text = self.text.replace(' ', "_");
        let prefix = namespace_prefix(self.namespace_id);
        if self.namespace_id != 0 && !prefix.is_empty() {
            format!("{prefix}:{text}")
        } else {
            text
        }
    }
}

/// Map a namespace ID to its canonical English prefix (fallback without SiteConfig).
fn namespace_prefix(ns_id: i32) -> &'static str {
    match ns_id {
        -2 => "Media",
        -1 => "Special",
        0 => "",
        1 => "Talk",
        2 => "User",
        3 => "User talk",
        4 => "Project",
        5 => "Project talk",
        6 => "File",
        7 => "File talk",
        8 => "MediaWiki",
        9 => "MediaWiki talk",
        10 => "Template",
        11 => "Template talk",
        12 => "Help",
        13 => "Help talk",
        14 => "Category",
        15 => "Category talk",
        828 => "Module",
        829 => "Module talk",
        _ => "",
    }
}

impl fmt::Display for Title {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        if let Some(ref iw) = self.interwiki {
            s.push_str(iw);
            s.push(':');
        }
        let prefix = namespace_prefix(self.namespace_id);
        if !prefix.is_empty() {
            s.push_str(prefix);
            s.push(':');
        }
        s.push_str(&self.text);
        if let Some(ref frag) = self.fragment {
            s.push('#');
            s.push_str(frag);
        }
        write!(f, "{s}")
    }
}

/// Uppercase the first character of a title, mirroring PHP's
/// `SiteConfig::ucfirst`. ASCII fast-path matches PHP's `ucfirst()` (and the
/// `$o < 96` / `$o < 128` byte checks); multibyte falls back to full
/// title-casing of the first grapheme. The Turkish `i` special case is included
/// for the configured language.
pub fn ucfirst(s: &str, language_code: &str) -> String {
    if s.is_empty() {
        return String::new();
    }

    let first = s.chars().next().unwrap();
    // If already uppercase (ASCII), or unusual, pass through. The PHP code
    // checks the first *byte* only; for ASCII this is equivalent to testing
    // whether the first char is already uppercase.
    if first.is_ascii_uppercase() {
        return s.to_string();
    }

    if first.is_ascii() {
        // ASCII lowercase (or other ASCII): uppercase the first byte.
        if first == 'i' && matches!(language_code, "az" | "tr" | "kaa" | "kk") {
            return format!("İ{}", &s[1..]);
        }
        let mut out = s.to_string();
        if let Some(c) = out.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        return out;
    }

    // Multibyte: title-case the first char, NFC-normalize.
    let mut it = s.char_indices();
    let (_, first_char) = it.next().unwrap();
    let rest_start = it.next().map(|(i, _)| i).unwrap_or(s.len());
    let upper = first_char.to_uppercase().collect::<String>();
    format!("{upper}{}", &s[rest_start..])
}

/// Parser for constructing `Title` from a string, using `SiteConfig` for
/// namespace alias resolution and interwiki prefix matching.
pub struct TitleParser;

impl TitleParser {
    /// Parse a title string into a `Title` using the given site configuration.
    ///
    /// Handles:
    /// - Leading `:` to force mainspace (e.g. `":Category:Foo"` → mainspace `"Category:Foo"`).
    /// - Namespace prefixes resolved via `SiteConfig.namespaces()`.
    /// - Interwiki prefixes resolved via `SiteConfig.interwiki_map()`.
    /// - URL fragments (`#Section`).
    ///
    /// First-letter capitalization (`ucfirst`) is applied to the title text
    /// when the resolved namespace is first-letter case-insensitive and there is
    /// no interwiki prefix, mirroring `Title::newFromText`.
    pub fn parse(input: &str, config: &dyn SiteConfig) -> Title {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Title::new_main(String::new());
        }

        let (rest, fragment) = split_fragment(trimmed);
        let rest = if rest.is_empty() { trimmed } else { rest };

        // Leading colon forces main namespace
        let (rest, force_main) = if let Some(stripped) = rest.strip_prefix(':') {
            (stripped, true)
        } else {
            (rest, false)
        };

        if !force_main {
            // Try to match interwiki prefix first (case-insensitive, mirroring
            // PHP's `$pLower = mb_strtolower($p)` lookup).
            for prefix in config.interwiki_map().keys() {
                let Some(colon) = rest.find(':') else {
                    continue;
                };
                if rest[..colon].to_lowercase() == prefix.to_lowercase() {
                    let after = &rest[colon + 1..];
                    // Interwiki titles are NOT first-letter capitalized (the
                    // remote wiki may be case-sensitive).
                    return Title {
                        interwiki: Some(prefix.clone()),
                        namespace_id: 0,
                        text: after.to_string(),
                        fragment,
                    };
                }
            }

            // Try to match namespace prefix by canonical name or alias.
            // The prefix is matched case-insensitively (PHP lowercases the
            // prefix with `mb_strtolower` before looking it up).
            for (&ns_id, ns_info) in config.namespaces() {
                let match_prefix = |name: &str| -> Option<&str> {
                    let lower_name = name.to_lowercase();
                    let colon = rest.find(':')?;
                    if rest[..colon].to_lowercase() == lower_name {
                        Some(&rest[colon + 1..])
                    } else {
                        None
                    }
                };
                if let Some(title_part) = match_prefix(&ns_info.canonical) {
                    return Self::with_case(
                        title_part,
                        ns_id,
                        ns_info.case_sensitive,
                        fragment,
                        config,
                    );
                }
                for alias in &ns_info.aliases {
                    if let Some(title_part) = match_prefix(alias) {
                        return Self::with_case(
                            title_part,
                            ns_id,
                            ns_info.case_sensitive,
                            fragment,
                            config,
                        );
                    }
                }
            }
        }

        // No namespace prefix matched — return mainspace title, applying
        // first-letter capitalization for the mainnamespace (case-insensitive).
        let ns_id = 0;
        let case_sensitive = config
            .namespaces()
            .get(&ns_id)
            .map(|info| info.case_sensitive)
            .unwrap_or(false);
        Self::with_case(rest, ns_id, case_sensitive, fragment, config)
    }

    /// Build a `Title`, applying first-letter capitalization when the namespace
    /// is case-insensitive (mirrors the `namespaceCase === 'first-letter'` branch
    /// of `Title::newFromText`).
    fn with_case(
        text: &str,
        namespace_id: i32,
        case_sensitive: bool,
        fragment: Option<String>,
        config: &dyn SiteConfig,
    ) -> Title {
        let text = if case_sensitive {
            text.to_string()
        } else {
            ucfirst(text, config.language_code())
        };
        Title {
            interwiki: None,
            namespace_id,
            text,
            fragment,
        }
    }
}

/// Config-aware relative link prefix. Mirrors PHP's
/// `SiteConfig::relativeLinkPrefix()` (defaults to `"./"` on enwiki).
pub fn relative_link_prefix(_config: &dyn SiteConfig) -> &'static str {
    // The standard MediaWiki relative link prefix is "./".
    "./"
}

/// Make a link href for a local Title. Mirrors PHP's `Env::makeLink`:
/// `relativeLinkPrefix() . Sanitizer::sanitizeTitleURI(title->getFullDBKey(), false)`.
pub fn make_link(title: &Title, config: &dyn SiteConfig) -> String {
    let sanitized = crate::sanitizer::sanitize_title_uri(&title.get_full_db_key(), false);
    format!("{}{}", relative_link_prefix(config), sanitized)
}

/// Split off the URL fragment (after `#`).
fn split_fragment(input: &str) -> (&str, Option<String>) {
    if let Some(pos) = input.find('#') {
        (&input[..pos], Some(input[pos + 1..].to_string()))
    } else {
        (input, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    fn test_config() -> MockSiteConfig {
        MockSiteConfig::new()
    }

    #[test]
    fn test_mainspace_title() {
        let config = test_config();
        let t = TitleParser::parse("Main Page", &config);
        assert_eq!(t.namespace_id, 0);
        assert_eq!(t.text, "Main Page");
        assert!(t.fragment.is_none());
    }

    #[test]
    fn test_template_title() {
        let config = test_config();
        let t = TitleParser::parse("Template:Foo", &config);
        assert_eq!(t.namespace_id, 10);
        assert_eq!(t.text, "Foo");
    }

    #[test]
    fn test_file_title() {
        let config = test_config();
        let t = TitleParser::parse("File:Example.jpg", &config);
        assert_eq!(t.namespace_id, 6);
        assert_eq!(t.text, "Example.jpg");
    }

    #[test]
    fn test_image_alias() {
        let config = test_config();
        let t = TitleParser::parse("Image:Example.jpg", &config);
        assert_eq!(t.namespace_id, 6);
        assert_eq!(t.text, "Example.jpg");
    }

    #[test]
    fn test_force_mainspace() {
        let config = test_config();
        let t = TitleParser::parse(":Category:People", &config);
        assert_eq!(t.namespace_id, 0);
        assert_eq!(t.text, "Category:People");
    }

    #[test]
    fn test_fragment() {
        let config = test_config();
        let t = TitleParser::parse("Foo#Section", &config);
        assert_eq!(t.namespace_id, 0);
        assert_eq!(t.text, "Foo");
        assert_eq!(t.fragment, Some("Section".to_string()));
    }

    #[test]
    fn test_interwiki() {
        let config = test_config();
        let t = TitleParser::parse("commons:File:Example.jpg", &config);
        assert_eq!(t.interwiki, Some("commons".to_string()));
        assert_eq!(t.namespace_id, 0);
        assert_eq!(t.text, "File:Example.jpg");
    }

    #[test]
    fn test_first_letter_capitalization() {
        let config = test_config();
        // Mainspace titles are first-letter case-insensitive: ucfirst is applied.
        let t = TitleParser::parse("foo", &config);
        assert_eq!(t.namespace_id, 0);
        assert_eq!(t.text, "Foo");

        // Template namespace is also first-letter case-insensitive.
        let t = TitleParser::parse("pre", &config);
        // (Note: no namespace prefix means mainspace; see the template flow.)
        assert_eq!(t.text, "Pre");

        let t = TitleParser::parse("template:pre", &config);
        assert_eq!(t.namespace_id, 10);
        assert_eq!(t.text, "Pre");

        // MediaWiki namespace is case-sensitive: no capitalization.
        let t = TitleParser::parse("MediaWiki:common.css", &config);
        assert_eq!(t.namespace_id, 8);
        assert_eq!(t.text, "common.css");
    }

    #[test]
    fn test_ucfirst_helper() {
        assert_eq!(ucfirst("", "en"), "");
        assert_eq!(ucfirst("foo", "en"), "Foo");
        assert_eq!(ucfirst("Foo", "en"), "Foo");
        assert_eq!(ucfirst("foo bar", "en"), "Foo bar");
        // Turkish: 'i' capitalizes to 'İ'.
        assert_eq!(ucfirst("istanbul", "tr"), "İstanbul");
    }

    #[test]
    fn test_module_title() {
        let config = test_config();
        let t = TitleParser::parse("Module:Citation", &config);
        assert_eq!(t.namespace_id, 828);
        assert_eq!(t.text, "Citation");
    }

    #[test]
    fn test_title_display() {
        let t = Title::new(10, "Foo");
        assert_eq!(t.to_string(), "Template:Foo");

        let t = Title::new_main("Main Page");
        assert_eq!(t.to_string(), "Main Page");
    }

    #[test]
    fn test_full_text_with_config() {
        let config = test_config();
        let t = Title::new(14, "People");
        assert_eq!(t.full_text_with_config(&config), "Category:People");
    }

    #[test]
    fn test_get_prefixed_text() {
        assert_eq!(
            Title::new_main("Main Page").get_prefixed_text(),
            "Main Page"
        );
        assert_eq!(Title::new(10, "Foo").get_prefixed_text(), "Template:Foo");
    }

    #[test]
    fn test_get_full_db_key() {
        assert_eq!(Title::new_main("Main Page").get_full_db_key(), "Main_Page");
        assert_eq!(
            Title::new(10, "Foo bar").get_full_db_key(),
            "Template:Foo_bar"
        );
    }

    #[test]
    fn test_make_link() {
        let config = test_config();
        assert_eq!(
            make_link(&Title::new_main("Main Page"), &config),
            "./Main_Page"
        );
        assert_eq!(make_link(&Title::new(10, "Foo"), &config), "./Template:Foo");
    }
}
