/// Page title handling with namespace-aware parsing and comparison.
use serde::{Deserialize, Serialize};
use std::fmt;

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

    /// Create a simple Title from text, assuming namespace 0.
    /// This is a convenience constructor; use `TitleParser` for full parsing.
    pub fn new(namespace_id: i32, text: impl Into<String>) -> Self {
        Self {
            interwiki: None,
            namespace_id,
            text: text.into(),
            fragment: None,
        }
    }

    /// The full page name including namespace prefix (e.g. `"Template:Foo"`).
    pub fn full_text(&self) -> String {
        // TODO: resolve namespace name from SiteConfig.
        // For now, use canonical namespace names for well-known IDs.
        match self.namespace_id {
            0 => self.text.clone(),
            1 => format!("Talk:{}", self.text),
            2 => format!("User:{}", self.text),
            3 => format!("User talk:{}", self.text),
            4 => format!("Project:{}", self.text),
            6 => format!("File:{}", self.text),
            8 => format!("MediaWiki:{}", self.text),
            10 => format!("Template:{}", self.text),
            12 => format!("Help:{}", self.text),
            14 => format!("Category:{}", self.text),
            828 => format!("Module:{}", self.text),
            _ => self.text.clone(),
        }
    }
}

impl fmt::Display for Title {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        if let Some(ref iw) = self.interwiki {
            s.push_str(iw);
            s.push(':');
        }
        if self.namespace_id != 0 {
            // Use canonical prefix
            let prefix = match self.namespace_id {
                1 => "Talk",
                2 => "User",
                3 => "User talk",
                4 => "Project",
                6 => "File",
                8 => "MediaWiki",
                10 => "Template",
                12 => "Help",
                14 => "Category",
                828 => "Module",
                _ => "",
            };
            if !prefix.is_empty() {
                s.push_str(prefix);
                s.push(':');
            }
        }
        s.push_str(&self.text);
        if let Some(ref frag) = self.fragment {
            s.push('#');
            s.push_str(frag);
        }
        write!(f, "{s}")
    }
}

/// Parser for constructing `Title` from a string, given namespace/prefix context.
///
/// This is a simplified parser; the full version will use `SiteConfig` for
/// namespace alias resolution and interwiki prefix matching.
pub struct TitleParser;

impl TitleParser {
    /// Parse a title string into a `Title`.
    ///
    /// Handles:
    /// - Leading `:` to force mainspace (e.g. `":Category:Foo"` → mainspace `"Category:Foo"`).
    /// - Namespace prefixes (only canonical English names for now).
    /// - Interwiki prefixes (delegates to `SiteConfig`).
    /// - URL fragments (`#Section`).
    pub fn parse(input: &str) -> Title {
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

        // For now, only handle canonical English namespace prefixes.
        let namespace_names: &[(i32, &str)] = &[
            (6, "File"),
            (6, "Image"),
            (10, "Template"),
            (14, "Category"),
            (12, "Help"),
            (828, "Module"),
        ];

        for &(ns_id, prefix) in namespace_names {
            if let Some(title_part) = rest.strip_prefix(prefix).and_then(|s| s.strip_prefix(':'))
                && !force_main
            {
                return Title {
                    interwiki: None,
                    namespace_id: ns_id,
                    text: title_part.to_string(),
                    fragment,
                };
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

    #[test]
    fn test_mainspace_title() {
        let t = TitleParser::parse("Main Page");
        assert_eq!(t.namespace_id, 0);
        assert_eq!(t.text, "Main Page");
        assert!(t.fragment.is_none());
    }

    #[test]
    fn test_template_title() {
        let t = TitleParser::parse("Template:Foo");
        assert_eq!(t.namespace_id, 10);
        assert_eq!(t.text, "Foo");
    }

    #[test]
    fn test_force_mainspace() {
        let t = TitleParser::parse(":Category:People");
        assert_eq!(t.namespace_id, 0);
        assert_eq!(t.text, "Category:People");
    }

    #[test]
    fn test_fragment() {
        let t = TitleParser::parse("Foo#Section");
        assert_eq!(t.namespace_id, 0);
        assert_eq!(t.text, "Foo");
        assert_eq!(t.fragment, Some("Section".to_string()));
    }
}
