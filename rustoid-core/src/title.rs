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
}

/// Map a namespace ID to its canonical English prefix (fallback without SiteConfig).
fn namespace_prefix(ns_id: i32) -> &'static str {
    match ns_id {
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
            // Try to match interwiki prefix first
            for prefix in config.interwiki_map().keys() {
                if let Some(after) = rest
                    .strip_prefix(prefix.as_str())
                    .and_then(|s| s.strip_prefix(':'))
                {
                    return Title {
                        interwiki: Some(prefix.clone()),
                        namespace_id: 0,
                        text: after.to_string(),
                        fragment,
                    };
                }
            }

            // Try to match namespace prefix by canonical name or alias
            for (&ns_id, ns_info) in config.namespaces() {
                // Check canonical name first
                if let Some(title_part) = rest
                    .strip_prefix(&ns_info.canonical)
                    .and_then(|s| s.strip_prefix(':'))
                {
                    return Title {
                        interwiki: None,
                        namespace_id: ns_id,
                        text: title_part.to_string(),
                        fragment,
                    };
                }
                // Check localized aliases
                for alias in &ns_info.aliases {
                    if let Some(title_part) = rest
                        .strip_prefix(alias.as_str())
                        .and_then(|s| s.strip_prefix(':'))
                    {
                        return Title {
                            interwiki: None,
                            namespace_id: ns_id,
                            text: title_part.to_string(),
                            fragment,
                        };
                    }
                }
            }
        }

        // No namespace prefix matched — return mainspace title.
        Title {
            interwiki: None,
            namespace_id: 0,
            text: rest.to_string(),
            fragment,
        }
    }
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
}
